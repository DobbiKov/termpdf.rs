use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Error, Result};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{instrument, warn};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedDocumentState {
    pub current_page: usize,
    pub scale: f32,
    pub dark_mode: bool,
    pub marks: HashMap<char, usize>,
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
struct SearchMatch {
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
}

#[derive(Debug, Clone, Default)]
pub struct SearchHighlights {
    pub current: Vec<NormalizedRect>,
    pub others: Vec<NormalizedRect>,
}

impl SearchHighlights {
    pub fn is_empty(&self) -> bool {
        self.current.is_empty() && self.others.is_empty()
    }
}

#[derive(Copy, Clone)]
enum SearchDirection {
    Forward,
    Backward,
}

pub struct DocumentInstance {
    pub info: DocumentInfo,
    pub backend: Arc<dyn DocumentBackend>,
    pub state: PersistedDocumentState,
    render_cache: Mutex<HashMap<CacheKey, RenderImage>>,
    outline: Vec<OutlineItem>,
    jump_history: JumpHistory,
    text_cache: Mutex<HashMap<usize, Arc<String>>>,
    search_state: Option<SearchState>,
}

impl DocumentInstance {
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
            text_cache: Mutex::new(HashMap::new()),
            search_state: None,
        };
        let initial = instance.current_position();
        instance.jump_history.record_initial(initial);
        instance
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
    pub fn add_mark(&mut self, mark: char, page: usize) {
        self.state.marks.insert(mark, page);
    }
    pub fn get_page_from_mark(&self, mark: char) -> Option<usize> {
        self.state.marks.get(&mark).map(|v| *v)
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

    fn load_page_text(&self, page_index: usize) -> Result<Arc<String>> {
        if page_index >= self.info.page_count {
            return Err(anyhow!("page {} out of range", page_index));
        }

        if let Some(text) = self.text_cache.lock().get(&page_index).cloned() {
            return Ok(text);
        }

        let text = self.backend.page_text(page_index)?;
        let text = Arc::new(text);
        self.text_cache.lock().insert(page_index, Arc::clone(&text));
        Ok(text)
    }

    fn build_search_matches(&self, query: &str) -> Result<Vec<SearchMatch>> {
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
                Ok(text) => {
                    if text.is_empty() {
                        continue;
                    }

                    let lower = text.to_lowercase();
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

    pub fn perform_search(&mut self, query: String) -> Result<bool> {
        let trimmed = query.trim().to_string();

        if trimmed.is_empty() {
            self.search_state = None;
            self.sync_jump_position();
            return Ok(false);
        }

        let matches = self.build_search_matches(&trimmed)?;
        let start_page = self.state.current_page;
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
            query: trimmed,
            matches,
            current_index: next_index,
        });

        if let Some(idx) = next_index {
            Ok(self.apply_search_index(idx))
        } else {
            self.sync_jump_position();
            Ok(false)
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
    NextPage { count: usize },
    PrevPage { count: usize },
    GotoPage { page: usize },
    ScaleBy { factor: f32 },
    ResetScale,
    AdjustViewport { delta_x: f32, delta_y: f32 },
    PutMark { key: char },
    GotoMark { key: char },
    Search { query: String },
    SearchNext { count: usize },
    SearchPrev { count: usize },
    ToggleDarkMode,
    SwitchDocument { index: usize },
    CloseDocument { index: usize },
    OpenDocument { path: PathBuf },
    JumpBackward,
    JumpForward,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    DocumentOpened(DocumentId),
    DocumentClosed(DocumentId),
    ActiveDocumentChanged(DocumentId),
    RedrawNeeded(DocumentId),
}

pub trait DocumentBackend: Send + Sync {
    fn info(&self) -> &DocumentInfo;
    fn render_page(&self, request: RenderRequest) -> Result<RenderImage>;
    fn outline(&self) -> Result<Vec<OutlineItem>> {
        Ok(Vec::new())
    }
    fn page_text(&self, _page_index: usize) -> Result<String> {
        Err(anyhow!("text extraction not supported"))
    }
    fn search_page(&self, _page_index: usize, _query: &str) -> Result<Vec<Vec<NormalizedRect>>> {
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

    pub fn active(&self) -> Option<&DocumentInstance> {
        self.documents.get(self.active)
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

        fn page_text(&self, page_index: usize) -> Result<String> {
            Ok(format!("This is sample page {} with keyword", page_index))
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

        store.save(&info, &state).unwrap();

        let restored = store.load(&info).unwrap().unwrap();
        assert_eq!(restored.current_page, 2);
        assert!(restored.dark_mode);
        assert_eq!(restored.scale, 1.5);
        assert_eq!(restored.marks.get(&'a'), Some(&1));
    }
}
