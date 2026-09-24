//! What the file browser needs from outside the sandbox, through the host's
//! `desktop` interface: the OS trash, symlink targets, `ls -l` details, and the
//! user's applications.
//!
//! A trait so the tests run natively. There the defaults ask the OS directly,
//! and the tests' fake trash moves items into a temp folder, because a test
//! must never put a fixture in the developer's real trash (about a thousand
//! runs once left 37 850 of them there). Inside the sandbox it is the host,
//! which only reaches paths this plugin was granted.

use std::path::{Path, PathBuf};

/// What `ls -l` shows beyond size and date, which WASI's metadata lacks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stat {
    /// Type and permission bits (`st_mode`).
    pub mode: Option<u32>,
    pub links: Option<u64>,
    pub owner: Option<String>,
    pub group: Option<String>,
    /// The user's UTC offset at the modification time, seconds east.
    pub utc_offset: i32,
}

/// An installed application: what the user reads, and what `open_with` takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub name: String,
    pub id: String,
}

pub trait Desktop {
    /// Move `path` to the OS trash, so the user can get it back.
    fn trash(&self, path: &Path) -> Result<(), String>;
    /// Restore the most recently trashed item that was at `path`.
    fn restore(&self, path: &Path) -> Result<(), String>;

    /// The target of the symlink at `path`. The sandbox never reads an
    /// absolute one itself, so this asks the host.
    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        std::fs::read_link(path).ok()
    }

    /// `path` through every symlink along it (`sicompass_sdk::fs_links`).
    fn resolve(&self, path: &Path) -> PathBuf {
        sicompass_sdk::fs_links::resolve_with(path, |p| self.read_link(p))
    }

    /// The entry at `path` itself (a link is not followed), as `ls -l` sees it.
    fn stat(&self, path: &Path) -> Option<Stat> {
        native::stat(path)
    }

    /// The user's installed applications, for "open file with".
    fn applications(&self) -> Vec<App> {
        Vec::new()
    }

    /// Open `path` with application `id` from [`Desktop::applications`].
    fn open_with(&self, _id: &str, _path: &Path) -> Result<(), String> {
        Err("no applications outside the sandbox".to_owned())
    }
}

/// The host's `desktop` interface.
pub struct HostDesktop;

#[cfg(target_arch = "wasm32")]
impl Desktop for HostDesktop {
    fn trash(&self, path: &Path) -> Result<(), String> {
        sicompass_pdk::desktop::trash(&path.to_string_lossy())
    }

    fn restore(&self, path: &Path) -> Result<(), String> {
        sicompass_pdk::desktop::restore(&path.to_string_lossy())
    }

    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        sicompass_pdk::desktop::read_link(&path.to_string_lossy())
            .ok()
            .map(PathBuf::from)
    }

    fn stat(&self, path: &Path) -> Option<Stat> {
        let f = sicompass_pdk::desktop::stat(&path.to_string_lossy()).ok()?;
        Some(Stat {
            mode: f.mode,
            links: f.links,
            owner: f.owner,
            group: f.group,
            utc_offset: f.utc_offset,
        })
    }

    fn applications(&self) -> Vec<App> {
        sicompass_pdk::desktop::applications()
            .into_iter()
            .map(|a| App {
                name: a.name,
                id: a.id,
            })
            .collect()
    }

    fn open_with(&self, id: &str, path: &Path) -> Result<(), String> {
        sicompass_pdk::desktop::open_with(id, &path.to_string_lossy())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Desktop for HostDesktop {
    fn trash(&self, _path: &Path) -> Result<(), String> {
        Err("no OS trash outside the sandbox".to_owned())
    }

    fn restore(&self, _path: &Path) -> Result<(), String> {
        Err("no OS trash outside the sandbox".to_owned())
    }
}

/// The defaults: the OS itself, for native test builds.
mod native {
    use super::Stat;
    use std::path::Path;

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    pub fn stat(path: &Path) -> Option<Stat> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path).ok()?;
        Some(Stat {
            mode: Some(meta.mode()),
            links: Some(meta.nlink()),
            owner: Some(user_name(meta.uid())),
            group: Some(group_name(meta.gid())),
            utc_offset: utc_offset_at(meta.mtime()),
        })
    }

    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    pub fn stat(_path: &Path) -> Option<Stat> {
        None
    }

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    fn utc_offset_at(secs: i64) -> i32 {
        let t = secs as libc::time_t;
        // SAFETY: `localtime_r` writes only into `tm`, which lives on this frame.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return 0;
        }
        tm.tm_gmtoff as i32
    }

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    fn user_name(uid: u32) -> String {
        let mut buf = vec![0 as libc::c_char; 4096];
        // SAFETY: `pwd` and `buf` outlive the call, and `buf.len()` is its
        // size; the name is read only when an entry was found.
        unsafe {
            let mut pwd: libc::passwd = std::mem::zeroed();
            let mut out: *mut libc::passwd = std::ptr::null_mut();
            if libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut out) == 0
                && !out.is_null()
            {
                return std::ffi::CStr::from_ptr(pwd.pw_name)
                    .to_string_lossy()
                    .into_owned();
            }
        }
        uid.to_string()
    }

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    fn group_name(gid: u32) -> String {
        let mut buf = vec![0 as libc::c_char; 4096];
        // SAFETY: as in `user_name`.
        unsafe {
            let mut grp: libc::group = std::mem::zeroed();
            let mut out: *mut libc::group = std::ptr::null_mut();
            if libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut out) == 0
                && !out.is_null()
            {
                return std::ffi::CStr::from_ptr(grp.gr_name)
                    .to_string_lossy()
                    .into_owned();
            }
        }
        gid.to_string()
    }
}
