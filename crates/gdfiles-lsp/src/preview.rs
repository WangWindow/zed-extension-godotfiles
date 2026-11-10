//! Small, bounded image thumbnails for hover previews.
//!
//! Preview generation is strictly on demand: nothing here is called while
//! indexing. Inputs are read only after callers prove that the path is inside
//! the owning Godot project root (including symlink checks), so this module can
//! never follow a target outside that root.

use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::{Cursor, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

/// Maximum preview width, in pixels.
///
/// A Markdown image is rendered by Zed at its intrinsic pixel size. Hover
/// content is bounded vertically and scrolls when it is too tall, so previews
/// use a wide-but-short box instead of one large square. This keeps a preview
/// comfortably inside the popover while still using the available width.
pub const MAX_PREVIEW_WIDTH: u32 = 192;
/// Maximum preview height, in pixels.
pub const MAX_PREVIEW_HEIGHT: u32 = 128;
/// Hard cap for the encoded PNG bytes (before base64 expansion).
pub const MAX_ENCODED_IMAGE_BYTES: usize = 48 * 1024;
/// Refuse to read a source image larger than this.
pub const MAX_INPUT_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Refuse to decode images wider or taller than this.
pub const MAX_DECODE_DIMENSION: u32 = 8_192;
/// Refuse to allocate more than this while decoding.
pub const MAX_DECODE_ALLOCATION: u64 = 64 * 1024 * 1024;
/// Bounded number of cached successes and failures.
pub const MAX_CACHE_ENTRIES: usize = 64;

/// Smallest fallback preview we try to encode when a thumbnail exceeds the
/// byte cap. A source this small is already encoded losslessly and cannot be
/// reduced further without destroying the image.
const MIN_PREVIEW_DIMENSION: u32 = 8;

/// A thumbnail encoded exactly as a plain `data:image/png;base64,...` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub base64: String,
}

impl Preview {
    pub fn data_uri(&self) -> String {
        format!("data:image/png;base64,{}", self.base64)
    }
}

#[derive(Clone, Debug)]
struct CacheEntry {
    size: u64,
    modified: Option<SystemTime>,
    preview: Option<Preview>,
}

/// Small LRU-ish cache keyed by canonical path, mtime and size.
///
/// The cache is intentionally bounded by entry count and each entry is itself
/// capped at `MAX_ENCODED_IMAGE_BYTES`, so no hover can grow it without bound.
pub struct PreviewCache {
    entries: HashMap<PathBuf, CacheEntry>,
    order: VecDeque<PathBuf>,
    max_entries: usize,
}

impl Default for PreviewCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PreviewCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            max_entries: MAX_CACHE_ENTRIES,
        }
    }

    /// Return a previously generated thumbnail, or generate one on demand.
    ///
    /// The path must canonically live below `root`. Missing, unreadable,
    /// non-image, oversized and symlink-escaping files all return `None`.
    pub fn get(&mut self, path: &Path, root: &Path) -> Option<Preview> {
        let canonical = safe_image_path(path, root)?;
        let metadata = fs::metadata(&canonical).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_INPUT_IMAGE_BYTES {
            return None;
        }

        let size = metadata.len();
        let modified = metadata.modified().ok();

        if let Some(entry) = self.entries.get(&canonical)
            && entry.size == size
            && entry.modified == modified
        {
            let preview = entry.preview.clone();
            self.touch(&canonical);
            return preview;
        }

        let preview = generate_thumbnail(&canonical);
        self.insert(
            canonical,
            CacheEntry {
                size,
                modified,
                preview: preview.clone(),
            },
        );
        preview
    }

    fn touch(&mut self, path: &Path) {
        if let Some(index) = self.order.iter().position(|entry| entry == path) {
            self.order.remove(index);
        }
        self.order.push_back(path.to_path_buf());
    }

    fn insert(&mut self, path: PathBuf, entry: CacheEntry) {
        if !self.entries.contains_key(&path)
            && self.entries.len() >= self.max_entries
            && let Some(oldest) = self.order.pop_front()
        {
            self.entries.remove(&oldest);
        }
        self.touch(&path);
        self.entries.insert(path, entry);
    }
}

/// Raster formats that have a native decoder in the `image` crate. Extensions
/// without a decoder are intentionally absent.
fn image_format_for_extension(extension: &str) -> Option<image::ImageFormat> {
    use image::ImageFormat;

    Some(match extension {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::WebP,
        "gif" => ImageFormat::Gif,
        "bmp" => ImageFormat::Bmp,
        "ico" => ImageFormat::Ico,
        "tga" => ImageFormat::Tga,
        "tif" | "tiff" => ImageFormat::Tiff,
        "dds" => ImageFormat::Dds,
        "hdr" => ImageFormat::Hdr,
        _ => return None,
    })
}

/// Only raster formats with a native decoder and SVG are previewable. Fonts,
/// audio/video, engine scene caches and arbitrary formats are never decoded for
/// a hover.
pub fn is_supported_image(path: &Path) -> bool {
    let Some(extension) = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
    else {
        return false;
    };
    extension == "svg" || image_format_for_extension(&extension).is_some()
}

fn safe_image_path(path: &Path, root: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let canonical = path.canonicalize().ok()?;
    if !canonical.starts_with(&root) || !canonical.is_file() {
        return None;
    }
    Some(canonical)
}

