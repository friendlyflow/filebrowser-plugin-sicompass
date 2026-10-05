# Project Instructions

filebrowser-plugin-sicompass was split out of the
[sicompass](https://github.com/friendlyflow/sicompass) workspace, and its git
history before that point is the history of `lib/lib_filebrowser` there. Work on it is
usually driven from a sicompass checkout next to this one (`../sicompass`),
whose `/commit-and-push`, `/release`, `/sync` and `/update-cargo` take this
repo's name as their first argument and then follow the skills in this repo's
`.claude/skills/`.

It is a sicompass **plugin process**: a program (`src/main.rs`) built with the
SDK's `plugin` feature, which sicompass starts and talks to over its stdin and
stdout. It runs with the user's rights. The Store installs it from this repo's
GitHub releases, one build per platform. The plugin platform is described in
`../sicompass/docs/plugin-platform.md`.

- `plugin.json` is the manifest. Its `name` is `filebrowser` (the renderer's
  save-as and open dialogs look it up by that name, so it must not change) and
  its `displayName` `file browser` is the settings section (`sortOrder`). It
  declares `"filesystem": ["/"]`, the whole disk, which the user approves at
  install.
- `locales/<lang>.ftl`, every id prefixed `filebrowser-`, in all four
  languages.

## What it does itself, and what it asks the app

- **The filesystem** is plain `std::fs`. Listings skip an entry that cannot be
  read rather than failing the folder.
- **Symlinks.** Every filesystem call goes through `Desktop::resolve`
  (`sicompass_sdk::fs_links`), so navigation keeps the path the user took
  while the calls see the real folder.
- **Deletes** go to the OS trash through the app
  (`sicompass_sdk::plugin::desktop::trash`), after a snapshot
  (`sicompass_sdk::fs_snapshot`) that rides in the `ProviderOp` undo payload,
  base64-encoded. Undo writes the snapshot back, or asks the app's
  `desktop::restore` when it was too large to keep. Renames, creates and pastes
  are recorded by the app itself.
- **Properties** are read from the OS: permission bits, links, owner and group
  (`getpwuid_r`/`getgrgid_r`, the number when there is no name) and the local
  UTC offset, on Unix. On Windows a row shows size and date only.
  `format_properties` builds the `ls -l` line itself.
- **"Open file with"** is the app's: `desktop::applications` and
  `desktop::open_with` (only ids the app listed).
- **Every call has 10 seconds**, after which the app ends the plugin. The
  extended search stops after 5 seconds with what it found.
- `Desktop` is a trait so the tests can swap the trash: the fake moves items
  into a temp folder and back. No test may reach the developer's real trash.
  Outside sicompass, `HostDesktop` has no app to ask and refuses.
- Strings come from the app (`host::translate`), which holds this plugin's
  `locales/`. The unit tests read `locales/en-US.ftl` instead.

## Environment (Nix)

The toolchain comes from the flake dev shell in [flake.nix](flake.nix): Rust
from rust-overlay with this computer's plugin target (static musl on Linux,
which nixpkgs' rustc has no `std` for) and `jq`. Nothing is installed
system-wide.

- **Check once per session**, then stick with the answer: `command -v cargo`.
  - Non-empty: the shell is inside `nix develop`, so run `cargo ...` directly.
  - Empty: prefix every toolchain command with `nix develop -c`.
- `nix develop -c <cmd>` prints a `warning: Git tree ... is dirty` line on
  stderr first. That warning is noise, not a failure.
- Evaluate the flake through `git+file://$PWD`, never a plain path (a plain path
  copies `target/` into the store and hangs), and always under `timeout`.
- The version lives in `plugin.json` and in `[package] version` in `Cargo.toml`.
  Bump both together.

## Generated files that are committed

- `THIRD-PARTY-LICENSES.html`: `cargo about generate about.hbs -o
  THIRD-PARTY-LICENSES.html` (cargo-about 0.9.2, the version the `licenses.yml`
  workflow pins). Regenerate and commit it with any dependency change. The
  workflow fails if it drifts.

## Code Style

Follow standard Rust idioms. Use `#[allow(...)]` sparingly and only when
justified. In `README.md`, do not use em dashes or semicolons. Use commas
instead, or split into separate sentences.

## Testing

- After implementing changes, always run the tests before finishing:
  `cargo test`, and `./scripts/release-plugin.sh --dry-run`, which also builds
  this computer's release and verifies it the way the Store will.
- When adding new code, write or update tests.
- If tests fail, fix the code. Never leave a task with failing tests.

## Test Integrity

- Never remove or weaken test assertions to make a failing test pass. Fix the
  code instead.
- If a test itself is genuinely wrong and needs changing, **ask the user
  first** before modifying it.

## Releasing

A release is a `vX.Y.Z` tag on `main`, equal to `plugin.json`'s version. See
`.claude/skills/release/SKILL.md`. Before tagging, run
`nix develop -c ./scripts/release-plugin.sh --dry-run` (needs the
`sicompass-plugin` tool: `cargo install --git
https://github.com/friendlyflow/sicompass-plugin-sdk sicompass-plugin`). The
release workflow signs with the `PLUGIN_SIGNING_KEY` secret and checks it
against the `PLUGIN_PUBLIC_KEY` variable, the key the sicompass store list
names. The secret key file is `~/.config/sicompass/plugin-keys/filebrowser.key`
on the maintainer's machine. Never print, copy or commit it.

The SDK comes from crates.io (the source is `../sicompass-plugin-sdk`). The
commented-out `[patch]` in `Cargo.toml` is for working on them together, and
stays commented on main.

A release has one archive per platform. The release workflow builds them on
five runners (Linux x86_64 and arm64 as static musl, macOS arm64 and x86_64,
Windows x86_64), then packs, signs and verifies them in one job.
