<img src="assets/wfiles.svg" width="96" align="right" alt="WFiles icon">

# WFiles

**A fast, clear, undoable cross-platform file manager.** Native desktop app, written in Rust 2024 with [GPUI Kit](https://crates.io/crates/gpui-kit).

Dual-pane and tabs, keyboard-first throughout, every destructive action undoable. Directories with 100k items scroll, sort and filter without dropping frames. Remote locations (WebDAV / FTP / SFTP / S3) go through the same operations and the same background task queue as local files.

> Version **0.1.0**. The feature list is complete per stage and individually verified (see [`ROADMAP.md`](ROADMAP.md), in Chinese), but only macOS has been verified on real hardware so far.

## Highlights

- **Dual pane + tabs** (`Cmd+Alt+D`, `Tab`, `F5`/`F6` to copy/move to the other pane), three view modes (list / grid / Miller columns, `Cmd+1/2/3`), per-directory sort and view
- **Background operation center**: queued tasks, pause / resume / abort, byte-level progress, retry, resumed transfers, and an undo stack (`Cmd+Z`) for copies, moves, renames and deletes. Deletes go to the system trash and can be put back
- **Search and analysis**: boolean / regex / glob search with content search (`Cmd+Shift+F`), folder size / duplicate files / checksums (`Cmd+Alt+I`), directory compare and file diff with sync plans
- **Remote**: WebDAV, FTP, SFTP (including key auth), S3-compatible object storage; offline cache, bandwidth limits, remote-backed editing; passwords live in the system keychain
- **Terminal**: open the system terminal here (`Cmd+Alt+T`) or a built-in terminal drawer (`Ctrl+Alt+T`)
- **CLI and single instance**: `wfiles <path>`, `--reveal`, `--new-tab`, `--new-window`, `--lang`, `--help`, `--version`; a second launch hands its target to the running window
- **Personalization**: Chinese / English UI, fully configurable keybindings, command palette (`Cmd+P`), tags and colors, optional metadata columns (Git status, image dimensions, media duration, EXIF, ID3, PDF page count), session and crash recovery

## Platform status

| Platform | Status |
|---|---|
| macOS | **Verified on real hardware** |
| Windows | Treated as a first-class target in the code, but **not yet compiled on a real machine** |
| Linux | Same as Windows |

Cross-platform parity is a hard rule of this project (byte-wise path handling, system trash, one shared validation table for reserved names). Parity currently holds at the code level only: there is no build environment for the other two platforms yet, so this table will stay honest until they are actually built and run.

## Build and run

Requires stable Rust (edition 2024).

```bash
cargo build
cargo run                 # development; see the macOS caveat below
cargo test
scripts/bundle-macos.sh   # build a double-clickable WFiles.app (macOS, release)
```

On macOS, a bare `cargo run` binary is treated as a background app — its window will not come to the front. Use `scripts/bundle-macos.sh` when you want the real thing.

The app icon lives in `assets/wfiles.svg` and is rendered by `scripts/make-icon.sh` (invoked automatically while bundling). The version number has a single source: `Cargo.toml`.

## Data locations

Settings, session and crash-recovery state: `~/Library/Application Support/WFiles/` on macOS, `~/.local/state/WFiles/` on Linux, `%APPDATA%\WFiles\` on Windows. Logs go to `~/Library/Caches/WFiles/logs/`, `~/.cache/WFiles/logs/` and `%LOCALAPPDATA%\WFiles\logs\` respectively. Set `WFILES_SESSION_DIR` to keep the state directory somewhere else.

## License

[MIT](LICENSE).
