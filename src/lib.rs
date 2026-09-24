//! The file browser: the filesystem as a list of lists.
//!
//! A sicompass WASM plugin. It asks for the whole disk (`"filesystem": ["/"]`
//! in `plugin.json`, shown at install), so the sandbox preopens `/` at its real
//! path and `std::fs` works as it always did:
//!
//! - Root is `/`.
//! - Each directory entry is wrapped in `<input>name</input>` so the user can
//!   rename it inline. Directories are `Obj`, files are `Str`.
//! - Commands: create directory, create file, show/hide properties, show/hide
//!   hidden files, sort alphanumerically or chronologically.
//! - `commit_edit(old, new)` renames, `delete_item` moves to the OS trash
//!   through the host (`desktop.trash`), `copy_item` copies recursively.
//! - Extended search is a BFS walk (up to 50 000 results).
//!
//! A delete is undoable: the item is snapshotted first
//! (`sicompass_sdk::fs_snapshot`) and the snapshot rides in the `ProviderOp`
//! the app keeps on its timeline, so an undo writes it back even after the
//! trash was emptied. Too large to snapshot, it asks `desktop.restore`.
//!
//! What the sandbox does not have: Unix permission bits and owner names (the
//! properties view shows size and date there), and the list of the system's
//! applications, so there is no "open file with" (the app opens a file with
//! its default program itself).

mod desktop;
pub mod localize;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use desktop::{Desktop, HostDesktop};
use sicompass_pdk::{Descriptor, Plugin, ProviderOp, SearchResult, export_plugin};
use sicompass_sdk::ffon::FfonElement;
use sicompass_sdk::fs_snapshot;
use sicompass_sdk::placeholders::new_obj_with_i_placeholder;
use sicompass_sdk::tags;
use sicompass_sdk::timeline::FsSideEffect;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The `ProviderOp` command of an undoable delete.
const OP_DELETE: &str = "delete";

// ---------------------------------------------------------------------------
// Sort mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    Alpha,
    Chrono,
}

// ---------------------------------------------------------------------------
// FilebrowserProvider
// ---------------------------------------------------------------------------

pub struct FilebrowserProvider {
    current_path: PathBuf,
    show_properties: bool,
    /// Toggled by the `show/hide hidden files` command. Dot-prefixed entries
    /// are hidden on every platform, not just Unix: Windows marks hidden with
    /// a file attribute instead, but a dot prefix is what the rest of the app
    /// and the user's `.gitignore`-style files assume.
    show_hidden: bool,
    sort_mode: SortMode,
    /// Undoable deletes since the last drain, each carrying its snapshot.
    /// Create, rename and paste are recorded by the app itself.
    pending_timeline_entries: Vec<ProviderOp>,
    /// The OS trash, through the host (a fake in the tests).
    desktop: Box<dyn Desktop>,
    error: Option<String>,
}

impl FilebrowserProvider {
    pub fn with_desktop(desktop: Box<dyn Desktop>) -> Self {
        FilebrowserProvider {
            current_path: PathBuf::from("/"),
            show_properties: false,
            show_hidden: false,
            sort_mode: SortMode::Alpha,
            pending_timeline_entries: Vec::new(),
            desktop,
            error: None,
        }
    }

    /// The folder on screen, through any symlinks along its path, as the
    /// sandbox needs it (see `sicompass_sdk::fs_links`).
    fn dir(&self) -> PathBuf {
        self.desktop.resolve(&self.current_path)
    }

    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    /// The path an undoable delete names, absolute and recorded at the time:
    /// the cursor may have moved since, so it is never rebuilt from the
    /// current directory.
    fn deleted_path(side_effect: &FsSideEffect) -> Option<&Path> {
        match side_effect {
            FsSideEffect::TrashedFile { original_path, .. }
            | FsSideEffect::TrashedDir { original_path, .. } => Some(original_path),
            FsSideEffect::RenameOnly { from, .. } => Some(from),
            FsSideEffect::None => None,
        }
    }

    fn list_directory(&self) -> Vec<FfonElement> {
        #[cfg(windows)]
        {
            if self.current_path == Path::new("/") {
                return list_drives();
            }
        }
        let path = &self.current_path;
        let mut raw = collect_raw_entries(&self.desktop.resolve(path), &*self.desktop);

        if !self.show_hidden {
            raw.retain(|e| !e.name.starts_with('.'));
        }

        match self.sort_mode {
            SortMode::Alpha => raw.sort_by(|a, b| natord::compare_ignore_case(&a.name, &b.name)),
            SortMode::Chrono => raw.sort_by_key(|e| std::cmp::Reverse(e.mtime)),
        }

        let mut out = Vec::with_capacity(raw.len());
        for entry in &raw {
            let prop = if self.show_properties {
                format_properties(entry)
            } else {
                String::new()
            };
            let label = format!(
                "{}{}<input>{}</input>",
                prop,
                // no extra prefix beyond property string
                "",
                entry.name,
            );
            let elem = if entry.is_dir {
                FfonElement::new_obj(&label)
            } else {
                FfonElement::Str(label)
            };
            out.push(elem);
        }
        out
    }
}

impl Plugin for FilebrowserProvider {
    fn new() -> Self {
        FilebrowserProvider::with_desktop(Box::new(HostDesktop))
    }

    fn describe(&self) -> Descriptor {
        Descriptor {
            name: "filebrowser".to_owned(),
            display_name: localize::t("filebrowser-display-name"),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            // The generic structural-edit keymap. Ctrl+D and Delete still reach
            // the app's file delete, and Ctrl+X/Ctrl+V its file clipboard.
            supports_structural_edit: true,
            path_is_filesystem: true,
            ..Default::default()
        }
    }

    fn init(&mut self) {
        self.current_path = PathBuf::from("/");
        if let Some(v) = sicompass_pdk::host::get_setting("sortOrder") {
            self.on_setting_change("sortOrder", &v);
        }
    }

    fn poll(&mut self) -> sicompass_pdk::PollResult {
        sicompass_pdk::PollResult {
            at_root: self.current_path == Path::new("/"),
            error: self.take_error(),
            structural_edit_here: true,
            dashboard_here: false,
            ..Default::default()
        }
    }

    fn fetch(&mut self) -> Vec<FfonElement> {
        self.list_directory()
    }

    fn push_path(&mut self, segment: &str) {
        #[cfg(windows)]
        {
            if self.current_path == Path::new("/") {
                // Pushing a drive letter from the sentinel
                self.current_path = PathBuf::from(segment);
                return;
            }
        }
        self.current_path
            .push(segment.trim_end_matches('/').trim_end_matches('\\'));
    }

    fn pop_path(&mut self) {
        #[cfg(windows)]
        {
            // At drive root (e.g. "C:\") → return to sentinel "/"
            if is_drive_root(&self.current_path) {
                self.current_path = PathBuf::from("/");
                return;
            }
            if self.current_path == Path::new("/") {
                return;
            }
        }
        if self.current_path.parent().is_some() && self.current_path != Path::new("/") {
            self.current_path.pop();
        }
    }

