use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use crossterm::cursor;
use crossterm::event;
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType};
use directories::ProjectDirs;
use termpdf_core::{
    Command, DocumentId, DocumentInstance, ExternalLink, FileStateStore, Highlights, LinkSummary,
    NormalizedRect, OutlineItem, RenderImage, Session, SessionEvent, StateStore,
};
use termpdf_render::PdfRenderFactory;
use termpdf_tty::{write_status_line, DrawParams, EventMapper, InputMode, KittyRenderer, UiEvent};
use tracing::{trace, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{prelude::*, EnvFilter};
use url::Url;

#[derive(Debug, Parser)]
#[command(
    name = "termpdf",
    version,
    about = "kitty-native PDF viewer rewritten in Rust"
)]
struct Args {
    /// Page to open each document on (0-based)
    #[arg(short = 'p', long = "page")]
    page: Option<usize>,

    /// Paths to PDF files to open
    #[arg(required = true)]
    files: Vec<PathBuf>,
}

const FILE_POLL_INTERVAL_MS: u64 = 300;

#[cfg(target_os = "macos")]
const OPEN_COMMAND: &str = "open";
#[cfg(all(unix, not(target_os = "macos")))]
const OPEN_COMMAND: &str = "xdg-open";

struct WatchedDocument {
    id: DocumentId,
    path: PathBuf,
    last_modified: Option<SystemTime>,
    last_checked: Instant,
}

impl WatchedDocument {
    fn new(id: DocumentId, path: PathBuf) -> Self {
        let last_modified = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
        Self {
            id,
            path,
            last_modified,
            last_checked: Instant::now(),
        }
    }

    fn should_check(&self, interval: Duration) -> bool {
        self.last_checked.elapsed() >= interval
    }

    fn mark_checked(&mut self) {
        self.last_checked = Instant::now();
    }

    fn update_snapshot(&mut self, modified: Option<SystemTime>) {
        self.last_modified = modified;
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn new() -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = crossterm::execute!(stdout, cursor::Show);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.files.is_empty() {
        return Err(anyhow!("no input files provided"));
    }

    let project_dirs = ProjectDirs::from("net", "termpdf", "termpdf")
        .ok_or_else(|| anyhow!("unable to resolve platform data directories"))?;
    let _log_guard = init_logging(&project_dirs)?;
    let state_dir = project_dirs.data_local_dir().join("state");
    let store: Arc<dyn StateStore> = Arc::new(FileStateStore::new(state_dir.clone())?);
    let mut session = Session::new(store);
    let mut watched_docs = Vec::new();

    let provider = PdfRenderFactory::new()?;
    for path in &args.files {
        session
            .open_with(&provider, path.clone())
            .await
            .with_context(|| format!("failed to open {:?}", path))?;

        if let Some(doc) = session.active() {
            if !watched_docs
                .iter()
                .any(|entry: &WatchedDocument| entry.id == doc.info.id)
            {
                watched_docs.push(WatchedDocument::new(doc.info.id, doc.info.path.clone()));
            }
        }
    }

    if let Some(page) = args.page {
        session.apply(Command::GotoPage { page })?;
    }

    let _raw = RawModeGuard::new()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, cursor::Hide)?;
    let mut renderer = KittyRenderer::new(stdout);
    let mut event_mapper = EventMapper::new();
    let mut overlay = OverlayState::None;
    let mut dirty = true;
    let mut needs_initial_clear = true;
    let file_poll_interval = Duration::from_millis(FILE_POLL_INTERVAL_MS);

    loop {
        if overlay.is_active() {
            if !matches!(event_mapper.mode(), InputMode::Toc | InputMode::TocSearch) {
                event_mapper.set_mode(InputMode::Toc);
            }
        } else if matches!(event_mapper.mode(), InputMode::Toc | InputMode::TocSearch) {
            event_mapper.set_mode(InputMode::Normal);
        }

        let mut reload_queue = Vec::new();
        for watched in watched_docs.iter_mut() {
            if !watched.should_check(file_poll_interval) {
                continue;
            }
            watched.mark_checked();
            let modified = match fs::metadata(&watched.path)
                .and_then(|meta| meta.modified())
                .ok()
            {
                Some(ts) => ts,
                None => continue,
            };
            if watched
                .last_modified
                .map(|prev| prev == modified)
                .unwrap_or(false)
            {
                continue;
            }
            reload_queue.push((watched.id, modified));
        }

        for (doc_id, modified) in reload_queue {
            match session.reload_document(&provider, doc_id).await {
                Ok(true) => {
                    {
                        if let Some(entry) =
                            watched_docs.iter_mut().find(|entry| entry.id == doc_id)
                        {
                            entry.update_snapshot(Some(modified));
                        }
                    }
                    if let Some(active) = session.active() {
                        if active.info.id == doc_id {
                            if let OverlayState::Toc(toc) = &mut overlay {
                                toc.entries = active.outline().to_vec();
                                toc.update_selection_for_page(active.state.current_page);
                                toc.rebuild_search_matches();
                            }
                            needs_initial_clear = true;
                            dirty = true;
                        }
                    }
                }
                Ok(false) => {
                    watched_docs.retain(|entry| entry.id != doc_id);
                }
                Err(err) => {
                    trace!(
                        ?err,
                        doc = %doc_id,
                        "failed to reload document after change"
                    );
                }
            }
        }

        if process_session_events(&session) {
            dirty = true;
        }

        if dirty {
            // Begin an atomic update.
            renderer.begin_sync_update()?;

            // Perform all drawing operations. The terminal won't show them yet.
            if needs_initial_clear {
                renderer.clear_all()?;
                needs_initial_clear = false;
            }

            let pending = event_mapper.pending_input();
            redraw(&mut renderer, &session, pending.as_deref(), &mut overlay)?;

            // End the atomic update. The terminal renders everything at once.
            renderer.end_sync_update()?;

            dirty = false;
        }

        if event::poll(Duration::from_millis(100))? {
            let ev = event::read()?;
            let ui_event = event_mapper.map_event(ev);
            let pending = event_mapper.pending_input();
            if !overlay.is_active() {
                if let Some(status) = combine_status(document_status(&session), pending.as_deref())
                {
                    draw_status_line(&mut renderer, &status)?;
                }
            }
            let overlay_was_active = overlay.is_active();
            match handle_event(ui_event, &mut session, &mut overlay, &mut event_mapper)? {
                LoopAction::ContinueRedraw => dirty = true,
                LoopAction::Continue => {}
                LoopAction::Quit => break,
            }
            watched_docs.retain(|entry| session.contains_document(entry.id));
            if overlay.is_active() != overlay_was_active {
                needs_initial_clear = true;
                dirty = true;
            }
        }
    }

