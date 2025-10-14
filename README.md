# `termpdf` (Rust)

A kitty-native PDF (and future EPUB/HTML) viewer written in Rust. This
project is a ground-up rewrite of the original Python implementation,
focused on a modular architecture, clean dependency management, and
better long-term maintainability.

## Features (MVP)
- Render PDF pages inside Kitty using the terminal graphics protocol.
- Vim-style key bindings for navigation (`j/k`, `g/G`, `+/-`, `d` for dark mode).
- Multiple document buffers with per-document session persistence
  (last page, scale, dark-mode flag).
- JSON-backed state store under the standard XDG data directory.
- Graceful CLI (`termpdf-cli`) with argument parsing and logging.

## Roadmap
The rewrite mirrors the scope of the legacy project. Items checked are
implemented in Rust; unchecked items are tracked in the original
backlog and slated for future iterations.

- [x] PDF rendering via `pdfium-render`.
- [x] Kitty graphics protocol renderer.
- [x] Session persistence.
- [x] Vim-style navigation basics.
- [ ] EPUB/HTML/CBZ backends.
- [ ] Annotations, outlines, and remote control RPC.
- [ ] Configurable key maps and rich config file parser.

## Requirements
- Rust toolchain (1.70+).
- [Kitty](https://sw.kovidgoyal.net/kitty/) terminal emulator.
- A Pdfium binary compatible with the host platform. You can:
  1. Place `libpdfium` (macOS/Linux) or `pdfium.dll` (Windows) alongside
     the executable; **or**
  2. Set `PDFIUM_LIBRARY_PATH` to point at the shared library; **or**
  3. Install a system-provided Pdfium (Homebrew: `brew install pdfium`).

If Pdfium cannot be located, the CLI will error on startup with guidance.

## Building
```bash
cargo build --workspace
```
This produces the CLI binary at `target/debug/termpdf-cli`.

## Running
```bash
cargo run --bin termpdf-cli -- <file.pdf>
```
Optional flags:
- `-p, --page <N>`: open documents at zero-based page `N`.

Within the viewer:
- `j` / `↓`: next page.
- `k` / `↑`: previous page.
- `g`: go to beginning.
- `G` / `End`: go to last page.
- `+` / `-`: zoom in/out (relative scaling).
- `d`: toggle dark mode inversion.
- `q`: quit.

## Configuration & State
Configuration is currently hard-coded, but session state is stored per
file under:
```
$XDG_DATA_HOME/termpdf/state/<document-id>.json
```
On macOS this resolves to `~/Library/Application Support/termpdf/state/`.

## Tests
```bash
cargo test
```
All unit tests are isolated from Pdfium by using fake backends in the
core crate. Rendering integration requires a Pdfium runtime.

## Project Layout
- `termpdf-core`: document/session state machine, storage interfaces.
- `termpdf-render`: Pdfium-backed rendering plus trait implementations.
- `termpdf-tty`: Kitty protocol renderer, key mapping helpers.
- `termpdf-cli`: end-user binary wiring everything together.

## Development Notes
- Enable `RUST_LOG=debug` to see tracing output.
- The Pdfium backend opens documents on demand; repeated renders reuse
  an in-memory cache for the last page.
- Future work: feature flags for alternate backends, RPC integration,
  richer configuration, and golden-image tests.

## License
MIT (same as the original project).