    fn current_path(&self) -> &str {
        self.current_path.to_str().unwrap_or("/")
    }

    fn set_current_path(&mut self, path: &str) {
        self.current_path = PathBuf::from(path);
    }

    fn on_setting_change(&mut self, key: &str, value: &str) {
        if key == "sortOrder" {
            self.sort_mode = match value {
                "chronologically" => SortMode::Chrono,
                _ => SortMode::Alpha,
            };
        }
    }

    fn commit_edit(&mut self, old: &str, new_content: &str) -> bool {
        let old_name = tags::strip_display(old);
        let new_name = tags::strip_display(new_content);

        if old_name.is_empty() {
            // Committing an `i` placeholder — treat as a create.
            // The generic handler appends `:` when the user typed `+name` or `name:`.
            if let Some(dir_name) = new_name.strip_suffix(':') {
                if dir_name.is_empty() {
                    return false;
                }
                return self.create_directory(dir_name);
            }
            if new_name.is_empty() {
                return false;
            }
            return self.create_file(&new_name);
        }

        if old_name == new_name {
            return false;
        }
        // The folder through any symlinks, the entries themselves as they are:
        // renaming a link renames the link.
        let dir = self.dir();
        let old_path = dir.join(old_name.trim_end_matches('/').trim_end_matches('\\'));
        let new_path = dir.join(new_name.trim_end_matches('/').trim_end_matches('\\'));
        std::fs::rename(&old_path, &new_path).is_ok()
    }

    fn delete_item(&mut self, name: &str) -> bool {
        let name_clean = entry_name(name);
        let name_clean = name_clean
            .trim_end_matches('/')
            .trim_end_matches('\\')
            .to_owned();
        let full = self.dir().join(&name_clean);

        // Snapshot the target before deletion so an undo can restore even if
        // the OS trash has been emptied (see `sicompass_sdk::fs_snapshot`).
        let side_effect = fs_snapshot::snapshot_for_delete(&full);
        if self.desktop.trash(&full).is_err() {
            return false;
        }
        self.pending_timeline_entries.push(ProviderOp {
            command: OP_DELETE.to_owned(),
            payload: encode_side_effect(&side_effect),
            label: format!("delete {name_clean}"),
        });
        true
    }

    fn take_timeline_entries(&mut self) -> Vec<ProviderOp> {
        std::mem::take(&mut self.pending_timeline_entries)
    }

    /// Put a deleted item back: its snapshot, or the OS trash when it was too
    /// large to keep.
    fn undo(&mut self, entry: &ProviderOp) -> Result<(), String> {
        let Some(side_effect) = decode_side_effect(entry) else {
            return Ok(());
        };
        let desktop = &self.desktop;
        fs_snapshot::restore(&side_effect, |p| desktop.restore(p))
    }

    /// Move it to the trash again, at the absolute path recorded at the time.
    fn redo(&mut self, entry: &ProviderOp) -> Result<(), String> {
        let Some(side_effect) = decode_side_effect(entry) else {
            return Ok(());
        };
        let Some(path) = Self::deleted_path(&side_effect) else {
            return Ok(());
        };
        self.desktop.trash(path).map_err(|e| {
            let mut args = localize::Args::new();
            args.set("err", e);
            localize::t_args("filebrowser-error-redo-delete-trash-failed", &args)
        })
    }

    fn create_directory(&mut self, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        let full = self.dir().join(name);
        std::fs::create_dir(&full).is_ok()
    }

    fn create_file(&mut self, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        let full = self.dir().join(name);
        std::fs::File::create(&full).is_ok()
    }

    fn copy_item(
        &mut self,
        src_dir: &str,
        src_name: &str,
        dest_dir: &str,
        dest_name: &str,
    ) -> bool {
        let src = self
            .desktop
            .resolve(Path::new(src_dir))
            .join(src_name.trim_end_matches('/').trim_end_matches('\\'));
        let dst = self
            .desktop
            .resolve(Path::new(dest_dir))
            .join(dest_name.trim_end_matches('/').trim_end_matches('\\'));
        copy_recursive(&src, &dst)
    }

    fn commands(&self) -> Vec<String> {
        vec![
            "create directory".into(),
            "create file".into(),
            "show/hide properties".into(),
            "show/hide hidden files".into(),
            "sort alphanumerically".into(),
            "sort chronologically".into(),
        ]
    }

    fn handle_command(
        &mut self,
        command: &str,
        _element_key: &str,
        _element_type: i32,
    ) -> Result<Option<FfonElement>, String> {
        Ok(match command {
            "create directory" => Some(new_obj_with_i_placeholder("<input></input>")),
            "create file" => Some(FfonElement::Str("<input></input>".into())),
            "show/hide properties" => {
                self.show_properties = !self.show_properties;
                None
            }
            "show/hide hidden files" => {
                self.show_hidden = !self.show_hidden;
                None
            }
            "sort alphanumerically" => {
                self.sort_mode = SortMode::Alpha;
                None
            }
            "sort chronologically" => {
                self.sort_mode = SortMode::Chrono;
                None
            }
            _ => None,
        })
    }

    fn collect_extended_search_items(&self) -> Option<Vec<SearchResult>> {
        Some(self.run_extended_search())
    }
}

export_plugin!(FilebrowserProvider);

/// A delete's snapshot, as a `ProviderOp` payload: FFON text, so the bytes
/// travel base64-encoded.
fn encode_side_effect(side_effect: &FsSideEffect) -> Vec<u8> {
    let text = B64.encode(fs_snapshot::encode(side_effect));
    sicompass_pdk::encode_one(&FfonElement::Str(text))
}

/// The inverse; `None` for an entry that is not one of this plugin's deletes.
fn decode_side_effect(entry: &ProviderOp) -> Option<FsSideEffect> {
    if entry.command != OP_DELETE {
        return None;
    }
    let payload = sicompass_pdk::decode_one(&entry.payload)?;
    let bytes = B64.decode(payload.as_str()?).ok()?;
    fs_snapshot::decode(&bytes)
}

