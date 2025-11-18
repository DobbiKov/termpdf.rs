use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Error, Result};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{instrument, trace, warn};
use uuid::Uuid;

pub type DocumentId = Uuid;

static DOCUMENT_NAMESPACE: Lazy<Uuid> = Lazy::new(|| {
    Uuid::parse_str("7b2c58f1-99c6-5a5c-a6ea-50f9e7f1cc20").expect("valid namespace UUID")
});

pub fn document_id_for_path(path: &Path) -> DocumentId {
    let resolved = path
        .canonicalize()
        .or_else(|_| {
            if path.is_absolute() {
                Ok(path.to_path_buf())
            } else {
                std::env::current_dir().map(|cwd| cwd.join(path))
            }
        })
        .unwrap_or_else(|_| path.to_path_buf());
    let rendered = resolved.to_string_lossy();
    Uuid::new_v5(&*DOCUMENT_NAMESPACE, rendered.as_bytes())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct OutlineItem {
    pub title: String,
    pub page_index: usize,
    pub depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ViewportOffset {
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
}

impl Default for ViewportOffset {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0 }
    }
}

impl ViewportOffset {
    pub fn reset(&mut self) {
        self.x = 0.0;
        self.y = 0.0;
    }

    pub fn adjust(&mut self, delta_x: f32, delta_y: f32) -> bool {
        let mut changed = false;
        if delta_x.abs() > f32::EPSILON {
            let next = (self.x + delta_x).clamp(0.0, 1.0);
            if (next - self.x).abs() > f32::EPSILON {
                self.x = next;
                changed = true;
            }
        }
        if delta_y.abs() > f32::EPSILON {
            let next = (self.y + delta_y).clamp(0.0, 1.0);
            if (next - self.y).abs() > f32::EPSILON {
                self.y = next;
                changed = true;
            }
        }
        changed
    }

    pub fn clamp(&mut self) {
        self.x = self.x.clamp(0.0, 1.0);
        self.y = self.y.clamp(0.0, 1.0);
    }
}

#[derive(Debug, Clone)]
pub struct DocumentInfo {
    pub id: DocumentId,
    pub path: PathBuf,
    pub page_count: usize,
    pub metadata: DocumentMetadata,
}

#[derive(Debug, Clone, Copy)]
pub struct RenderRequest {
    pub page_index: usize,
    pub scale: f32,
    pub dark_mode: bool,
}

