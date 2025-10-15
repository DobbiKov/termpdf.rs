use std::convert::TryFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use pdfium_render::prelude::*;
use termpdf_core::{
    document_id_for_path, DocumentBackend, DocumentInfo, DocumentMetadata, DocumentProvider,
    OutlineItem, RenderImage, RenderRequest,
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
        }
    }

    fn load_document(&self) -> Result<PdfDocument<'_>> {
        self.pdfium
            .load_pdf_from_file(&self.path, None)
            .with_context(|| format!("failed to open {:?}", self.path))
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

        let document = self.load_document()?;
        let image = self.render_internal(&document, &request)?;

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

        let document = self.load_document()?;
        let mut outline = Vec::new();

        if let Some(root) = document.bookmarks().root() {
            collect_outline(root, 0, &mut outline);
        }

        let mut cache = self.outline_cache.lock();
        *cache = Some(outline.clone());

        Ok(outline)
    }
}

fn collect_outline(mut bookmark: PdfBookmark<'_>, depth: usize, out: &mut Vec<OutlineItem>) {
    loop {
        if let Some(title) = bookmark.title() {
            if let Some(destination) = bookmark.destination() {
                if let Ok(page_index) = destination.page_index() {
                    if let Ok(page_index) = usize::try_from(page_index) {
                        out.push(OutlineItem {
                            title,
                            page_index,
                            depth,
                        });
                    }
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