    {
        let mut writer = renderer.writer();
        crossterm::execute!(&mut writer, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
    }

    session.persist()?;
    Ok(())
}

enum LoopAction {
    Continue,
    ContinueRedraw,
    Quit,
}

enum OverlayState {
    None,
    Toc(TocWindow),
}

impl OverlayState {
    fn deactivate(&mut self) {
        *self = OverlayState::None;
    }

    fn is_active(&self) -> bool {
        !matches!(self, OverlayState::None)
    }

    fn toc_mut(&mut self) -> Option<&mut TocWindow> {
        match self {
            OverlayState::Toc(ref mut toc) => Some(toc),
            OverlayState::None => None,
        }
    }
}

struct TocWindow {
    entries: Vec<OutlineItem>,
    selected: usize,
    current: Option<usize>,
    scroll_offset: usize,
    search_query: Option<String>,
    search_matches: Vec<usize>,
    search_input: Option<String>,
}

impl TocWindow {
    fn from_outline(entries: Vec<OutlineItem>, current_page: usize) -> Self {
        let current = Self::entry_for_page(&entries, current_page);
        let selected = current.unwrap_or(0);
        Self {
            entries,
            selected,
            current,
            scroll_offset: 0,
            search_query: None,
            search_matches: Vec::new(),
            search_input: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn selected_entry(&self) -> Option<&OutlineItem> {
        self.entries.get(self.selected)
    }

    fn entry_for_page(entries: &[OutlineItem], current_page: usize) -> Option<usize> {
        if entries.is_empty() {
            return None;
        }
        let mut selected = 0;
        for (idx, item) in entries.iter().enumerate() {
            if item.page_index <= current_page {
                selected = idx;
            } else {
                break;
            }
        }
        Some(selected)
    }

    fn current_index(&self) -> Option<usize> {
        self.current.filter(|&idx| idx < self.entries.len())
    }

    fn set_selected(&mut self, index: usize) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let clamped = index.min(self.entries.len().saturating_sub(1));
        if clamped != self.selected {
            self.selected = clamped;
            true
        } else {
            false
        }
    }

    fn move_selection(&mut self, delta: isize) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let len = self.entries.len() as isize;
        let next = (self.selected as isize + delta).clamp(0, len - 1) as usize;
        self.set_selected(next)
    }

    fn select_first(&mut self) -> bool {
        self.set_selected(0)
    }

    fn select_last(&mut self) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let last = self.entries.len() - 1;
        self.set_selected(last)
    }

    fn ensure_visible(&mut self, viewport_height: usize) {
        if viewport_height == 0 {
            self.scroll_offset = 0;
            return;
        }
        if self.entries.is_empty() {
            self.scroll_offset = 0;
            return;
        }
        let max_offset = self.entries.len().saturating_sub(viewport_height.max(1));
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
            return;
        }
        let bottom = self.scroll_offset + viewport_height;
        if self.selected >= bottom {
            self.scroll_offset = self
                .selected
                .saturating_sub(viewport_height.saturating_sub(1));
        }
    }

    fn update_selection_for_page(&mut self, current_page: usize) {
        if self.entries.is_empty() {
            self.selected = 0;
            self.scroll_offset = 0;
            self.current = None;
            return;
        }
        if let Some(next) = Self::entry_for_page(&self.entries, current_page) {
            self.selected = next;
            self.current = Some(next);
        } else {
            self.selected = 0;
            self.current = None;
        }
    }

    fn begin_search(&mut self) -> bool {
        if self.search_input.is_some() {
            false
        } else {
            self.search_input = Some(String::new());
            self.search_query = None;
            self.search_matches.clear();
            true
        }
    }

    fn update_search_query(&mut self, query: &str) -> bool {
        self.search_input = Some(query.to_string());
        self.apply_search_query(query)
    }

    fn finish_search_input(&mut self) -> bool {
        if self.search_input.take().is_some() {
            true
        } else {
            false
        }
    }

    fn cancel_search(&mut self) -> bool {
        let mut changed = false;
        if self.search_input.take().is_some() {
            changed = true;
        }
        if self.search_query.take().is_some() || !self.search_matches.is_empty() {
            self.search_matches.clear();
            changed = true;
        }
        changed
    }

    fn apply_search_query(&mut self, query: &str) -> bool {
        if query.is_empty() {
            let cleared = self.search_query.take().is_some() || !self.search_matches.is_empty();
            self.search_matches.clear();
            return cleared;
        }
        self.search_query = Some(query.to_string());
        self.recompute_search_matches(query)
    }

    fn recompute_search_matches(&mut self, query: &str) -> bool {
        let needle = query.to_lowercase();
        let mut matches = Vec::new();
        for (idx, entry) in self.entries.iter().enumerate() {
            if entry.title.to_lowercase().contains(&needle) {
                matches.push(idx);
            }
        }
        self.search_matches = matches;
        if self.search_matches.is_empty() {
            return false;
        }
        let target = self
            .search_matches
            .iter()
            .copied()
            .find(|&idx| idx >= self.selected)
            .or_else(|| self.search_matches.first().copied());
        if let Some(target) = target {
            self.set_selected(target)
        } else {
            false
        }
    }

    fn search_prompt(&self) -> Option<String> {
        self.search_input
            .as_ref()
            .map(|input| format!("/{}", input))
    }

    fn active_query(&self) -> Option<&str> {
        self.search_query.as_deref()
    }

    fn entry_matches(&self, index: usize) -> bool {
        self.search_matches.binary_search(&index).is_ok()
    }

    fn search_next(&mut self, count: usize) -> bool {
        self.advance_search(count.max(1), true)
    }

    fn search_prev(&mut self, count: usize) -> bool {
        self.advance_search(count.max(1), false)
    }

    fn advance_search(&mut self, count: usize, forward: bool) -> bool {
        if self.search_matches.is_empty() {
            return false;
        }
        let len = self.search_matches.len();
        let mut index = if forward {
            self.search_matches
                .iter()
                .position(|&idx| idx > self.selected)
                .unwrap_or(0)
        } else {
            self.search_matches
                .iter()
                .rposition(|&idx| idx < self.selected)
                .unwrap_or(len - 1)
        };
        if len > 0 {
            let offset = (count - 1) % len;
            if forward {
                index = (index + offset) % len;
            } else {
                index = (index + len - (offset % len)) % len;
            }
        }
        let target = self.search_matches[index];
        self.set_selected(target)
    }

    fn rebuild_search_matches(&mut self) {
        if let Some(query) = self.search_query.clone() {
            let _ = self.recompute_search_matches(&query);
        } else {
            self.search_matches.clear();
        }
    }
}

