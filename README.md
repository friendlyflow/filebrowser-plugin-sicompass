# filebrowser-plugin-sicompass

*Your files, in Sicompass.*

This plugin is part of [Sicompass](https://github.com/friendlyflow/sicompass), a
keyboard-first, accessibility-first way to use your entire computer.

The file browser shows your files and folders as a list of lists. Right enters a
folder, i renames what the cursor is on, and ctrl+a and ctrl+i create a file or,
with a name ending in a colon, a folder. Ctrl+c, ctrl+x and ctrl+v copy, cut and
paste, and ctrl+d moves an item to the trash. Every change can be undone with
ctrl+z, a delete too, even after the trash was emptied.

Sicompass uses the file browser to pick a place when you save or open a file
(ctrl+s, ctrl+shift+s, ctrl+o), so install it before you do either.

Colon commands: open file with (one of your installed applications), show or
hide properties (permissions, owner, group, size and date, like `ls -l`), show or hide hidden
files (names starting with a dot), and sort alphanumerically or
chronologically. The sort order is also in Settings, under file browser.

The file browser asks for your whole disk, because browsing it is what it is
for. The Store shows that before you install it, and installing it is your
approval.

## Install

In Sicompass, open store, then programs, and press Enter on install next to
filebrowser. The Store checks the release's signature before installing it, and
keeps it up to date.

## Building from source

```bash
nix develop          # the toolchain, with the wasm32-wasip2 target
cargo test           # natively
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/filebrowser_plugin.wasm plugin.wasm
```

`./scripts/release-plugin.sh --dry-run` does the build, checks the component
against `plugin.json`, and signs and verifies it with a throwaway key, the way
a release is made.

## Related repositories

- [sicompass](https://github.com/friendlyflow/sicompass), the application
- [sicompass-plugin-sdk](https://github.com/friendlyflow/sicompass-plugin-sdk),
  the SDK, the WASM plugin kit and the cloud backup library

## Community

Join the conversation on
[Discord](https://discord.com/channels/1464152138753249313/1464152139231137894).

## License

#### Open source license

If you are creating an open source application under a license compatible with
the GNU GPL license v3, you may use this project under the terms of the GPLv3.
See [LICENSE](LICENSE).

## Contributing

Contributions are welcome. Whether it is code, documentation, or feedback, your
input helps make computing more accessible for everyone.
