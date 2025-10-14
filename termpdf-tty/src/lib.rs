use std::io::{self, Write};

use anyhow::Result;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use png::{BitDepth, ColorType, Encoder};
use termpdf_core::{Command, RenderImage};

pub struct KittyRenderer<W: Write> {
    writer: W,
    image_id: u32,
    placement_id: u32,
}

pub struct DrawParams {
    pub columns: u32,
    pub rows: u32,
}

impl DrawParams {
    pub fn clamped(columns: u32, rows: u32) -> Self {
        Self {
            columns: columns.max(1),
            rows: rows.max(1),
        }
    }
}

impl<W: Write> KittyRenderer<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            image_id: 1,
            placement_id: 1,
        }
    }

    pub fn writer(&mut self) -> &mut W {
        &mut self.writer
    }

    pub fn draw(&mut self, image: &RenderImage, params: DrawParams) -> Result<()> {
        let mut buffer = Vec::new();
        let mut encoder = Encoder::new(&mut buffer, image.width, image.height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&image.pixels)?;
        writer.finish()?;

        let encoded = BASE64.encode(&buffer);
        let mut chunks = encoded.as_bytes().chunks(4096).peekable();
        let mut first = true;

        while let Some(chunk) = chunks.next() {
            let more = chunks.peek().is_some();
            if first {
                write!(
                    self.writer,
                    "\u{1b}_Ga=T,f=100,C=1,q=2,i={},p={},c={},r={},s={},v={},m={}",
                    self.image_id,
                    self.placement_id,
                    params.columns,
                    params.rows,
                    image.width,
                    image.height,
                    if more { 1 } else { 0 }
                )?;
                first = false;
            } else {
                write!(self.writer, "\u{1b}_Gm={},q=2", if more { 1 } else { 0 })?;
            }
            if !chunk.is_empty() {
                self.writer.write_all(b";")?;
                self.writer.write_all(chunk)?;
            }
            write!(self.writer, "\u{1b}\\")?;
        }

        self.writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_draw_emits_protocol() {
        let mut renderer = KittyRenderer::new(Vec::new());
        let image = RenderImage {
            width: 1,
            height: 1,
            pixels: vec![255, 0, 0, 255],
        };

        renderer.draw(&image, DrawParams::clamped(10, 5)).unwrap();
        let output = renderer.writer;
        assert_eq!(output[0], 0x1b);
        assert_eq!(output[1], b'_');
        assert_eq!(output[2], b'G');
    }
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Command(Command),
    Quit,
    None,
}

pub fn map_event(event: Event) -> UiEvent {
    match event {
        Event::Key(KeyEvent {
            code, modifiers, ..
        }) => match (code, modifiers) {
            (KeyCode::Char('q'), _) => UiEvent::Quit,
            (KeyCode::Char('j'), KeyModifiers::NONE) | (KeyCode::Down, KeyModifiers::NONE) => {
                UiEvent::Command(Command::NextPage { count: 1 })
            }
            (KeyCode::Char('k'), KeyModifiers::NONE) | (KeyCode::Up, KeyModifiers::NONE) => {
                UiEvent::Command(Command::PrevPage { count: 1 })
            }
            (KeyCode::Char('+'), _) => UiEvent::Command(Command::ScaleBy { factor: 1.1 }),
            (KeyCode::Char('-'), _) => UiEvent::Command(Command::ScaleBy { factor: 0.9 }),
            (KeyCode::Char('d'), _) => UiEvent::Command(Command::ToggleDarkMode),
            (KeyCode::Char('g'), KeyModifiers::NONE) => {
                UiEvent::Command(Command::GotoPage { page: 0 })
            }
            (KeyCode::Char('G'), KeyModifiers::SHIFT) | (KeyCode::End, _) => {
                UiEvent::Command(Command::GotoPage { page: usize::MAX })
            }
            _ => UiEvent::None,
        },
        _ => UiEvent::None,
    }
}

pub fn write_status_line<W: Write>(writer: &mut W, label: &str) -> io::Result<()> {
    write!(writer, "{}", label)?;
    writer.flush()
}
