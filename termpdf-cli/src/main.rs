use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use crossterm::cursor;
use crossterm::event;
use crossterm::terminal::{self, Clear, ClearType};
use directories::ProjectDirs;
use termpdf_core::{Command, DocumentInstance, FileStateStore, RenderImage, Session, StateStore};
use termpdf_render::PdfRenderFactory;
use termpdf_tty::{write_status_line, DrawParams, EventMapper, KittyRenderer, UiEvent};
use tracing::warn;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{prelude::*, EnvFilter};

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

    let provider = PdfRenderFactory::new()?;
    for path in &args.files {
        session
            .open_with(&provider, path.clone())
            .await
            .with_context(|| format!("failed to open {:?}", path))?;
    }

    if let Some(page) = args.page {
        session.apply(Command::GotoPage { page })?;
    }

    let _raw = RawModeGuard::new()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, cursor::Hide)?;
    let mut renderer = KittyRenderer::new(stdout);
    let mut event_mapper = EventMapper::new();
    let mut dirty = true;
    let mut needs_initial_clear = true;

    loop {
        if dirty {
            let pending = event_mapper.pending_input();
            if needs_initial_clear {
                {
                    let mut writer = renderer.writer();
                    crossterm::execute!(&mut writer, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
                }
                needs_initial_clear = false;
            }
            redraw(&mut renderer, &session, pending.as_deref())?;
            dirty = false;
        }

        if event::poll(Duration::from_millis(100))? {
            let ev = event::read()?;
            let ui_event = event_mapper.map_event(ev);
            let pending = event_mapper.pending_input();
            if let Some(status) = combine_status(document_status(&session), pending.as_deref()) {
                draw_status_line(&mut renderer, &status)?;
            }
            match handle_event(ui_event, &mut session)? {
                LoopAction::ContinueRedraw => dirty = true,
                LoopAction::Continue => {}
                LoopAction::Quit => break,
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

fn handle_event(event: UiEvent, session: &mut Session) -> Result<LoopAction> {
    match event {
        UiEvent::Command(cmd) => {
            let redraw = matches!(
                cmd,
                Command::GotoPage { .. }
                    | Command::NextPage { .. }
                    | Command::PrevPage { .. }
                    | Command::ScaleBy { .. }
                    | Command::AdjustViewport { .. }
                    | Command::GotoMark { .. }
                    | Command::ToggleDarkMode
                    | Command::SwitchDocument { .. }
            );
            session.apply(cmd)?;
            if redraw {
                Ok(LoopAction::ContinueRedraw)
            } else {
                Ok(LoopAction::Continue)
            }
        }
        UiEvent::Quit => Ok(LoopAction::Quit),
        UiEvent::None => Ok(LoopAction::Continue),
    }
}

fn redraw(
    renderer: &mut KittyRenderer<io::Stdout>,
    session: &Session,
    pending_input: Option<&str>,
) -> Result<()> {
    if let Some(doc) = session.active() {
        let window = terminal::window_size()?;
        let total_cols = u32::from(window.columns).max(1);
        let total_rows = u32::from(window.rows).max(1);
        let pixel_width = u32::from(window.width);
        let pixel_height = u32::from(window.height);

        let image_rows_available = total_rows.saturating_sub(1).max(1);
        let margin_cols = total_cols.min(2);
        let margin_rows = image_rows_available.min(2);
        let available_cols = total_cols.saturating_sub(margin_cols).max(1);
        let available_rows = image_rows_available.saturating_sub(margin_rows).max(1);

        let base_scale = doc.state.scale;
        let mut render_scale = base_scale;
        let mut image = doc.render_with_scale(base_scale)?;

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
                }
            }
        }

        let zoom_scale = doc.state.scale;
        let mut display_image = image;

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
                    display_image = crop_render_image(
                        &display_image,
                        offset_x,
                        offset_y,
                        crop_width,
                        crop_height,
                    );
                }
            }
        }

        let (draw_cols, draw_rows) = compute_scaled_dimensions(
            &display_image,
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

    if pixel_width > 0 && pixel_height > 0 && total_cols > 0 && total_rows > 0 {
        let cell_width = pixel_width as f32 / total_cols as f32;
        let cell_height = pixel_height as f32 / total_rows as f32;

        if cell_width > 0.0 && cell_height > 0.0 {
            let mut cols = (image.width as f32 / cell_width).round().max(1.0);
            let mut rows = (image.height as f32 / cell_height).round().max(1.0);

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
        let ratio = image.width as f32 / image.height as f32;
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

fn format_document_status(doc: &DocumentInstance) -> String {
    let zoom_percent = doc.state.scale * 100.0;
    let zoom_display = if zoom_percent.is_finite() {
        format!("{:.0}%", zoom_percent)
    } else {
        "—".to_string()
    };

    format!(
        "{} — page {}/{} — {}",
        doc.info
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>"),
        doc.state.current_page + 1,
        doc.info.page_count,
        zoom_display
    )
}
