//! Shrink picked images and videos before they are staged for an agent.
//!
//! ACP embeds images as base64 and refuses anything past a native 5 MiB
//! encoded budget (~3.75 MiB of file bytes). Phone photos and screen
//! recordings sit well above that, so the App re-encodes before upload;
//! Desktop does the same here so a paste or file pick does not bounce.

use std::path::{Path, PathBuf};
use std::process::Command;

use image::imageops::FilterType;

/// Long edge after resize — same budget the App uses (`MAX_IMAGE_EDGE`).
const MAX_IMAGE_EDGE: u32 = 2048;
/// JPEG quality matching the App's 0.8 WebP setting (lossy WebP needs
/// libwebp; the `image` crate only ships a lossless WebP writer).
const IMAGE_QUALITY: u8 = 80;
/// Raw file bytes that still fit under ACP's 5 MiB base64 ceiling.
const ACP_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024 / 4 * 3;
/// Videos below this are left alone (App's `VIDEO_COMPRESS_MIN_BYTES`).
const VIDEO_COMPRESS_MIN_BYTES: usize = 8 * 1024 * 1024;
/// Long edge for a re-encoded video (App's `VIDEO_MAX_EDGE`).
const VIDEO_MAX_EDGE: u32 = 1920;

/// Re-encode `bytes` when that shrinks an image or video for the agent.
///
/// Failures and "already fine" fall through to the original — compression
/// is an optimisation, never a gate that blocks the pick.
pub fn prepare_attachment(name: &str, bytes: Vec<u8>) -> (String, Vec<u8>) {
    if looks_like_gif(&bytes) {
        return (name.to_string(), bytes);
    }
    if looks_like_image(&bytes) {
        return compress_image(name, bytes);
    }
    if looks_like_video(&bytes) {
        return compress_video(name, bytes);
    }
    (name.to_string(), bytes)
}

fn compress_image(name: &str, bytes: Vec<u8>) -> (String, Vec<u8>) {
    let Ok(image) = image::load_from_memory(&bytes) else {
        return (name.to_string(), bytes);
    };
    let (w, h) = (image.width(), image.height());
    let long = w.max(h);
    let already_small = bytes.len() <= ACP_MAX_IMAGE_BYTES && long <= MAX_IMAGE_EDGE;
    let already_lossy = looks_like_jpeg(&bytes) || looks_like_webp(&bytes);
    if already_small && already_lossy {
        return (name.to_string(), bytes);
    }

    let image = if long > MAX_IMAGE_EDGE {
        image.resize(MAX_IMAGE_EDGE, MAX_IMAGE_EDGE, FilterType::Triangle)
    } else {
        image
    };

    for quality in [IMAGE_QUALITY, 65, 50, 35] {
        match encode_jpeg(&image, quality) {
            Ok(out) if out.len() < bytes.len() || bytes.len() > ACP_MAX_IMAGE_BYTES => {
                if out.len() <= ACP_MAX_IMAGE_BYTES {
                    return (with_extension(name, "jpg"), out);
                }
            }
            Ok(_) | Err(_) => {}
        }
    }

    // Last resort: pin a shorter edge until it fits, still JPEG.
    let mut edge = MAX_IMAGE_EDGE / 2;
    while edge >= 512 {
        let scaled = image.resize(edge, edge, FilterType::Triangle);
        if let Ok(out) = encode_jpeg(&scaled, 50) {
            if out.len() <= ACP_MAX_IMAGE_BYTES {
                return (with_extension(name, "jpg"), out);
            }
        }
        edge /= 2;
    }

    (name.to_string(), bytes)
}

fn encode_jpeg(image: &image::DynamicImage, quality: u8) -> Result<Vec<u8>, image::ImageError> {
    let rgb = image.to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode_image(&rgb)?;
    Ok(out)
}

