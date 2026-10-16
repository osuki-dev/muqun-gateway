//! Bounded producer-image helpers. Terminal history retains its small inline
//! export policy. Service messages admit typed inline references separately;
//! message_images normalizes those to authorized original URLs at egress.
use std::io::Read;
use std::path::Path;

use base64::Engine as _;

pub(crate) const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_RESPONSE_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Service timelines admit references separately from the terminal's bounded
/// inline-image export. Full decode/persistence happens at blocking egress.
pub(crate) const MAX_PREVIEW_SOURCE_BYTES: usize = 25 * 1024 * 1024;
pub(crate) const MAX_REFERENCE_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
// Covers every sniffed image data-URI header and one bounded read lookahead.
// Charge attempts too: invalid inputs must not bypass the shared work limit.
const IMAGE_ATTEMPT_BYTES: usize = 32;

pub(crate) fn inline_reference(mime: &str, encoded: &str, budget: &mut usize) -> Option<String> {
    // Accounting is advisory for optional previews, never grounds to erase an
    // original reference. Full originals are decoded to disk in small chunks.
    *budget = budget.saturating_sub(encoded.len().saturating_add(IMAGE_ATTEMPT_BYTES));
    // This retains producer content, not an image classification. Actual MIME
    // and encoding validation belong to blocking egress; even invalid content
    // must retain its source with an explicit failure, not turn into a label.
    let token = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'+' | b'.'))
    };
    let safe = mime.len() <= 128
        && mime
            .split_once('/')
            .is_some_and(|(kind, subtype)| token(kind) && token(subtype));
    // A bad declaration cannot discard valid producer bytes or inject header
    // syntax. Egress sniffs the decoded file; octet-stream grants no image trust.
    let mime = if safe {
        mime
    } else {
        "application/octet-stream"
    };
    Some(format!("data:{mime};base64,{encoded}"))
}

fn reserve(budget: &mut usize, bytes: usize) -> Option<()> {
    match budget.checked_sub(bytes) {
        Some(remaining) => {
            *budget = remaining;
            Some(())
        }
        None => {
            *budget = 0;
            None
        }
    }
}

pub(crate) fn image_uri(source: &str, budget: &mut usize) -> Option<String> {
    // This allowance bounds both exported bytes and cumulative read/decode/
    // encode work. Reserve before allocation or I/O, and never refund failures.
    reserve(budget, IMAGE_ATTEMPT_BYTES)?;
    let bytes = if source.starts_with("data:") {
        let (header, encoded) = source.split_once(',')?;
        if !header.starts_with("data:image/")
            || !header.ends_with(";base64")
            || encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4
        {
            return None;
        }
        reserve(budget, encoded.len())?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?
    } else {
        let path = Path::new(source);
        if !path.is_absolute() {
            return None;
        }
        // Refuse symlinks and special files. O_NONBLOCK avoids waiting on a
        // FIFO even if it replaces a regular file between inspection/open.
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > MAX_IMAGE_BYTES as u64 {
            return None;
        }
        let length = metadata.len() as usize;
        reserve(budget, length.div_ceil(3) * 4)?;
        let mut bytes = Vec::new();
        file.take((length + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        // A file growing after metadata inspection cannot increase its reserved
        // read or encode allowance. Omit it instead of sampling a partial image.
        if bytes.len() > length {
            return None;
        }
        bytes
    };
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let kind = crate::platform::uploads::sniff_upload_kind(&bytes)?;
    if !kind.mime.starts_with("image/") {
        return None;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(format!("data:{};base64,{encoded}", kind.mime))
}

#[cfg(test)]
mod tests {
    use super::*;
    const IMAGE: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6qlsAAAAASUVORK5CYII=";

    #[test]
    fn attachment_export_checks_content_size_and_budget() {
        assert_eq!(image_uri(IMAGE, &mut 1024).as_deref(), Some(IMAGE));
        assert!(image_uri(IMAGE, &mut 1).is_none());
        assert!(image_uri("data:image/png;base64,c2VjcmV0", &mut 1024).is_none());
        assert!(image_uri("data:text/plain;base64,c2VjcmV0", &mut 1024).is_none());
        assert!(image_uri("relative.png", &mut 1024).is_none());
        let too_large = format!("data:image/png;base64,{}", "A".repeat(MAX_IMAGE_BYTES * 2));
        let mut budget = usize::MAX;
        assert!(image_uri(&too_large, &mut budget).is_none());
    }

    #[test]
    fn invalid_decode_and_sniff_attempts_exhaust_the_shared_work_allowance() {
        for source in [
            "data:image/png;base64,!!!!!!!!",
            "data:image/png;base64,c2VjcmV0",
        ] {
            let mut budget = 256;
            for _ in 0..16 {
                assert!(image_uri(source, &mut budget).is_none());
            }
            assert_eq!(budget, 0);
            // Failed attempts cannot leave the full output budget available
            // for an arbitrarily long sequence of expensive rejected images.
            assert!(image_uri(IMAGE, &mut budget).is_none());
        }
    }

    #[test]
    fn local_files_share_the_budget_even_when_content_is_invalid() {
        let root = std::env::temp_dir().join(format!("gateway-budget-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("image.png");
        let mut bytes = vec![0; MAX_IMAGE_BYTES];
        for valid in [false, true] {
            if valid {
                bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
            }
            std::fs::write(&path, &bytes).unwrap();
            let mut budget = MAX_RESPONSE_IMAGE_BYTES;
            let mut exported = 0;
            for attempt in 0..100 {
                if let Some(uri) = image_uri(path.to_str().unwrap(), &mut budget) {
                    assert!(valid);
                    exported += uri.len();
                }
                // Even rejected regular files exhaust the allowance by the
                // third attempt, and all remaining attempts stay empty.
                if attempt >= 2 {
                    assert_eq!(budget, 0);
                }
            }
            assert_eq!(exported > 0, valid);
            assert!(exported <= MAX_RESPONSE_IMAGE_BYTES);
            assert!(image_uri(IMAGE, &mut budget).is_none());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn typed_local_image_is_exported_but_text_files_and_symlinks_are_not() {
        let root = std::env::temp_dir().join(format!("gateway-image-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(IMAGE.split_once(',').unwrap().1)
            .unwrap();
        let image = root.join("image.png");
        std::fs::write(&image, bytes).unwrap();
        assert_eq!(
            image_uri(image.to_str().unwrap(), &mut 1024).as_deref(),
            Some(IMAGE)
        );
        let text = root.join("fake.png");
        std::fs::write(&text, b"not an image").unwrap();
        assert!(image_uri(text.to_str().unwrap(), &mut 1024).is_none());
        #[cfg(unix)]
        {
            let link = root.join("link.png");
            std::os::unix::fs::symlink(&image, &link).unwrap();
            assert!(image_uri(link.to_str().unwrap(), &mut 1024).is_none());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
