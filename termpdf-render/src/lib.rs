use std::convert::TryFrom;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use pdfium_render::prelude::*;
use termpdf_core::{
    document_id_for_path, DocumentBackend, DocumentInfo, DocumentMetadata, DocumentProvider,
    LinkAction, LinkDefinition, NormalizedRect, OutlineItem, RenderImage, RenderRequest,
};
use tracing::{instrument, warn};

pub struct PdfiumRenderFactory {
    pdfium: Arc<Pdfium>,
}

impl PdfiumRenderFactory {
    pub fn new() -> Result<Self> {
        let pdfium = match bind_pdfium_from_build_hint() {
            Some(pdfium) => pdfium,
            None => bind_pdfium_default()?,
        };
        Ok(Self {
            pdfium: Arc::new(pdfium),
        })
    }
}

#[async_trait]
impl DocumentProvider for PdfiumRenderFactory {
    async fn open(&self, path: &Path) -> Result<Arc<dyn DocumentBackend>> {
        let absolute = path
            .canonicalize()
            .with_context(|| format!("failed to resolve path for {:?}", path))?;
        let info = build_document_info(&self.pdfium, &absolute)?;
        Ok(Arc::new(PdfiumDocument::new(
            Arc::clone(&self.pdfium),
            absolute,
            info,
        )))
    }
}

struct PdfiumDocument {
    pdfium: Arc<Pdfium>,
    path: PathBuf,
    info: DocumentInfo,
    cache: Mutex<Option<RenderCacheEntry>>,
    outline_cache: Mutex<Option<Vec<OutlineItem>>>,
    document: Mutex<Option<PdfDocument<'static>>>,
}

struct RenderCacheEntry {
    page_index: usize,
    scale: f32,
    dark_mode: bool,
    image: RenderImage,
}

impl PdfiumDocument {
    fn new(pdfium: Arc<Pdfium>, path: PathBuf, info: DocumentInfo) -> Self {
        Self {
            pdfium,
            path,
            info,
            cache: Mutex::new(None),
            outline_cache: Mutex::new(None),
            document: Mutex::new(None),
        }
    }

