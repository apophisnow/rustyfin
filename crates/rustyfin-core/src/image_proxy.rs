use image::imageops::FilterType;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::fs;

/// The largest side, in pixels, that an image is served at.
pub const MAX_IMAGE_SIDE: u32 = 4096;

/// The size an image of `width`x`height` is served at: it fits the bounds and is never upscaled.
pub fn target_size(
    width: u32,
    height: u32,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> (u32, u32) {
    let Some((bw, bh)) = capped_bounds(max_width, max_height) else {
        return (width, height);
    };
    if width == 0 || height == 0 || (width <= bw && height <= bh) {
        return (width, height);
    }
    if u64::from(width) * u64::from(bh) >= u64::from(height) * u64::from(bw) {
        (bw, scale_rounded(height, bw, width).max(1))
    } else {
        (scale_rounded(width, bh, height).max(1), bh)
    }
}

/// `a * b / c` rounded half up, in u64 so that the product cannot overflow.
fn scale_rounded(a: u32, b: u32, c: u32) -> u32 {
    let (a, b, c) = (u64::from(a), u64::from(b), u64::from(c));
    ((2 * a * b + c) / (2 * c)) as u32
}

/// The bounds a request asks for, each capped at `MAX_IMAGE_SIDE`. `Some(0)` counts as no bound,
/// and the result is `None` when neither side is bounded.
fn capped_bounds(max_width: Option<u32>, max_height: Option<u32>) -> Option<(u32, u32)> {
    let max_width = max_width.filter(|&w| w > 0);
    let max_height = max_height.filter(|&h| h > 0);
    if max_width.is_none() && max_height.is_none() {
        return None;
    }
    Some((
        max_width.unwrap_or(MAX_IMAGE_SIDE).min(MAX_IMAGE_SIDE),
        max_height.unwrap_or(MAX_IMAGE_SIDE).min(MAX_IMAGE_SIDE),
    ))
}

/// What a request is served as: the source's own bytes, or a JPEG copy at a size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rendition {
    /// The source's own bytes.
    Original,
    /// A JPEG that fits within `width`x`height`, exactly that size when the source size is known.
    Copy { width: u32, height: u32 },
}

impl Rendition {
    /// The rendition's name in a cache key: `0x0` for the original, else `{width}x{height}`.
    pub fn key(&self) -> String {
        match self {
            Rendition::Original => "0x0".to_string(),
            Rendition::Copy { width, height } => format!("{width}x{height}"),
        }
    }
}

