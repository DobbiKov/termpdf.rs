use std::io::{self, Write};

use anyhow::Result;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use crossterm::{
    cursor,
    event::{Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{Clear, ClearType},
};
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
                    "\u{1b}_Ga=T,f=100,C=1,q=2,i={},p={},c={},r={},s={},v={},z=-1,m={}",
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

    pub fn begin_sync_update(&mut self) -> Result<()> {
        write!(self.writer, "\u{1b}[?2026h")?;
        Ok(())
    }

    /// Disables synchronized updates.
    /// The terminal will render all buffered changes at once.
    pub fn end_sync_update(&mut self) -> Result<()> {
        write!(self.writer, "\u{1b}[?2026l")?;
        self.writer.flush()?;
        Ok(())
    }

    /// Clears the entire screen.
    pub fn clear_all(&mut self) -> Result<()> {
        crossterm::execute!(
            &mut self.writer,
            Clear(ClearType::All),
            cursor::MoveTo(0, 0)
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

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

    fn key_event(code: KeyCode) -> Event {
        key_event_with_modifiers(code, KeyModifiers::NONE)
    }

    fn key_event_with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn event_mapper_uses_numeric_prefix_for_next_page() {
        let mut mapper = EventMapper::new();
        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('1'))),
            UiEvent::None
        ));
        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('2'))),
            UiEvent::None
        ));

        match mapper.map_event(key_event(KeyCode::Char('j'))) {
            UiEvent::Command(Command::NextPage { count }) => assert_eq!(count, 12),
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_resets_prefix_after_use() {
        let mut mapper = EventMapper::new();
        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('3'))),
            UiEvent::None
        ));

        match mapper.map_event(key_event(KeyCode::Char('k'))) {
            UiEvent::Command(Command::PrevPage { count }) => assert_eq!(count, 3),
            other => panic!("unexpected event: {:?}", other),
        }

        match mapper.map_event(key_event(KeyCode::Char('k'))) {
            UiEvent::Command(Command::PrevPage { count }) => assert_eq!(count, 1),
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_drops_prefix_on_other_command() {
        let mut mapper = EventMapper::new();
        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('4'))),
            UiEvent::None
        ));
        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('q'))),
            UiEvent::Quit
        ));

        match mapper.map_event(key_event(KeyCode::Char('j'))) {
            UiEvent::Command(Command::NextPage { count }) => assert_eq!(count, 1),
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_pending_input_shows_digits_until_consumed() {
        let mut mapper = EventMapper::new();
        assert!(mapper.pending_input().is_none());
        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('1'))),
            UiEvent::None
        ));
        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('2'))),
            UiEvent::None
        ));
        assert_eq!(mapper.pending_input().as_deref(), Some("12"));

        match mapper.map_event(key_event(KeyCode::Char('j'))) {
            UiEvent::Command(Command::NextPage { count }) => assert_eq!(count, 12),
            other => panic!("unexpected event: {:?}", other),
        }
        assert!(mapper.pending_input().is_none());
    }

    #[test]
    fn event_mapper_pending_input_shows_char_stack_until_completed() {
        let mut mapper = EventMapper::new();
        assert!(mapper.pending_input().is_none());
        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('m'))),
            UiEvent::None
        ));
        assert_eq!(mapper.pending_input().as_deref(), Some("m"));

        match mapper.map_event(key_event(KeyCode::Char('G'))) {
            UiEvent::Command(Command::PutMark { key }) => assert_eq!(key, 'G'),
            other => panic!("unexpected event: {:?}", other),
        }
        assert!(mapper.pending_input().is_none());
    }

    #[test]
    fn event_mapper_maps_ctrl_arrows_to_viewport_adjustment() {
        let mut mapper = EventMapper::new();

        match mapper.map_event(key_event_with_modifiers(
            KeyCode::Right,
            KeyModifiers::CONTROL,
        )) {
            UiEvent::Command(Command::AdjustViewport { delta_x, delta_y }) => {
                assert!((delta_x - EventMapper::PAN_STEP).abs() < f32::EPSILON);
                assert_eq!(delta_y, 0.0);
            }
            other => panic!("unexpected event: {:?}", other),
        }

        match mapper.map_event(key_event_with_modifiers(KeyCode::Up, KeyModifiers::CONTROL)) {
            UiEvent::Command(Command::AdjustViewport { delta_x, delta_y }) => {
                assert_eq!(delta_x, 0.0);
                assert!((delta_y + EventMapper::PAN_STEP).abs() < f32::EPSILON);
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_maps_equal_to_reset_scale() {
        let mut mapper = EventMapper::new();
        match mapper.map_event(key_event(KeyCode::Char('='))) {
            UiEvent::Command(Command::ResetScale) => {}
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_maps_letter_shortcuts_to_viewport_adjustment() {
        let mut mapper = EventMapper::new();

        match mapper.map_event(key_event(KeyCode::Char('h'))) {
            UiEvent::Command(Command::AdjustViewport { delta_x, delta_y }) => {
                assert!((delta_x + EventMapper::PAN_STEP).abs() < f32::EPSILON);
                assert_eq!(delta_y, 0.0);
            }
            other => panic!("unexpected event: {:?}", other),
        }

        match mapper.map_event(key_event_with_modifiers(
            KeyCode::Char('J'),
            KeyModifiers::SHIFT,
        )) {
            UiEvent::Command(Command::AdjustViewport { delta_x, delta_y }) => {
                assert_eq!(delta_x, 0.0);
                assert!((delta_y - EventMapper::PAN_STEP).abs() < f32::EPSILON);
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_numeric_prefix_scales_pan_distance() {
        let mut mapper = EventMapper::new();
        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('3'))),
            UiEvent::None
        ));
        match mapper.map_event(key_event(KeyCode::Char('l'))) {
            UiEvent::Command(Command::AdjustViewport { delta_x, delta_y }) => {
                assert!((delta_x - 3.0 * EventMapper::PAN_STEP).abs() < f32::EPSILON);
                assert_eq!(delta_y, 0.0);
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_maps_t_to_open_toc() {
        let mut mapper = EventMapper::new();
        match mapper.map_event(key_event(KeyCode::Char('t'))) {
            UiEvent::OpenTableOfContents => {}
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_maps_ctrl_o_to_jump_backward() {
        let mut mapper = EventMapper::new();
        match mapper.map_event(key_event_with_modifiers(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
        )) {
            UiEvent::Command(Command::JumpBackward) => {}
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_maps_ctrl_i_variants_to_jump_forward() {
        let mut mapper = EventMapper::new();
        match mapper.map_event(key_event_with_modifiers(
            KeyCode::Char('i'),
            KeyModifiers::CONTROL,
        )) {
            UiEvent::Command(Command::JumpForward) => {}
            other => panic!("unexpected event: {:?}", other),
        }

        match mapper.map_event(key_event_with_modifiers(
            KeyCode::Tab,
            KeyModifiers::CONTROL,
        )) {
            UiEvent::Command(Command::JumpForward) => {}
            other => panic!("unexpected event: {:?}", other),
        }

        match mapper.map_event(key_event_with_modifiers(KeyCode::Tab, KeyModifiers::NONE)) {
            UiEvent::Command(Command::JumpForward) => {}
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_maps_n_and_uppercase_n_to_search_navigation() {
        let mut mapper = EventMapper::new();

        match mapper.map_event(key_event(KeyCode::Char('n'))) {
            UiEvent::Command(Command::SearchNext { count }) => assert_eq!(count, 1),
            other => panic!("unexpected event: {:?}", other),
        }

        match mapper.map_event(key_event_with_modifiers(
            KeyCode::Char('N'),
            KeyModifiers::SHIFT,
        )) {
            UiEvent::Command(Command::SearchPrev { count }) => assert_eq!(count, 1),
            other => panic!("unexpected event: {:?}", other),
        }

        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('3'))),
            UiEvent::None
        ));

        match mapper.map_event(key_event(KeyCode::Char('n'))) {
            UiEvent::Command(Command::SearchNext { count }) => assert_eq!(count, 3),
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_slash_enters_search_mode_and_collects_input() {
        let mut mapper = EventMapper::new();

        match mapper.map_event(key_event(KeyCode::Char('/'))) {
            UiEvent::BeginSearch => {}
            other => panic!("unexpected event: {:?}", other),
        }
        assert_eq!(mapper.pending_input().as_deref(), Some("/"));

        match mapper.map_event(key_event(KeyCode::Char('f'))) {
            UiEvent::SearchQueryChanged { ref query } => assert_eq!(query, "f"),
            other => panic!("unexpected event: {:?}", other),
        }
        assert_eq!(mapper.pending_input().as_deref(), Some("/f"));

        match mapper.map_event(key_event(KeyCode::Backspace)) {
            UiEvent::SearchQueryChanged { ref query } => assert!(query.is_empty()),
            other => panic!("unexpected event: {:?}", other),
        }
        assert_eq!(mapper.pending_input().as_deref(), Some("/"));

        match mapper.map_event(key_event(KeyCode::Char('g'))) {
            UiEvent::SearchQueryChanged { ref query } => assert_eq!(query, "g"),
            other => panic!("unexpected event: {:?}", other),
        }

        match mapper.map_event(key_event(KeyCode::Enter)) {
            UiEvent::SearchSubmit { ref query } => assert_eq!(query, "g"),
            other => panic!("unexpected event: {:?}", other),
        }
        assert!(mapper.pending_input().is_none());
    }

    #[test]
    fn event_mapper_toc_mode_maps_navigation_keys() {
        let mut mapper = EventMapper::new();
        mapper.set_mode(InputMode::Toc);

        match mapper.map_event(key_event(KeyCode::Char('j'))) {
            UiEvent::TocMoveSelection { delta } => assert_eq!(delta, 1),
            other => panic!("unexpected event: {:?}", other),
        }

        match mapper.map_event(key_event(KeyCode::Char('k'))) {
            UiEvent::TocMoveSelection { delta } => assert_eq!(delta, -1),
            other => panic!("unexpected event: {:?}", other),
        }

        match mapper.map_event(key_event(KeyCode::Enter)) {
            UiEvent::TocActivateSelection => {}
            other => panic!("unexpected event: {:?}", other),
        }

        match mapper.map_event(key_event(KeyCode::Esc)) {
            UiEvent::CloseOverlay => {}
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn event_mapper_switching_modes_clears_pending_state() {
        let mut mapper = EventMapper::new();
        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('1'))),
            UiEvent::None
        ));
        assert_eq!(mapper.pending_input().as_deref(), Some("1"));

        mapper.set_mode(InputMode::Toc);
        assert!(mapper.pending_input().is_none());
        mapper.set_mode(InputMode::Normal);
        assert!(mapper.pending_input().is_none());

        assert!(matches!(
            mapper.map_event(key_event(KeyCode::Char('/'))),
            UiEvent::BeginSearch
        ));
        assert_eq!(mapper.pending_input().as_deref(), Some("/"));

        mapper.set_mode(InputMode::Normal);
        assert!(mapper.pending_input().is_none());
    }
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Command(Command),
    OpenTableOfContents,
    CloseOverlay,
    TocMoveSelection { delta: isize },
    TocActivateSelection,
    BeginSearch,
    SearchQueryChanged { query: String },
    SearchSubmit { query: String },
    SearchCancel,
    Quit,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Toc,
    Search,
}

impl Default for InputMode {
    fn default() -> Self {
        InputMode::Normal
    }
}

#[derive(Debug, Default)]
pub struct EventMapper {
    pending_count: Option<usize>,
    pending_digits: String,
    char_stack: String,
    mode: InputMode,
    search_buffer: String,
}

impl EventMapper {
    const PAN_STEP: f32 = 0.1;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_mode(&mut self, mode: InputMode) {
        if self.mode != mode {
            if matches!(self.mode, InputMode::Search) {
                self.search_buffer.clear();
            }
            self.reset_count();
            self.reset_char_stack();
            self.mode = mode;
            if matches!(self.mode, InputMode::Search) {
                self.search_buffer.clear();
            }
        }
    }

    pub fn mode(&self) -> InputMode {
        self.mode
    }

    pub fn map_event(&mut self, event: Event) -> UiEvent {
        match self.mode {
            InputMode::Normal => self.map_event_normal(event),
            InputMode::Toc => self.map_event_toc(event),
            InputMode::Search => self.map_event_search(event),
        }
    }

    fn map_event_normal(&mut self, event: Event) -> UiEvent {
        match event {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match (code, modifiers) {
                (KeyCode::Char(c), KeyModifiers::NONE) if c.is_ascii_digit() => {
                    if let Some(digit) = c.to_digit(10) {
                        self.push_digit(digit as usize);
                    }
                    UiEvent::None
                }
                (KeyCode::Char(c), _) if (self.char_stack.as_str() == "m") => {
                    self.reset_char_stack();
                    UiEvent::Command(Command::PutMark { key: c })
                }
                (KeyCode::Char(c), _) if (self.char_stack.as_str() == "\'") => {
                    self.reset_char_stack();
                    UiEvent::Command(Command::GotoMark { key: c })
                }
                (KeyCode::Char('m'), _) => {
                    if self.char_stack.is_empty() {
                        self.push_char('m');
                    }
                    UiEvent::None
                }
                (KeyCode::Char('\''), _) => {
                    if self.char_stack.is_empty() {
                        self.push_char('\'');
                    }
                    UiEvent::None
                }
                (KeyCode::Char('='), _) => {
                    self.reset_count();
                    self.reset_char_stack();
                    UiEvent::Command(Command::ResetScale)
                }
                (KeyCode::Left, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.pan(-Self::PAN_STEP, 0.0)
                }
                (KeyCode::Right, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.pan(Self::PAN_STEP, 0.0)
                }
                (KeyCode::Up, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.pan(0.0, -Self::PAN_STEP)
                }
                (KeyCode::Down, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.pan(0.0, Self::PAN_STEP)
                }
                (KeyCode::Char('H'), KeyModifiers::SHIFT)
                | (KeyCode::Char('h'), KeyModifiers::NONE) => self.pan(-Self::PAN_STEP, 0.0),
                (KeyCode::Char('L'), KeyModifiers::SHIFT)
                | (KeyCode::Char('l'), KeyModifiers::NONE) => self.pan(Self::PAN_STEP, 0.0),
                (KeyCode::Char('K'), KeyModifiers::SHIFT) => self.pan(0.0, -Self::PAN_STEP),
                (KeyCode::Char('J'), KeyModifiers::SHIFT) => self.pan(0.0, Self::PAN_STEP),
                (KeyCode::Char('j'), KeyModifiers::NONE) | (KeyCode::Down, KeyModifiers::NONE) => {
                    let count = self.take_count();
                    UiEvent::Command(Command::NextPage { count })
                }
                (KeyCode::Char('k'), KeyModifiers::NONE) | (KeyCode::Up, KeyModifiers::NONE) => {
                    let count = self.take_count();
                    UiEvent::Command(Command::PrevPage { count })
                }
                (KeyCode::Char('/'), KeyModifiers::NONE) => {
                    self.start_search();
                    UiEvent::BeginSearch
                }
                (KeyCode::Char('n'), KeyModifiers::NONE) => {
                    let count = self.take_count();
                    UiEvent::Command(Command::SearchNext { count })
                }
                (KeyCode::Char('N'), modifiers)
                    if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
                {
                    let count = self.take_count();
                    UiEvent::Command(Command::SearchPrev { count })
                }
                (KeyCode::Char('q'), _) => {
                    self.reset_count();
                    UiEvent::Quit
                }
                (KeyCode::Char('o'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.reset_count();
                    self.reset_char_stack();
                    UiEvent::Command(Command::JumpBackward)
                }
                (KeyCode::Char('i'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.reset_count();
                    self.reset_char_stack();
                    UiEvent::Command(Command::JumpForward)
                }
                (KeyCode::Tab, modifiers)
                    if modifiers.is_empty() || modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.reset_count();
                    self.reset_char_stack();
                    UiEvent::Command(Command::JumpForward)
                }
                (KeyCode::Char('+'), _) => {
                    self.reset_count();
                    UiEvent::Command(Command::ScaleBy { factor: 1.1 })
                }
                (KeyCode::Char('-'), _) => {
                    self.reset_count();
                    UiEvent::Command(Command::ScaleBy { factor: 0.9 })
                }
                (KeyCode::Char('d'), _) => {
                    self.reset_count();
                    UiEvent::Command(Command::ToggleDarkMode)
                }
                (KeyCode::Char('g'), KeyModifiers::NONE) => {
                    self.reset_count();
                    UiEvent::Command(Command::GotoPage { page: 0 })
                }
                (KeyCode::Char('G'), KeyModifiers::SHIFT) | (KeyCode::End, _) => {
                    self.reset_count();
                    UiEvent::Command(Command::GotoPage { page: usize::MAX })
                }
                (KeyCode::Char('t'), _) | (KeyCode::Char('T'), _) => {
                    self.reset_count();
                    self.reset_char_stack();
                    UiEvent::OpenTableOfContents
                }
                _ => {
                    self.reset_count();
                    UiEvent::None
                }
            },
            _ => UiEvent::None,
        }
    }

    fn map_event_toc(&mut self, event: Event) -> UiEvent {
        match event {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => {
                match (code, modifiers) {
                    (KeyCode::Esc, _) => UiEvent::CloseOverlay,
                    (KeyCode::Char('t'), _) | (KeyCode::Char('T'), _) => UiEvent::CloseOverlay,
                    (KeyCode::Enter, _) => UiEvent::TocActivateSelection,
                    (KeyCode::Char('j'), KeyModifiers::NONE)
                    | (KeyCode::Down, KeyModifiers::NONE) => UiEvent::TocMoveSelection { delta: 1 },
                    (KeyCode::Char('k'), KeyModifiers::NONE)
                    | (KeyCode::Up, KeyModifiers::NONE) => UiEvent::TocMoveSelection { delta: -1 },
                    (KeyCode::Char('q'), _) => UiEvent::Quit,
                    _ => UiEvent::None,
                }
            }
            _ => UiEvent::None,
        }
    }

    fn map_event_search(&mut self, event: Event) -> UiEvent {
        match event {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match (code, modifiers) {
                (KeyCode::Esc, _) => {
                    self.set_mode(InputMode::Normal);
                    UiEvent::SearchCancel
                }
                (KeyCode::Enter, _) => {
                    let query = self.search_buffer.clone();
                    self.set_mode(InputMode::Normal);
                    UiEvent::SearchSubmit { query }
                }
                (KeyCode::Backspace, _) => {
                    self.search_buffer.pop();
                    UiEvent::SearchQueryChanged {
                        query: self.search_buffer.clone(),
                    }
                }
                (KeyCode::Char(c), mods) if mods.is_empty() || mods == KeyModifiers::SHIFT => {
                    self.search_buffer.push(c);
                    UiEvent::SearchQueryChanged {
                        query: self.search_buffer.clone(),
                    }
                }
                _ => UiEvent::None,
            },
            _ => UiEvent::None,
        }
    }

    fn push_digit(&mut self, digit: usize) {
        let current = self.pending_count.unwrap_or(0);
        let next = current.saturating_mul(10).saturating_add(digit);
        self.pending_count = Some(next);
        if let Some(c) = char::from_digit(digit as u32, 10) {
            self.pending_digits.push(c);
        }
    }

    fn take_count(&mut self) -> usize {
        let count = self
            .pending_count
            .take()
            .filter(|&count| count > 0)
            .unwrap_or(1);
        self.pending_digits.clear();
        count
    }

    fn reset_count(&mut self) {
        self.pending_count = None;
        self.pending_digits.clear();
    }

    fn push_char(&mut self, char: char) {
        self.char_stack.push(char);
    }
    fn reset_char_stack(&mut self) {
        self.char_stack = String::new();
    }

    fn start_search(&mut self) {
        self.set_mode(InputMode::Search);
    }

    fn pan(&mut self, delta_x: f32, delta_y: f32) -> UiEvent {
        let multiplier = self.take_count() as f32;
        self.reset_char_stack();
        UiEvent::Command(Command::AdjustViewport {
            delta_x: delta_x * multiplier,
            delta_y: delta_y * multiplier,
        })
    }

    pub fn pending_input(&self) -> Option<String> {
        if matches!(self.mode, InputMode::Search) {
            return Some(format!("/{}", self.search_buffer));
        }
        let mut pending = String::new();
        if !self.pending_digits.is_empty() {
            pending.push_str(&self.pending_digits);
        }
        if !self.char_stack.is_empty() {
            pending.push_str(&self.char_stack);
        }
        if pending.is_empty() {
            None
        } else {
            Some(pending)
        }
    }
}

#[deprecated(note = "Use EventMapper to retain numeric prefixes between key events")]
pub fn map_event(event: Event) -> UiEvent {
    EventMapper::new().map_event(event)
}

pub fn write_status_line<W: Write>(writer: &mut W, label: &str) -> io::Result<()> {
    write!(writer, "{}", label)?;
    writer.flush()
}