    fn open_document(&self) -> Result<PdfDocument<'static>> {
        let document = self
            .pdfium
            .load_pdf_from_file(&self.path, None)
            .with_context(|| format!("failed to open {:?}", self.path))?;
        // SAFETY: the returned PdfDocument holds a reference to the Pdfium bindings owned by
        // self.pdfium. The document is stored inside self.document and will be dropped before the
        // Pdfium instance because struct fields drop in reverse order of declaration (document
        // precedes pdfium). This ensures the reference remains valid for the lifetime of the
        // cached PdfDocument.
        let document = unsafe { mem::transmute::<PdfDocument<'_>, PdfDocument<'static>>(document) };
        Ok(document)
    }

    fn with_document<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&PdfDocument<'static>) -> Result<R>,
    {
        let mut guard = self.document.lock();
        if guard.is_none() {
            let document = self.open_document()?;
            *guard = Some(document);
        }
        let document = guard.as_ref().expect("document must be loaded");
        f(document)
    }

    fn render_internal(
        &self,
        document: &PdfDocument<'_>,
        request: &RenderRequest,
    ) -> Result<RenderImage> {
        let page_index: PdfPageIndex = request
            .page_index
            .try_into()
            .map_err(|_| anyhow!("page {} is out of supported range", request.page_index))?;
        let page = document
            .pages()
            .get(page_index)
            .with_context(|| format!("page {} out of range", request.page_index))?;

        let config = PdfRenderConfig::new().scale_page_by_factor(request.scale.max(0.1));
        let bitmap = page
            .render_with_config(&config)
            .with_context(|| format!("failed to render page {}", request.page_index))?;
        let image = bitmap.as_image().to_rgba8();
        let mut pixels = image.into_raw();

        if request.dark_mode {
            invert_pixels(&mut pixels);
        }

        Ok(RenderImage {
            width: u32::try_from(bitmap.width()).unwrap_or_default(),
            height: u32::try_from(bitmap.height()).unwrap_or_default(),
            pixels,
        })
    }

    fn link_action_from_pdfium(&self, link: &PdfLink<'_>) -> Option<LinkAction> {
        if let Some(action) = link.action() {
            match action.action_type() {
                PdfActionType::GoToDestinationInSameDocument => {
                    if let Some(local) = action.as_local_destination_action() {
                        if let Ok(destination) = local.destination() {
                            if let Ok(page_index) = destination.page_index() {
                                return Some(LinkAction::GoTo {
                                    page: page_index as usize,
                                });
                            }
                        }
                    }
                }
                PdfActionType::Uri => {
                    if let Some(uri_action) = action.as_uri_action() {
                        if let Ok(uri) = uri_action.uri() {
                            if !uri.is_empty() {
                                return Some(LinkAction::Uri { uri });
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if let Some(destination) = link.destination() {
            if let Ok(page_index) = destination.page_index() {
                return Some(LinkAction::GoTo {
                    page: page_index as usize,
                });
            }
        }

        None
    }
}

impl DocumentBackend for PdfiumDocument {
    fn info(&self) -> &DocumentInfo {
        &self.info
    }

    #[instrument(skip(self))]
    fn render_page(&self, request: RenderRequest) -> Result<RenderImage> {
        {
            let cache = self.cache.lock();
            if let Some(entry) = cache.as_ref() {
                if entry.page_index == request.page_index
                    && (entry.scale - request.scale).abs() < f32::EPSILON
                    && entry.dark_mode == request.dark_mode
                {
                    return Ok(entry.image.clone());
                }
            }
        }

        let image = self.with_document(|document| self.render_internal(document, &request))?;

        let mut cache = self.cache.lock();
        *cache = Some(RenderCacheEntry {
            page_index: request.page_index,
            scale: request.scale,
            dark_mode: request.dark_mode,
            image: image.clone(),
        });

        Ok(image)
    }

    fn outline(&self) -> Result<Vec<OutlineItem>> {
        {
            let cache = self.outline_cache.lock();
            if let Some(cached) = cache.as_ref() {
                return Ok(cached.clone());
            }
        }

        let outline = self.with_document(|document| {
            let mut outline = Vec::new();
            if let Some(root) = document.bookmarks().root() {
                collect_outline(root, 0, &mut outline);
            }
            Ok(outline)
        })?;

        let mut cache = self.outline_cache.lock();
        *cache = Some(outline.clone());

        Ok(outline)
    }

    fn page_text(&self, page_index: usize) -> Result<String> {
        self.with_document(|document| {
            let page_index: PdfPageIndex = page_index
                .try_into()
                .map_err(|_| anyhow!("page {} is out of supported range", page_index))?;
            let page = document
                .pages()
                .get(page_index)
                .with_context(|| format!("page {} out of range", page_index))?;
            let text = page
                .text()
                .with_context(|| format!("failed to extract text for page {}", page_index))?;
            Ok(text.all())
        })
    }

    fn search_page(&self, page_index: usize, query: &str) -> Result<Vec<Vec<NormalizedRect>>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        self.with_document(|document| {
            let page_index: PdfPageIndex = page_index
                .try_into()
                .map_err(|_| anyhow!("page {} is out of supported range", page_index))?;
            let page = document
                .pages()
                .get(page_index)
                .with_context(|| format!("page {} out of range", page_index))?;
            let text = page
                .text()
                .with_context(|| format!("failed to extract text for page {}", page_index))?;

            let options = PdfSearchOptions::new();
            let search = text
                .search(query, &options)
                .with_context(|| format!("failed to perform search on page {}", page_index))?;

            let page_width = page.width().value;
            let page_height = page.height().value;
            if page_width <= 0.0 || page_height <= 0.0 {
                return Ok(Vec::new());
            }

            let mut results = Vec::new();
            while let Some(segments) = search.find_next() {
                let mut rects = Vec::new();
                for segment in segments.iter() {
                    let bounds = segment.bounds();
                    let left = (bounds.left().value / page_width).clamp(0.0, 1.0);
                    let right = (bounds.right().value / page_width).clamp(0.0, 1.0);
                    let top_ratio = bounds.top().value / page_height;
                    let bottom_ratio = bounds.bottom().value / page_height;
                    let top_norm = (1.0 - top_ratio).clamp(0.0, 1.0);
                    let bottom_norm = (1.0 - bottom_ratio).clamp(0.0, 1.0);
                    let rect = NormalizedRect {
                        left,
                        top: top_norm,
                        right,
                        bottom: bottom_norm,
                    }
                    .clamp();
                    if rect.is_valid() {
                        rects.push(rect);
                    }
                }
                results.push(rects);
            }

            Ok(results)
        })
    }

    fn page_links(&self, page_index: usize) -> Result<Vec<LinkDefinition>> {
        self.with_document(|document| {
            let page_index: PdfPageIndex = page_index
                .try_into()
                .map_err(|_| anyhow!("page {} is out of supported range", page_index))?;
            let page = document
                .pages()
                .get(page_index)
                .with_context(|| format!("page {} out of range", page_index))?;

            let page_width = page.width().value;
            let page_height = page.height().value;
            if page_width <= 0.0 || page_height <= 0.0 {
                return Ok(Vec::new());
            }

            let mut definitions = Vec::new();
            let links = page.links();
            for link in links.iter() {
                let rect = match link.rect() {
                    Ok(rect) => rect,
                    Err(err) => {
                        warn!(
                            ?err,
                            page = page_index as usize,
                            path = %self.path.display(),
                            "failed to resolve link rectangle"
                        );
                        continue;
                    }
                };

                let left = (rect.left().value / page_width).clamp(0.0, 1.0);
                let right = (rect.right().value / page_width).clamp(0.0, 1.0);
                let top_ratio = rect.top().value / page_height;
                let bottom_ratio = rect.bottom().value / page_height;
                let top = (1.0 - top_ratio).clamp(0.0, 1.0);
                let bottom = (1.0 - bottom_ratio).clamp(0.0, 1.0);
                let rect = NormalizedRect {
                    left,
                    top,
                    right,
                    bottom,
                }
                .clamp();

                if !rect.is_valid() {
                    continue;
                }

                let Some(action) = self.link_action_from_pdfium(&link) else {
                    continue;
                };

                definitions.push(LinkDefinition {
                    rects: vec![rect],
                    action,
                });
            }

            Ok(definitions)
        })
    }
}

fn collect_outline(mut bookmark: PdfBookmark<'_>, depth: usize, out: &mut Vec<OutlineItem>) {
    loop {
        if let Some(title) = bookmark.title() {
            if let Some(destination) = bookmark.destination() {
                if let Ok(page_index) = destination.page_index() {
                    let page_index = page_index as usize;
                    out.push(OutlineItem {
                        title,
                        page_index,
                        depth,
                    });
                }
            }
        }

        if let Some(child) = bookmark.first_child() {
            collect_outline(child, depth + 1, out);
        }

        match bookmark.next_sibling() {
            Some(next) => bookmark = next,
            None => break,
        }
    }
}

fn build_document_info(pdfium: &Pdfium, path: &Path) -> Result<DocumentInfo> {
    let document = pdfium
        .load_pdf_from_file(path, None)
        .with_context(|| format!("failed to open {:?}", path))?;
    let page_count = usize::try_from(document.pages().len()).unwrap_or_default();
    let metadata = document.metadata();

    let title = metadata
        .get(PdfDocumentMetadataTagType::Title)
        .map(|t| t.value().to_owned());
    let author = metadata
        .get(PdfDocumentMetadataTagType::Author)
        .map(|t| t.value().to_owned());
    let keywords = metadata
        .get(PdfDocumentMetadataTagType::Keywords)
        .map(|t| t.value().split(',').map(|s| s.trim().to_owned()).collect())
        .unwrap_or_else(Vec::new);

    Ok(DocumentInfo {
        id: document_id_for_path(path),
        path: path.to_path_buf(),
        page_count,
        metadata: DocumentMetadata {
            title,
            author,
            keywords,
        },
    })
}

fn invert_pixels(pixels: &mut [u8]) {
    for chunk in pixels.chunks_exact_mut(4) {
        chunk[0] = 255 - chunk[0];
        chunk[1] = 255 - chunk[1];
        chunk[2] = 255 - chunk[2];
    }
}

pub type PdfRenderFactory = PdfiumRenderFactory;

fn bind_pdfium_from_build_hint() -> Option<Pdfium> {
    match option_env!("TERMPDF_PDFIUM_LIBRARY_PATH") {
        Some(path) if !path.is_empty() => match Pdfium::bind_to_library(path) {
            Ok(bindings) => Some(Pdfium::new(bindings)),
            Err(err) => {
                warn!(
                    "failed to load Pdfium from build-provided path {}: {}",
                    path, err
                );
                None
            }
        },
        _ => None,
    }
}

fn bind_pdfium_default() -> Result<Pdfium> {
    let mut errors = Vec::new();

    let cwd_path = Pdfium::pdfium_platform_library_name_at_path("./");

    match Pdfium::bind_to_library(&cwd_path) {
        Ok(bindings) => return Ok(Pdfium::new(bindings)),
        Err(err) => {
            errors.push(format!("{}: {}", cwd_path.display(), err));
        }
    }

    match Pdfium::bind_to_system_library() {
        Ok(bindings) => Ok(Pdfium::new(bindings)),
        Err(err) => {
            errors.push(format!("system: {err}"));
            Err(anyhow!(
                "failed to bind to a pdfium library; ensure it is installed ({})",
                errors.join(", ")
            ))
        }
    }
}
