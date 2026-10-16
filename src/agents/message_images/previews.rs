//! Disposable, versioned WebP derivatives. Never rewrites or sweeps originals.
use std::io::{Cursor, Read as _, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use image::{ExtendedColorType, ImageFormat, ImageReader, Limits};

use super::originals::{digest, open_fenced, private_dir};

const MAX_PIXELS: u64 = 8_000_000;
const MAX_ALLOC: u64 = 64 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CACHE_FILES: usize = 256;
// One cache transaction/transform at a time: same-key misses are singleflight,
// with no unbounded map of locks. Runs only inside the bounded blocking worker.
static CACHE: Mutex<()> = Mutex::new(());

fn cache_dir() -> Option<PathBuf> {
    let parent = crate::state_dir().ok()?.join("message-images");
    private_dir(&parent).ok()?;
    let dir = parent.join("previews");
    private_dir(&dir).ok()?;
    // Recover only our interrupted derivative writes, never original files.
    for entry in std::fs::read_dir(&dir).ok()? {
        let entry = entry.ok()?;
        if entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".pending"))
            .is_some_and(|stem| uuid::Uuid::parse_str(stem).is_ok())
        {
            std::fs::remove_file(entry.path()).ok()?;
        }
    }
    Some(dir)
}

pub(super) fn supported(bytes: &[u8]) -> bool {
    match image::guess_format(bytes) {
        Ok(ImageFormat::Jpeg) => true,
        Ok(ImageFormat::Png) => {
            // APNG is intentionally original-only, rather than silently
            // replacing its animation with a static image.
            let mut offset = 8usize;
            while let Some(header) = bytes.get(offset..offset.saturating_add(8)) {
                if &header[4..8] == b"acTL" {
                    return false;
                }
                let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
                let Some(next) = offset
                    .checked_add(12)
                    .and_then(|offset| offset.checked_add(length))
                else {
                    return false;
                };
                offset = next;
            }
            true
        }
        Ok(ImageFormat::WebP) => static_webp_bounds(bytes),
        _ => false,
    }
}

fn static_webp_bounds(bytes: &[u8]) -> bool {
    let mut offset = 12usize;
    let mut frame = false;
    while let Some(header) = bytes.get(offset..offset.saturating_add(8)) {
        let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let Some(end) = offset
            .checked_add(8)
            .and_then(|start| start.checked_add(length))
        else {
            return false;
        };
        let Some(data) = bytes.get(offset + 8..end) else {
            return false;
        };
        let dimensions = match &header[..4] {
            b"ANIM" | b"ANMF" => return false,
            b"VP8X" if data.first().is_none_or(|flags| flags & 2 != 0) => return false,
            b"VP8 " => {
                let Some(data) = data.get(..10) else {
                    return false;
                };
                Some((
                    u32::from(u16::from_le_bytes([data[6], data[7]]) & 0x3fff),
                    u32::from(u16::from_le_bytes([data[8], data[9]]) & 0x3fff),
                ))
            }
            b"VP8L" => {
                let Some(bits) = data.get(1..5) else {
                    return false;
                };
                let bits = u32::from_le_bytes(bits.try_into().unwrap());
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            _ => None,
        };
        if let Some((width, height)) = dimensions {
            // Extended canvas dimensions alone are insufficient: inspect the
            // actual frame header before a codec can allocate its own buffer.
            if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
                return false;
            }
            frame = true;
        }
        let Some(next) = end.checked_add(length % 2) else {
            return false;
        };
        offset = next;
    }
    frame && offset == bytes.len()
}

