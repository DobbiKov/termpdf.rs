use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use crossterm::cursor;
use crossterm::event;
use crossterm::event::Event;
use crossterm::terminal::{self, Clear, ClearType};
use directories::ProjectDirs;
use termpdf_core::{Command, FileStateStore, Session, StateStore};
use termpdf_render::PdfRenderFactory;
use termpdf_tty::{map_event, write_status_line, KittyRenderer, UiEvent};

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
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    if args.files.is_empty() {
        return Err(anyhow!("no input files provided"));
    }

    let project_dirs = ProjectDirs::from("net", "termpdf", "termpdf")
        .ok_or_else(|| anyhow!("unable to resolve platform data directories"))?;
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
    let mut dirty = true;

    loop {
        if dirty {
            redraw(&mut renderer, &session)?;
            dirty = false;
        }

        if event::poll(Duration::from_millis(100))? {
            let ev = event::read()?;
            match handle_event(ev, &mut session)? {
                LoopAction::ContinueRedraw => dirty = true,
                LoopAction::Continue => {}
                LoopAction::Quit => break,
            }
        }
    }

    session.persist()?;
    Ok(())
}

enum LoopAction {
    Continue,
    ContinueRedraw,
    Quit,
}

fn handle_event(ev: Event, session: &mut Session) -> Result<LoopAction> {
    match map_event(ev) {
        UiEvent::Command(cmd) => {
            let redraw = matches!(
                cmd,
                Command::GotoPage { .. }
                    | Command::NextPage { .. }
                    | Command::PrevPage { .. }
                    | Command::ScaleBy { .. }
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

fn redraw(renderer: &mut KittyRenderer<io::Stdout>, session: &Session) -> Result<()> {
    {
        let mut writer = renderer.writer();
        crossterm::execute!(&mut writer, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
    }

    if let Some(doc) = session.active() {
        let image = doc.render()?;
        renderer.draw(&image)?;
        let info = &doc.info;
        let status = format!(
            "{} — page {}/{}",
            info.path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("<unknown>"),
            doc.state.current_page + 1,
            info.page_count
        );
        let (_, rows) = terminal::size()?;
        {
            let mut writer = renderer.writer();
            crossterm::execute!(
                &mut writer,
                cursor::MoveTo(0, rows.saturating_sub(1)),
                Clear(ClearType::CurrentLine)
            )?;
            write_status_line(&mut writer, &status)?;
        }
    }
    Ok(())
}