fn generate_thumbnail(path: &Path) -> Option<Preview> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())?;
    let bytes = read_bounded(path)?;

    if extension == "svg" {
        return generate_svg_thumbnail(&bytes);
    }

    let format = image_format_for_extension(&extension)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOCATION);

    // Use the extension-selected decoder rather than content sniffing: TGA has
    // no magic number, and a project file must not be able to switch formats by
    // disguising its contents.
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);

    let image = reader.decode().ok()?;
    thumbnail_preview(image)
}

/// Read at most one byte beyond the limit so a file that grows concurrently is
/// still rejected rather than partially decoded.
fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_INPUT_IMAGE_BYTES {
        return None;
    }

    let mut bytes = Vec::with_capacity(metadata.len().min(MAX_INPUT_IMAGE_BYTES) as usize);
    file.take(MAX_INPUT_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_INPUT_IMAGE_BYTES {
        return None;
    }
    Some(bytes)
}

/// Rasterize a standalone SVG. No external files, URLs, embedded raster images,
/// system fonts, font files or scripts are loaded; text is not rendered because
/// the `resvg` text/font features are disabled at build time.
fn generate_svg_thumbnail(bytes: &[u8]) -> Option<Preview> {
    use resvg::usvg::{ImageHrefResolver, Options, Tree};

    let text = std::str::from_utf8(bytes).ok()?;

    let options = Options {
        resources_dir: None,
        image_href_resolver: ImageHrefResolver {
            // Both data: URIs and arbitrary strings (absolute paths, file://,
            // http(s)://, project-relative paths) are refused.
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..Options::default()
    };

    let tree = Tree::from_str(text, &options).ok()?;

    // Filters can be arbitrarily expensive. A hover is never allowed to become
    // an unbounded render job, so filtered SVGs fall back to metadata only.
    if !tree.filters().is_empty() {
        return None;
    }

    let size = tree.size();
    let width = size.width();
    let height = size.height();
    if !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
        || width > MAX_DECODE_DIMENSION as f32
        || height > MAX_DECODE_DIMENSION as f32
    {
        return None;
    }

    let scale = (MAX_PREVIEW_WIDTH as f32 / width)
        .min(MAX_PREVIEW_HEIGHT as f32 / height)
        .min(1.0);
    let target_width = ((width * scale).ceil() as u32).clamp(1, MAX_PREVIEW_WIDTH);
    let target_height = ((height * scale).ceil() as u32).clamp(1, MAX_PREVIEW_HEIGHT);

    let mut pixmap = resvg::tiny_skia::Pixmap::new(target_width, target_height)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny-skia stores premultiplied RGBA; PNG output must be straight alpha.
    let mut rgba = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.pixels() {
        let color = pixel.demultiply();
        rgba.extend_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
    }
    let image = image::RgbaImage::from_raw(target_width, target_height, rgba)?;
    thumbnail_preview(image::DynamicImage::ImageRgba8(image))
}

fn thumbnail_preview(image: image::DynamicImage) -> Option<Preview> {
    if image.width() > MAX_DECODE_DIMENSION || image.height() > MAX_DECODE_DIMENSION {
        return None;
    }

    let source_width = image.width();
    let source_height = image.height();
    let mut max_width = MAX_PREVIEW_WIDTH;
    let mut max_height = MAX_PREVIEW_HEIGHT;

    loop {
        let (width, height) =
            fit_without_upscale(source_width, source_height, max_width, max_height)?;
        // Normalize to 8-bit RGBA so HDR/16-bit/grayscale inputs all encode.
        // At native size this copy is tiny; larger sources are downscaled.
        let thumbnail = if width == source_width && height == source_height {
            image.to_rgba8()
        } else {
            image.thumbnail(width, height).to_rgba8()
        };
        debug_assert!(thumbnail.width() <= MAX_PREVIEW_WIDTH);
        debug_assert!(thumbnail.height() <= MAX_PREVIEW_HEIGHT);

        let encoded = encode_png(&image::DynamicImage::ImageRgba8(thumbnail))?;
        if encoded.len() <= MAX_ENCODED_IMAGE_BYTES {
            return Some(Preview {
                base64: BASE64.encode(encoded),
            });
        }

        // A pathologically incompressible thumbnail is retried at a smaller
        // size, but never enlarged and never past the byte budget.
        if max_width <= MIN_PREVIEW_DIMENSION && max_height <= MIN_PREVIEW_DIMENSION {
            return None;
        }
        max_width = (max_width * 3 / 4).max(MIN_PREVIEW_DIMENSION);
        max_height = (max_height * 3 / 4).max(MIN_PREVIEW_DIMENSION);
    }
}

/// Fit `width` x `height` inside `max_width` x `max_height`, preserving aspect
/// ratio and never scaling up. Small sources keep every original pixel, which
/// is essential for pixel art and for assets whose transparent pixels carry a
/// palette color: resampling those colors would introduce colored fringes.
fn fit_without_upscale(
    width: u32,
    height: u32,
    max_width: u32,
    max_height: u32,
) -> Option<(u32, u32)> {
    if width == 0 || height == 0 || max_width == 0 || max_height == 0 {
        return None;
    }
    if width <= max_width && height <= max_height {
        return Some((width, height));
    }

    let scale =
        (f64::from(max_width) / f64::from(width)).min(f64::from(max_height) / f64::from(height));
    let target_width = ((f64::from(width) * scale).round() as u32).clamp(1, max_width);
    let target_height = ((f64::from(height) * scale).round() as u32).clamp(1, max_height);
    Some((target_width, target_height))
}

fn encode_png(image: &image::DynamicImage) -> Option<Vec<u8>> {
    let mut encoded = Vec::new();
    image
        .write_to(Cursor::new(&mut encoded), image::ImageFormat::Png)
        .ok()?;
    Some(encoded)
}
