# `termpdf` (Rust)

Kitty-native PDF viewer rewrite in Rust. The workspace currently ships a single CLI (`termpdf-cli`) backed by `pdfium-render` and a Kitty-specific renderer.

## Current Capabilities
- Render PDF pages inside Kitty via its graphics protocol; the PDF backend is the only backend implemented today.
- Real time PDF; Useful when working with LaTeX and Typst and when the PDF file is constantly being recompiled.
- Vim-flavoured navigation (`j/k`, `g/G`, `+/-`, `d`, `q`) with numeric prefixes (`12j`), mark support (`m<char>` to set, `'<char>` to jump), and jump history (`Ctrl-o`/`Ctrl-i`).
- Inline search (`/pattern`) with live feedback, highlighted matches, and `n`/`N` navigation.
- Automatic page scaling that fits the current terminal window plus a dark-mode inversion toggle.
- Prefetch and cache of neighbouring pages to keep navigation snappy.
- Accept multiple files on the CLI; the last one opened becomes the active document in the viewer.

## Gaps & Roadmap
- No interactive document switching UI, annotations, outlines, or remote-control RPC.
- EPUB/HTML/CBZ backends and configurable key mappings remain future work.

## Requirements
- Rust toolchain (1.70+ recommended).
- [`kitty`](https://sw.kovidgoyal.net/kitty/) terminal emulator; other terminals do not understand the graphics protocol used here.
- A Pdfium dynamic library. During `cargo build` the `termpdf-render` build script will try to download a matching binary from `pdfium-binaries`. To supply your own instead:
  - Place the library next to the executable and expose it via `PDFIUM_LIBRARY_PATH`, or
  - Pre-set `PDFIUM_DYNAMIC_LIB_PATH` / `PDFIUM_STATIC_LIB_PATH`, or
  - Set `TERMPDF_PDFIUM_ARCHIVE_PATH` or `TERMPDF_PDFIUM_SKIP_DOWNLOAD` to control the build step.

If Pdfium cannot be found at runtime the CLI exits with a descriptive error.

## Building
```bash
cargo build --workspace
```
The CLI binary lands at `target/debug/termpdf-cli`.

## Running
```bash
cargo run --bin termpdf-cli -- [-p <page>] <file.pdf> [<more.pdf> ...]
```
Flags:
- `-p`, `--page <N>`: start documents at zero-based page `N`.

### Viewer Controls
- `j` / `↓`: next page (`12j` works for counts).
- `k` / `↑`: previous page.
- `g`: jump to the first page.
- `G` / `End`: jump to the last page.
- `+` / `-`: zoom in/out (clamped between 0.25x and 4x; auto-fit may request a higher scale when there is space).
- `=`: reset zoom to 100%.
- `Ctrl` + arrow keys: pan the current page when zoomed (horizontal panning also works with `h`/`l`, vertical with `Shift+J`/`Shift+K`).
- `d`: toggle dark-mode inversion.
- `m<char>`: record a mark for the active page.
- `'<char>`: jump to a recorded mark.
- `q`: quit.

A status line appears at the bottom showing the filename, current page, and any partially entered numeric prefix or command.

## Session Data
State files are written under the platform data directory reported by `directories::ProjectDirs` (for example `~/.local/share/termpdf/state/` on Linux or `~/Library/Application Support/net.termpdf.termpdf/state/` on macOS). Document IDs are derived from the document's canonical path, so reopening the same file restores the last page, scale, dark-mode flag, and marks. Opening the file through a different path (e.g. a new symlink) generates a fresh session.

## Project Layout
- `termpdf-core`: document/session state machine, caching, and persistence helpers.
- `termpdf-render`: Pdfium-backed renderer and the build script that fetches Pdfium binaries when needed.
- `termpdf-tty`: Kitty protocol renderer, key/event mapper, and status-line helpers.
- `termpdf-cli`: clap-based CLI wiring the pieces together.

## Development
```bash
cargo test
```
Tests cover the session state machine, key-event mapper, and renderer protocol helpers. Rendering integration still requires a Pdfium runtime. Enable `RUST_LOG=debug` to surface tracing output during manual runs.

## License
MIT