fn handle_event(
    event: UiEvent,
    session: &mut Session,
    overlay: &mut OverlayState,
    mapper: &mut EventMapper,
) -> Result<LoopAction> {
    match event {
        UiEvent::BeginSearch => Ok(LoopAction::Continue),
        UiEvent::SearchQueryChanged { query } => {
            session.apply(Command::Search { query })?;
            let _ = process_session_events(session);
            Ok(LoopAction::ContinueRedraw)
        }
        UiEvent::SearchSubmit { query } => {
            session.apply(Command::Search { query })?;
            let _ = process_session_events(session);
            Ok(LoopAction::ContinueRedraw)
        }
        UiEvent::SearchCancel => {
            session.apply(Command::Search {
                query: String::new(),
            })?;
            let _ = process_session_events(session);
            Ok(LoopAction::ContinueRedraw)
        }
        UiEvent::Command(cmd) => {
            let mut redraw = matches!(
                cmd,
                Command::GotoPage { .. }
                    | Command::NextPage { .. }
                    | Command::PrevPage { .. }
                    | Command::ScaleBy { .. }
                    | Command::ResetScale
                    | Command::AdjustViewport { .. }
                    | Command::GotoMark { .. }
                    | Command::ToggleDarkMode
                    | Command::Search { .. }
                    | Command::SearchNext { .. }
                    | Command::SearchPrev { .. }
                    | Command::EnterLinkMode
                    | Command::LeaveLinkMode
                    | Command::LinkNext { .. }
                    | Command::LinkPrev { .. }
                    | Command::ActivateLink
                    | Command::JumpBackward
                    | Command::JumpForward
                    | Command::SwitchDocument { .. }
                    | Command::CloseDocument { .. }
            );
            let resets_overlay = matches!(
                cmd,
                Command::CloseDocument { .. } | Command::SwitchDocument { .. }
            );

            session.apply(cmd)?;
            let event_redraw = process_session_events(session);
            redraw = redraw || event_redraw;

            if resets_overlay {
                overlay.deactivate();
                mapper.set_mode(InputMode::Normal);
            } else if let OverlayState::Toc(toc) = overlay {
                if let Some(doc) = session.active() {
                    toc.update_selection_for_page(doc.state.current_page);
                } else {
                    overlay.deactivate();
                    mapper.set_mode(InputMode::Normal);
                }
            }

            if redraw || resets_overlay {
                Ok(LoopAction::ContinueRedraw)
            } else {
                Ok(LoopAction::Continue)
            }
        }
        UiEvent::OpenTableOfContents => {
            if let Some(doc) = session.active() {
                let entries = doc.outline().to_vec();
                let toc = TocWindow::from_outline(entries, doc.state.current_page);
                *overlay = OverlayState::Toc(toc);
                mapper.set_mode(InputMode::Toc);
                Ok(LoopAction::ContinueRedraw)
            } else {
                Ok(LoopAction::Continue)
            }
        }
        UiEvent::CloseOverlay => {
            if overlay.is_active() {
                overlay.deactivate();
                mapper.set_mode(InputMode::Normal);
                Ok(LoopAction::ContinueRedraw)
            } else {
                Ok(LoopAction::Continue)
            }
        }
        UiEvent::TocMoveSelection { delta } => {
            if let OverlayState::Toc(toc) = overlay {
                if toc.move_selection(delta) {
                    return Ok(LoopAction::ContinueRedraw);
                }
            }
            Ok(LoopAction::Continue)
        }
        UiEvent::TocBeginSearch => {
            if let OverlayState::Toc(toc) = overlay {
                if toc.begin_search() {
                    return Ok(LoopAction::ContinueRedraw);
                }
            }
            Ok(LoopAction::Continue)
        }
        UiEvent::TocSearchQueryChanged { query } => {
            if let OverlayState::Toc(toc) = overlay {
                toc.update_search_query(&query);
                return Ok(LoopAction::ContinueRedraw);
            }
            Ok(LoopAction::Continue)
        }
        UiEvent::TocSearchSubmit { query } => {
            if let OverlayState::Toc(toc) = overlay {
                let mut redraw = toc.apply_search_query(&query);
                redraw |= toc.finish_search_input();
                if redraw {
                    return Ok(LoopAction::ContinueRedraw);
                }
            }
            Ok(LoopAction::Continue)
        }
        UiEvent::TocSearchCancel => {
            if let OverlayState::Toc(toc) = overlay {
                if toc.cancel_search() {
                    return Ok(LoopAction::ContinueRedraw);
                }
            }
            Ok(LoopAction::Continue)
        }
        UiEvent::TocSearchNext { count } => {
            if let OverlayState::Toc(toc) = overlay {
                if toc.search_next(count) {
                    return Ok(LoopAction::ContinueRedraw);
                }
            }
            Ok(LoopAction::Continue)
        }
        UiEvent::TocSearchPrev { count } => {
            if let OverlayState::Toc(toc) = overlay {
                if toc.search_prev(count) {
                    return Ok(LoopAction::ContinueRedraw);
                }
            }
            Ok(LoopAction::Continue)
        }
        UiEvent::TocGotoStart => {
            if let OverlayState::Toc(toc) = overlay {
                if toc.select_first() {
                    return Ok(LoopAction::ContinueRedraw);
                }
            }
            Ok(LoopAction::Continue)
        }
        UiEvent::TocGotoEnd => {
            if let OverlayState::Toc(toc) = overlay {
                if toc.select_last() {
                    return Ok(LoopAction::ContinueRedraw);
                }
            }
            Ok(LoopAction::Continue)
        }
        UiEvent::TocActivateSelection => {
            if let OverlayState::Toc(toc) = overlay {
                if let Some(entry) = toc.selected_entry() {
                    session.apply(Command::GotoPage {
                        page: entry.page_index,
                    })?;
                    let _ = process_session_events(session);
                    overlay.deactivate();
                    mapper.set_mode(InputMode::Normal);
                    return Ok(LoopAction::ContinueRedraw);
                }
            }
            Ok(LoopAction::Continue)
        }
        UiEvent::Quit => Ok(LoopAction::Quit),
        UiEvent::None => Ok(LoopAction::Continue),
    }
}

