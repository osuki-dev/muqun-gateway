//! Read images on the Bun worker before its private, per-view directory is removed.
use base64::Engine;
use std::{
    collections::HashMap,
    fs,
    io::{self, Read},
    path::Path,
    sync::Arc,
};

const FILE_LIMIT: u64 = 8 * 1024 * 1024;
const VIEW_BYTES: usize = 16 * 1024 * 1024;
const VIEW_PIXELS: u64 = 32_000_000;

#[derive(Clone, Debug)]
pub(super) struct Picture {
    pub bytes: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

pub(super) type Images = HashMap<String, Picture>;

pub(super) fn invocation_dir() -> io::Result<tempfile::TempDir> {
    let root = dirs::cache_dir()
        .ok_or(io::ErrorKind::NotFound)?
        .join("muqun-desktop/plugin-media");
    fs::create_dir_all(&root)?;
    tempfile::Builder::new().prefix("view-").tempdir_in(root)
}

fn source_bytes(src: &str, dir: &Path, limit: u64) -> Option<Vec<u8>> {
    if let Some(name) = src.strip_prefix("media:") {
        if name.is_empty()
            || name.len() > 128
            || name.starts_with('.')
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        {
            return None;
        }
        let path = dir.join(name);
        if !path.symlink_metadata().ok()?.is_file() {
            return None;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > limit {
            return None;
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(limit + 1).read_to_end(&mut bytes).ok()?;
        return (bytes.len() as u64 <= limit).then_some(bytes);
    }
    let (mime, data) = src.split_once(',')?;
    if !matches!(
        mime,
        "data:image/jpeg;base64" | "data:image/png;base64" | "data:image/webp;base64"
    ) || data.len() as u64 > limit.div_ceil(3) * 4
    {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

pub(super) fn read_view(output: &str, dir: &Path) -> Images {
    let mut images = Images::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return images;
    };
    let mut nodes = vec![&value["view"]];
    let mut remaining_bytes = VIEW_BYTES;
    let mut remaining_pixels = VIEW_PIXELS;
    while let Some(node) = nodes.pop() {
        if let Some(children) = node["children"].as_array() {
            nodes.extend(children.iter().rev());
        }
        if node["type"] != "image" {
            continue;
        }
        let Some(src) = node["src"].as_str() else {
            continue;
        };
        if images.contains_key(src) {
            continue;
        }
        let Some(bytes) = source_bytes(src, dir, FILE_LIMIT.min(remaining_bytes as u64)) else {
            continue;
        };
        let Ok(
            format
            @ (image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP),
        ) = image::guess_format(&bytes)
        else {
            continue;
        };
        let Ok((width, height)) =
            image::ImageReader::with_format(io::Cursor::new(&bytes), format).into_dimensions()
        else {
            continue;
        };
        let pixels = u64::from(width) * u64::from(height);
        if pixels == 0 || pixels > 12_000_000.min(remaining_pixels) {
            continue;
        }
        remaining_bytes -= bytes.len();
        remaining_pixels -= pixels;
        images.insert(
            src.to_owned(),
            Picture {
                bytes: bytes.into(),
                width,
                height,
            },
        );
    }
    images
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_bytes_survive_the_action_but_other_actions_and_unreferenced_files_stay_private() {
        let dir = tempfile::tempdir().unwrap();
        let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j+T8AAAAASUVORK5CYII=";
        fs::write(
            dir.path().join("picture.png"),
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
        )
        .unwrap();
        let adjacent = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            dir.path().join("picture.png"),
            adjacent.path().join("linked.png"),
        )
        .unwrap();
        let view = serde_json::json!({"view": {"type":"column","children":[
            {"type":"image","src":"media:picture.png"},
            {"type":"image","src":"media:../picture.png"},
            {"type":"image","src":"media:linked.png"},
            {"type":"image","src":"https://example.com/picture.png"}
        ]}})
        .to_string();
        assert!(read_view(&view, adjacent.path()).is_empty());
        assert!(read_view(r#"{"view":{"type":"text","text":"unused"}}"#, dir.path()).is_empty());
        let images = read_view(&view, dir.path());
        drop(dir);
        assert_eq!(images.len(), 1);
        let img = &images["media:picture.png"];
        assert_eq!((img.width, img.height), (1, 1));
        assert!(!img.bytes.is_empty());
    }
}