/// The rendition for a request. `known_size` is the source's size when it is known.
pub fn rendition_for(
    known_size: Option<(u32, u32)>,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> Rendition {
    let Some((bw, bh)) = capped_bounds(max_width, max_height) else {
        return Rendition::Original;
    };
    match known_size {
        Some((width, height)) if width > 0 && height > 0 => {
            let (w, h) = target_size(width, height, max_width, max_height);
            if (w, h) == (width, height) {
                Rendition::Original
            } else {
                Rendition::Copy {
                    width: w,
                    height: h,
                }
            }
        }
        _ => Rendition::Copy {
            width: bw,
            height: bh,
        },
    }
}

/// Plans how a request is served. A local image's size is read from its header unless it is known.
pub async fn plan_rendition(
    source_path: &str,
    known_size: Option<(u32, u32)>,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> Result<Rendition, String> {
    if capped_bounds(max_width, max_height).is_none() {
        return Ok(Rendition::Original);
    }
    let remote = source_path.starts_with("http://") || source_path.starts_with("https://");
    if known_size.is_some() || remote {
        return Ok(rendition_for(known_size, max_width, max_height));
    }
    let path = source_path.to_string();
    let size = tokio::task::spawn_blocking(move || read_dimensions(&path))
        .await
        .map_err(|e| format!("failed to read local image: {}", e))??;
    Ok(rendition_for(Some(size), max_width, max_height))
}

/// Reads a local image's size from its header alone.
fn read_dimensions(path: &str) -> Result<(u32, u32), String> {
    image::ImageReader::open(path)
        .map_err(|e| format!("failed to read local image: {}", e))?
        .with_guessed_format()
        .map_err(|e| format!("failed to read local image: {}", e))?
        .into_dimensions()
        .map_err(|e| format!("failed to decode image: {}", e))
}

/// A local image's version and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageStamp {
    /// The file's version, from `file_stamp`.
    pub stamp: i64,
    /// The picture's size in pixels, read from its header; `None` when the header cannot be read.
    pub size: Option<(u32, u32)>,
}

/// The version of a file: the first 8 bytes, big-endian, of SHA-256 over `{len}:{mtime_ns}`. The
/// modification time is in nanoseconds since 1970, or 0 when it is earlier or unavailable, so two
/// writes within one second still differ.
pub fn file_stamp(meta: &std::fs::Metadata) -> i64 {
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    let digest = Sha256::digest(format!("{}:{mtime_ns}", meta.len()));
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(first)
}

/// Reads a local image's stamp, and its size from the header when that can be read. A link is read
/// as the file it points to. Fails only when the file's metadata cannot be read.
pub fn read_image_stamp(path: &Path) -> std::io::Result<ImageStamp> {
    let meta = std::fs::metadata(path)?;
    let size = image::ImageReader::open(path)
        .and_then(|reader| reader.with_guessed_format())
        .ok()
        .and_then(|reader| reader.into_dimensions().ok());
    Ok(ImageStamp {
        stamp: file_stamp(&meta),
        size,
    })
}

/// The tag of a local image, which changes with its picture: the first 32 hex digits of SHA-256 over
/// the path and the stamp (0 when there is none).
pub fn image_tag(path: &str, stamp: Option<i64>) -> String {
    let digest = Sha256::digest(format!("{path}\n{}", stamp.unwrap_or(0)));
    hex(&digest[..16])
}

/// The name of the disk copy of a source at a size. It holds the full SHA-256 of the path, as a
/// short hash let different paths share a file, and the stamp, so a replaced file gets a new copy.
pub fn cached_copy_name(source_path: &str, stamp: Option<i64>, width: u32, height: u32) -> String {
    let hash = hex(&Sha256::digest(source_path.as_bytes()));
    format!(
        "{hash}_{:016x}_{width}x{height}.jpg",
        stamp.unwrap_or(0) as u64
    )
}

/// The lower-case hex digits of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Serves a rendition of an image. `Original` is the source's own bytes; a `Copy` is a JPEG
/// cached on disk under its stamp and size, so a replaced file is not served from an old copy.
pub async fn render_image(
    source_path: &str,
    stamp: Option<i64>,
    rendition: Rendition,
    cache_dir: &Path,
    http_client: Option<&reqwest::Client>,
) -> Result<Vec<u8>, String> {
    let Rendition::Copy { width, height } = rendition else {
        return read_image_bytes(source_path, http_client).await;
    };

    let name = cached_copy_name(source_path, stamp, width, height);
    let cache_path = cache_dir.join(&name);

    // Return the cached copy if it is whole; an empty or cut-short one is made again
    if let Ok(bytes) = fs::read(&cache_path).await
        && is_whole_jpeg(&bytes)
    {
        mark_copy_used(cache_path).await;
        return Ok(bytes);
    }

    // Read and resize. The target comes from the decoded size, so a stale size cannot upscale.
    let source_bytes = read_image_bytes(source_path, http_client).await?;
    let img = image::load_from_memory(&source_bytes)
        .map_err(|e| format!("failed to decode image: {}", e))?;
    // Fitting to both bounds at once re-rounds, so try each bound alone and keep the larger.
    let (w, h) = (img.width(), img.height());
    let target = [
        target_size(w, h, Some(width), None),
        target_size(w, h, None, Some(height)),
    ]
    .into_iter()
    .filter(|&(tw, th)| tw <= width && th <= height)
    .max_by_key(|&(tw, th)| u64::from(tw) * u64::from(th))
    .unwrap_or_else(|| target_size(w, h, Some(width), Some(height)));
    let img = if target != (w, h) {
        img.resize_exact(target.0, target.1, FilterType::Lanczos3)
    } else {
        img
    };

    // Encode as JPEG; the encoder rejects alpha, so the image goes in as RGB
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img.to_rgb8())
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .map_err(|e| format!("failed to encode image: {}", e))?;
    let bytes = buf.into_inner();

    // Cache the copy. A failure is logged, and the bytes are served all the same.
    if let Err(e) = write_copy(cache_dir, &name, &bytes).await {
        tracing::warn!("failed to cache image copy {}: {}", cache_path.display(), e);
    }

    Ok(bytes)
}

/// Whether a disk copy is whole: at least 4 bytes, from the JPEG start marker to the end marker.
fn is_whole_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes.starts_with(&[0xFF, 0xD8]) && bytes.ends_with(&[0xFF, 0xD9])
}

