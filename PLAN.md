# TermPDF Rust Rewrite Plan

## Goals
- Reimplement the existing kitty-native PDF/EPUB viewer in pure Rust while matching the core feature set of the Python version.
- Establish a modular architecture that supports additional document formats and integrations in future iterations.
- Ship a usable MVP that can render PDFs in kitty, respect persisted state (page, zoom, crops), handle multiple documents, and expose Vim-style key bindings.

## Crate Layout
1. `termpdf-core`
   - Document/session state machine, configuration handling, persistence helpers.
   - Defines shared traits: `DocumentBackend`, `Renderer`, `UiEvent`, `Command`, `SessionSnapshot`.
   - Owns routing of commands (navigation, metadata, bookmarks, labels) and produces render requests.

2. `termpdf-render`
   - Concrete document backends.
   - Starts with PDF via `pdfium-render` (fallback: stub trait impl when the dynamic lib is missing) and EPUB/HTML/CBZ using appropriate crates.
   - Handles decoding, page rasterization to RGBA buffers, metadata, table of contents.

3. `termpdf-tty`
   - Kitty graphics protocol client, keyboard handling, and status line rendering.
   - Encodes RGBA buffers into PNG (via `png` crate) and streams escape sequences over stdout.
   - Provides key mapping layer (defaults mimic Vim) and hooks that translate keystrokes to `Command`s.

4. `termpdf-cli`
   - End-user binary crate.
   - CLI parsing (`clap`), session bootstrap, document discovery, and wiring of core ↔ UI ↔ renderer components.
   - Emits logs via `tracing` and persists session snapshots in standard directories.

## Supporting Components
- Configuration: load from `$XDG_CONFIG_HOME/termpdf/config.toml` using `serde` + `toml`. Support overrides via CLI flags. Config hot reloading is optional.
- Persistence: store last-viewed page, crop, zoom, and custom labels per document in `$XDG_DATA_HOME/termpdf/state.json` (one file per document). Consider `sled` or plain JSON for MVP.
- RPC Integration: optional feature flag enabling msgpack-RPC (using `rmpv`) for Neovim callbacks. Out of scope for MVP implementation but architected for later.
- Logging & Diagnostics: `tracing` + `tracing-subscriber`, propagate log level via CLI (`-v`).

## Dependencies
- Core: `anyhow`, `thiserror`, `serde`, `serde_json`, `serde_with`, `directories`, `parking_lot`.
- Rendering: `pdfium-render`, `epub`, `image`, `usvg`/`resvg` (future), `indicatif` (optional progress for pre-render).
- UI: `crossterm` (for keystroke parsing), `termios`/`nix` for raw mode if needed, `png`, `base64` (Kitty protocol encoding), `bytes` for buffer management.
- CLI/Tooling: `clap`, `once_cell`, `rayon` (for background page decoding).
- Testing: `assert_cmd`, `insta` (golden images), `tempfile`.

## Milestones
1. **Scaffold**: create workspace, set up crates with shared linting (`rustfmt`, `clippy`), configure dependencies, and stub public APIs.
2. **Rendering MVP**: implement PDF backend with `pdfium-render`, add fallback error messaging when pdfium is unavailable, return RGBA buffers for pages.
3. **Core State Machine**: support document open, navigation, multi-document buffer switching, session persistence.
4. **Kitty UI**: implement Kitty graphics renderer, key handling with Vim-style bindings, command routing to core.
5. **CLI + Config**: parse CLI options, handle multiple files, load config, spawn event loop.
6. **Testing & QA**: add unit/integration tests, golden image snapshots for sample PDFs, manual smoke test inside kitty.

## Testing Strategy
- Unit tests for document state transitions (core crate) and command handling.
- Backend tests using sample PDFs/EPUBs stored under `tests/fixtures` with feature-gated rendering; compare rendered hashes instead of raw bytes to avoid flakiness.
- Integration test running the CLI in a pseudo-terminal (using `expectrl`) to simulate key presses and validate expected commands without needing a real Kitty session.

## Risks & Mitigations
- **Pdfium Availability**: ship instructions for installing pdfium; provide build-time feature to swap in alternative renderer or stub. Detect at runtime and degrade gracefully.
- **Kitty Protocol Differences**: isolate the protocol implementation so we can mock it during tests and evolve without touching the rest of the stack.
- **Complex Feature Parity**: prioritize core navigation and rendering, leave advanced manipulation (annotations, SyncTeX) for post-MVP.

## Next Steps
- Scaffold the Rust workspace following the crate layout above.
- Port the minimal end-to-end flow (open PDF, render page into kitty, handle navigation).
- Iterate on extended features (multi-format support, RPC) once MVP is stable.
- Package or document Pdfium distribution so end users can run the CLI without manual setup.
- Extend `termpdf-render` with additional backends (EPUB/HTML/CBZ) and add golden-image integration tests backed by fixtures.