impl FilebrowserProvider {
    fn run_extended_search(&self) -> Vec<SearchResult> {
        const MAX_ITEMS: usize = 50_000;
        let mut results = Vec::new();
        let root = self.dir();

        // BFS queue: (dir_path, breadcrumb)
        let mut queue: std::collections::VecDeque<(PathBuf, String)> =
            std::collections::VecDeque::new();
        queue.push_back((root, String::new()));

        while let Some((dir, breadcrumb)) = queue.pop_front() {
            if results.len() >= MAX_ITEMS {
                break;
            }

            let Ok(names) = sicompass_pdk::fs::list_dir(&dir) else {
                continue;
            };

            for name in names {
                if results.len() >= MAX_ITEMS {
                    break;
                }
                let entry_path = dir.join(&name);
                // Agree with `list_directory`: an extended search that surfaced the
                // contents of `.git` while the listing hides it would be both
                // confusing and, on a large repo, most of the item budget.
                if !self.show_hidden && name.starts_with('.') {
                    continue;
                }
                // Use symlink_metadata to avoid following symlinks (guards against loops)
                let meta = match entry_path.clone().symlink_metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let is_dir = meta.is_dir();
                let label = if is_dir {
                    format!("+ {name}")
                } else {
                    format!("- {name}")
                };
                let nav_path = entry_path.clone().to_string_lossy().into_owned();
                results.push(SearchResult {
                    label,
                    breadcrumb: breadcrumb.clone(),
                    nav_path: nav_path.clone(),
                });

                if is_dir {
                    let child_bc = if breadcrumb.is_empty() {
                        format!("{name} > ")
                    } else {
                        format!("{breadcrumb}{name} > ")
                    };
                    queue.push_back((entry_path.clone(), child_bc));
                }
            }
        }

        results
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Raw directory entry
// ---------------------------------------------------------------------------

struct RawEntry {
    name: String,
    mtime: SystemTime,
    is_dir: bool,
    size: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    nlink: u64,
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
}

/// The on-disk name of an entry given its rendered label.
///
/// Entries are wrapped as `<prop><input>name</input>` — with "show properties"
/// on, a permissions/owner/size prefix sits *outside* the `<input>` tag, and
/// `strip_display` would keep it (yielding `drwxr-xr-x … name`). The `<input>`
/// content is the true filename, so prefer it whenever the tag is present and
/// fall back to `strip_display` for labels without one.
fn entry_name(label: &str) -> String {
    if tags::has_input(label) {
        tags::extract_input(label).unwrap_or_else(|| tags::strip_display(label))
    } else {
        tags::strip_display(label)
    }
}

/// The entries of `path`, through `sicompass_pdk::fs::list_dir`: in a folder
/// other programs change, `std::fs::read_dir` inside the sandbox stops at the
/// first entry that vanished and loses every entry after it. An entry that
/// vanishes between listing and reading its metadata is left out.
fn collect_raw_entries(path: &Path, desktop: &dyn Desktop) -> Vec<RawEntry> {
    let Ok(names) = sicompass_pdk::fs::list_dir(path) else {
        return Vec::new();
    };

    let mut entries = Vec::new();
    for name in names {
        let entry_path = path.join(&name);
        // `symlink_metadata` does not traverse symlinks, which is what we
        // want for the *properties* view: `format_properties` renders the
        // link's own mode as a leading `l`, and its own size and mtime, the
        // way `ls -l` does.
        let meta = match std::fs::symlink_metadata(&entry_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        // Navigability is the one thing that has to look through the link.
        // `is_dir` alone decides whether the entry becomes an `Obj` (the user
        // can arrow into it, and `navigate_to_path` can walk through it) or a
        // `Str`, and a symlink to a directory is a directory to anyone using
        // it. `delete_item` and `copy_recursive` already follow links, so
        // without this the listing contradicts them. A broken link resolves to
        // nothing and stays a plain entry.
        let is_dir = if meta.file_type().is_symlink() {
            std::fs::metadata(desktop.resolve(&entry_path))
                .map(|m| m.is_dir())
                .unwrap_or(false)
        } else {
            meta.is_dir()
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            entries.push(RawEntry {
                name,
                mtime,
                is_dir,
                size: meta.size(),
                mode: meta.mode(),
                nlink: meta.nlink(),
                uid: meta.uid(),
                gid: meta.gid(),
            });
        }
        #[cfg(not(unix))]
        entries.push(RawEntry {
            name,
            mtime,
            is_dir,
            size: meta.len(),
        });
    }
    entries
}

// ---------------------------------------------------------------------------
// Property formatting (Unix)
// ---------------------------------------------------------------------------

#[cfg(unix)]
// The `libc::S_*` casts are no-ops on Linux but not on macOS, where the
// constants are `u16`; one source for both.
#[allow(clippy::unnecessary_cast)]
fn format_properties(e: &RawEntry) -> String {
    use libc::{getgrgid, getpwuid};
    use std::ffi::CStr;

    // Permission string (e.g. "drwxr-xr-x"). The `libc::S_*` constants are
    // `u16` on macOS but `u32` on Linux, so cast them to match `mode`.
    let mode = e.mode;
    let mut perm = [b'-'; 10];
    perm[0] = if mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32 {
        b'd'
    } else if mode & libc::S_IFMT as u32 == libc::S_IFLNK as u32 {
        b'l'
    } else {
        b'-'
    };
    perm[1] = if mode & libc::S_IRUSR as u32 != 0 {
        b'r'
    } else {
        b'-'
    };
    perm[2] = if mode & libc::S_IWUSR as u32 != 0 {
        b'w'
    } else {
        b'-'
    };
    perm[3] = if mode & libc::S_IXUSR as u32 != 0 {
        b'x'
    } else {
        b'-'
    };
    perm[4] = if mode & libc::S_IRGRP as u32 != 0 {
        b'r'
    } else {
        b'-'
    };
    perm[5] = if mode & libc::S_IWGRP as u32 != 0 {
        b'w'
    } else {
        b'-'
    };
    perm[6] = if mode & libc::S_IXGRP as u32 != 0 {
        b'x'
    } else {
        b'-'
    };
    perm[7] = if mode & libc::S_IROTH as u32 != 0 {
        b'r'
    } else {
        b'-'
    };
    perm[8] = if mode & libc::S_IWOTH as u32 != 0 {
        b'w'
    } else {
        b'-'
    };
    perm[9] = if mode & libc::S_IXOTH as u32 != 0 {
        b'x'
    } else {
        b'-'
    };
    let perm_str = std::str::from_utf8(&perm).unwrap_or("----------");

    // Owner and group names (fall back to numeric ids)
    let owner = unsafe {
        let pw = getpwuid(e.uid);
        if !pw.is_null() {
            CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned()
        } else {
            e.uid.to_string()
        }
    };
    let group = unsafe {
        let gr = getgrgid(e.gid);
        if !gr.is_null() {
            CStr::from_ptr((*gr).gr_name).to_string_lossy().into_owned()
        } else {
            e.gid.to_string()
        }
    };

    // Date formatted like ls -l: "Mon DD HH:MM" (recent) or "Mon DD  YYYY" (older)
    let mtime_secs = e
        .mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as libc::time_t;
    let date_str = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&mtime_secs, &mut tm);
        let fmt = if now - mtime_secs < 6 * 30 * 24 * 3600 {
            c"%b %e %H:%M".as_ptr()
        } else {
            c"%b %e  %Y".as_ptr()
        };
        let mut buf = [0i8; 16];
        libc::strftime(buf.as_mut_ptr(), buf.len(), fmt, &tm);
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    };

    format!(
        "{} {:2} {:<8} {:<8} {:5} {} ",
        perm_str, e.nlink, owner, group, e.size, date_str
    )
}

#[cfg(not(unix))]
fn format_properties(e: &RawEntry) -> String {
    let secs = e
        .mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
    let (y, mo, d) = civil_from_days(days);
    let h = (secs % 86400) / 3600;
    let mi = (secs % 3600) / 60;
    format!(
        "{:>9} {:04}-{:02}-{:02} {:02}:{:02} ",
        e.size, y, mo, d, h, mi
    )
}

// Howard Hinnant's civil_from_days: converts days-since-1970-01-01 to (year,
// month, day) in the proleptic Gregorian calendar. Used for UTC date display
// on Windows where libc::localtime_r is unavailable.
#[cfg(not(unix))]
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 {
        (mp + 3) as u32
    } else {
        (mp - 9) as u32
    };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

fn copy_recursive(src: &Path, dst: &Path) -> bool {
    if src.is_dir() {
        if std::fs::create_dir_all(dst).is_err() {
            return false;
        }
        let rd = match std::fs::read_dir(src) {
            Ok(r) => r,
            Err(_) => return false,
        };
        for entry in rd.flatten() {
            let child_dst = dst.join(entry.file_name());
            if !copy_recursive(&entry.path(), &child_dst) {
                return false;
            }
        }
        true
    } else {
        std::fs::copy(src, dst).is_ok()
    }
}

// ---------------------------------------------------------------------------
// Windows helpers
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn is_drive_root(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.len() == 3 && s.as_bytes()[1] == b':' && (s.as_bytes()[2] == b'\\' || s.as_bytes()[2] == b'/')
}

#[cfg(windows)]
fn list_drives() -> Vec<FfonElement> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
    }
    // SAFETY: GetLogicalDrives takes no arguments and has no failure mode
    // other than returning 0 (no drives), which we handle gracefully.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) != 0 {
            let letter = (b'A' + i as u8) as char;
            let drive = format!("{}:\\", letter);
            let label = format!("<input>{}</input>", drive);
            out.push(FfonElement::new_obj(&label));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests — port of tests/lib_filebrowser/ (25 + 27 tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sicompass_sdk::fs_snapshot::TRASH_SNAPSHOT_LIMIT_BYTES;
    use std::cell::RefCell;
    use std::rc::Rc;
    use tempfile::TempDir;

    /// The OS trash, simulated: a trashed item is moved into a folder of its
    /// own and can be restored from it, newest first. Nothing a test deletes
    /// ever reaches the developer's real trash.
    #[derive(Clone, Default)]
    struct FakeDesktop(Rc<RefCell<FakeTrash>>);

    #[derive(Default)]
    struct FakeTrash {
        dir: Option<TempDir>,
        /// (original path, where it is kept), oldest first.
        items: Vec<(PathBuf, PathBuf)>,
        restores: Vec<PathBuf>,
        /// Answer restores with this refusal, as macOS's trash does.
        refuse_restore: Option<String>,
    }

    impl Desktop for FakeDesktop {
        fn trash(&self, path: &Path) -> Result<(), String> {
            let mut t = self.0.borrow_mut();
            std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let dir = t
                .dir
                .get_or_insert_with(|| TempDir::new().unwrap())
                .path()
                .to_path_buf();
            let kept = dir.join(t.items.len().to_string());
            std::fs::rename(path, &kept).map_err(|e| e.to_string())?;
            t.items.push((path.to_path_buf(), kept));
            Ok(())
        }

        fn restore(&self, path: &Path) -> Result<(), String> {
            let mut t = self.0.borrow_mut();
            t.restores.push(path.to_path_buf());
            if let Some(why) = &t.refuse_restore {
                return Err(why.clone());
            }
            let at = t
                .items
                .iter()
                .rposition(|(orig, _)| orig == path)
                .ok_or("no matching item in the trash")?;
            let (orig, kept) = t.items.remove(at);
            std::fs::rename(kept, orig).map_err(|e| e.to_string())
        }
    }

    /// A file browser on the fake trash.
    fn fb() -> FilebrowserProvider {
        FilebrowserProvider::with_desktop(Box::new(FakeDesktop::default()))
    }

    fn fb_on(desktop: &FakeDesktop) -> FilebrowserProvider {
        FilebrowserProvider::with_desktop(Box::new(desktop.clone()))
    }

    fn make_provider() -> (FilebrowserProvider, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut p = fb();
        p.set_current_path(dir.path().to_str().unwrap());
        (p, dir)
    }

    /// Outside the sandbox there is no trash at all: the plugin's own host
    /// desktop refuses rather than deleting anything, so a native test can only
    /// ever reach the fake.
    #[test]
    fn natively_the_host_desktop_touches_nothing() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("x.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(HostDesktop.trash(&file).is_err());
        assert!(file.exists());
        let mut p = FilebrowserProvider::new();
        p.set_current_path(dir.path().to_str().unwrap());
        assert!(
            !p.delete_item("x.txt"),
            "a delete the trash refused is not done"
        );
        assert!(file.exists());
    }

    /// The fake must really take the item away, or every `!path.exists()`
    /// assertion in this module is inert; and a path that is not there stays
    /// an error, so `delete_item` keeps returning false for it.
    #[test]
    fn the_fake_trash_takes_items_away() {
        let dir = TempDir::new().unwrap();
        let d = FakeDesktop::default();
        let file = dir.path().join("x.txt");
        std::fs::write(&file, b"x").unwrap();
        d.trash(&file).unwrap();
        assert!(!file.exists());
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner"), b"x").unwrap();
        d.trash(&sub).unwrap();
        assert!(!sub.exists());
        assert!(d.trash(&dir.path().join("never-existed")).is_err());
    }

    #[test]
    fn test_fetch_empty_dir_only_meta() {
        let (mut p, _dir) = make_provider();
        let items = p.fetch();
        assert_eq!(items.len(), 0);
    }

    #[test]
    fn test_fetch_file_is_str() {
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
        let items = p.fetch();
        assert!(!items.is_empty());
        assert!(items[0].as_str().is_some());
    }

    #[test]
    fn test_fetch_dir_is_obj() {
        let (mut p, dir) = make_provider();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let items = p.fetch();
        assert!(!items.is_empty());
        assert!(items[0].as_obj().is_some());
    }

    #[test]
    fn test_fetch_item_wrapped_in_input_tag() {
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("notes.txt"), b"").unwrap();
        let items = p.fetch();
        let label = items[0].as_str().unwrap();
        assert!(tags::has_input(label));
        assert_eq!(tags::strip_display(label), "notes.txt");
    }

    // ---- sort modes --------------------------------------------------------

    #[test]
    fn test_sort_alpha() {
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("zebra.txt"), b"").unwrap();
        std::fs::write(dir.path().join("apple.txt"), b"").unwrap();
        p.sort_mode = SortMode::Alpha;
        let items = p.fetch();
        let names: Vec<_> = items
            .iter()
            .map(|e| {
                tags::strip_display(
                    e.as_str()
                        .or_else(|| e.as_obj().map(|o| o.key.as_str()))
                        .unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            names,
            vec!["apple.txt".to_string(), "zebra.txt".to_string()]
        );
    }

    // ---- rename (commit_edit) ---------------------------------------------

    #[test]
    fn test_rename_file() {
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("old.txt"), b"").unwrap();
        let ok = p.commit_edit("<input>old.txt</input>", "<input>new.txt</input>");
        assert!(ok);
        assert!(dir.path().join("new.txt").exists());
        assert!(!dir.path().join("old.txt").exists());
    }

    #[test]
    fn test_rename_same_name_returns_false() {
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("file.txt"), b"").unwrap();
        let ok = p.commit_edit("<input>file.txt</input>", "<input>file.txt</input>");
        assert!(!ok);
    }

    // ---- create_file / create_directory -----------------------------------

    #[test]
    fn test_create_file() {
        let (mut p, dir) = make_provider();
        assert!(p.create_file("new_file.txt"));
        assert!(dir.path().join("new_file.txt").exists());
    }

    #[test]
    fn test_create_directory() {
        let (mut p, dir) = make_provider();
        assert!(p.create_directory("new_dir"));
        assert!(dir.path().join("new_dir").is_dir());
    }

    #[test]
    fn test_create_file_empty_name_fails() {
        let (mut p, _dir) = make_provider();
        assert!(!p.create_file(""));
    }

    // ---- delete_item -------------------------------------------------------

    #[test]
    fn test_delete_file() {
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("del.txt"), b"x").unwrap();
        assert!(p.delete_item("<input>del.txt</input>"));
        assert!(!dir.path().join("del.txt").exists());
    }

    #[test]
    fn test_delete_file_with_properties_prefix() {
        // With "show properties" on, the label carries a permissions/owner/size
        // prefix *outside* the <input> tag. Deletion must still target the bare
        // filename, not "drwxr-xr-x … del.txt".
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("del.txt"), b"x").unwrap();
        let label = "-rw-r--r-- 1 user group 1 May 31 12:00 <input>del.txt</input>";
        assert!(p.delete_item(label));
        assert!(!dir.path().join("del.txt").exists());
    }

    #[test]
    fn test_entry_name_strips_properties_prefix() {
        assert_eq!(entry_name("<input>file.txt</input>"), "file.txt");
        assert_eq!(
            entry_name("drwxr-xr-x 1 user group 4096 May 31 12:00 <input>Downloads</input>"),
            "Downloads"
        );
        // No input tag → fall back to strip_display.
        assert_eq!(entry_name("plain"), "plain");
    }

    #[test]
    fn test_delete_directory_recursive() {
        let (mut p, dir) = make_provider();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("file.txt"), b"x").unwrap();
        assert!(p.delete_item("<input>sub</input>"));
        assert!(!sub.exists());
    }

    // ---- copy_item ---------------------------------------------------------

    #[test]
    fn test_copy_file() {
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("src.txt"), b"hello").unwrap();
        let src_dir = dir.path().to_str().unwrap();
        let dst_dir = dir.path().to_str().unwrap();
        assert!(p.copy_item(src_dir, "src.txt", dst_dir, "dst.txt"));
        assert!(dir.path().join("dst.txt").exists());
    }

    // ---- navigation --------------------------------------------------------

    #[test]
    fn test_push_pop_path() {
        let (mut p, dir) = make_provider();
        std::fs::create_dir(dir.path().join("child")).unwrap();
        p.push_path("child");
        assert!(p.current_path().ends_with("child"));
        p.pop_path();
        assert_eq!(p.current_path(), dir.path().to_str().unwrap());
    }

    #[test]
    fn test_pop_at_root_is_noop() {
        let mut p = fb();
        p.pop_path();
        assert_eq!(p.current_path(), "/");
    }

    // ---- commands ----------------------------------------------------------

    #[test]
    fn test_commands_list() {
        let p = fb();
        let cmds = p.commands();
        assert!(cmds.contains(&"create directory".to_string()));
        assert!(cmds.contains(&"create file".to_string()));
        assert!(cmds.contains(&"show/hide properties".to_string()));
    }

    #[test]
    fn test_handle_command_create_file_returns_input_elem() {
        let (mut p, _dir) = make_provider();
        let result = p.handle_command("create file", "", 0).unwrap();
        assert!(result.is_some());
        let elem = result.unwrap();
        assert!(elem.as_str().is_some());
    }

    #[test]
    fn test_handle_command_create_directory_returns_obj() {
        let (mut p, _dir) = make_provider();
        let result = p.handle_command("create directory", "", 0).unwrap();
        let elem = result.unwrap();
        let obj = elem.as_obj().expect("create directory must return an Obj");
        assert_eq!(
            obj.children.len(),
            1,
            "new directory Obj must have exactly one child"
        );
        assert!(
            matches!(&obj.children[0], FfonElement::Str(s) if s == sicompass_sdk::placeholders::I_PLACEHOLDER),
            "child must be I_PLACEHOLDER, got: {:?}",
            obj.children[0]
        );
    }

    // ---- commit_edit (create on empty old) ------------------------------------

    #[test]
    fn test_commit_edit_empty_old_creates_file() {
        let (mut p, dir) = make_provider();
        let ok = p.commit_edit("", "notes.txt");
        assert!(ok, "commit_edit with empty old should create the file");
        assert!(
            dir.path().join("notes.txt").exists(),
            "notes.txt should exist on disk"
        );
    }

    #[test]
    fn test_commit_edit_empty_old_creates_directory() {
        let (mut p, dir) = make_provider();
        let ok = p.commit_edit("", "subdir:");
        assert!(
            ok,
            "commit_edit with empty old and trailing colon should create a directory"
        );
        assert!(
            dir.path().join("subdir").is_dir(),
            "subdir should exist as a directory"
        );
    }

    #[test]
    fn test_commit_edit_empty_old_empty_new_returns_false() {
        let (mut p, _dir) = make_provider();
        assert!(
            !p.commit_edit("", ""),
            "empty old + empty new must return false"
        );
        assert!(
            !p.commit_edit("", ":"),
            "empty old + colon-only new must return false"
        );
    }

    #[test]
    fn test_commit_edit_rename_still_works() {
        let (mut p, dir) = make_provider();
        std::fs::File::create(dir.path().join("alpha.txt")).unwrap();
        let ok = p.commit_edit("alpha.txt", "beta.txt");
        assert!(ok, "rename should succeed");
        assert!(
            !dir.path().join("alpha.txt").exists(),
            "alpha.txt should be gone"
        );
        assert!(
            dir.path().join("beta.txt").exists(),
            "beta.txt should exist"
        );
    }

    #[test]
    fn test_handle_command_toggle_properties() {
        let (mut p, _dir) = make_provider();
        assert!(!p.show_properties);
        p.handle_command("show/hide properties", "", 0).unwrap();
        assert!(p.show_properties);
        p.handle_command("show/hide properties", "", 0).unwrap();
        assert!(!p.show_properties);
    }

    /// Names of the entries the provider currently lists, tags stripped.
    fn entry_names(p: &mut FilebrowserProvider) -> Vec<String> {
        p.fetch()
            .iter()
            .map(|e| {
                tags::strip_display(
                    e.as_str()
                        .or_else(|| e.as_obj().map(|o| o.key.as_str()))
                        .unwrap_or(""),
                )
            })
            .collect()
    }

    #[test]
    fn test_handle_command_toggle_hidden_files() {
        let (mut p, _dir) = make_provider();
        assert!(!p.show_hidden, "hidden files are off by default");
        p.handle_command("show/hide hidden files", "", 0).unwrap();
        assert!(p.show_hidden);
        p.handle_command("show/hide hidden files", "", 0).unwrap();
        assert!(!p.show_hidden);
    }

    #[test]
    fn test_list_directory_hides_dotfiles_by_default() {
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join(".DS_Store"), "").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        let labels = entry_names(&mut p);
        assert!(labels.iter().any(|l| l == "visible.txt"));
        assert!(!labels.iter().any(|l| l == ".DS_Store"));
        assert!(!labels.iter().any(|l| l == ".git"));

        p.handle_command("show/hide hidden files", "", 0).unwrap();
        let labels = entry_names(&mut p);
        assert!(labels.iter().any(|l| l == ".DS_Store"));
        assert!(labels.iter().any(|l| l == ".git"));
    }

    #[test]
    fn test_extended_search_skips_hidden_when_disabled() {
        let (mut p, dir) = make_provider();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("config"), "").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();

        let results = p.collect_extended_search_items().unwrap_or_default();
        assert!(results.iter().any(|r| r.label.contains("visible.txt")));
        assert!(
            !results.iter().any(|r| r.label.contains(".git")),
            "extended search must agree with the listing"
        );
        assert!(
            !results.iter().any(|r| r.label.contains("config")),
            "hidden directories must not be descended into"
        );

        p.handle_command("show/hide hidden files", "", 0).unwrap();
        let results = p.collect_extended_search_items().unwrap_or_default();
        assert!(results.iter().any(|r| r.label.contains(".git")));
        assert!(results.iter().any(|r| r.label.contains("config")));
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinked_directory_is_navigable() {
        let (mut p, dir) = make_provider();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();

        let items = p.fetch();
        let link = items
            .iter()
            .find(|e| {
                tags::strip_display(
                    e.as_str()
                        .or_else(|| e.as_obj().map(|o| o.key.as_str()))
                        .unwrap_or(""),
                ) == "link"
            })
            .expect("symlink should be listed");
        assert!(
            link.as_obj().is_some(),
            "a symlink to a directory must be an Obj, or the user cannot arrow \
             into it and navigate_to_path cannot walk through it"
        );
    }

    #[cfg(unix)]
    /// Through a symlink with an absolute target, which the sandbox does not
    /// follow on its own: still a folder, and its contents still list.
    #[cfg(unix)]
    #[test]
    fn a_folder_reached_through_an_absolute_link_lists_its_contents() {
        let (mut p, dir) = make_provider();
        let real = dir.path().canonicalize().unwrap();
        std::fs::create_dir(real.join("actual")).unwrap();
        std::fs::write(real.join("actual/11.json"), "[]").unwrap();
        std::os::unix::fs::symlink(real.join("actual"), real.join("via-link")).unwrap();

        let top = p.fetch();
        assert!(
            top.iter()
                .any(|e| matches!(e, FfonElement::Obj(o) if o.key.contains("via-link")))
        );
        p.push_path("via-link");
        let inside = p.fetch();
        assert!(
            inside
                .iter()
                .any(|e| e.as_str().is_some_and(|s| s.contains("11.json"))),
            "{inside:?}"
        );
    }

    #[test]
    fn test_broken_symlink_is_not_a_directory() {
        let (mut p, dir) = make_provider();
        std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("dangling")).unwrap();

        let items = p.fetch();
        let link = items
            .iter()
            .find(|e| tags::strip_display(e.as_str().unwrap_or("")) == "dangling")
            .expect("broken symlink should still be listed");
        assert!(link.as_str().is_some(), "a broken link resolves to nothing");
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_properties_still_show_link_mode() {
        let (mut p, dir) = make_provider();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        p.show_properties = true;

        let items = p.fetch();
        let key = items
            .iter()
            .filter_map(|e| e.as_obj())
            .find(|o| entry_name(&o.key) == "link")
            .map(|o| o.key.clone())
            .expect("symlink should be listed as a directory");
        assert!(
            key.starts_with('l'),
            "properties must still describe the link itself, got {key}"
        );
    }

    #[test]
    fn test_handle_command_sort_chrono() {
        let (mut p, _dir) = make_provider();
        p.handle_command("sort chronologically", "", 0).unwrap();
        assert_eq!(p.sort_mode, SortMode::Chrono);
        p.handle_command("sort alphanumerically", "", 0).unwrap();
        assert_eq!(p.sort_mode, SortMode::Alpha);
    }

    // ---- extended_search ---------------------------------------------------

    #[test]
    fn test_extended_search_finds_nested_files() {
        let (mut p, dir) = make_provider();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("deep.txt"), b"").unwrap();
        p.set_current_path(dir.path().to_str().unwrap());
        let results = p.collect_extended_search_items().unwrap_or_default();
        assert!(results.iter().any(|r| r.label.contains("deep.txt")));
    }

    #[test]
    fn test_extended_search_dir_prefix() {
        let (p, dir) = make_provider();
        std::fs::create_dir(dir.path().join("mydir")).unwrap();
        let results = p.collect_extended_search_items().unwrap_or_default();
        assert!(results.iter().any(|r| r.label.starts_with("+ ")));
    }

    #[test]
    fn test_extended_search_file_prefix() {
        let (p, dir) = make_provider();
        std::fs::write(dir.path().join("myfile.txt"), b"").unwrap();
        let results = p.collect_extended_search_items().unwrap_or_default();
        assert!(results.iter().any(|r| r.label.starts_with("- ")));
    }

    #[test]
    fn test_handle_command_sort_alpha() {
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("cherry.txt"), b"").unwrap();
        std::fs::write(dir.path().join("apple.txt"), b"").unwrap();
        std::fs::write(dir.path().join("banana.txt"), b"").unwrap();
        p.handle_command("sort alphanumerically", "", 0).unwrap();
        assert_eq!(p.sort_mode, SortMode::Alpha);
        let items = p.fetch();
        let file_labels: Vec<_> = items
            .iter()
            .filter_map(|e| e.as_str())
            .map(|s| sicompass_sdk::tags::strip_display(s).to_string())
            .collect();
        assert_eq!(file_labels, vec!["apple.txt", "banana.txt", "cherry.txt"]);
    }

    #[test]
    fn test_sort_alpha_natural_order() {
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("file10.txt"), b"").unwrap();
        std::fs::write(dir.path().join("file2.txt"), b"").unwrap();
        std::fs::write(dir.path().join("file1.txt"), b"").unwrap();
        p.handle_command("sort alphanumerically", "", 0).unwrap();
        let items = p.fetch();
        let file_labels: Vec<_> = items
            .iter()
            .filter_map(|e| e.as_str())
            .map(|s| sicompass_sdk::tags::strip_display(s).to_string())
            .collect();
        assert_eq!(
            file_labels,
            vec!["file1.txt", "file2.txt", "file10.txt"],
            "natural sort should order file2 before file10"
        );
    }

    #[test]
    fn test_handle_command_unknown() {
        let (mut p, _dir) = make_provider();
        let result = p.handle_command("nonexistent command", "", 0).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_extended_search_empty_dir() {
        let (mut p, dir) = make_provider();
        p.set_current_path(dir.path().to_str().unwrap());
        let results = p.collect_extended_search_items().unwrap_or_default();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_extended_search_flat_files() {
        let (p, dir) = make_provider();
        std::fs::write(dir.path().join("alpha.txt"), b"").unwrap();
        std::fs::write(dir.path().join("beta.txt"), b"").unwrap();
        std::fs::write(dir.path().join("gamma.txt"), b"").unwrap();
        let results = p.collect_extended_search_items().unwrap_or_default();
        assert_eq!(results.len(), 3);
        for item in &results {
            assert!(
                item.label.starts_with("- "),
                "flat files should have '- ' prefix"
            );
            assert_eq!(
                item.breadcrumb, "",
                "flat files should have empty breadcrumb"
            );
            assert!(item.nav_path.contains(dir.path().to_str().unwrap()));
        }
    }

    #[test]
    fn test_get_command_list_items_non_open_with() {
        let (p, _dir) = make_provider();
        let items = p.command_list_items("create directory");
        assert!(items.is_empty());
    }

    #[test]
    fn test_execute_command_unknown() {
        let (mut p, _dir) = make_provider();
        let result = p.execute_command("nonexistent", "anything");
        assert!(!result);
    }

    #[test]
    #[cfg(unix)]
    fn test_extended_search_symlink_not_followed() {
        let (p, dir) = make_provider();
        // Create a symlink pointing back to the root dir (circular)
        let link_path = dir.path().join("loop");
        std::os::unix::fs::symlink(dir.path(), &link_path).unwrap();
        // Also create a regular file
        std::fs::write(dir.path().join("regular.txt"), b"").unwrap();
        let results = p.collect_extended_search_items().unwrap_or_default();
        // Should find: loop (as non-dir via symlink_metadata) + regular.txt = 2
        assert_eq!(results.len(), 2, "symlink should not be traversed as dir");
        let loop_item = results.iter().find(|r| r.label.contains("loop"));
        assert!(loop_item.is_some(), "loop symlink should appear in results");
        assert!(
            loop_item.unwrap().label.starts_with("- "),
            "symlink should show as file, not dir"
        );
    }

    // ---- additional coverage to match C test suite -------------------------

    #[test]
    fn test_create_file_already_exists() {
        // Creating an already-existing file should not crash; file still exists.
        let (mut p, dir) = make_provider();
        p.create_file("existing.txt");
        p.create_file("existing.txt"); // second call — should not panic
        assert!(dir.path().join("existing.txt").exists());
    }

    #[test]
    fn test_fetch_nonexistent_path_returns_only_meta() {
        let mut p = fb();
        p.set_current_path("/nonexistent/path/xyz/abc");
        let items = p.fetch();
        // On a nonexistent path the listing is empty
        assert_eq!(items.len(), 0);
    }

    #[test]
    fn test_rename_nonexistent_returns_false() {
        let (mut p, _dir) = make_provider();
        let result = p.commit_edit("<input>nonexistent.txt</input>", "<input>new.txt</input>");
        assert!(!result);
    }

    #[test]
    fn test_rename_directory() {
        let (mut p, dir) = make_provider();
        std::fs::create_dir(dir.path().join("olddir")).unwrap();
        let ok = p.commit_edit("<input>olddir</input>", "<input>newdir</input>");
        assert!(ok);
        assert!(!dir.path().join("olddir").exists());
        assert!(dir.path().join("newdir").exists());
    }

    #[test]
    fn test_delete_nonexistent_returns_false() {
        let (mut p, _dir) = make_provider();
        assert!(!p.delete_item("<input>nonexistent_xyz</input>"));
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_directory() {
        let (mut p, dir) = make_provider();
        let src = dir.path().join("srcdir");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("inner.txt"), b"data").unwrap();
        let src_str = dir.path().to_str().unwrap();
        assert!(p.copy_item(src_str, "srcdir", src_str, "cpdir"));
        assert!(dir.path().join("cpdir").is_dir());
        assert!(dir.path().join("cpdir/inner.txt").exists());
    }

    #[test]
    fn test_fetch_special_chars_in_filename() {
        // Files with spaces and dashes should not crash the listing.
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("hello world.txt"), b"").unwrap();
        std::fs::write(dir.path().join("file-with-dashes.txt"), b"").unwrap();
        let items = p.fetch();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_get_commands_returns_six() {
        let p = fb();
        let cmds = p.commands();
        assert_eq!(cmds.len(), 6);
        assert!(cmds.contains(&"create directory".to_string()));
        assert!(cmds.contains(&"create file".to_string()));
        // Not in the sandbox: it cannot list the system's applications.
        assert!(!cmds.contains(&"open file with".to_string()));
        assert!(cmds.contains(&"show/hide properties".to_string()));
        assert!(cmds.contains(&"show/hide hidden files".to_string()));
        assert!(cmds.contains(&"sort alphanumerically".to_string()));
        assert!(cmds.contains(&"sort chronologically".to_string()));
    }

    #[test]
    fn test_provider_path_starts_at_root() {
        // On non-Windows, the initial path is "/".
        #[cfg(not(windows))]
        {
            let p = fb();
            assert_eq!(p.current_path(), "/");
        }
    }

    // ---- chrono sort ordering ----------------------------------------------

    #[test]
    #[cfg(unix)]
    fn test_list_directory_chrono_sort() {
        use std::time::{Duration, UNIX_EPOCH};
        let (mut p, dir) = make_provider();

        // Create three files with distinct mtime set via FileTimes
        let make_file_at = |name: &str, secs: u64| {
            let path = dir.path().join(name);
            std::fs::write(&path, b"").unwrap();
            let mtime = UNIX_EPOCH + Duration::from_secs(secs);
            let ft = std::fs::FileTimes::new().set_modified(mtime);
            let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.set_times(ft).unwrap();
        };
        make_file_at("oldest.txt", 1_000_000);
        make_file_at("middle.txt", 2_000_000);
        make_file_at("newest.txt", 3_000_000);

        p.sort_mode = SortMode::Chrono;
        let items = p.fetch();
        let names: Vec<String> = items
            .iter()
            .filter_map(|e| e.as_str())
            .map(|s| tags::strip_display(s).to_string())
            .collect();
        assert_eq!(
            names[0], "newest.txt",
            "newest should come first in chrono sort, got: {:?}",
            names
        );
        assert_eq!(names[1], "middle.txt");
        assert_eq!(names[2], "oldest.txt");
    }

    // ---- executables always shown ------------------------------------------

    #[test]
    #[cfg(unix)]
    fn test_fetch_executable_always_shown() {
        use std::os::unix::fs::PermissionsExt;
        let (mut p, dir) = make_provider();
        std::fs::write(dir.path().join("script.sh"), b"#!/bin/sh").unwrap();
        std::fs::set_permissions(
            dir.path().join("script.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(dir.path().join("data.txt"), b"").unwrap();

        // Rust filebrowser always shows executables — no separate "commands mode"
        let items = p.fetch();
        // Should have script.sh + data.txt = 2 entries
        assert_eq!(items.len(), 2, "expected 2 files, got {}", items.len());
    }

    #[test]
    #[cfg(unix)]
    fn test_fetch_symlink_appears_in_listing() {
        let (mut p, dir) = make_provider();
        let target = dir.path().join("real.txt");
        std::fs::write(&target, b"content").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let items = p.fetch();
        // Should have real.txt + link.txt = 2 entries
        assert_eq!(items.len(), 2);
        let names: Vec<_> = items
            .iter()
            .filter_map(|e| e.as_str())
            .map(|s| tags::strip_display(s).to_string())
            .collect();
        assert!(names.contains(&"link.txt".to_string()));
    }

    // -- Undoable delete: a ProviderOp carrying the snapshot ----------------

    fn snapshot_of(entry: &ProviderOp) -> FsSideEffect {
        assert_eq!(entry.command, OP_DELETE);
        decode_side_effect(entry).expect("the payload decodes")
    }

    #[test]
    fn delete_item_records_an_undo_with_the_file_snapshot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("doomed.txt");
        std::fs::write(&target, b"important content").unwrap();

        let mut p = fb();
        p.set_current_path(tmp.path().to_str().unwrap());
        assert!(p.delete_item("doomed.txt"));

        let entries = p.take_timeline_entries();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].label.contains("doomed.txt"),
            "{}",
            entries[0].label
        );
        match snapshot_of(&entries[0]) {
            FsSideEffect::TrashedFile {
                content_snapshot,
                original_path,
            } => {
                assert_eq!(content_snapshot, b"important content");
                assert_eq!(original_path, target);
            }
            other => panic!("expected TrashedFile, got {other:?}"),
        }
    }

    #[test]
    fn undo_restores_a_deleted_file_from_its_snapshot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("doomed.txt");
        std::fs::write(&target, b"restore me").unwrap();

        let desktop = FakeDesktop::default();
        let mut p = fb_on(&desktop);
        p.set_current_path(tmp.path().to_str().unwrap());
        assert!(p.delete_item("doomed.txt"));
        assert!(!target.exists(), "file is gone from disk");

        let entries = p.take_timeline_entries();
        p.undo(&entries[0]).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"restore me");
        assert!(
            desktop.0.borrow().restores.is_empty(),
            "a snapshot never needs the trash"
        );
    }

    #[test]
    fn undo_restores_a_deleted_directory_tree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("a");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("inner.txt"), b"nested").unwrap();
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("deep.txt"), b"deeper").unwrap();

        let mut p = fb();
        p.set_current_path(tmp.path().to_str().unwrap());
        assert!(p.delete_item("a"));
        assert!(!dir.exists());

        let entries = p.take_timeline_entries();
        p.undo(&entries[0]).unwrap();
        assert!(dir.is_dir());
        assert_eq!(std::fs::read(dir.join("inner.txt")).unwrap(), b"nested");
        assert_eq!(std::fs::read(sub.join("deep.txt")).unwrap(), b"deeper");
    }

    /// Too large to snapshot: the undo asks the trash (`desktop.restore`), and
    /// where the trash cannot restore (macOS) it says so and names the file.
    #[test]
    fn undo_of_an_oversized_delete_restores_it_from_the_trash() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("huge.bin");
        let big = vec![7u8; (TRASH_SNAPSHOT_LIMIT_BYTES + 1024) as usize];
        std::fs::write(&target, &big).unwrap();

        let desktop = FakeDesktop::default();
        let mut p = fb_on(&desktop);
        p.set_current_path(tmp.path().to_str().unwrap());
        assert!(p.delete_item("huge.bin"));
        assert!(!target.exists(), "oversized file is gone from disk");

        let entries = p.take_timeline_entries();
        assert!(
            matches!(snapshot_of(&entries[0]), FsSideEffect::RenameOnly { .. }),
            "an oversized delete keeps no snapshot"
        );
        assert!(
            entries[0].payload.len() < 4096,
            "and its undo payload stays small"
        );

        p.undo(&entries[0]).unwrap();
        assert_eq!(desktop.0.borrow().restores, vec![target.clone()]);
        assert_eq!(std::fs::read(&target).unwrap(), big);

        // Again, on a trash that cannot restore.
        assert!(p.delete_item("huge.bin"));
        desktop.0.borrow_mut().refuse_restore = Some("unsupported here".to_owned());
        let entries = p.take_timeline_entries();
        let err = p.undo(&entries[0]).unwrap_err();
        assert!(
            err.contains("huge.bin") && err.contains("unsupported here"),
            "{err}"
        );
        assert!(!target.exists(), "it stays in the trash");
    }

    #[test]
    fn redo_moves_the_file_to_the_trash_again() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("doomed.txt");
        std::fs::write(&target, b"x").unwrap();

        let desktop = FakeDesktop::default();
        let mut p = fb_on(&desktop);
        p.set_current_path(tmp.path().to_str().unwrap());
        assert!(p.delete_item("doomed.txt"));
        let entries = p.take_timeline_entries();
        p.undo(&entries[0]).unwrap();
        assert!(target.exists());

        // From somewhere else: the recorded path is used, not the cursor's.
        p.set_current_path("/");
        p.redo(&entries[0]).unwrap();
        assert!(!target.exists(), "redo deletes again");
        assert_eq!(desktop.0.borrow().items.len(), 2);
    }

    /// Only this plugin's own deletes are undone; anything else on the tab's
    /// timeline is left alone.
    #[test]
    fn an_entry_that_is_not_a_delete_is_ignored() {
        let mut p = fb();
        let other = ProviderOp {
            command: "something-else".to_owned(),
            payload: sicompass_pdk::encode_one(&FfonElement::Str("x".into())),
            label: "x".to_owned(),
        };
        assert!(p.undo(&other).is_ok());
        assert!(p.redo(&other).is_ok());
    }

    // ---- property formatting ----------------------------------------------

    #[cfg(unix)]
    #[test]
    // The same macOS `u16` casts as in `format_properties`.
    #[allow(clippy::unnecessary_cast)]
    fn test_format_properties_permission_string() {
        let mk = |mode: u32| RawEntry {
            name: "x".into(),
            mtime: SystemTime::UNIX_EPOCH,
            is_dir: false,
            size: 0,
            mode,
            nlink: 1,
            uid: 0,
            gid: 0,
        };
        // Casting the `libc::S_*` constants to `u32` must keep the bit tests
        // correct (they are `u16` on macOS, `u32` on Linux).
        let dir = format_properties(&mk(libc::S_IFDIR as u32 | 0o755));
        assert!(dir.starts_with("drwxr-xr-x "), "got: {dir}");

        let file = format_properties(&mk(libc::S_IFREG as u32 | 0o644));
        assert!(file.starts_with("-rw-r--r-- "), "got: {file}");

        let link = format_properties(&mk(libc::S_IFLNK as u32 | 0o777));
        assert!(link.starts_with("lrwxrwxrwx "), "got: {link}");
    }
}