impl Default for RenderRequest {
    fn default() -> Self {
        Self {
            page_index: 0,
            scale: 1.0,
            dark_mode: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RenderImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TextGlyph {
    pub range: Range<usize>,
    pub rect: NormalizedRect,
}

#[derive(Debug, Clone)]
pub struct PageText {
    pub text: String,
    pub glyphs: Vec<TextGlyph>,
    boundary_offsets: Vec<usize>,
    line_map: Vec<PageLine>,
    glyph_line_index: Vec<usize>,
}

impl PageText {
    pub fn new(text: String, glyphs: Vec<TextGlyph>) -> Self {
        let mut boundary_offsets = Vec::with_capacity(glyphs.len().saturating_add(1));
        boundary_offsets.push(0);
        for glyph in &glyphs {
            boundary_offsets.push(glyph.range.end.min(text.len()));
        }
        let (line_map, glyph_line_index) = build_line_map(&glyphs);
        Self {
            text,
            glyphs,
            boundary_offsets,
            line_map,
            glyph_line_index,
        }
    }

    pub fn glyph_count(&self) -> usize {
        self.glyphs.len()
    }

    pub fn boundary_offset(&self, boundary: usize) -> usize {
        if boundary >= self.boundary_offsets.len() {
            return self.text.len();
        }
        self.boundary_offsets[boundary]
    }

    pub fn line_index_for_glyph(&self, index: usize) -> Option<usize> {
        self.glyph_line_index.get(index).copied()
    }

    pub fn line(&self, index: usize) -> Option<&PageLine> {
        self.line_map.get(index)
    }

    pub fn glyph_char(&self, index: usize) -> Option<char> {
        let glyph = self.glyphs.get(index)?;
        self.text[glyph.range.clone()].chars().next()
    }
}

#[derive(Debug, Clone)]
pub struct PageLine {
    pub glyph_range: Range<usize>,
    pub center_y: f32,
}

fn build_line_map(glyphs: &[TextGlyph]) -> (Vec<PageLine>, Vec<usize>) {
    if glyphs.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut lines = Vec::new();
    let mut glyph_line_index = Vec::with_capacity(glyphs.len());
    let mut last_center: Option<f32> = None;
    let threshold = 0.015;

    for (idx, glyph) in glyphs.iter().enumerate() {
        let center = (glyph.rect.top + glyph.rect.bottom) * 0.5;
        let new_line = match last_center {
            Some(prev) => (prev - center).abs() > threshold,
            None => true,
        };
        if new_line {
            lines.push(PageLine {
                glyph_range: idx..idx,
                center_y: center,
            });
        }
        if let Some(line) = lines.last_mut() {
            line.glyph_range.end = idx + 1;
            line.center_y = (line.center_y * 0.8) + (center * 0.2);
            glyph_line_index.push(lines.len() - 1);
        }
        last_center = Some(center);
    }

    (lines, glyph_line_index)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedDocumentState {
    pub current_page: usize,
    pub scale: f32,
    pub dark_mode: bool,
    pub marks: HashMap<char, usize>,
    #[serde(default)]
    pub named_marks: HashMap<String, usize>,
    #[serde(default)]
    pub viewport: ViewportOffset,
}

impl Default for PersistedDocumentState {
    fn default() -> Self {
        Self {
            current_page: 0,
            scale: 1.0,
            dark_mode: false,
            marks: HashMap::new(),
            named_marks: HashMap::new(),
            viewport: ViewportOffset::default(),
        }
    }
}

const JUMP_HISTORY_CAPACITY: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq)]
struct DocumentPosition {
    page: usize,
    scale: f32,
    viewport: ViewportOffset,
}

#[derive(Debug, Default)]
struct JumpHistory {
    back_stack: Vec<DocumentPosition>,
    forward_stack: Vec<DocumentPosition>,
    last_known: Option<DocumentPosition>,
}

impl JumpHistory {
    fn record_initial(&mut self, position: DocumentPosition) {
        self.last_known = Some(position);
    }

    fn record_navigation(&mut self, from: DocumentPosition, to: DocumentPosition) {
        if from == to {
            return;
        }
        self.push_back(from);
        self.forward_stack.clear();
        self.last_known = Some(to);
    }

    fn record_current(&mut self, position: DocumentPosition) {
        self.last_known = Some(position);
    }

    fn jump_backward(&mut self, current: DocumentPosition) -> Option<DocumentPosition> {
        while let Some(target) = self.back_stack.pop() {
            if target == current {
                continue;
            }
            self.push_forward(current);
            self.last_known = Some(target);
            return Some(target);
        }
        None
    }

    fn jump_forward(&mut self, current: DocumentPosition) -> Option<DocumentPosition> {
        while let Some(target) = self.forward_stack.pop() {
            if target == current {
                continue;
            }
            self.push_back(current);
            self.last_known = Some(target);
            return Some(target);
        }
        None
    }

    fn push_back(&mut self, position: DocumentPosition) {
        if self.back_stack.last().copied() == Some(position) {
            return;
        }
        self.back_stack.push(position);
        if self.back_stack.len() > JUMP_HISTORY_CAPACITY {
            let overflow = self.back_stack.len() - JUMP_HISTORY_CAPACITY;
            self.back_stack.drain(0..overflow);
        }
    }

    fn push_forward(&mut self, position: DocumentPosition) {
        if self.forward_stack.last().copied() == Some(position) {
            return;
        }
        self.forward_stack.push(position);
        if self.forward_stack.len() > JUMP_HISTORY_CAPACITY {
            let overflow = self.forward_stack.len() - JUMP_HISTORY_CAPACITY;
            self.forward_stack.drain(0..overflow);
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchMatch {
    page: usize,
    rects: Vec<NormalizedRect>,
}

#[derive(Debug, Clone)]
struct SearchState {
    query: String,
    matches: Vec<SearchMatch>,
    current_index: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct SearchSummary {
    pub query: String,
    pub total: usize,
    pub current_index: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct LinkDefinition {
    pub rects: Vec<NormalizedRect>,
    pub action: LinkAction,
}

#[derive(Debug, Clone)]
pub enum LinkAction {
    GoTo { page: usize },
    Uri { uri: String },
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct LinkSummary {
    pub total: usize,
    pub current_index: Option<usize>,
}

#[derive(Debug, Clone)]
pub enum ExternalLink {
    Url(String),
    File(PathBuf),
}

#[derive(Debug, Clone)]
pub enum LinkFollowResult {
    Navigated { page_changed: bool },
    External { target: ExternalLink },
    Unsupported,
    NoActiveLink,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedRect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl NormalizedRect {
    pub fn clamp(mut self) -> Self {
        self.left = self.left.clamp(0.0, 1.0);
        self.top = self.top.clamp(0.0, 1.0);
        self.right = self.right.clamp(0.0, 1.0);
        self.bottom = self.bottom.clamp(0.0, 1.0);
        self
    }

    pub fn is_valid(&self) -> bool {
        self.right > self.left && self.bottom > self.top
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.top && y <= self.bottom
    }

    pub fn center(&self) -> (f32, f32) {
        let cx = (self.left + self.right) * 0.5;
        let cy = (self.top + self.bottom) * 0.5;
        (cx, cy)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Highlights {
    pub current: Vec<NormalizedRect>,
    pub others: Vec<NormalizedRect>,
}

impl Highlights {
    pub fn is_empty(&self) -> bool {
        self.current.is_empty() && self.others.is_empty()
    }
}

pub type SearchHighlights = Highlights;
pub type LinkHighlights = Highlights;

#[derive(Copy, Clone)]
enum SearchDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy)]
pub enum SelectionMotion {
    Left,
    Right,
    Up,
    Down,
    WordForward,
    WordBackward,
    LineStart,
    LineEnd,
    DocumentStart,
    DocumentEnd,
    PageForward,
    PageBackward,
}

#[derive(Debug, Clone)]
struct LinkEntry {
    page: usize,
    rects: Vec<NormalizedRect>,
    action: LinkAction,
}

#[derive(Debug, Clone)]
struct LinkState {
    links: Vec<LinkEntry>,
    current_index: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionPoint {
    page: usize,
    glyph_index: usize,
}

#[derive(Debug, Clone)]
struct SelectionState {
    anchor: SelectionPoint,
    head: SelectionPoint,
}

#[derive(Debug, Clone)]
struct SelectionSnapshot {
    start: SelectionPoint,
    end: SelectionPoint,
}

impl SelectionState {
    fn normalized(&self) -> SelectionSnapshot {
        if compare_points(self.anchor, self.head) == Ordering::Greater {
            SelectionSnapshot {
                start: self.head,
                end: self.anchor,
            }
        } else {
            SelectionSnapshot {
                start: self.anchor,
                end: self.head,
            }
        }
    }
}

impl SelectionSnapshot {
    fn points(&self) -> (SelectionPoint, SelectionPoint) {
        (self.start, self.end)
    }
}

fn compare_points(a: SelectionPoint, b: SelectionPoint) -> Ordering {
    match a.page.cmp(&b.page) {
        Ordering::Equal => a.glyph_index.cmp(&b.glyph_index),
        other => other,
    }
}

pub struct DocumentInstance {
    pub info: DocumentInfo,
    pub backend: Arc<dyn DocumentBackend>,
    pub state: PersistedDocumentState,
    render_cache: Mutex<HashMap<CacheKey, RenderImage>>,
    outline: Vec<OutlineItem>,
    jump_history: JumpHistory,
    text_cache: Arc<Mutex<HashMap<usize, Arc<PageText>>>>,
    search_state: Option<SearchState>,
    link_state: Option<LinkState>,
    selection_state: Option<SelectionState>,
    visual_cursor: Option<SelectionPoint>,
    last_selection: Option<SelectionSnapshot>,
    visual_column_hint: f32,
}

#[derive(Clone)]
pub struct DocumentSearchContext {
    info: DocumentInfo,
    backend: Arc<dyn DocumentBackend>,
    text_cache: Arc<Mutex<HashMap<usize, Arc<PageText>>>>,
}

impl DocumentSearchContext {
    fn load_page_text(&self, page_index: usize) -> Result<Arc<PageText>> {
        load_cached_page_text(page_index, &self.info, &self.backend, &self.text_cache)
    }

    pub fn build_search_matches(&self, query: &str) -> Result<Vec<SearchMatch>> {
        let mut matches = Vec::new();

        if query.is_empty() {
            return Ok(matches);
        }

        let query_lower = query.to_lowercase();
        let step = query_lower.len().max(1);

        for page in 0..self.info.page_count {
            let mut page_matches = match self.backend.search_page(page, query) {
                Ok(rect_sets) => rect_sets,
                Err(err) => {
                    warn!(
                        ?err,
                        page,
                        path = %self.info.path.display(),
                        "backend search failed; falling back to text search"
                    );
                    Vec::new()
                }
            };

            if !page_matches.is_empty() {
                for rects in page_matches.drain(..) {
                    let rects: Vec<NormalizedRect> = rects
                        .into_iter()
                        .map(|rect| rect.clamp())
                        .filter(|rect| rect.is_valid())
                        .collect();
                    matches.push(SearchMatch { page, rects });
                }
                continue;
            }

            match self.load_page_text(page) {
                Ok(page_text) => {
                    if page_text.text.is_empty() {
                        continue;
                    }

                    let lower = page_text.text.to_lowercase();
                    let mut offset = 0usize;
                    while offset < lower.len() {
                        if let Some(pos) = lower[offset..].find(&query_lower) {
                            let absolute = offset + pos;
                            matches.push(SearchMatch {
                                page,
                                rects: Vec::new(),
                            });
                            let next = absolute.saturating_add(step);
                            if next <= offset {
                                break;
                            }
                            offset = next;
                        } else {
                            break;
                        }
                    }
                }
                Err(err) => {
                    warn!(
                        ?err,
                        page,
                        path = %self.info.path.display(),
                        "failed to extract text for search"
                    );
                }
            }
        }

        Ok(matches)
    }
}

fn load_cached_page_text(
    page_index: usize,
    info: &DocumentInfo,
    backend: &Arc<dyn DocumentBackend>,
    cache: &Arc<Mutex<HashMap<usize, Arc<PageText>>>>,
) -> Result<Arc<PageText>> {
    if page_index >= info.page_count {
        return Err(anyhow!("page {} out of range", page_index));
    }

    if let Some(text) = cache.lock().get(&page_index).cloned() {
        return Ok(text);
    }

    let text = Arc::new(backend.page_text(page_index)?);
    cache.lock().insert(page_index, Arc::clone(&text));
    Ok(text)
}

fn glyph_near_point(text: &PageText, x: f32, y: f32) -> usize {
    if text.glyphs.is_empty() {
        return 0;
    }
    let mut best_index = 0usize;
    let mut best_score = f32::MAX;
    for (idx, glyph) in text.glyphs.iter().enumerate() {
        let rect = &glyph.rect;
        if rect.is_valid() && rect.contains(x, y) {
            return idx;
        }
        if rect.is_valid() {
            let (cx, cy) = rect.center();
            let dx = cx - x;
            let dy = cy - y;
            let score = dx * dx + dy * dy;
            if score < best_score {
                best_score = score;
                best_index = idx;
            }
        }
    }
    best_index
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

impl DocumentInstance {
    fn page_text_entry(&self, page_index: usize) -> Result<Arc<PageText>> {
        load_cached_page_text(page_index, &self.info, &self.backend, &self.text_cache)
    }

    fn ensure_visual_cursor(&mut self) -> Result<bool> {
        if let Some(point) = self.visual_cursor {
            let clamped = self.clamp_point(point)?;
            if clamped != point {
                self.visual_cursor = Some(clamped);
                self.update_column_hint(clamped);
                return Ok(true);
            }
            return Ok(false);
        }
        if self.info.page_count == 0 {
            return Ok(false);
        }
        let point = self.initial_cursor_point()?;
        self.visual_cursor = Some(point);
        self.update_column_hint(point);
        Ok(true)
    }

    fn start_selection(&mut self) -> Result<bool> {
        if self.selection_state.is_some() {
            return Ok(false);
        }
        self.ensure_visual_cursor()?;
        let Some(point) = self.visual_cursor else {
            return Ok(false);
        };
        self.selection_state = Some(SelectionState {
            anchor: point,
            head: point,
        });
        Ok(true)
    }

    fn initial_cursor_point(&self) -> Result<SelectionPoint> {
        if let Some(point) = self.visual_cursor {
            return self.clamp_point(point);
        }
        let page = self
            .state
            .current_page
            .min(self.info.page_count.saturating_sub(1));
        let page_text = self.page_text_entry(page)?;
        let glyph_index = if page_text.glyphs.is_empty() {
            0
        } else {
            glyph_near_point(&page_text, self.state.viewport.x, self.state.viewport.y)
        };
        Ok(SelectionPoint { page, glyph_index })
    }

    fn update_column_hint(&mut self, point: SelectionPoint) {
        if let Some((x, _)) = self.cursor_center(point) {
            self.visual_column_hint = x;
        }
    }

    fn cursor_center(&self, point: SelectionPoint) -> Option<(f32, f32)> {
        let page_text = self.page_text_entry(point.page).ok()?;
        let glyph_count = page_text.glyph_count();
        if glyph_count == 0 {
            return None;
        }
        let idx = point.glyph_index.min(glyph_count.saturating_sub(1));
        let glyph = page_text.glyphs.get(idx)?;
        if glyph.rect.is_valid() {
            Some(glyph.rect.center())
        } else {
            None
        }
    }

    fn clamp_point(&self, mut point: SelectionPoint) -> Result<SelectionPoint> {
        if self.info.page_count == 0 {
            point.page = 0;
            point.glyph_index = 0;
            return Ok(point);
        }
        if point.page >= self.info.page_count {
            point.page = self.info.page_count - 1;
        }
        let page_text = self.page_text_entry(point.page)?;
        let limit = page_text.glyph_count();
        point.glyph_index = point.glyph_index.min(limit);
        Ok(point)
    }

    fn increment_point(&self, point: &mut SelectionPoint) -> Result<bool> {
        let page_text = self.page_text_entry(point.page)?;
        if point.glyph_index < page_text.glyph_count() {
            point.glyph_index += 1;
            return Ok(true);
        }
        if point.page + 1 < self.info.page_count {
            point.page += 1;
            point.glyph_index = 0;
            return Ok(true);
        }
        Ok(false)
    }

    fn decrement_point(&self, point: &mut SelectionPoint) -> Result<bool> {
        if point.glyph_index > 0 {
            point.glyph_index -= 1;
            return Ok(true);
        }
        if point.page == 0 {
            return Ok(false);
        }
        point.page -= 1;
        let prev_text = self.page_text_entry(point.page)?;
        point.glyph_index = prev_text.glyph_count();
        Ok(true)
    }

    fn adjust_point(&self, point: &mut SelectionPoint, delta: isize) -> Result<bool> {
        if delta == 0 {
            return Ok(false);
        }
        let mut moved = false;
        if delta > 0 {
            for _ in 0..delta {
                if !self.increment_point(point)? {
                    break;
                }
                moved = true;
            }
        } else {
            for _ in 0..(-delta) {
                if !self.decrement_point(point)? {
                    break;
                }
                moved = true;
            }
        }
        Ok(moved)
    }

    fn move_pages(&self, point: &mut SelectionPoint, delta: isize) -> Result<bool> {
        if delta == 0 || self.info.page_count == 0 {
            return Ok(false);
        }
        let mut target = point.page as isize + delta;
        let max_page = (self.info.page_count.saturating_sub(1)) as isize;
        target = target.clamp(0, max_page);
        if target as usize == point.page {
            return Ok(false);
        }
        point.page = target as usize;
        let page_text = self.page_text_entry(point.page)?;
        point.glyph_index = point.glyph_index.min(page_text.glyph_count());
        Ok(true)
    }

    fn move_lines(&self, point: &mut SelectionPoint, delta: isize) -> Result<bool> {
        if delta == 0 {
            return Ok(false);
        }
        let mut remaining = delta;
        let mut moved = false;
        while remaining != 0 {
            let page_text = self.page_text_entry(point.page)?;
            if page_text.line_map.is_empty() {
                if remaining > 0 {
                    if point.page + 1 >= self.info.page_count {
                        break;
                    }
                    point.page += 1;
                    point.glyph_index = 0;
                    remaining -= 1;
                    moved = true;
                } else {
                    if point.page == 0 {
                        break;
                    }
                    point.page -= 1;
                    point.glyph_index = 0;
                    remaining += 1;
                    moved = true;
                }
                continue;
            }
            let glyph_count = page_text.glyph_count();
            let idx = if glyph_count == 0 {
                0
            } else {
                point.glyph_index.min(glyph_count.saturating_sub(1))
            };
            let mut line_idx = page_text
                .line_index_for_glyph(idx)
                .unwrap_or_else(|| page_text.line_map.len().saturating_sub(1));
            if remaining > 0 {
                if line_idx + 1 < page_text.line_map.len() {
                    line_idx += 1;
                    point.glyph_index = page_text.line_map[line_idx].glyph_range.start;
                    remaining -= 1;
                    moved = true;
                } else {
                    if point.page + 1 >= self.info.page_count {
                        break;
                    }
                    point.page += 1;
                    point.glyph_index = 0;
                    remaining -= 1;
                    moved = true;
                }
            } else {
                if line_idx > 0 {
                    line_idx -= 1;
                    point.glyph_index = page_text.line_map[line_idx].glyph_range.start;
                    remaining += 1;
                    moved = true;
                } else {
                    if point.page == 0 {
                        break;
                    }
                    point.page -= 1;
                    if let Some(prev_line) = self.page_text_entry(point.page)?.line_map.last() {
                        point.glyph_index = prev_line.glyph_range.start;
                    } else {
                        point.glyph_index = 0;
                    }
                    remaining += 1;
                    moved = true;
                }
            }
        }
        Ok(moved)
    }

    fn move_word(&self, point: &mut SelectionPoint, count: usize, forward: bool) -> Result<bool> {
        let mut moved = false;
        let steps = count.max(1);
        for _ in 0..steps {
            moved |= self.skip_while(point, |ch| !is_word_char(ch), forward)?;
            moved |= self.skip_while(point, |ch| is_word_char(ch), forward)?;
        }
        Ok(moved)
    }

    fn move_to_line_boundary(&self, point: &mut SelectionPoint, to_start: bool) -> Result<bool> {
        let page_text = self.page_text_entry(point.page)?;
        if page_text.line_map.is_empty() {
            return Ok(false);
        }
        let glyph_count = page_text.glyph_count();
        let idx = if glyph_count == 0 {
            0
        } else {
            point.glyph_index.min(glyph_count.saturating_sub(1))
        };
        let line_idx = page_text
            .line_index_for_glyph(idx)
            .unwrap_or_else(|| page_text.line_map.len().saturating_sub(1));
        let target_index = if to_start {
            page_text.line_map[line_idx].glyph_range.start
        } else {
            page_text.line_map[line_idx]
                .glyph_range
                .end
                .min(glyph_count)
        };
        if point.glyph_index == target_index {
            return Ok(false);
        }
        point.glyph_index = target_index;
        Ok(true)
    }

    fn skip_while<F>(&self, point: &mut SelectionPoint, predicate: F, forward: bool) -> Result<bool>
    where
        F: Fn(char) -> bool,
    {
        let mut changed = false;
        loop {
            let page_text = self.page_text_entry(point.page)?;
            let glyph_count = page_text.glyph_count();
            if forward {
                if point.glyph_index >= glyph_count {
                    if !self.increment_point(point)? {
                        break;
                    }
                    changed = true;
                    continue;
                }
                match page_text.glyph_char(point.glyph_index) {
                    Some(ch) if predicate(ch) => {
                        if !self.increment_point(point)? {
                            break;
                        }
                        changed = true;
                    }
                    _ => break,
                }
            } else {
                if point.glyph_index == 0 {
                    if !self.decrement_point(point)? {
                        break;
                    }
                    changed = true;
                    continue;
                }
                let idx = point.glyph_index.saturating_sub(1);
                match page_text.glyph_char(idx) {
                    Some(ch) if predicate(ch) => {
                        if !self.decrement_point(point)? {
                            break;
                        }
                        changed = true;
                    }
                    _ => break,
                }
            }
        }
        Ok(changed)
    }

    fn clear_selection_state(&mut self, remember: bool) -> bool {
        if self.selection_state.is_some() {
            if remember {
                self.remember_selection();
            }
            if let Some(state) = self.selection_state.take() {
                self.visual_cursor = Some(state.head);
                self.update_column_hint(state.head);
            }
            true
        } else {
            false
        }
    }

    fn remember_selection(&mut self) {
        if let Some(selection) = self.selection_state.as_ref() {
            self.last_selection = Some(selection.normalized());
        }
    }

    fn leave_visual_mode(&mut self, remember: bool) -> bool {
        let mut changed = self.clear_selection_state(remember);
        if self.visual_cursor.take().is_some() {
            changed = true;
        }
        changed
    }

    fn restore_last_selection(&mut self) -> Result<bool> {
        let snapshot = match self.last_selection.clone() {
            Some(snapshot) => snapshot,
            None => return Ok(false),
        };
        let start = self.clamp_point(snapshot.start)?;
        let end = self.clamp_point(snapshot.end)?;
        self.selection_state = Some(SelectionState {
            anchor: start,
            head: end,
        });
        self.visual_cursor = Some(end);
        self.update_column_hint(end);
        Ok(true)
    }

    fn swap_visual_cursor(&mut self) -> bool {
        if let Some(state) = self.selection_state.as_mut() {
            std::mem::swap(&mut state.anchor, &mut state.head);
            let head = state.head;
            self.visual_cursor = Some(head);
            self.update_column_hint(head);
            true
        } else {
            false
        }
    }

    fn build_selection_highlights(
        &self,
        snapshot: &SelectionSnapshot,
        page_index: usize,
    ) -> Option<Highlights> {
        let (start, end) = snapshot.points();
        if page_index < start.page || page_index > end.page {
            return None;
        }
        let page_text = self.page_text_entry(page_index).ok()?;
        let mut highlights = Highlights::default();
        let start_idx = if page_index == start.page {
            start.glyph_index.min(page_text.glyph_count())
        } else {
            0
        };
        let end_idx = if page_index == end.page {
            end.glyph_index.min(page_text.glyph_count())
        } else {
            page_text.glyph_count()
        };
        if start_idx >= end_idx {
            return None;
        }
        for glyph in page_text.glyphs[start_idx..end_idx].iter() {
            if glyph.rect.is_valid() {
                highlights.current.push(glyph.rect);
            }
        }
        if highlights.is_empty() {
            None
        } else {
            Some(highlights)
        }
    }

    fn extract_selection_text(&self, snapshot: &SelectionSnapshot) -> Result<String> {
        let (start, end) = snapshot.points();
        let mut buffer = String::new();
        let mut page = start.page;
        while page <= end.page {
            let page_text = self.page_text_entry(page)?;
            let glyph_count = page_text.glyph_count();
            let start_idx = if page == start.page {
                start.glyph_index.min(glyph_count)
            } else {
                0
            };
            let end_idx = if page == end.page {
                end.glyph_index.min(glyph_count)
            } else {
                glyph_count
            };
            if start_idx < end_idx {
                let start_offset = page_text.boundary_offset(start_idx);
                let end_offset = page_text.boundary_offset(end_idx);
                if end_offset > start_offset && end_offset <= page_text.text.len() {
                    if !buffer.is_empty() {
                        buffer.push('\n');
                    }
                    buffer.push_str(&page_text.text[start_offset..end_offset]);
                }
            }
            if page == end.page {
                break;
            }
            page += 1;
        }
        Ok(buffer)
    }

    fn move_visual_cursor(&mut self, motion: SelectionMotion, count: usize) -> Result<bool> {
        if self.info.page_count == 0 {
            return Ok(false);
        }
        self.ensure_visual_cursor()?;
        let mut cursor = match self.visual_cursor {
            Some(point) => point,
            None => return Ok(false),
        };
        let mut changed = false;
        let steps = count.max(1);
        match motion {
            SelectionMotion::Left => {
                changed = self.adjust_point(&mut cursor, -(steps as isize))?;
            }
            SelectionMotion::Right => {
                changed = self.adjust_point(&mut cursor, steps as isize)?;
            }
            SelectionMotion::WordForward => {
                changed = self.move_word(&mut cursor, steps, true)?;
            }
            SelectionMotion::WordBackward => {
                changed = self.move_word(&mut cursor, steps, false)?;
            }
            SelectionMotion::LineStart => {
                changed = self.move_to_line_boundary(&mut cursor, true)?;
            }
            SelectionMotion::LineEnd => {
                changed = self.move_to_line_boundary(&mut cursor, false)?;
            }
            SelectionMotion::DocumentStart => {
                let target_page = if steps > 1 {
                    steps
                        .saturating_sub(1)
                        .min(self.info.page_count.saturating_sub(1))
                } else {
                    0
                };
                if cursor.page != target_page || cursor.glyph_index != 0 {
                    cursor.page = target_page;
                    cursor.glyph_index = 0;
                    changed = true;
                }
            }
            SelectionMotion::DocumentEnd => {
                let target_page = self.info.page_count.saturating_sub(1);
                let page_changed = cursor.page != target_page;
                cursor.page = target_page;
                let page_text = self.page_text_entry(cursor.page)?;
                let target = page_text.glyph_count();
                if page_changed || cursor.glyph_index != target {
                    cursor.glyph_index = target;
                    changed = true;
                }
            }
            SelectionMotion::PageForward => {
                changed = self.move_pages(&mut cursor, steps as isize)?;
            }
            SelectionMotion::PageBackward => {
                changed = self.move_pages(&mut cursor, -(steps as isize))?;
            }
            SelectionMotion::Down => {
                changed = self.move_lines(&mut cursor, steps as isize)?;
            }
            SelectionMotion::Up => {
                changed = self.move_lines(&mut cursor, -(steps as isize))?;
            }
        }

        if changed {
            self.visual_cursor = Some(cursor);
            self.update_column_hint(cursor);
            if self.state.current_page != cursor.page {
                self.state.current_page = cursor.page;
                self.state.viewport.reset();
            }
            if let Some(state) = self.selection_state.as_mut() {
                state.head = cursor;
            }
        }
        Ok(changed)
    }

    pub fn new(
        info: DocumentInfo,
        backend: Arc<dyn DocumentBackend>,
        state: PersistedDocumentState,
        outline: Vec<OutlineItem>,
    ) -> Self {
        let mut instance = Self {
            info,
            backend,
            state,
            render_cache: Mutex::new(HashMap::new()),
            outline,
            jump_history: JumpHistory::default(),
            text_cache: Arc::new(Mutex::new(HashMap::new())),
            search_state: None,
            link_state: None,
            selection_state: None,
            visual_cursor: None,
            last_selection: None,
            visual_column_hint: 0.5,
        };
        let initial = instance.current_position();
        instance.jump_history.record_initial(initial);
        instance
    }

    pub fn current_page(&self) -> usize {
        self.state.current_page
    }

    pub fn search_context(&self) -> DocumentSearchContext {
        DocumentSearchContext {
            info: self.info.clone(),
            backend: Arc::clone(&self.backend),
            text_cache: Arc::clone(&self.text_cache),
        }
    }

    pub fn render(&self) -> Result<RenderImage> {
        self.render_with_scale(self.state.scale)
    }

    pub fn render_with_scale(&self, scale: f32) -> Result<RenderImage> {
        self.render_page_internal(
            self.state.current_page,
            scale,
            self.state.dark_mode,
            self.state.current_page,
        )
    }

    pub fn reload(
        &mut self,
        info: DocumentInfo,
        backend: Arc<dyn DocumentBackend>,
        outline: Vec<OutlineItem>,
    ) {
        let previous_query = self.search_state.as_ref().map(|state| state.query.clone());

        self.info = info;
        self.backend = backend;
        self.outline = outline;

        self.render_cache.lock().clear();
        self.text_cache.lock().clear();
        self.search_state = None;
        self.link_state = None;
        self.selection_state = None;
        self.visual_cursor = None;
        self.visual_column_hint = 0.5;
        self.last_selection = None;

        if self.info.page_count == 0 {
            self.state.current_page = 0;
        } else if self.state.current_page >= self.info.page_count {
            self.state.current_page = self.info.page_count - 1;
        }
        self.state
            .marks
            .retain(|_, page| *page < self.info.page_count);
        self.state
            .named_marks
            .retain(|_, page| *page < self.info.page_count);

        if self.state.scale <= 1.0 + f32::EPSILON {
            self.state.viewport.reset();
        } else {
            self.state.viewport.clamp();
        }

        if let Some(query) = previous_query {
            if let Err(err) = self.perform_search(query) {
                trace!(
                    ?err,
                    path = %self.info.path.display(),
                    "failed to rebuild search state after reload"
                );
            }
        }

        self.sync_jump_position();
    }
    pub fn add_mark(&mut self, mark: char, page: usize) {
        self.state.marks.insert(mark, page);
    }
    pub fn get_page_from_mark(&self, mark: char) -> Option<usize> {
        self.state.marks.get(&mark).map(|v| *v)
    }

    pub fn add_named_mark(&mut self, name: String, page: usize) {
        self.state.named_marks.insert(name, page);
    }

    pub fn named_mark_page(&self, name: &str) -> Option<usize> {
        self.state.named_marks.get(name).copied()
    }

    pub fn named_marks(&self) -> &HashMap<String, usize> {
        &self.state.named_marks
    }

    pub fn prefetch_neighbors(&self, range: usize, scale: f32) -> Result<()> {
        if range == 0 {
            return Ok(());
        }

        let current_page = self.state.current_page;
        let dark_mode = self.state.dark_mode;
        let mut last_error: Option<Error> = None;

        for offset in 1..=range {
            if let Some(prev) = current_page.checked_sub(offset) {
                if prev < self.info.page_count {
                    if let Err(err) =
                        self.render_page_internal(prev, scale, dark_mode, current_page)
                    {
                        last_error = Some(err);
                    }
                }
            }

            let next = current_page + offset;
            if next < self.info.page_count {
                if let Err(err) = self.render_page_internal(next, scale, dark_mode, current_page) {
                    last_error = Some(err);
                }
            }
        }

        if let Some(err) = last_error {
            Err(err)
        } else {
            Ok(())
        }
    }

    fn render_page_internal(
        &self,
        page_index: usize,
        scale: f32,
        dark_mode: bool,
        reference_page: usize,
    ) -> Result<RenderImage> {
        if page_index >= self.info.page_count {
            return Err(anyhow!("page {} out of range", page_index));
        }

        let key = CacheKey::new(page_index, scale, dark_mode);
        if let Some(image) = self.try_get_cached(&key) {
            return Ok(image);
        }

        let request = RenderRequest {
            page_index,
            scale,
            dark_mode,
        };
        let image = self.backend.render_page(request)?;
        self.store_cached_render(key, &image, reference_page);
        Ok(image)
    }

    fn current_position(&self) -> DocumentPosition {
        DocumentPosition {
            page: self.state.current_page,
            scale: self.state.scale,
            viewport: self.state.viewport,
        }
    }

    fn record_jump_from(&mut self, previous: DocumentPosition) {
        let current = self.current_position();
        self.jump_history.record_navigation(previous, current);
    }

    fn sync_jump_position(&mut self) {
        let current = self.current_position();
        self.jump_history.record_current(current);
    }

    fn apply_document_position(&mut self, position: DocumentPosition) -> bool {
        let mut changed = false;
        let last_page = self.info.page_count.saturating_sub(1);
        let target_page = position.page.min(last_page);
        if target_page != self.state.current_page {
            self.state.current_page = target_page;
            changed = true;
        }

        let target_scale = position.scale.clamp(0.25, 4.0);
        if (self.state.scale - target_scale).abs() > f32::EPSILON {
            self.state.scale = target_scale;
            changed = true;
        }

        let mut next_viewport = position.viewport;
        next_viewport.clamp();
        if (self.state.viewport.x - next_viewport.x).abs() > f32::EPSILON
            || (self.state.viewport.y - next_viewport.y).abs() > f32::EPSILON
        {
            self.state.viewport = next_viewport;
            changed = true;
        } else if changed {
            self.state.viewport = next_viewport;
        }

        if self.state.scale <= 1.0 + f32::EPSILON {
            self.state.viewport.reset();
        } else {
            self.state.viewport.clamp();
        }

        self.sync_jump_position();
        changed
    }

    fn pop_jump_backward(&mut self) -> Option<DocumentPosition> {
        let current = self.current_position();
        self.jump_history.jump_backward(current)
    }

    fn pop_jump_forward(&mut self) -> Option<DocumentPosition> {
        let current = self.current_position();
        self.jump_history.jump_forward(current)
    }

    pub fn perform_search(&mut self, query: String) -> Result<bool> {
        let trimmed = query.trim().to_string();

        if trimmed.is_empty() {
            self.search_state = None;
            self.sync_jump_position();
            return Ok(false);
        }

        let context = self.search_context();
        let matches = context.build_search_matches(&trimmed)?;
        Ok(self.apply_search_results(trimmed, matches, self.state.current_page))
    }

    pub fn apply_search_results(
        &mut self,
        query: String,
        matches: Vec<SearchMatch>,
        start_page: usize,
    ) -> bool {
        if query.is_empty() {
            self.search_state = None;
            self.sync_jump_position();
            return false;
        }

        let start_page = start_page.min(self.info.page_count.saturating_sub(1));
        let next_index = if matches.is_empty() {
            None
        } else {
            Some(
                matches
                    .iter()
                    .position(|m| m.page >= start_page)
                    .unwrap_or(0),
            )
        };

        self.search_state = Some(SearchState {
            query,
            matches,
            current_index: next_index,
        });

        if let Some(idx) = next_index {
            self.apply_search_index(idx)
        } else {
            self.sync_jump_position();
            false
        }
    }

    pub fn next_search_match(&mut self, count: usize) -> Option<bool> {
        self.advance_search(SearchDirection::Forward, count)
    }

    pub fn previous_search_match(&mut self, count: usize) -> Option<bool> {
        self.advance_search(SearchDirection::Backward, count)
    }

    fn advance_search(&mut self, direction: SearchDirection, count: usize) -> Option<bool> {
        if count == 0 {
            return Some(false);
        }

        let (total, current) = match self.search_state.as_ref() {
            Some(state) if !state.matches.is_empty() => {
                (state.matches.len(), state.current_index.unwrap_or(0))
            }
            Some(_) => return Some(false),
            None => return None,
        };

        if total == 0 {
            return Some(false);
        }

        let steps = count % total;
        if steps == 0 {
            return Some(self.apply_search_index(current));
        }

        let target = match direction {
            SearchDirection::Forward => (current + steps) % total,
            SearchDirection::Backward => (current + total - steps) % total,
        };

        Some(self.apply_search_index(target))
    }

    fn apply_search_index(&mut self, index: usize) -> bool {
        let Some(state) = self.search_state.as_mut() else {
            return false;
        };

        if state.matches.is_empty() || index >= state.matches.len() {
            state.current_index = None;
            return false;
        }

        state.current_index = Some(index);
        let target_page = state.matches[index]
            .page
            .min(self.info.page_count.saturating_sub(1));
        let previous = self.current_position();
        let changed = if target_page != self.state.current_page {
            self.state.current_page = target_page;
            self.state.viewport.reset();
            self.record_jump_from(previous);
            true
        } else {
            false
        };
        self.sync_jump_position();
        changed
    }

    pub fn search_summary(&self) -> Option<SearchSummary> {
        self.search_state.as_ref().map(|state| SearchSummary {
            query: state.query.clone(),
            total: state.matches.len(),
            current_index: state.current_index,
        })
    }

    pub fn search_highlights_for_current_page(&self) -> Option<SearchHighlights> {
        let state = self.search_state.as_ref()?;
        let current_page = self.state.current_page;
        let mut highlights = SearchHighlights::default();
        for (idx, match_entry) in state.matches.iter().enumerate() {
            if match_entry.page != current_page {
                continue;
            }
            if Some(idx) == state.current_index {
                highlights.current.extend(match_entry.rects.iter().copied());
            } else {
                highlights.others.extend(match_entry.rects.iter().copied());
            }
        }
        if highlights.is_empty() {
            None
        } else {
            Some(highlights)
        }
    }

    pub fn selection_highlights_for_current_page(&self) -> Option<Highlights> {
        let selection = self.selection_state.as_ref()?;
        let snapshot = selection.normalized();
        self.build_selection_highlights(&snapshot, self.state.current_page)
    }

    pub fn selection_text(&self) -> Option<String> {
        let selection = self.selection_state.as_ref()?;
        self.extract_selection_text(&selection.normalized()).ok()
    }

    pub fn visual_cursor_highlight(&self) -> Option<NormalizedRect> {
        if self.selection_state.is_some() {
            return None;
        }
        let point = self.visual_cursor?;
        if point.page != self.state.current_page {
            return None;
        }
        let page_text = self.page_text_entry(point.page).ok()?;
        if page_text.glyphs.is_empty() {
            return None;
        }
        let glyph_count = page_text.glyph_count();
        if glyph_count == 0 {
            return None;
        }
        let idx = point.glyph_index.min(glyph_count.saturating_sub(1));
        page_text
            .glyphs
            .get(idx)
            .map(|glyph| glyph.rect)
            .filter(|rect| rect.is_valid())
    }

    pub fn start_link_mode(&mut self) -> Result<()> {
        let entries = self.build_link_entries()?;
        let current_page = self.state.current_page;
        let current_index = entries.iter().position(|link| link.page == current_page);
        self.link_state = Some(LinkState {
            links: entries,
            current_index,
        });
        Ok(())
    }

    pub fn clear_link_state(&mut self) {
        self.link_state = None;
    }

    pub fn next_link(&mut self, count: usize) -> Option<bool> {
        self.advance_link(SearchDirection::Forward, count)
    }

    pub fn previous_link(&mut self, count: usize) -> Option<bool> {
        self.advance_link(SearchDirection::Backward, count)
    }

    fn advance_link(&mut self, direction: SearchDirection, count: usize) -> Option<bool> {
        if count == 0 {
            return Some(false);
        }

        let Some(state) = self.link_state.as_mut() else {
            return None;
        };

        if state.links.is_empty() {
            return Some(false);
        }

        let mut initialized = false;
        if state.current_index.is_none() {
            let desired = state
                .links
                .iter()
                .position(|link| link.page == self.state.current_page)
                .or_else(|| {
                    state
                        .links
                        .iter()
                        .position(|link| link.page > self.state.current_page)
                })
                .or(Some(0));
            state.current_index = desired;
            initialized = state.current_index.is_some();
        }

        let Some(current_index) = state.current_index else {
            return Some(false);
        };

        if initialized {
            return Some(self.apply_link_index(current_index));
        }

        let total = state.links.len();
        let current = current_index.min(total.saturating_sub(1));
        let steps = count % total;
        if steps == 0 {
            return Some(self.apply_link_index(current));
        }

        let target = match direction {
            SearchDirection::Forward => (current + steps) % total,
            SearchDirection::Backward => (current + total - steps) % total,
        };

        Some(self.apply_link_index(target))
    }

    fn apply_link_index(&mut self, index: usize) -> bool {
        let Some(state) = self.link_state.as_mut() else {
            return false;
        };

        if state.links.is_empty() || index >= state.links.len() {
            state.current_index = None;
            return false;
        }

        state.current_index = Some(index);
        let link = &state.links[index];
        let target_page = link.page.min(self.info.page_count.saturating_sub(1));
        let previous = self.current_position();
        let changed = if target_page != self.state.current_page {
            self.state.current_page = target_page;
            self.state.viewport.reset();
            self.record_jump_from(previous);
            true
        } else {
            false
        };
        self.sync_jump_position();
        changed
    }

    pub fn link_summary(&self) -> Option<LinkSummary> {
        self.link_state.as_ref().map(|state| LinkSummary {
            total: state.links.len(),
            current_index: state.current_index,
        })
    }

    pub fn link_highlights_for_current_page(&self) -> Option<LinkHighlights> {
        let state = self.link_state.as_ref()?;
        let current_page = self.state.current_page;
        let mut highlights = LinkHighlights::default();

        for (idx, link) in state.links.iter().enumerate() {
            if link.page != current_page {
                continue;
            }
            if Some(idx) == state.current_index {
                highlights.current.extend(link.rects.iter().copied());
            } else {
                highlights.others.extend(link.rects.iter().copied());
            }
        }

        if highlights.is_empty() {
            None
        } else {
            Some(highlights)
        }
    }

    pub fn activate_link(&mut self) -> LinkFollowResult {
        let Some(state) = self.link_state.as_ref() else {
            return LinkFollowResult::NoActiveLink;
        };
        let Some(index) = state.current_index else {
            return LinkFollowResult::NoActiveLink;
        };
        let Some(link) = state.links.get(index) else {
            return LinkFollowResult::NoActiveLink;
        };

        match &link.action {
            LinkAction::GoTo { page } => {
                let target_page = (*page).min(self.info.page_count.saturating_sub(1));
                let previous = self.current_position();
                let page_changed = if target_page != self.state.current_page {
                    self.state.current_page = target_page;
                    self.state.viewport.reset();
                    self.record_jump_from(previous);
                    true
                } else {
                    false
                };
                self.sync_jump_position();
                LinkFollowResult::Navigated { page_changed }
            }
            LinkAction::Uri { uri } => LinkFollowResult::External {
                target: ExternalLink::Url(uri.clone()),
            },
            LinkAction::Unsupported => LinkFollowResult::Unsupported,
        }
    }

    fn build_link_entries(&self) -> Result<Vec<LinkEntry>> {
        let mut entries = Vec::new();
        for page in 0..self.info.page_count {
            let definitions = self.backend.page_links(page)?;
            if definitions.is_empty() {
                continue;
            }
            for definition in definitions {
                let rects: Vec<NormalizedRect> = definition
                    .rects
                    .into_iter()
                    .map(|rect| rect.clamp())
                    .filter(|rect| rect.is_valid())
                    .collect();
                if rects.is_empty() {
                    continue;
                }
                entries.push(LinkEntry {
                    page,
                    rects,
                    action: definition.action,
                });
            }
        }
        Ok(entries)
    }

    fn try_get_cached(&self, key: &CacheKey) -> Option<RenderImage> {
        self.render_cache.lock().get(key).cloned()
    }

    fn store_cached_render(&self, key: CacheKey, image: &RenderImage, reference_page: usize) {
        let mut cache = self.render_cache.lock();
        cache.insert(key, image.clone());

        if cache.len() > CACHE_CAPACITY {
            let mut keys: Vec<_> = cache.keys().cloned().collect();
            keys.sort_by_key(|k| k.distance(reference_page));
            for stale in keys.into_iter().skip(CACHE_CAPACITY) {
                cache.remove(&stale);
            }
        }
    }

    pub fn outline(&self) -> &[OutlineItem] {
        &self.outline
    }
}

const CACHE_CAPACITY: usize = 10;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    page_index: usize,
    scale_milli: u32,
    dark_mode: bool,
}

impl CacheKey {
    fn new(page_index: usize, scale: f32, dark_mode: bool) -> Self {
        Self {
            page_index,
            scale_milli: quantize_scale(scale),
            dark_mode,
        }
    }

    fn distance(&self, reference_page: usize) -> usize {
        self.page_index.abs_diff(reference_page)
    }
}

fn quantize_scale(scale: f32) -> u32 {
    let scaled = (scale * 1000.0).round();
    if !scaled.is_finite() || scaled <= 0.0 {
        1
    } else if scaled > u32::MAX as f32 {
        u32::MAX
    } else {
        scaled as u32
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    NextPage {
        count: usize,
    },
    PrevPage {
        count: usize,
    },
    GotoPage {
        page: usize,
    },
    ScaleBy {
        factor: f32,
    },
    ResetScale,
    AdjustViewport {
        delta_x: f32,
        delta_y: f32,
    },
    PutMark {
        key: char,
    },
    GotoMark {
        key: char,
    },
    SaveNamedMark {
        name: String,
    },
    GotoNamedMark {
        name: String,
    },
    Search {
        query: String,
    },
    SearchNext {
        count: usize,
    },
    SearchPrev {
        count: usize,
    },
    EnterVisualMode,
    StartSelection,
    MoveVisualCursor {
        motion: SelectionMotion,
        count: usize,
    },
    ClearSelection,
    LeaveVisualMode,
    RestoreSelection,
    SwapVisualCursor,
    EnterLinkMode,
    LeaveLinkMode,
    LinkNext {
        count: usize,
    },
    LinkPrev {
        count: usize,
    },
    ActivateLink,
    ToggleDarkMode,
    SwitchDocument {
        index: usize,
    },
    CloseDocument {
        index: usize,
    },
    OpenDocument {
        path: PathBuf,
    },
    JumpBackward,
    JumpForward,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    DocumentOpened(DocumentId),
    DocumentClosed(DocumentId),
    ActiveDocumentChanged(DocumentId),
    RedrawNeeded(DocumentId),
    FollowExternalLink { target: ExternalLink },
}

pub trait DocumentBackend: Send + Sync {
    fn info(&self) -> &DocumentInfo;
    fn render_page(&self, request: RenderRequest) -> Result<RenderImage>;
    fn outline(&self) -> Result<Vec<OutlineItem>> {
        Ok(Vec::new())
    }
    fn page_text(&self, _page_index: usize) -> Result<PageText> {
        Err(anyhow!("text extraction not supported"))
    }
    fn search_page(&self, _page_index: usize, _query: &str) -> Result<Vec<Vec<NormalizedRect>>> {
        Ok(Vec::new())
    }
    fn page_links(&self, _page_index: usize) -> Result<Vec<LinkDefinition>> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
pub trait DocumentProvider: Send + Sync {
    async fn open(&self, path: &Path) -> Result<Arc<dyn DocumentBackend>>;
}

pub trait StateStore: Send + Sync {
    fn load(&self, doc: &DocumentInfo) -> Result<Option<PersistedDocumentState>>;
    fn save(&self, doc: &DocumentInfo, state: &PersistedDocumentState) -> Result<()>;
}

pub struct FileStateStore {
    root: PathBuf,
}

impl FileStateStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create state directory at {:?}", root))?;
        Ok(Self { root })
    }

    fn state_path(&self, doc: &DocumentInfo) -> PathBuf {
        let mut path = self.root.join(format!("{}.json", doc.id));
        if let Some(ext) = path.extension() {
            if ext != "json" {
                path.set_extension("json");
            }
        }
        path
    }
}

impl StateStore for FileStateStore {
    fn load(&self, doc: &DocumentInfo) -> Result<Option<PersistedDocumentState>> {
        let path = self.state_path(doc);
        if !path.exists() {
            return Ok(None);
        }
        let mut file =
            File::open(&path).with_context(|| format!("failed to open state file {:?}", path))?;
        let mut buf = String::new();
        file.read_to_string(&mut buf)?;
        let state = serde_json::from_str(&buf)
            .with_context(|| format!("failed to decode state file {:?}", path))?;
        Ok(Some(state))
    }

    fn save(&self, doc: &DocumentInfo, state: &PersistedDocumentState) -> Result<()> {
        let path = self.state_path(doc);
        let tmp = path.with_extension("json.tmp");
        let payload = serde_json::to_string_pretty(state)?;
        let mut file = File::create(&tmp)
            .with_context(|| format!("failed to open temp state file {:?}", tmp))?;
        file.write_all(payload.as_bytes())?;
        file.flush()?;
        fs::rename(tmp, path)?;
        Ok(())
    }
}

pub struct Session {
    documents: Vec<DocumentInstance>,
    active: usize,
    store: Arc<dyn StateStore>,
    events: Arc<Mutex<Vec<SessionEvent>>>,
}

impl Session {
    pub fn new(store: Arc<dyn StateStore>) -> Self {
        Self {
            documents: Vec::new(),
            active: 0,
            store,
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn events(&self) -> Arc<Mutex<Vec<SessionEvent>>> {
        Arc::clone(&self.events)
    }

    pub fn drain_events(&self) -> Vec<SessionEvent> {
        let mut events = self.events.lock();
        events.drain(..).collect()
    }

    pub fn apply_search_results(
        &mut self,
        doc_id: DocumentId,
        query: String,
        matches: Vec<SearchMatch>,
        start_page: usize,
    ) -> Result<bool> {
        let Some(doc) = self.documents.iter_mut().find(|doc| doc.info.id == doc_id) else {
            return Ok(false);
        };

        let changed = doc.apply_search_results(query, matches, start_page);
        self.events
            .lock()
            .push(SessionEvent::RedrawNeeded(doc.info.id));
        Ok(changed)
    }

    pub fn active(&self) -> Option<&DocumentInstance> {
        self.documents.get(self.active)
    }

    pub fn selection_text(&self) -> Option<String> {
        self.active().and_then(|doc| doc.selection_text())
    }

    pub fn contains_document(&self, doc_id: DocumentId) -> bool {
        self.documents.iter().any(|doc| doc.info.id == doc_id)
    }

    #[instrument(skip(self, provider))]
    pub async fn open_with<P: DocumentProvider>(
        &mut self,
        provider: &P,
        path: PathBuf,
    ) -> Result<()> {
        let backend = provider.open(&path).await?;
        let info = backend.info().clone();
        let state = self.store.load(&info)?.unwrap_or_default();
        let outline = match backend.outline() {
            Ok(outline) => outline,
            Err(err) => {
                warn!(
                    ?err,
                    path = %info.path.display(),
                    "failed to load document outline"
                );
                Vec::new()
            }
        };
        let doc = DocumentInstance::new(info.clone(), backend, state, outline);
        self.documents.push(doc);
        self.active = self.documents.len().saturating_sub(1);
        self.events
            .lock()
            .push(SessionEvent::DocumentOpened(info.id));
        self.events
            .lock()
            .push(SessionEvent::ActiveDocumentChanged(info.id));
        Ok(())
    }

    #[instrument(skip(self, provider))]
    pub async fn reload_document<P: DocumentProvider>(
        &mut self,
        provider: &P,
        doc_id: DocumentId,
    ) -> Result<bool> {
        let Some(index) = self.documents.iter().position(|doc| doc.info.id == doc_id) else {
            return Ok(false);
        };

        let path = self.documents[index].info.path.clone();
        let backend = provider.open(&path).await?;
        let info = backend.info().clone();
        let outline = match backend.outline() {
            Ok(outline) => outline,
            Err(err) => {
                warn!(
                    ?err,
                    path = %info.path.display(),
                    "failed to reload document outline"
                );
                Vec::new()
            }
        };

        self.documents[index].reload(info, backend, outline);
        self.events.lock().push(SessionEvent::RedrawNeeded(doc_id));
        trace!(doc = %doc_id, "reloaded document after change");
        Ok(true)
    }

    pub fn apply(&mut self, command: Command) -> Result<()> {
        match command {
            Command::PutMark { key } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    let curr_page = doc.state.current_page;
                    doc.add_mark(key, curr_page);
                }
            }
            Command::GotoMark { key } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if let Some(page) = doc.get_page_from_mark(key) {
                        let previous = doc.current_position();
                        let next = page.min(doc.info.page_count.saturating_sub(1));
                        if next != doc.state.current_page {
                            doc.state.current_page = next;
                            doc.state.viewport.reset();
                            doc.record_jump_from(previous);
                            self.events
                                .lock()
                                .push(SessionEvent::RedrawNeeded(doc.info.id));
                        }
                    }
                }
            }
            Command::SaveNamedMark { name } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    let curr_page = doc.state.current_page;
                    doc.add_named_mark(name, curr_page);
                }
            }
            Command::GotoNamedMark { name } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if let Some(page) = doc.named_mark_page(&name) {
                        let previous = doc.current_position();
                        let next = page.min(doc.info.page_count.saturating_sub(1));
                        if next != doc.state.current_page {
                            doc.state.current_page = next;
                            doc.state.viewport.reset();
                            doc.record_jump_from(previous);
                            self.events
                                .lock()
                                .push(SessionEvent::RedrawNeeded(doc.info.id));
                        }
                    }
                }
            }
            Command::Search { query } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    doc.perform_search(query)?;
                    self.events
                        .lock()
                        .push(SessionEvent::RedrawNeeded(doc.info.id));
                }
            }
            Command::SearchNext { count } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.next_search_match(count.max(1)).is_some() {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::SearchPrev { count } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.previous_search_match(count.max(1)).is_some() {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::EnterVisualMode => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.ensure_visual_cursor()? {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::StartSelection => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.start_selection()? {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::MoveVisualCursor { motion, count } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.move_visual_cursor(motion, count.max(1))? {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::ClearSelection => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.clear_selection_state(true) {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::LeaveVisualMode => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.leave_visual_mode(true) {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::RestoreSelection => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.restore_last_selection()? {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::SwapVisualCursor => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.swap_visual_cursor() {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::EnterLinkMode => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    doc.start_link_mode()?;
                    self.events
                        .lock()
                        .push(SessionEvent::RedrawNeeded(doc.info.id));
                }
            }
            Command::LeaveLinkMode => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    doc.clear_link_state();
                    self.events
                        .lock()
                        .push(SessionEvent::RedrawNeeded(doc.info.id));
                }
            }
            Command::LinkNext { count } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.next_link(count.max(1)).is_some() {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::LinkPrev { count } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.previous_link(count.max(1)).is_some() {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::ActivateLink => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    match doc.activate_link() {
                        LinkFollowResult::Navigated { .. } => {
                            self.events
                                .lock()
                                .push(SessionEvent::RedrawNeeded(doc.info.id));
                        }
                        LinkFollowResult::External { target } => {
                            let mut events = self.events.lock();
                            events.push(SessionEvent::RedrawNeeded(doc.info.id));
                            events.push(SessionEvent::FollowExternalLink { target });
                        }
                        LinkFollowResult::Unsupported | LinkFollowResult::NoActiveLink => {}
                    }
                }
            }
            Command::OpenDocument { path: _ } => {
                anyhow::bail!("use `open_with` to open documents asynchronously");
            }
            Command::CloseDocument { index } => {
                if index >= self.documents.len() {
                    return Ok(());
                }
                let doc = self.documents.remove(index);
                self.store.save(&doc.info, &doc.state)?;
                self.events
                    .lock()
                    .push(SessionEvent::DocumentClosed(doc.info.id));
                if self.documents.is_empty() {
                    self.active = 0;
                } else if self.active >= self.documents.len() {
                    self.active = self.documents.len() - 1;
                    let id = self.documents[self.active].info.id;
                    self.events
                        .lock()
                        .push(SessionEvent::ActiveDocumentChanged(id));
                }
            }
            Command::SwitchDocument { index } => {
                if index < self.documents.len() {
                    self.active = index;
                    let id = self.documents[self.active].info.id;
                    self.events
                        .lock()
                        .push(SessionEvent::ActiveDocumentChanged(id));
                }
            }
            Command::NextPage { count } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    let previous = doc.current_position();
                    let next =
                        (doc.state.current_page + count).min(doc.info.page_count.saturating_sub(1));
                    if next != doc.state.current_page {
                        let diff = previous.page.abs_diff(next);
                        doc.state.current_page = next;
                        doc.state.viewport.reset();
                        if diff > 1 {
                            doc.record_jump_from(previous);
                        } else {
                            doc.sync_jump_position();
                        }
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::PrevPage { count } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    let previous = doc.current_position();
                    let next = doc.state.current_page.saturating_sub(count);
                    if next != doc.state.current_page {
                        let diff = previous.page.abs_diff(next);
                        doc.state.current_page = next;
                        doc.state.viewport.reset();
                        if diff > 1 {
                            doc.record_jump_from(previous);
                        } else {
                            doc.sync_jump_position();
                        }
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::GotoPage { page } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    let previous = doc.current_position();
                    let next = page.min(doc.info.page_count.saturating_sub(1));
                    if next != doc.state.current_page {
                        doc.state.current_page = next;
                        doc.state.viewport.reset();
                        doc.record_jump_from(previous);
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::ScaleBy { factor } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    let scale = (doc.state.scale * factor).clamp(0.25, 4.0);
                    if (doc.state.scale - scale).abs() > f32::EPSILON {
                        doc.state.scale = scale;
                        if scale <= 1.0 + f32::EPSILON {
                            doc.state.viewport.reset();
                        } else {
                            doc.state.viewport.clamp();
                        }
                        doc.sync_jump_position();
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::ResetScale => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    let prev_scale = doc.state.scale;
                    let viewport_changed = (doc.state.viewport.x.abs() > f32::EPSILON)
                        || (doc.state.viewport.y.abs() > f32::EPSILON);
                    doc.state.scale = 1.0;
                    doc.state.viewport.reset();
                    doc.sync_jump_position();
                    if (prev_scale - 1.0).abs() > f32::EPSILON || viewport_changed {
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::AdjustViewport { delta_x, delta_y } => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if doc.state.viewport.adjust(delta_x, delta_y) {
                        doc.state.viewport.clamp();
                        doc.sync_jump_position();
                        self.events
                            .lock()
                            .push(SessionEvent::RedrawNeeded(doc.info.id));
                    }
                }
            }
            Command::ToggleDarkMode => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    doc.state.dark_mode = !doc.state.dark_mode;
                    self.events
                        .lock()
                        .push(SessionEvent::RedrawNeeded(doc.info.id));
                }
            }
            Command::JumpBackward => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if let Some(position) = doc.pop_jump_backward() {
                        if doc.apply_document_position(position) {
                            self.events
                                .lock()
                                .push(SessionEvent::RedrawNeeded(doc.info.id));
                        }
                    }
                }
            }
            Command::JumpForward => {
                if let Some(doc) = self.documents.get_mut(self.active) {
                    if let Some(position) = doc.pop_jump_forward() {
                        if doc.apply_document_position(position) {
                            self.events
                                .lock()
                                .push(SessionEvent::RedrawNeeded(doc.info.id));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn persist(&self) -> Result<()> {
        for doc in &self.documents {
            self.store.save(&doc.info, &doc.state)?;
        }
        Ok(())
    }
}

pub struct MemoryStateStore {
    inner: Mutex<HashMap<DocumentId, PersistedDocumentState>>,
}

impl MemoryStateStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemoryStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StateStore for MemoryStateStore {
    fn load(&self, doc: &DocumentInfo) -> Result<Option<PersistedDocumentState>> {
        Ok(self.inner.lock().get(&doc.id).cloned())
    }

    fn save(&self, doc: &DocumentInfo, state: &PersistedDocumentState) -> Result<()> {
        self.inner.lock().insert(doc.id, state.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use tempfile::tempdir;

    struct FakeBackend {
        info: DocumentInfo,
    }

    #[async_trait::async_trait]
    impl DocumentBackend for FakeBackend {
        fn info(&self) -> &DocumentInfo {
            &self.info
        }

        fn render_page(&self, request: RenderRequest) -> Result<RenderImage> {
            Ok(RenderImage {
                width: 1,
                height: 1,
                pixels: vec![request.page_index as u8],
            })
        }

        fn page_text(&self, page_index: usize) -> Result<PageText> {
            let content = format!("This is sample page {} with keyword", page_index);
            let mut offset = 0;
            let mut glyphs = Vec::new();
            for ch in content.chars() {
                let start = offset;
                offset += ch.len_utf8();
                glyphs.push(TextGlyph {
                    range: start..offset,
                    rect: NormalizedRect {
                        left: 0.0,
                        top: 0.0,
                        right: 0.0,
                        bottom: 0.0,
                    },
                });
            }
            Ok(PageText::new(content, glyphs))
        }

        fn search_page(&self, page_index: usize, query: &str) -> Result<Vec<Vec<NormalizedRect>>> {
            if query.trim().is_empty() {
                return Ok(Vec::new());
            }
            let text = format!("This is sample page {} with keyword", page_index);
            if text.to_lowercase().contains(&query.to_lowercase()) {
                Ok(vec![vec![NormalizedRect {
                    left: 0.1,
                    top: 0.1,
                    right: 0.9,
                    bottom: 0.2,
                }]])
            } else {
                Ok(Vec::new())
            }
        }

        fn page_links(&self, _page_index: usize) -> Result<Vec<LinkDefinition>> {
            Ok(Vec::new())
        }
    }

    struct FakeProvider;

    #[async_trait::async_trait]
    impl DocumentProvider for FakeProvider {
        async fn open(&self, path: &Path) -> Result<Arc<dyn DocumentBackend>> {
            let info = DocumentInfo {
                id: document_id_for_path(path),
                path: path.to_path_buf(),
                page_count: 100,
                metadata: DocumentMetadata::default(),
            };
            Ok(Arc::new(FakeBackend { info }))
        }
    }

    #[tokio::test]
    async fn session_navigation_updates_state() {
        let store = Arc::new(MemoryStateStore::new());
        let mut session = Session::new(store.clone());
        let provider = FakeProvider;
        session
            .open_with(&provider, PathBuf::from("/tmp/example.pdf"))
            .await
            .unwrap();

        session.apply(Command::NextPage { count: 10 }).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 10);
        session.apply(Command::PrevPage { count: 5 }).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 5);
        session.apply(Command::GotoPage { page: 99 }).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 99);
        session.apply(Command::GotoPage { page: 150 }).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 99);

        session.persist().unwrap();
        let info = session.active().unwrap().info.clone();
        let stored = store.load(&info).unwrap().unwrap();
        assert_eq!(stored.current_page, 99);
    }

    #[tokio::test]
    async fn session_jump_history_tracks_positions() {
        let store = Arc::new(MemoryStateStore::new());
        let mut session = Session::new(store);
        let provider = FakeProvider;
        session
            .open_with(&provider, PathBuf::from("/tmp/example.pdf"))
            .await
            .unwrap();

        session.apply(Command::NextPage { count: 12 }).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 12);

        session.apply(Command::JumpBackward).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 0);

        session.apply(Command::JumpForward).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 12);

        session.apply(Command::GotoPage { page: 25 }).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 25);

        session.apply(Command::PrevPage { count: 5 }).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 20);

        session.apply(Command::JumpBackward).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 25);

        session.apply(Command::JumpBackward).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 12);

        session.apply(Command::JumpBackward).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 0);

        session.apply(Command::JumpForward).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 12);

        session.apply(Command::JumpForward).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 25);

        session.apply(Command::JumpForward).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 20);