/// A file last modified longer ago than this is stale: a disk copy this old is marked as used when
/// it is hit, and a temporary file this old was left behind by a write that did not finish.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// How long ago a file was last modified; `None` when that is unknown or in the future.
fn file_age(meta: &std::fs::Metadata) -> Option<std::time::Duration> {
    meta.modified().ok()?.elapsed().ok()
}

/// Marks a disk copy as used, for the cap's oldest-first trim: a copy stale by `STALE_AFTER` gets
/// the time now. Errors are ignored, as the copy is served either way.
async fn mark_copy_used(path: std::path::PathBuf) {
    let _ = tokio::task::spawn_blocking(move || {
        let stale = std::fs::metadata(&path)
            .ok()
            .and_then(|meta| file_age(&meta))
            .is_some_and(|age| age > STALE_AFTER);
        if stale {
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(std::time::SystemTime::now()));
        }
    })
    .await;
}

/// Writes a disk copy under a temporary name, then renames it into place, so a reader never sees a
/// part-written copy. The temporary file is removed when the write or the rename fails.
async fn write_copy(cache_dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    fs::create_dir_all(cache_dir).await?;
    let temp = cache_dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = match fs::write(&temp, bytes).await {
        Ok(()) => fs::rename(&temp, cache_dir.join(name)).await,
        Err(e) => Err(e),
    };
    if written.is_err() {
        let _ = fs::remove_file(&temp).await;
    }
    written
}

/// Serve an image with optional resizing, using a cache directory.
/// If `http_client` is provided, it will be reused for remote image downloads,
/// avoiding the overhead of creating a new client per request.
pub async fn get_or_resize_image(
    source_path: &str,
    max_width: Option<u32>,
    max_height: Option<u32>,
    cache_dir: &Path,
    http_client: Option<&reqwest::Client>,
) -> Result<Vec<u8>, String> {
    let rendition = plan_rendition(source_path, None, max_width, max_height).await?;
    render_image(source_path, None, rendition, cache_dir, http_client).await
}

async fn read_image_bytes(
    source: &str,
    http_client: Option<&reqwest::Client>,
) -> Result<Vec<u8>, String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        // Download remote image with proper headers and status check
        // Reuse provided client if available, otherwise create a new one
        let client = match http_client {
            Some(client) => client,
            None => &reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (compatible; RustyFin/1.0)")
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .map_err(|e| format!("failed to create HTTP client: {}", e))?,
        };

        let resp = client
            .get(source)
            .send()
            .await
            .map_err(|e| format!("failed to fetch remote image: {}", e))?;

        // Check for successful HTTP status
        if !resp.status().is_success() {
            return Err(format!(
                "remote image returned status {}: {}",
                resp.status(),
                resp.status().canonical_reason().unwrap_or("unknown")
            ));
        }

        // Check content-type header to verify it's an image
        let content_type = resp.headers().get(reqwest::header::CONTENT_TYPE);
        if let Some(ct) = content_type
            && let Ok(ct_str) = ct.to_str()
            && !ct_str.starts_with("image/")
        {
            return Err(format!(
                "remote image returned non-image content type: {}",
                ct_str
            ));
        }

        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("failed to read remote image bytes: {}", e))
    } else {
        fs::read(source)
            .await
            .map_err(|e| format!("failed to read local image: {}", e))
    }
}

/// Compute a blurhash string for an image file.
/// Returns a short blurhash (8x8 components) suitable for progressive image loading.
/// If `http_client` is provided, it will be reused for remote image downloads.
pub async fn compute_blurhash(
    source_path: &str,
    http_client: Option<&reqwest::Client>,
) -> Option<String> {
    let bytes = read_image_bytes(source_path, http_client).await.ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    // Downscale to a small image for faster encoding
    let small = img.resize(16, 16, FilterType::Triangle);
    let rgba = small.to_rgba8();
    let width = rgba.width() as usize;
    let height = rgba.height() as usize;
    let pixels = rgba.as_raw();

    // Use 4x4 components for a short hash
    let hash = blurhash::encode(4, 4, width as u32, height as u32, pixels.as_slice()).ok()?;
    Some(hash)
}

/// The most the disk copies of images may total; `sweep_image_cache` trims them to this.
pub const IMAGE_CACHE_MAX_BYTES: u64 = 1 << 30;

/// What `sweep_image_cache` removed, and what it kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// The number of files removed.
    pub removed: usize,
    /// The total length of the files removed, in bytes.
    pub freed_bytes: u64,
    /// The total length of the disk copies kept, in bytes. Temporary files count in neither total.
    pub kept_bytes: u64,
}