struct BoundedOutput {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            return Err(std::io::Error::other("image derivative too large"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode(bytes: &[u8], edge: u32, limit: usize) -> Option<Vec<u8>> {
    if !supported(bytes) {
        return None;
    }
    let format = image::guess_format(bytes).ok()?;
    let reader = || {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(16_384);
        limits.max_image_height = Some(16_384);
        limits.max_alloc = Some(MAX_ALLOC);
        reader.limits(limits);
        reader
    };
    let (width, height) = reader().into_dimensions().ok()?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return None;
    }
    let decoded = reader().decode().ok()?;
    let resized = decoded
        .thumbnail(edge.min(width), edge.min(height))
        .to_rgba8();
    let mut output = BoundedOutput {
        bytes: Vec::new(),
        limit,
    };
    image::codecs::webp::WebPEncoder::new_lossless(&mut output)
        .encode(
            resized.as_raw(),
            resized.width(),
            resized.height(),
            ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(output.bytes)
}

fn cache_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("v1-") else {
        return false;
    };
    let Some((hash, suffix)) = rest.split_once('-') else {
        return false;
    };
    hash.len() == 64
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        && matches!(suffix, "384.webp" | "1600.webp")
}

fn evict(dir: &Path, incoming: u64) -> Option<()> {
    let mut entries = Vec::new();
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        if !cache_name(entry.file_name().to_str()?) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
        if !metadata.is_file() {
            continue;
        }
        total = total.saturating_add(metadata.len());
        entries.push((metadata.modified().ok()?, metadata.len(), entry.path()));
    }
    entries.sort_by_key(|entry| entry.0);
    let mut count = entries.len();
    for (_, size, path) in entries {
        if total.saturating_add(incoming) <= MAX_CACHE_BYTES && count < MAX_CACHE_FILES {
            break;
        }
        std::fs::remove_file(path).ok()?;
        total = total.saturating_sub(size);
        count -= 1;
    }
    (total.saturating_add(incoming) <= MAX_CACHE_BYTES && count < MAX_CACHE_FILES).then_some(())
}

pub(super) fn derivative(bytes: &[u8], preview: bool) -> Option<Vec<u8>> {
    let _transaction = CACHE.lock().ok()?;
    let (edge, limit) = if preview {
        (1600, 4 * 1024 * 1024)
    } else {
        (384, 1024 * 1024)
    };
    let dir = cache_dir()?;
    let path = dir.join(format!("v1-{}-{edge}.webp", digest(bytes)));
    if let Ok(mut file) = open_fenced(&path, &dir) {
        if file.metadata().ok()?.len() <= limit as u64 {
            let mut cached = Vec::new();
            (&mut file)
                .take(limit as u64 + 1)
                .read_to_end(&mut cached)
                .ok()?;
            if cached.len() <= limit
                && matches!(image::guess_format(&cached), Ok(ImageFormat::WebP))
            {
                return Some(cached);
            }
        }
        // Only a disposable derivative may be replaced if corrupt.
        std::fs::remove_file(&path).ok()?;
    }
    let encoded = encode(bytes, edge, limit)?;
    evict(&dir, encoded.len() as u64)?;
    let staging = dir.join(format!("{}.pending", uuid::Uuid::new_v4()));
    if crate::platform::uploads::write_upload_file(&staging, &encoded).is_err() {
        let _ = std::fs::remove_file(&staging);
        return None;
    }
    let published = std::fs::rename(&staging, path);
    let _ = std::fs::remove_file(staging);
    published.ok()?;
    Some(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_codecs_preserve_aspect_and_bound_output_with_explicit_unsupported_formats() {
        let image = image::RgbImage::from_fn(1200, 300, |x, y| image::Rgb([x as u8, y as u8, 71]));
        let mut jpeg = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut jpeg, ImageFormat::Jpeg)
            .unwrap();
        let webp = encode(jpeg.get_ref(), 384, 1024 * 1024).unwrap();
        let thumb = image::load_from_memory_with_format(&webp, ImageFormat::WebP).unwrap();
        assert_eq!((thumb.width(), thumb.height()), (384, 96));
        assert!(encode(&webp, 384, 1).is_none());
        assert!(encode(b"%PDF-1.7 fixture", 384, 1024).is_none());
        assert!(encode(b"GIF89a\x01\x00\x01\x00", 384, 1024).is_none());
        let mut animated = b"RIFF\x20\0\0\0WEBPVP8X\x0a\0\0\0\x02\0\0\0\x01\0\0\x01\0\0".to_vec();
        assert!(!supported(&animated));
        animated[20] = 0;
        assert!(!supported(&animated)); // no actual bounded frame
        let mut mismatch =
            b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0\0\0\0\0\0\0VP8L\x05\0\0\0\x2f".to_vec();
        mismatch.extend((16383u32 | (16383u32 << 14)).to_le_bytes());
        mismatch.push(0);
        assert!(!static_webp_bounds(&mismatch));
    }

    #[test]
    fn eviction_is_bounded_and_cannot_delete_originals_or_unrelated_files() {
        let root = crate::test_support::asset_test_dir("preview-eviction");
        let dir = root.join("previews");
        std::fs::create_dir(&dir).unwrap();
        let original = root.join("original.png");
        std::fs::write(&original, b"original").unwrap();
        let unrelated = dir.join("leave-me");
        std::fs::write(&unrelated, b"unrelated").unwrap();
        for index in 0..18 {
            let path = dir.join(format!(
                "v1-{}-1600.webp",
                digest(format!("{index}").as_bytes())
            ));
            std::fs::File::create(path)
                .unwrap()
                .set_len(4 * 1024 * 1024)
                .unwrap();
        }
        evict(&dir, 4 * 1024 * 1024).unwrap();
        let retained: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| cache_name(entry.file_name().to_str().unwrap()))
            .collect();
        assert!(
            retained
                .iter()
                .map(|entry| entry.metadata().unwrap().len())
                .sum::<u64>()
                + 4 * 1024 * 1024
                <= MAX_CACHE_BYTES
        );
        assert_eq!(std::fs::read(original).unwrap(), b"original");
        assert_eq!(std::fs::read(unrelated).unwrap(), b"unrelated");
        std::fs::remove_dir_all(root).unwrap();
    }
}
