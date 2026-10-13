//! Export only typed producer image attachments, never paths found in prose.
//! The authenticated transcript carries the bytes, so there is no reusable
//! filesystem URL or new file-read API. Keep reads and output bounded.
use std::io::Read;
use std::path::Path;

use base64::Engine as _;

pub(crate) const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_RESPONSE_IMAGE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn image_uri(source: &str, budget: &mut usize) -> Option<String> {
    let bytes = if source.starts_with("data:") {
        let (header, encoded) = source.split_once(',')?;
        if !header.starts_with("data:image/")
            || !header.ends_with(";base64")
            || encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4
        {
            return None;
        }
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
        let mut bytes = Vec::new();
        file.take((MAX_IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
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
    let size = encoded.len() + kind.mime.len() + 13;
    *budget = budget.checked_sub(size)?;
    Some(format!("data:{};base64,{encoded}", kind.mime))
}

/// Native Codex's mapper fills `attachments` only from image/localImage input
/// variants. The caller checks the owning adapter before calling this helper.
pub(crate) fn export_native(items: &mut [crate::agents::domain::TimelineItem]) {
    let mut budget = MAX_RESPONSE_IMAGE_BYTES;
    for item in items {
        if item.role != crate::agents::domain::TimelineRole::User {
            continue;
        }
        if let Some(sources) = item.attachments.take() {
            let exported: Vec<_> = sources
                .into_iter()
                .take(16)
                .filter_map(|source| {
                    if source.starts_with("https://") || source.starts_with("http://") {
                        // Already-renderable producer URLs require no host read.
                        Some(source)
                    } else {
                        image_uri(&source, &mut budget)
                    }
                })
                .collect();
            item.attachments = (!exported.is_empty()).then_some(exported);
        }
    }
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