/// Sweeps the folder of disk copies of images at `dir`. Only its top-level regular files are looked
/// at: a file whose name is not a copy's is removed, and so is a temporary file (`.*.tmp`) last
/// modified more than an hour ago. Symlinks and folders are left alone, and so is a fresh temporary
/// file. Then the copies are removed, least recently modified first, until they total no more than
/// `max_bytes`. A missing `dir` sweeps nothing.
pub fn sweep_image_cache(dir: &Path, max_bytes: u64) -> std::io::Result<SweepReport> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SweepReport::default()),
        Err(e) => return Err(e),
    };
    let mut report = SweepReport::default();
    // The copies kept so far, as (last modified, path, length)
    let mut copies = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(file_type) = present(entry.file_type())? else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let Some(meta) = present(entry.metadata())? else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        if is_temp_name(name) {
            if file_age(&meta).is_some_and(|age| age > STALE_AFTER) {
                remove_counted(&path, meta.len(), &mut report)?;
            }
        } else if is_copy_name(name) {
            copies.push((meta.modified()?, path, meta.len()));
        } else {
            remove_counted(&path, meta.len(), &mut report)?;
        }
    }

    // Trim the copies to the cap, the least recently modified first
    copies.sort_by_key(|copy| copy.0);
    report.kept_bytes = copies.iter().map(|copy| copy.2).sum();
    for (_, path, len) in copies {
        if report.kept_bytes <= max_bytes {
            break;
        }
        remove_counted(&path, len, &mut report)?;
        report.kept_bytes -= len;
    }
    Ok(report)
}

/// A result for a file that may have vanished meanwhile: `None` when it is not found.
fn present<T>(result: std::io::Result<T>) -> std::io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Removes a file and counts it in `report`. A file that is already gone is skipped, not counted.
fn remove_counted(path: &Path, len: u64, report: &mut SweepReport) -> std::io::Result<()> {
    if present(std::fs::remove_file(path))?.is_some() {
        report.removed += 1;
        report.freed_bytes += len;
    }
    Ok(())
}

/// Whether a file name is a temporary file's: it starts with `.` and ends with `.tmp`.
fn is_temp_name(name: &[u8]) -> bool {
    name.starts_with(b".") && name.ends_with(b".tmp")
}