fn compress_video(name: &str, bytes: Vec<u8>) -> (String, Vec<u8>) {
    if bytes.len() < VIDEO_COMPRESS_MIN_BYTES {
        return (name.to_string(), bytes);
    }
    let Some(ffmpeg) = ffmpeg_bin() else {
        return (name.to_string(), bytes);
    };

    let stamp = std::process::id();
    let dir = std::env::temp_dir().join(format!("muqun-media-{stamp}"));
    if std::fs::create_dir_all(&dir).is_err() {
        return (name.to_string(), bytes);
    }
    let input = dir.join("in.bin");
    let output = dir.join("out.mp4");
    let cleanup = |paths: &[&Path]| {
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&dir);
    };

    if std::fs::write(&input, &bytes).is_err() {
        cleanup(&[&input, &output]);
        return (name.to_string(), bytes);
    }

    // Scale the long edge down to 1080p-ish; `-2` keeps the other even for H.264.
    let scale = format!("scale='min({VIDEO_MAX_EDGE},iw)':-2");
    let status = Command::new(ffmpeg)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&input)
        .args([
            "-vf",
            &scale,
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "28",
            "-c:a",
            "aac",
            "-movflags",
            "+faststart",
        ])
        .arg(&output)
        .status();

    let produced = match status {
        Ok(status) if status.success() => std::fs::read(&output).ok(),
        _ => None,
    };
    cleanup(&[&input, &output]);

    match produced {
        Some(out) if !out.is_empty() && out.len() < bytes.len() => {
            (with_extension(name, "mp4"), out)
        }
        _ => (name.to_string(), bytes),
    }
}

fn ffmpeg_bin() -> Option<PathBuf> {
    which("ffmpeg")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn with_extension(name: &str, ext: &str) -> String {
    let trimmed = name.trim();
    let stem = if trimmed.is_empty() {
        "file"
    } else {
        let path = Path::new(trimmed);
        path.file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(trimmed)
    };
    format!("{stem}.{ext}")
}

fn looks_like_gif(bytes: &[u8]) -> bool {
    bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")
}

fn looks_like_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\xff\xd8\xff")
}

fn looks_like_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP"
}

fn looks_like_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

fn looks_like_image(bytes: &[u8]) -> bool {
    looks_like_png(bytes)
        || looks_like_jpeg(bytes)
        || looks_like_webp(bytes)
        || looks_like_gif(bytes)
}

fn looks_like_video(bytes: &[u8]) -> bool {
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        // Still-image / audio HEIF brands are not re-encoded as video.
        return !matches!(
            brand,
            b"heic"
                | b"heix"
                | b"hevc"
                | b"hevx"
                | b"heim"
                | b"heis"
                | b"mif1"
                | b"msf1"
                | b"M4A "
                | b"M4B "
                | b"M4P "
        );
    }
    // EBML (WebM / Matroska).
    bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder as _;

    #[test]
    fn extension_swap_keeps_stem() {
        assert_eq!(with_extension("shot.PNG", "jpg"), "shot.jpg");
        assert_eq!(with_extension("noext", "mp4"), "noext.mp4");
    }

    #[test]
    fn small_jpeg_passes_through() {
        // 2×2 solid red JPEG — already under every budget.
        let mut bytes = Vec::new();
        {
            let img = image::RgbImage::from_pixel(2, 2, image::Rgb([200, 40, 40]));
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90)
                .encode(img.as_raw(), 2, 2, image::ExtendedColorType::Rgb8)
                .unwrap();
        }
        let (name, out) = prepare_attachment("tiny.jpg", bytes.clone());
        assert_eq!(name, "tiny.jpg");
        assert_eq!(out, bytes);
    }

    #[test]
    fn large_png_becomes_jpeg_under_acp_budget() {
        // Oversized PNG so a re-encode is required.
        let img = image::RgbImage::from_fn(2400, 1600, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        assert!(png.len() > ACP_MAX_IMAGE_BYTES || img.width().max(img.height()) > MAX_IMAGE_EDGE);

        let (name, out) = prepare_attachment("photo.png", png);
        assert_eq!(name, "photo.jpg");
        assert!(looks_like_jpeg(&out));
        assert!(out.len() <= ACP_MAX_IMAGE_BYTES);
        let decoded = image::load_from_memory(&out).unwrap();
        assert!(decoded.width().max(decoded.height()) <= MAX_IMAGE_EDGE);
    }
}