        session.apply(Command::GotoPage { page: 40 }).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 40);

        session.apply(Command::JumpForward).unwrap();
        assert_eq!(session.active().unwrap().state.current_page, 40);
    }

    #[tokio::test]
    async fn session_search_navigates_matches() {
        let store = Arc::new(MemoryStateStore::new());
        let mut session = Session::new(store);
        let provider = FakeProvider;
        session
            .open_with(&provider, PathBuf::from("/tmp/example.pdf"))
            .await
            .unwrap();

        session
            .apply(Command::Search {
                query: "KEYWORD".to_string(),
            })
            .unwrap();
        {
            let doc = session.active().unwrap();
            assert_eq!(doc.state.current_page, 0);
            let summary = doc.search_summary().unwrap();
            assert_eq!(summary.total, doc.info.page_count);
            assert_eq!(summary.current_index, Some(0));
            let highlights = doc.search_highlights_for_current_page().unwrap();
            assert!(!highlights.current.is_empty() || !highlights.others.is_empty());
        }

        session.apply(Command::GotoPage { page: 5 }).unwrap();
        session
            .apply(Command::Search {
                query: "keyword".to_string(),
            })
            .unwrap();
        {
            let doc = session.active().unwrap();
            assert_eq!(doc.state.current_page, 5);
            let summary = doc.search_summary().unwrap();
            assert_eq!(summary.current_index, Some(5));
            let highlights = doc.search_highlights_for_current_page().unwrap();
            assert!(!highlights.current.is_empty());
        }

        session.apply(Command::SearchNext { count: 1 }).unwrap();
        {
            let doc = session.active().unwrap();
            assert_eq!(doc.state.current_page, 6);
            let summary = doc.search_summary().unwrap();
            assert_eq!(summary.current_index, Some(6));
            let highlights = doc.search_highlights_for_current_page().unwrap();
            assert!(!highlights.current.is_empty());
        }

        session.apply(Command::SearchPrev { count: 2 }).unwrap();
        {
            let doc = session.active().unwrap();
            assert_eq!(doc.state.current_page, 4);
            let summary = doc.search_summary().unwrap();
            assert_eq!(summary.current_index, Some(4));
            let highlights = doc.search_highlights_for_current_page().unwrap();
            assert!(!highlights.current.is_empty());
        }

        session
            .apply(Command::Search {
                query: "missing".to_string(),
            })
            .unwrap();
        {
            let doc = session.active().unwrap();
            let summary = doc.search_summary().unwrap();
            assert_eq!(summary.total, 0);
            assert!(summary.current_index.is_none());
            assert!(doc.search_highlights_for_current_page().is_none());
        }
    }

    struct LinkBackend {
        info: DocumentInfo,
        links: Vec<Vec<LinkDefinition>>,
    }

    impl LinkBackend {
        fn new(info: DocumentInfo, links: Vec<Vec<LinkDefinition>>) -> Self {
            Self { info, links }
        }
    }

    #[async_trait::async_trait]
    impl DocumentBackend for LinkBackend {
        fn info(&self) -> &DocumentInfo {
            &self.info
        }

        fn render_page(&self, _request: RenderRequest) -> Result<RenderImage> {
            Ok(RenderImage {
                width: 1,
                height: 1,
                pixels: vec![0, 0, 0, 0],
            })
        }

        fn page_text(&self, _page_index: usize) -> Result<PageText> {
            Ok(PageText::new(String::new(), Vec::new()))
        }

        fn search_page(
            &self,
            _page_index: usize,
            _query: &str,
        ) -> Result<Vec<Vec<NormalizedRect>>> {
            Ok(Vec::new())
        }

        fn page_links(&self, page_index: usize) -> Result<Vec<LinkDefinition>> {
            Ok(self.links.get(page_index).cloned().unwrap_or_default())
        }
    }

    #[test]
    fn link_mode_navigation_and_activation() {
        let path = PathBuf::from("/tmp/link-test.pdf");
        let info = DocumentInfo {
            id: document_id_for_path(&path),
            path,
            page_count: 3,
            metadata: DocumentMetadata::default(),
        };

        let links = vec![
            vec![LinkDefinition {
                rects: vec![NormalizedRect {
                    left: 0.1,
                    top: 0.1,
                    right: 0.3,
                    bottom: 0.2,
                }],
                action: LinkAction::GoTo { page: 1 },
            }],
            vec![LinkDefinition {
                rects: vec![NormalizedRect {
                    left: 0.2,
                    top: 0.2,
                    right: 0.4,
                    bottom: 0.3,
                }],
                action: LinkAction::Uri {
                    uri: "https://example.com".to_string(),
                },
            }],
            Vec::new(),
        ];

        let backend = Arc::new(LinkBackend::new(info.clone(), links));
        let mut instance =
            DocumentInstance::new(info, backend, PersistedDocumentState::default(), Vec::new());

        assert!(instance.link_summary().is_none());

        instance.start_link_mode().expect("link mode");

        let summary = instance.link_summary().expect("link summary present");
        assert_eq!(summary.total, 2);
        assert_eq!(summary.current_index, Some(0));

        let highlights = instance
            .link_highlights_for_current_page()
            .expect("highlights on current page");
        assert!(!highlights.current.is_empty());

        match instance.activate_link() {
            LinkFollowResult::Navigated { page_changed } => assert!(page_changed),
            other => panic!("unexpected activation result: {:?}", other),
        }
        assert_eq!(instance.state.current_page, 1);

        assert!(instance.next_link(1).is_some());

        match instance.activate_link() {
            LinkFollowResult::External { target } => match target {
                ExternalLink::Url(url) => assert_eq!(url, "https://example.com"),
                other => panic!("unexpected external target: {:?}", other),
            },
            other => panic!("unexpected activation result: {:?}", other),
        }
    }

    #[test]
    fn link_mode_skips_links_before_current_page() {
        let path = PathBuf::from("/tmp/link-skip.pdf");
        let info = DocumentInfo {
            id: document_id_for_path(&path),
            path,
            page_count: 3,
            metadata: DocumentMetadata::default(),
        };

        let links = vec![
            vec![LinkDefinition {
                rects: vec![NormalizedRect {
                    left: 0.1,
                    top: 0.1,
                    right: 0.2,
                    bottom: 0.2,
                }],
                action: LinkAction::GoTo { page: 0 },
            }],
            Vec::new(),
            vec![LinkDefinition {
                rects: vec![NormalizedRect {
                    left: 0.3,
                    top: 0.3,
                    right: 0.4,
                    bottom: 0.4,
                }],
                action: LinkAction::GoTo { page: 2 },
            }],
        ];

        let backend = Arc::new(LinkBackend::new(info.clone(), links));
        let mut state = PersistedDocumentState::default();
        state.current_page = 1;
        let mut instance = DocumentInstance::new(info, backend, state, Vec::new());

        instance.start_link_mode().expect("link mode");
        let summary = instance.link_summary().expect("link summary present");
        assert_eq!(summary.current_index, None);

        let result = instance.next_link(1).expect("advance link");
        assert!(result);
        assert_eq!(instance.state.current_page, 2);
    }

    #[test]
    fn document_id_is_stable_for_same_path() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("sample.pdf");
        std::fs::write(&file_path, b"dummy").unwrap();

        let first = document_id_for_path(&file_path);
        let second = document_id_for_path(&file_path);

        assert_eq!(first, second);
    }

    #[test]
    fn file_state_store_restores_state_with_stable_id() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("sample.pdf");
        std::fs::write(&file_path, b"dummy").unwrap();

        let info = DocumentInfo {
            id: document_id_for_path(&file_path),
            path: file_path.clone(),
            page_count: 3,
            metadata: DocumentMetadata::default(),
        };

        let store = FileStateStore::new(dir.path().join("state")).unwrap();

        let mut state = PersistedDocumentState::default();
        state.current_page = 2;
        state.scale = 1.5;
        state.dark_mode = true;
        state.marks.insert('a', 1);
        state.named_marks.insert("foo".into(), 2);

        store.save(&info, &state).unwrap();

        let restored = store.load(&info).unwrap().unwrap();
        assert_eq!(restored.current_page, 2);
        assert!(restored.dark_mode);
        assert_eq!(restored.scale, 1.5);
        assert_eq!(restored.marks.get(&'a'), Some(&1));
        assert_eq!(restored.named_marks.get("foo"), Some(&2));
    }
}