/// Whether a file name is a disk copy's, `{hash}_{stamp}_{width}x{height}.jpg`, which matches
/// `^[0-9a-f]{64}_[0-9a-f]{16}_[0-9]+x[0-9]+\.jpg$`.
fn is_copy_name(name: &[u8]) -> bool {
    let Some(stem) = name.strip_suffix(b".jpg") else {
        return false;
    };
    let mut parts = stem.split(|&b| b == b'_');
    let (Some(hash), Some(stamp), Some(size), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let mut dims = size.split(|&b| b == b'x');
    let (Some(width), Some(height), None) = (dims.next(), dims.next(), dims.next()) else {
        return false;
    };
    is_hex(hash, 64) && is_hex(stamp, 16) && is_decimal(width) && is_decimal(height)
}

/// Whether `part` is exactly `len` lower-case hex digits.
fn is_hex(part: &[u8], len: usize) -> bool {
    part.len() == len && part.iter().all(|&b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether `part` is one or more decimal digits.
fn is_decimal(part: &[u8]) -> bool {
    !part.is_empty() && part.iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    // B27: cache keys

    /// Removes a test's directory when the test ends, pass or fail.
    struct RemoveOnDrop(std::path::PathBuf);

    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The djb2 hash that named resized images before B27.
    fn djb2(s: &str) -> u64 {
        s.bytes().fold(5381, |hash: u64, b| {
            hash.wrapping_mul(33).wrapping_add(u64::from(b))
        })
    }

    /// Writes a 60x90 JPEG of one colour.
    fn write_poster(path: &Path, rgb: [u8; 3]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbImage::from_pixel(60, 90, image::Rgb(rgb))
            .save(path)
            .unwrap();
    }

    /// The mean colour of an image.
    fn mean_rgb(bytes: &[u8]) -> [u64; 3] {
        let img = image::load_from_memory(bytes).unwrap().to_rgb8();
        let mut sum = [0u64; 3];
        for pixel in img.pixels() {
            for (total, channel) in sum.iter_mut().zip(pixel.0) {
                *total += u64::from(channel);
            }
        }
        let pixels = u64::from(img.width()) * u64::from(img.height());
        sum.map(|total| total / pixels)
    }

    #[tokio::test]
    async fn b27_resized_images_of_paths_that_collide_under_djb2_are_kept_apart() {
        let root = std::env::temp_dir().join(format!("rustyfin_b27_{}", uuid::Uuid::new_v4()));
        let _remove = RemoveOnDrop(root.clone());
        let red = root.join("media").join("ab").join("poster.jpg");
        let blue = root.join("media").join("bA").join("poster.jpg");
        write_poster(&red, [255, 0, 0]);
        write_poster(&blue, [0, 0, 255]);
        let (red, blue) = (red.to_str().unwrap(), blue.to_str().unwrap());
        // "ab" and "bA" add the same to djb2, so the two paths had one name.
        assert_eq!(djb2(red), djb2(blue));
        let cache_dir = root.join("cache");

        let red_small = get_or_resize_image(red, Some(8), None, &cache_dir, None)
            .await
            .unwrap();
        let blue_small = get_or_resize_image(blue, Some(8), None, &cache_dir, None)
            .await
            .unwrap();
        let [r, _, b] = mean_rgb(&red_small);
        assert!(r > 200 && b < 60, "ab's poster came back {:?}", [r, b]);
        let [r, _, b] = mean_rgb(&blue_small);
        assert!(b > 200 && r < 60, "bA's poster came back {:?}", [r, b]);

        // Each has its own file, named by a full hash (64 hex digits), the source's stamp (none
        // here) and the size it holds...
        let mut names: Vec<String> = std::fs::read_dir(&cache_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "{names:?}");
        for name in &names {
            let (hash, size) = name.split_once('_').unwrap_or((name, ""));
            assert!(
                hash.len() == 64 && hash.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')),
                "{name}"
            );
            assert_eq!(size, format!("{}_8x12.jpg", "0".repeat(16)), "{name}");
        }
        // ...from which each is served again.
        let again = get_or_resize_image(red, Some(8), None, &cache_dir, None).await;
        assert_eq!(again.unwrap(), red_small);
        let again = get_or_resize_image(blue, Some(8), None, &cache_dir, None).await;
        assert_eq!(again.unwrap(), blue_small);
    }

    // B30: image and search bounds

    #[test]
    fn b30_p1_target_size_never_upscales_and_caps_each_side() {
        // (source, maxWidth, maxHeight, the size served)
        let cases = [
            ((1000, 1500), Some(300), None, (300, 450)),
            ((1000, 1500), None, Some(300), (200, 300)),
            ((1000, 1500), Some(300), Some(300), (200, 300)),
            ((1920, 1080), Some(300), Some(300), (300, 169)),
            // Bounds at or above the source never upscale it.
            ((1000, 1500), Some(40000), None, (1000, 1500)),
            ((1000, 1500), Some(40000), Some(40000), (1000, 1500)),
            ((1000, 1500), Some(1000), None, (1000, 1500)),
            // No side above 4096, whatever is asked.
            ((8000, 12000), Some(40000), None, (2731, 4096)),
            ((12000, 8000), None, Some(40000), (4096, 2731)),
            ((8000, 12000), Some(5000), None, (2731, 4096)),
            ((4096, 4096), Some(4096), None, (4096, 4096)),
            ((4097, 1), Some(40000), None, (4096, 1)),
            ((1000, 5000), Some(1000), None, (819, 4096)),
            ((10, 5000), Some(5000), None, (8, 4096)),
            ((u32::MAX, 1), Some(u32::MAX), None, (4096, 1)),
            // Halves round up, and a side never rounds to 0.
            ((4, 2), Some(3), None, (3, 2)),
            ((5000, 10), Some(300), None, (300, 1)),
            ((3, 2), Some(1), None, (1, 1)),
            ((1000, 1), Some(10), None, (10, 1)),
            // No bound, or 0, leaves that side free.
            ((1000, 1500), None, None, (1000, 1500)),
            ((1000, 1500), Some(0), Some(0), (1000, 1500)),
            ((1000, 1500), Some(0), Some(300), (200, 300)),
            ((0, 0), Some(300), None, (0, 0)),
        ];
        for ((width, height), max_width, max_height, served) in cases {
            assert_eq!(
                target_size(width, height, max_width, max_height),
                served,
                "{width}x{height} with maxWidth {max_width:?}, maxHeight {max_height:?}"
            );
        }
    }

    #[test]
    fn b30_p1_rendition_for_maps_oversize_requests_to_one_rendition() {
        let poster = Some((1000, 1500));
        // At or above the source's size: the original, whatever the bounds.
        assert_eq!(
            rendition_for(poster, Some(40000), None),
            Rendition::Original
        );
        assert_eq!(
            rendition_for(poster, Some(1000), Some(1500)),
            Rendition::Original
        );
        assert_eq!(rendition_for(poster, None, None), Rendition::Original);
        assert_eq!(rendition_for(poster, Some(0), Some(0)), Rendition::Original);
        // Below it: a copy of the size served, so bounds that give one size share it.
        let copy = Rendition::Copy {
            width: 300,
            height: 450,
        };
        assert_eq!(rendition_for(poster, Some(300), None), copy);
        assert_eq!(rendition_for(poster, None, Some(450)), copy);
        assert_eq!(rendition_for(poster, Some(300), Some(450)), copy);
        assert_eq!(rendition_for(poster, Some(300), Some(40000)), copy);
        // An unknown size: a copy within the bounds, each capped at 4096.
        let capped = Rendition::Copy {
            width: 4096,
            height: 4096,
        };
        assert_eq!(rendition_for(None, Some(40000), None), capped);
        let narrow = Rendition::Copy {
            width: 300,
            height: 4096,
        };
        assert_eq!(rendition_for(None, Some(300), None), narrow);
        assert_eq!(rendition_for(None, None, None), Rendition::Original);
        // The key names a rendition in the memory cache.
        assert_eq!(Rendition::Original.key(), "0x0");
        assert_eq!(copy.key(), "300x450");
    }

    #[tokio::test]
    async fn b30_p1_a_copy_has_the_size_planned_for_it() {
        let root = std::env::temp_dir().join(format!("rustyfin_b30_{}", uuid::Uuid::new_v4()));
        let _remove = RemoveOnDrop(root.clone());
        std::fs::create_dir_all(&root).unwrap();
        let cache_dir = root.join("cache");
        // A banner and a tall poster: fitting them to the planned box again, with both bounds,
        // came out a few pixels short of it.
        for ((width, height), max_width, max_height, planned) in [
            ((758, 140), Some(300), None, (300, 55)),
            ((140, 758), None, Some(300), (55, 300)),
        ] {
            let path = root.join(format!("{width}x{height}.jpg"));
            image::RgbImage::from_pixel(width, height, image::Rgb([255, 0, 0]))
                .save(&path)
                .unwrap();
            let path = path.to_str().unwrap();
            assert_eq!(
                rendition_for(Some((width, height)), max_width, max_height),
                Rendition::Copy {
                    width: planned.0,
                    height: planned.1,
                }
            );
            let bytes = get_or_resize_image(path, max_width, max_height, &cache_dir, None)
                .await
                .unwrap();
            let served = image::load_from_memory(&bytes).unwrap();
            assert_eq!(
                (served.width(), served.height()),
                planned,
                "{width}x{height}"
            );
        }
    }

    /// A test's own directory, removed when the test ends.
    fn b30_root() -> (std::path::PathBuf, RemoveOnDrop) {
        let root = std::env::temp_dir().join(format!("rustyfin_b30_{}", uuid::Uuid::new_v4()));
        (root.clone(), RemoveOnDrop(root))
    }

    /// The names in a folder, sorted.
    fn b30_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn b30_p2_stamp_changes_with_the_length_and_the_modified_time() {
        let (root, _remove) = b30_root();
        let poster = root.join("poster.jpg");
        write_poster(&poster, [255, 0, 0]);
        let first = read_image_stamp(&poster).unwrap();
        assert_eq!(first.size, Some((60, 90)));
        assert_eq!(read_image_stamp(&poster).unwrap(), first);

        // Another picture, of another length.
        image::RgbImage::from_pixel(30, 45, image::Rgb([0, 0, 255]))
            .save(&poster)
            .unwrap();
        let second = read_image_stamp(&poster).unwrap();
        assert_eq!(second.size, Some((30, 45)));
        assert_ne!(second.stamp, first.stamp);
        // The same length, modified later.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&poster)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let third = read_image_stamp(&poster).unwrap();
        assert_ne!(third.stamp, second.stamp);
        // A link reads as the file it points to.
        #[cfg(unix)]
        {
            let link = root.join("link.jpg");
            std::os::unix::fs::symlink(&poster, &link).unwrap();
            assert_eq!(read_image_stamp(&link).unwrap(), third);
        }

        // A file that is no picture has a stamp but no size; a missing one is an error.
        std::fs::write(&poster, b"no picture").unwrap();
        assert_eq!(read_image_stamp(&poster).unwrap().size, None);
        assert!(read_image_stamp(&root.join("missing.jpg")).is_err());
    }

    #[test]
    fn b30_p2_tag_and_copy_name_carry_the_stamp() {
        let path = "/media/Film/Heat (1995)/poster.jpg";
        let tag = image_tag(path, Some(42));
        assert!(
            tag.len() == 32 && tag.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')),
            "{tag}"
        );
        assert_eq!(image_tag(path, Some(42)), tag);
        assert_ne!(image_tag(path, Some(43)), tag);
        assert_ne!(
            image_tag("/media/Film/Heat (1995)/fanart.jpg", Some(42)),
            tag
        );
        assert_eq!(image_tag(path, None), image_tag(path, Some(0)));

        let name = cached_copy_name(path, Some(42), 300, 450);
        let parts: Vec<&str> = name.split('_').collect();
        assert_eq!(parts.len(), 3, "{name}");
        assert!(
            parts[0].len() == 64
                && parts[0]
                    .bytes()
                    .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')),
            "{name}"
        );
        assert_eq!(parts[1], "000000000000002a", "{name}");
        assert_eq!(parts[2], "300x450.jpg", "{name}");
        let negative = cached_copy_name(path, Some(-1), 300, 450);
        assert_eq!(negative.split('_').nth(1), Some("ffffffffffffffff"));
        assert_eq!(
            cached_copy_name(path, None, 300, 450),
            cached_copy_name(path, Some(0), 300, 450)
        );
        assert_ne!(
            cached_copy_name("/media/other.jpg", Some(42), 300, 450),
            name
        );
    }

    #[tokio::test]
    async fn b30_p2_render_image_names_its_copy_by_the_stamp() {
        let (root, _remove) = b30_root();
        let poster = root.join("media").join("poster.jpg");
        let cache_dir = root.join("cache");
        let path = poster.to_str().unwrap();
        let copy = Rendition::Copy {
            width: 8,
            height: 12,
        };
        write_poster(&poster, [255, 0, 0]);
        let red = render_image(path, Some(1), copy, &cache_dir, None)
            .await
            .unwrap();

        // Replaced in place: its new stamp names a new copy...
        write_poster(&poster, [0, 0, 255]);
        let blue = render_image(path, Some(2), copy, &cache_dir, None)
            .await
            .unwrap();
        let [r, _, b] = mean_rgb(&blue);
        assert!(
            b > 200 && r < 60,
            "the new stamp got the old copy: {:?}",
            [r, b]
        );
        // ...and the old stamp still names the old one.
        let again = render_image(path, Some(1), copy, &cache_dir, None).await;
        assert_eq!(again.unwrap(), red);
        let mut expected = vec![
            cached_copy_name(path, Some(1), 8, 12),
            cached_copy_name(path, Some(2), 8, 12),
        ];
        expected.sort();
        assert_eq!(b30_names(&cache_dir), expected);
    }

    /// Writes `len` bytes to `dir/name`, last modified `age` ago.
    fn b30_file(dir: &Path, name: &str, len: usize, age: std::time::Duration) {
        let path = dir.join(name);
        std::fs::write(&path, vec![0u8; len]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - age)
            .unwrap();
    }

    const B30_MINUTE: std::time::Duration = std::time::Duration::from_secs(60);
    const B30_HOUR: std::time::Duration = std::time::Duration::from_secs(3600);

    #[tokio::test]
    async fn b30_p4_an_empty_or_cut_short_copy_is_made_again() {
        let (root, _remove) = b30_root();
        let poster = root.join("media").join("poster.jpg");
        write_poster(&poster, [255, 0, 0]);
        let path = poster.to_str().unwrap();
        let cache_dir = root.join("cache");
        let copy = Rendition::Copy {
            width: 8,
            height: 12,
        };
        let first = render_image(path, Some(7), copy, &cache_dir, None)
            .await
            .unwrap();
        let file = cache_dir.join(cached_copy_name(path, Some(7), 8, 12));
        assert_eq!(std::fs::read(&file).unwrap(), first);

        for cut in [first.len() / 2, 0] {
            std::fs::write(&file, &first[..cut]).unwrap();
            let again = render_image(path, Some(7), copy, &cache_dir, None).await;
            assert_eq!(
                again.unwrap(),
                first,
                "a copy cut to {cut} bytes was served"
            );
            assert_eq!(
                std::fs::read(&file).unwrap(),
                first,
                "a copy cut to {cut} bytes was not made again"
            );
        }

        // A hit marks the copy as used, for the cap's oldest-first trim.
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - 2 * B30_HOUR)
            .unwrap();
        let again = render_image(path, Some(7), copy, &cache_dir, None).await;
        assert_eq!(again.unwrap(), first);
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert!(
            modified > std::time::SystemTime::now() - 10 * B30_MINUTE,
            "a hit left the copy's time as it was"
        );
        // Copies are written under a temporary name and renamed, so none is left over.
        assert_eq!(
            b30_names(&cache_dir),
            [cached_copy_name(path, Some(7), 8, 12)]
        );
    }

    #[test]
    fn b30_r5_sweep_removes_old_names_and_stale_temp_files() {
        let (root, _remove) = b30_root();
        let dir = root.join("images");
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        let (hash, stamp) = ("a".repeat(64), "0".repeat(16));
        let current = format!("{hash}_{stamp}_300x450.jpg");
        let other = format!("{}_ffffffffffffffff_8x12.jpg", "0123456789abcdef".repeat(4));
        let fresh_temp = format!(".{current}.{}.tmp", uuid::Uuid::new_v4().simple());
        let stale_temp = format!(".{other}.{}.tmp", uuid::Uuid::new_v4().simple());
        for name in [&current, &other, &fresh_temp] {
            b30_file(&dir, name, 10, B30_MINUTE);
        }
        b30_file(&dir, &stale_temp, 10, 2 * B30_HOUR);
        b30_file(&dir.join("folder"), "notes.txt", 10, B30_MINUTE);
        let old_names = [
            // djb2's names, before B27
            "12345678901234567890_poster_300x0.jpg".to_string(),
            // B27's names, without a stamp
            format!("{hash}_300x450.jpg"),
            // and other names no version writes
            format!("{}_{stamp}_300x450.jpg", "A".repeat(64)),
            format!("{}_{stamp}_300x450.jpg", "a".repeat(63)),
            format!("{hash}_{}_300x450.jpg", "0".repeat(15)),
            format!("{hash}_{stamp}_300x450.jpeg"),
            format!("{hash}_{stamp}_300x.jpg"),
            format!("{hash}_{stamp}_x450.jpg"),
            format!("{hash}_{stamp}_300x450.jpg.part"),
            "notes.txt".to_string(),
        ];
        for name in &old_names {
            b30_file(&dir, name, 10, B30_MINUTE);
        }

        let report = sweep_image_cache(&dir, IMAGE_CACHE_MAX_BYTES).unwrap();
        let removed = old_names.len() + 1;
        assert_eq!(
            (report.removed, report.freed_bytes, report.kept_bytes),
            (removed, 10 * removed as u64, 20)
        );
        let mut kept = vec![current, other, fresh_temp, "folder".to_string()];
        kept.sort();
        assert_eq!(b30_names(&dir), kept);
        assert!(dir.join("folder").join("notes.txt").exists());
    }

    #[test]
    fn b30_r5_sweep_trims_the_least_recently_used_copies_to_the_cap() {
        let (root, _remove) = b30_root();
        let dir = root.join("images");
        std::fs::create_dir_all(&dir).unwrap();
        let name = |side: u32| format!("{}_{}_{side}x{side}.jpg", "b".repeat(64), "0".repeat(16));
        b30_file(&dir, &name(1), 1000, 3 * B30_HOUR);
        b30_file(&dir, &name(2), 1000, 2 * B30_HOUR);
        b30_file(&dir, &name(3), 1000, B30_HOUR);
        // A copy being written counts toward no cap.
        let fresh_temp = format!(".{}.{}.tmp", name(4), uuid::Uuid::new_v4().simple());
        b30_file(&dir, &fresh_temp, 5000, B30_MINUTE);

        let report = sweep_image_cache(&dir, 2500).unwrap();
        assert_eq!(
            (report.removed, report.freed_bytes, report.kept_bytes),
            (1, 1000, 2000)
        );
        let mut kept = vec![name(2), name(3), fresh_temp];
        kept.sort();
        assert_eq!(b30_names(&dir), kept);
        // At the cap, nothing more goes.
        let report = sweep_image_cache(&dir, 2000).unwrap();
        assert_eq!(
            (report.removed, report.freed_bytes, report.kept_bytes),
            (0, 0, 2000)
        );
    }

    #[test]
    fn b30_r5_sweep_of_a_missing_folder_is_ok() {
        let (root, _remove) = b30_root();
        let report = sweep_image_cache(&root.join("images"), 0).unwrap();
        assert_eq!(
            (report.removed, report.freed_bytes, report.kept_bytes),
            (0, 0, 0)
        );
    }
}