fn process_session_events(session: &Session) -> bool {
    let mut redraw = false;
    for event in session.drain_events() {
        match event {
            SessionEvent::RedrawNeeded(_) => redraw = true,
            SessionEvent::FollowExternalLink { target } => {
                if let Err(err) = open_external_link(&target) {
                    warn!(?err, "failed to open external link");
                }
            }
            SessionEvent::DocumentOpened(_)
            | SessionEvent::DocumentClosed(_)
            | SessionEvent::ActiveDocumentChanged(_) => {}
        }
    }
    redraw
}

fn redraw(
    renderer: &mut KittyRenderer<io::Stdout>,
    session: &Session,
    pending_input: Option<&str>,
    overlay: &mut OverlayState,
) -> Result<()> {
    let window = terminal::window_size()?;
    let total_cols = u32::from(window.columns).max(1);
    let total_rows = u32::from(window.rows).max(1);
    let pixel_width = u32::from(window.width);
    let pixel_height = u32::from(window.height);
    let image_rows_available = total_rows.saturating_sub(1).max(1);

    if let Some(doc) = session.active() {
        if overlay.is_active() {
            {
                let mut writer = renderer.writer();
                crossterm::execute!(&mut writer, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
            }
            draw_overlay(renderer, overlay, total_cols, image_rows_available)?;
            return Ok(());
        }

        let margin_cols = total_cols.min(2);
        let margin_rows = image_rows_available.min(2);
        let available_cols = total_cols.saturating_sub(margin_cols).max(1);
        let available_rows = image_rows_available.saturating_sub(margin_rows).max(1);

        let base_scale = doc.state.scale;
        let mut render_scale = base_scale;
        let search_highlights = doc.search_highlights_for_current_page();
        let link_highlights = doc.link_highlights_for_current_page();
        let mut image = doc.render_with_scale(base_scale)?;
        let mut highlight_geom = HighlightGeometry::new(image.width, image.height);

        let cell_width = if total_cols > 0 {
            pixel_width as f32 / total_cols as f32
        } else {
            0.0
        };
        let cell_height = if total_rows > 0 {
            pixel_height as f32 / total_rows as f32
        } else {
            0.0
        };

        if cell_width > 0.0
            && cell_height > 0.0
            && image.width > 0
            && image.height > 0
            && pixel_width > 0
            && pixel_height > 0
        {
            let desired_pixel_width = cell_width * available_cols as f32;
            let desired_pixel_height = cell_height * available_rows as f32;
            if desired_pixel_width > 0.0 && desired_pixel_height > 0.0 {
                let width_ratio = desired_pixel_width / image.width as f32;
                let height_ratio = desired_pixel_height / image.height as f32;
                let scale_ratio = width_ratio.min(height_ratio);
                if scale_ratio > 1.05 {
                    let target_scale = (base_scale * scale_ratio).min(8.0);
                    render_scale = target_scale;
                    image = doc.render_with_scale(target_scale)?;
                    highlight_geom.set_base(image.width, image.height);
                }
            }
        }

        let zoom_scale = doc.state.scale;
        let mut display_image = image;

        if zoom_scale <= 1.0 {
            highlight_geom.set_base(display_image.width, display_image.height);
        }

        if zoom_scale > 1.0 {
            let crop_ratio = (1.0 / zoom_scale).min(1.0);
            if crop_ratio.is_finite() && crop_ratio > 0.0 {
                let crop_width = (display_image.width as f32 * crop_ratio)
                    .round()
                    .clamp(1.0, display_image.width as f32) as u32;
                let crop_height = (display_image.height as f32 * crop_ratio)
                    .round()
                    .clamp(1.0, display_image.height as f32)
                    as u32;
                if crop_width < display_image.width || crop_height < display_image.height {
                    let viewport = doc.state.viewport;
                    let offset_x =
                        compute_viewport_origin(display_image.width, crop_width, viewport.x);
                    let offset_y =
                        compute_viewport_origin(display_image.height, crop_height, viewport.y);
                    highlight_geom.set_crop(offset_x, offset_y, crop_width, crop_height);
                    display_image = crop_render_image(
                        &display_image,
                        offset_x,
                        offset_y,
                        crop_width,
                        crop_height,
                    );
                }
            }
        } else {
            highlight_geom.clear_crop();
        }

        let effective_pixel_width = if zoom_scale > 1.0 {
            display_image.width as f32 * zoom_scale
        } else {
            display_image.width as f32
        };
        let effective_pixel_height = if zoom_scale > 1.0 {
            display_image.height as f32 * zoom_scale
        } else {
            display_image.height as f32
        };

        let (draw_cols, draw_rows) = compute_scaled_dimensions(
            &display_image,
            effective_pixel_width,
            effective_pixel_height,
            available_cols,
            available_rows,
            total_cols,
            total_rows,
            pixel_width,
            pixel_height,
        );

        let start_col = (total_cols.saturating_sub(draw_cols)) / 2;
        let start_row = (image_rows_available.saturating_sub(draw_rows)) / 2;

        {
            let mut writer = renderer.writer();
            crossterm::execute!(
                &mut writer,
                cursor::MoveTo(start_col as u16, start_row as u16)
            )?;
        }

        if let Some(highlights) = link_highlights.as_ref().or(search_highlights.as_ref()) {
            apply_highlights(&mut display_image, highlights, &highlight_geom);
        }

        renderer.draw(&display_image, DrawParams::clamped(draw_cols, draw_rows))?;
        let status_text = format_document_status(doc);
        if let Some(status) = combine_status(Some(status_text), pending_input) {
            draw_status_line(renderer, &status)?;
        }

        if let Err(err) = doc.prefetch_neighbors(2, render_scale) {
            warn!(
                ?err,
                page = doc.state.current_page,
                "failed to prefetch neighboring pages"
            );
        }

        draw_overlay(renderer, overlay, total_cols, image_rows_available)?;
    } else {
        overlay.deactivate();
    }

    Ok(())
}

fn document_status(session: &Session) -> Option<String> {
    session.active().map(format_document_status)
}

fn combine_status(base: Option<String>, pending_input: Option<&str>) -> Option<String> {
    match (base, pending_input.filter(|s| !s.is_empty())) {
        (Some(mut base), Some(pending)) => {
            base.push_str(" | ");
            base.push_str(pending);
            Some(base)
        }
        (Some(base), None) => Some(base),
        (None, Some(pending)) => Some(pending.to_string()),
        (None, None) => None,
    }
}

fn draw_status_line(renderer: &mut KittyRenderer<io::Stdout>, status: &str) -> Result<()> {
    let window = terminal::window_size()?;
    let total_rows = u32::from(window.rows).max(1);
    let status_row = total_rows.saturating_sub(1);
    let mut writer = renderer.writer();
    crossterm::execute!(
        &mut writer,
        cursor::MoveTo(0, status_row as u16),
        Clear(ClearType::CurrentLine)
    )?;
    write_status_line(&mut writer, status)?;
    Ok(())
}

fn draw_overlay(
    renderer: &mut KittyRenderer<io::Stdout>,
    overlay: &mut OverlayState,
    total_cols: u32,
    image_rows_available: u32,
) -> Result<()> {
    match overlay {
        OverlayState::Toc(toc) => draw_toc_overlay(renderer, toc, total_cols, image_rows_available),
        OverlayState::None => Ok(()),
    }
}

fn draw_toc_overlay(
    renderer: &mut KittyRenderer<io::Stdout>,
    toc: &mut TocWindow,
    total_cols: u32,
    image_rows_available: u32,
) -> Result<()> {
    const TITLE: &str = "Table of Contents";
    const EMPTY_MESSAGE: &str = "No table of contents available";

    if total_cols < 20 || image_rows_available < 6 {
        return Ok(());
    }

    let max_inner_width = total_cols.saturating_sub(6) as usize;
    if max_inner_width < 10 {
        return Ok(());
    }

    let base_width = if toc.is_empty() {
        EMPTY_MESSAGE.len() + 2
    } else {
        toc.entries
            .iter()
            .map(toc_line_length)
            .max()
            .unwrap_or(0)
            .max(TITLE.len())
    };

    let mut inner_width = base_width.min(max_inner_width);
    let min_inner_width = 20.min(max_inner_width);
    if inner_width < min_inner_width {
        inner_width = min_inner_width;
    }

    let max_window_height = image_rows_available.saturating_sub(2);
    if max_window_height < 6 {
        return Ok(());
    }

    let search_prompt = toc.search_prompt();
    let extra_header_rows = if search_prompt.is_some() { 1u32 } else { 0u32 };
    let header_rows = 4 + extra_header_rows;
    if max_window_height < header_rows {
        return Ok(());
    }

    let max_content_height = max_window_height.saturating_sub(header_rows) as usize;
    if max_content_height == 0 {
        return Ok(());
    }

    let total_entries = if toc.is_empty() { 1 } else { toc.entries.len() };
    let content_height = total_entries.min(max_content_height).max(1);
    toc.ensure_visible(content_height);
    let max_scroll = total_entries.saturating_sub(content_height);
    if toc.scroll_offset > max_scroll {
        toc.scroll_offset = max_scroll;
    }

    let window_height = (content_height as u32).saturating_add(header_rows);
    if window_height > max_window_height {
        return Ok(());
    }
    let window_width = (inner_width + 2) as u32;
    if window_width > total_cols {
        return Ok(());
    }

    let start_col = (total_cols.saturating_sub(window_width)) / 2;
    let start_row = (image_rows_available.saturating_sub(window_height)) / 2;

    let mut writer = renderer.writer();
    let mut current_row = start_row as u16;
    let start_col_u16 = start_col as u16;
    let horizontal_border = "-".repeat(inner_width);

    print_inverted(
        &mut writer,
        start_col_u16,
        current_row,
        &format!("+{}+", horizontal_border),
        false,
    )?;
    current_row = current_row.saturating_add(1);

    let title_line = format!("|{: ^inner_width$}|", TITLE, inner_width = inner_width);
    print_inverted(&mut writer, start_col_u16, current_row, &title_line, false)?;
    current_row = current_row.saturating_add(1);

    if let Some(prompt) = search_prompt.as_ref() {
        let content = truncate_with_ellipsis(format!("  {}", prompt), inner_width);
        let line = format!("|{}|", content);
        print_inverted(&mut writer, start_col_u16, current_row, &line, false)?;
        current_row = current_row.saturating_add(1);
    }

    let divider = format!("|{}|", "-".repeat(inner_width));
    print_inverted(&mut writer, start_col_u16, current_row, &divider, false)?;
    current_row = current_row.saturating_add(1);

    let active_query = toc.active_query().map(|q| q.to_string());

    if toc.is_empty() {
        let content = truncate_with_ellipsis(format!("  {}", EMPTY_MESSAGE), inner_width);
        let line = format!("|{}|", content);
        print_inverted(&mut writer, start_col_u16, current_row, &line, false)?;
        current_row = current_row.saturating_add(1);
    } else {
        let start_index = toc.scroll_offset;
        let end_index = (start_index + content_height).min(toc.entries.len());
        for idx in start_index..end_index {
            let entry = &toc.entries[idx];
            let selected = idx == toc.selected;
            let current = toc
                .current_index()
                .map(|current| current == idx)
                .unwrap_or(false);
            let matching = toc.entry_matches(idx);
            let content = format_toc_line(
                entry,
                selected,
                current,
                matching,
                active_query.as_deref(),
                inner_width,
            );
            let line = format!("|{}|", content);
            print_inverted(&mut writer, start_col_u16, current_row, &line, matching)?;
            current_row = current_row.saturating_add(1);
        }

        let rendered = end_index - start_index;
        for _ in rendered..content_height {
            let line = format!("|{}|", " ".repeat(inner_width));
            print_inverted(&mut writer, start_col_u16, current_row, &line, false)?;
            current_row = current_row.saturating_add(1);
        }
    }

    print_inverted(
        &mut writer,
        start_col_u16,
        current_row,
        &format!("+{}+", horizontal_border),
        false,
    )?;

    Ok(())
}

fn print_inverted(
    writer: &mut impl Write,
    col: u16,
    row: u16,
    content: &str,
    highlight: bool,
) -> Result<()> {
    if highlight {
        crossterm::execute!(
            writer,
            cursor::MoveTo(col, row),
            SetAttribute(Attribute::Reverse),
            SetAttribute(Attribute::Bold),
            Print(content),
            SetAttribute(Attribute::Reset)
        )?;
    } else {
        crossterm::execute!(
            writer,
            cursor::MoveTo(col, row),
            SetAttribute(Attribute::Reverse),
            Print(content),
            SetAttribute(Attribute::Reset)
        )?;
    }
    Ok(())
}

fn toc_line_length(entry: &OutlineItem) -> usize {
    let indent_levels = entry.depth.min(8);
    let indent_width = indent_levels * 2;
    let page_suffix = format!(" (p{})", entry.page_index + 1);
    4 + indent_width + entry.title.len() + page_suffix.len()
}

fn format_toc_line(
    entry: &OutlineItem,
    selected: bool,
    current: bool,
    matching: bool,
    active_query: Option<&str>,
    inner_width: usize,
) -> String {
    let selected_marker = if selected { '>' } else { ' ' };
    let current_marker = if current { '*' } else { ' ' };
    let match_marker = if matching { '+' } else { ' ' };
    let indent_levels = entry.depth.min(8);
    let indent = "  ".repeat(indent_levels);
    let page_suffix = format!(" (p{})", entry.page_index + 1);

    let title = if matching {
        highlight_search_segment(&entry.title, active_query)
    } else {
        entry.title.clone()
    };

    let mut text = String::new();
    text.push(selected_marker);
    text.push(current_marker);
    text.push(match_marker);
    text.push(' ');
    text.push_str(&indent);
    text.push_str(&title);
    text.push_str(&page_suffix);

    truncate_with_ellipsis(text, inner_width)
}

fn highlight_search_segment(title: &str, query: Option<&str>) -> String {
    if let Some(query) = query {
        if !query.is_empty() && title.is_ascii() && query.is_ascii() {
            if let Some((start, end)) = find_ascii_match_range(title, query) {
                let mut highlighted = String::with_capacity(title.len() + 2);
                highlighted.push_str(&title[..start]);
                highlighted.push('[');
                highlighted.push_str(&title[start..end]);
                highlighted.push(']');
                highlighted.push_str(&title[end..]);
                return highlighted;
            }
        }
    }
    title.to_string()
}

fn find_ascii_match_range(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    if !haystack.is_ascii() || !needle.is_ascii() {
        return None;
    }
    let hay = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.len() > hay.len() {
        return None;
    }
    for start in 0..=hay.len() - needle_bytes.len() {
        if hay[start..start + needle_bytes.len()]
            .iter()
            .zip(needle_bytes)
            .all(|(&h, &n)| h.to_ascii_lowercase() == n.to_ascii_lowercase())
        {
            return Some((start, start + needle_bytes.len()));
        }
    }
    None
}

fn truncate_with_ellipsis(mut text: String, width: usize) -> String {
    if text.len() > width {
        if width <= 3 {
            text.truncate(width);
        } else {
            let mut truncated = text.chars().take(width - 3).collect::<String>();
            truncated.push_str("...");
            text = truncated;
        }
    }
    if text.len() < width {
        text.push_str(&" ".repeat(width - text.len()));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline(title: &str, page_index: usize) -> OutlineItem {
        OutlineItem {
            title: title.to_string(),
            page_index,
            depth: 0,
        }
    }

    #[test]
    fn toc_search_moves_between_matches() {
        let entries = vec![
            outline("Intro", 0),
            outline("Chapter One", 1),
            outline("Chapter Two", 2),
        ];
        let mut toc = TocWindow::from_outline(entries, 0);
        assert!(toc.begin_search());
        assert!(toc.update_search_query("chapter"));
        assert_eq!(toc.selected, 1);
        assert!(toc.search_next(1));
        assert_eq!(toc.selected, 2);
        assert!(toc.search_next(1));
        assert_eq!(toc.selected, 1);
        assert!(toc.search_prev(1));
        assert_eq!(toc.selected, 2);
    }

    #[test]
    fn toc_search_cancel_resets_state() {
        let entries = vec![outline("Intro", 0)];
        let mut toc = TocWindow::from_outline(entries, 0);
        assert!(toc.begin_search());
        toc.update_search_query("intro");
        assert_eq!(toc.search_matches.len(), 1);
        assert!(toc.finish_search_input());
        assert!(toc.cancel_search());
        assert!(toc.search_query.is_none());
        assert!(toc.search_matches.is_empty());
        assert!(toc.search_prompt().is_none());
    }
}

fn init_logging(project_dirs: &ProjectDirs) -> Result<WorkerGuard> {
    let log_dir = project_dirs.data_local_dir().join("logs");
    fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::never(log_dir, "termpdf.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(file_writer);
    let console_layer = tracing_subscriber::fmt::layer();

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(console_layer)
        .try_init()
        .map_err(|err| anyhow!(err))?;

    Ok(guard)
}

fn compute_scaled_dimensions(
    image: &RenderImage,
    effective_pixel_width: f32,
    effective_pixel_height: f32,
    available_cols: u32,
    available_rows: u32,
    total_cols: u32,
    total_rows: u32,
    pixel_width: u32,
    pixel_height: u32,
) -> (u32, u32) {
    let mut draw_cols = available_cols.max(1);
    let mut draw_rows = available_rows.max(1);

    if image.width == 0 || image.height == 0 {
        return (draw_cols, draw_rows);
    }

    if pixel_width > 0
        && pixel_height > 0
        && total_cols > 0
        && total_rows > 0
        && effective_pixel_width.is_finite()
        && effective_pixel_height.is_finite()
        && effective_pixel_width > 0.0
        && effective_pixel_height > 0.0
    {
        let cell_width = pixel_width as f32 / total_cols as f32;
        let cell_height = pixel_height as f32 / total_rows as f32;

        if cell_width > 0.0 && cell_height > 0.0 {
            let mut cols = (effective_pixel_width / cell_width).round().max(1.0);
            let mut rows = (effective_pixel_height / cell_height).round().max(1.0);

            if cols > available_cols as f32 {
                cols = available_cols as f32;
            }
            if rows > available_rows as f32 {
                rows = available_rows as f32;
            }

            draw_cols = cols as u32;
            draw_rows = rows as u32;
        }
    } else {
        let ratio = if effective_pixel_height > 0.0 {
            effective_pixel_width / effective_pixel_height
        } else {
            image.width as f32 / image.height as f32
        };
        if ratio.is_finite() && ratio > 0.0 {
            let mut cols = available_cols as f32;
            let mut rows = (cols / ratio).round().max(1.0);

            if rows > available_rows as f32 {
                rows = available_rows as f32;
                cols = (rows * ratio).round().max(1.0);
            }

            draw_cols = cols.min(available_cols as f32) as u32;
            draw_rows = rows.min(available_rows as f32) as u32;
        }
    }

    draw_cols = draw_cols.max(1).min(available_cols);
    draw_rows = draw_rows.max(1).min(available_rows);

    (draw_cols, draw_rows)
}

fn compute_viewport_origin(total: u32, viewport: u32, fraction: f32) -> u32 {
    if viewport >= total || total == 0 {
        return 0;
    }
    let max_offset = total - viewport;
    if max_offset == 0 {
        return 0;
    }
    let clamped = fraction.clamp(0.0, 1.0);
    let raw = (max_offset as f32 * clamped).round();
    raw.max(0.0).min(max_offset as f32) as u32
}

fn crop_render_image(
    image: &RenderImage,
    origin_x: u32,
    origin_y: u32,
    width: u32,
    height: u32,
) -> RenderImage {
    if image.width == 0 || image.height == 0 {
        return RenderImage {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
    }

    let width = width.min(image.width).max(1);
    let height = height.min(image.height).max(1);
    let max_origin_x = image.width.saturating_sub(width);
    let max_origin_y = image.height.saturating_sub(height);
    let origin_x = origin_x.min(max_origin_x);
    let origin_y = origin_y.min(max_origin_y);

    let stride = image.width as usize * 4;
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);

    for row in 0..height {
        let src_y = origin_y + row;
        let start = src_y as usize * stride + origin_x as usize * 4;
        let end = start + width as usize * 4;
        pixels.extend_from_slice(&image.pixels[start..end]);
    }

    RenderImage {
        width,
        height,
        pixels,
    }
}

struct HighlightGeometry {
    base_width: u32,
    base_height: u32,
    crop: Option<CropRegion>,
}

impl HighlightGeometry {
    fn new(base_width: u32, base_height: u32) -> Self {
        Self {
            base_width,
            base_height,
            crop: None,
        }
    }

    fn set_base(&mut self, width: u32, height: u32) {
        self.base_width = width.max(1);
        self.base_height = height.max(1);
    }

    fn set_crop(&mut self, offset_x: u32, offset_y: u32, width: u32, height: u32) {
        self.crop = Some(CropRegion {
            offset_x,
            offset_y,
            width,
            height,
        });
    }

    fn clear_crop(&mut self) {
        self.crop = None;
    }
}

#[derive(Clone, Copy)]
struct CropRegion {
    offset_x: u32,
    offset_y: u32,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy)]
struct PixelRect {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

fn apply_highlights(image: &mut RenderImage, highlights: &Highlights, geom: &HighlightGeometry) {
    if image.width == 0 || image.height == 0 {
        return;
    }

    if highlights.is_empty() {
        return;
    }

    let other_rects: Vec<PixelRect> = highlights
        .others
        .iter()
        .filter_map(|rect| normalized_to_pixel_rect(*rect, geom))
        .collect();
    let current_rects: Vec<PixelRect> = highlights
        .current
        .iter()
        .filter_map(|rect| normalized_to_pixel_rect(*rect, geom))
        .collect();

    for rect in other_rects {
        stroke_rect(image, rect, [255, 200, 0]);
    }
    for rect in current_rects {
        fill_rect(image, rect, [255, 235, 0], 0.35);
        stroke_rect(image, rect, [255, 235, 0]);
    }
}

fn open_external_link(target: &ExternalLink) -> Result<()> {
    match target {
        ExternalLink::Url(uri) => open_uri(uri),
        ExternalLink::File(path) => open_path(path),
    }
}

fn open_uri(uri: &str) -> Result<()> {
    if let Ok(url) = Url::parse(uri) {
        if url.scheme() == "file" {
            if let Ok(path) = url.to_file_path() {
                return open_path(&path);
            }
        }
    }
    spawn_open_command(OsStr::new(uri))
}

fn open_path(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("link target {:?} does not exist", path));
    }
    spawn_open_command(path.as_os_str())
}

fn spawn_open_command(target: &OsStr) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let status = ProcessCommand::new("cmd")
            .arg("/C")
            .arg("start")
            .arg("")
            .arg(target)
            .status()
            .with_context(|| format!("failed to spawn open command for {:?}", target))?;
        if !status.success() {
            return Err(anyhow!(
                "open command exited with status {:?}",
                status.code()
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let status = ProcessCommand::new(OPEN_COMMAND)
            .arg(target)
            .status()
            .with_context(|| format!("failed to spawn '{}' for {:?}", OPEN_COMMAND, target))?;
        if !status.success() {
            return Err(anyhow!(
                "'{}' exited with status {:?}",
                OPEN_COMMAND,
                status.code()
            ));
        }
        Ok(())
    }
}

fn normalized_to_pixel_rect(rect: NormalizedRect, geom: &HighlightGeometry) -> Option<PixelRect> {
    let width_f = geom.base_width as f32;
    let height_f = geom.base_height as f32;
    if width_f <= 0.0 || height_f <= 0.0 {
        return None;
    }

    let mut x0 = (rect.left * width_f).floor() as i32;
    let mut x1 = (rect.right * width_f).ceil() as i32;
    let mut y0 = (rect.top * height_f).floor() as i32;
    let mut y1 = (rect.bottom * height_f).ceil() as i32;

    let max_x = geom.base_width as i32;
    let max_y = geom.base_height as i32;
    x0 = x0.clamp(0, max_x);
    x1 = x1.clamp(0, max_x);
    y0 = y0.clamp(0, max_y);
    y1 = y1.clamp(0, max_y);

    if let Some(crop) = &geom.crop {
        x0 -= crop.offset_x as i32;
        x1 -= crop.offset_x as i32;
        y0 -= crop.offset_y as i32;
        y1 -= crop.offset_y as i32;

        let crop_max_x = crop.width as i32;
        let crop_max_y = crop.height as i32;
        x0 = x0.clamp(0, crop_max_x);
        x1 = x1.clamp(0, crop_max_x);
        y0 = y0.clamp(0, crop_max_y);
        y1 = y1.clamp(0, crop_max_y);
    }

    if x1 - x0 <= 0 || y1 - y0 <= 0 {
        return None;
    }

    Some(PixelRect {
        x0: x0 as u32,
        y0: y0 as u32,
        x1: x1 as u32,
        y1: y1 as u32,
    })
}

fn fill_rect(image: &mut RenderImage, rect: PixelRect, color: [u8; 3], alpha: f32) {
    if rect.x0 >= rect.x1 || rect.y0 >= rect.y1 {
        return;
    }
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let x1 = rect.x1.min(image.width);
    let y1 = rect.y1.min(image.height);
    let x0 = rect.x0.min(x1);
    let y0 = rect.y0.min(y1);

    for y in y0..y1 {
        let row_start = (y as usize) * width * 4;
        for x in x0..x1 {
            let idx = row_start + (x as usize) * 4;
            blend_pixel(&mut image.pixels[idx..idx + 4], color, alpha);
        }
    }
}

fn blend_pixel(pixel: &mut [u8], color: [u8; 3], alpha: f32) {
    let alpha = alpha.clamp(0.0, 1.0);
    let inv = 1.0 - alpha;
    pixel[0] = ((pixel[0] as f32 * inv) + (color[0] as f32 * alpha))
        .round()
        .clamp(0.0, 255.0) as u8;
    pixel[1] = ((pixel[1] as f32 * inv) + (color[1] as f32 * alpha))
        .round()
        .clamp(0.0, 255.0) as u8;
    pixel[2] = ((pixel[2] as f32 * inv) + (color[2] as f32 * alpha))
        .round()
        .clamp(0.0, 255.0) as u8;
}

fn stroke_rect(image: &mut RenderImage, rect: PixelRect, color: [u8; 3]) {
    if rect.x0 >= rect.x1 || rect.y0 >= rect.y1 {
        return;
    }
    let width = image.width as usize;
    if width == 0 {
        return;
    }
    let x0 = rect.x0.min(image.width - 1);
    let x1 = rect.x1.min(image.width);
    let y0 = rect.y0.min(image.height - 1);
    let y1 = rect.y1.min(image.height);
    let thickness = 2u32;

    for y in y0..y1 {
        let row_start = (y as usize) * width * 4;
        for x in x0..(x0 + thickness.min(x1 - x0)) {
            overwrite_pixel(
                &mut image.pixels[row_start + (x as usize) * 4..][..4],
                color,
            );
        }
        if x1 > x0 + thickness {
            for x in (x1.saturating_sub(thickness))..x1 {
                overwrite_pixel(
                    &mut image.pixels[row_start + (x as usize) * 4..][..4],
                    color,
                );
            }
        }
    }

    for y in y0..(y0 + thickness.min(y1 - y0)) {
        let row_start = (y as usize) * width * 4;
        for x in x0..x1 {
            overwrite_pixel(
                &mut image.pixels[row_start + (x as usize) * 4..][..4],
                color,
            );
        }
    }
    if y1 > y0 + thickness {
        for y in (y1.saturating_sub(thickness))..y1 {
            let row_start = (y as usize) * width * 4;
            for x in x0..x1 {
                overwrite_pixel(
                    &mut image.pixels[row_start + (x as usize) * 4..][..4],
                    color,
                );
            }
        }
    }
}

fn overwrite_pixel(pixel: &mut [u8], color: [u8; 3]) {
    pixel[0] = color[0];
    pixel[1] = color[1];
    pixel[2] = color[2];
}

fn format_document_status(doc: &DocumentInstance) -> String {
    let zoom_percent = doc.state.scale * 100.0;
    let zoom_display = if zoom_percent.is_finite() {
        format!("{:.0}%", zoom_percent)
    } else {
        "—".to_string()
    };

    let mut status = format!(
        "{} — page {}/{} — {}",
        doc.info
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>"),
        doc.state.current_page + 1,
        doc.info.page_count,
        zoom_display
    );

    if let Some(summary) = doc.search_summary() {
        status.push_str(" — /");
        status.push_str(&summary.query);
        if summary.total == 0 {
            status.push_str(" (no matches)");
        } else if let Some(index) = summary.current_index {
            status.push_str(&format!(" ({}/{})", index + 1, summary.total));
        } else {
            status.push_str(&format!(" (0/{})", summary.total));
        }
    }

    if let Some(summary) = doc.link_summary() {
        status.push_str(" — link");
        if summary.total == 0 {
            status.push_str(" (no links)");
        } else if let Some(index) = summary.current_index {
            status.push_str(&format!(" ({}/{})", index + 1, summary.total));
        } else {
            status.push_str(&format!(" (0/{})", summary.total));
        }
    }

    status
}
