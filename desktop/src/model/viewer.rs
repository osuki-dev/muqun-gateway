//! What the file viewer is asked to open, what kind of file it is, and its
//! contents -- everything but drawing it (`ui/viewer.rs`).
//!
//! Reading runs off the UI thread ([`spawn_load`]); the answer comes back as
//! [`UiAction::ViewerLoaded`](crate::model::inbox::UiAction::ViewerLoaded).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use muqun_gateway::agents::message_images::resolve_host_media;
use muqun_gateway::agents::MessageImageAsset;

use crate::model::inbox::UiAction;

/// The most of a text file the viewer reads (and draws): 512 KiB. Past it the
/// viewer says so and offers the system app.
pub const MAX_TEXT_BYTES: u64 = 512 * 1024;

/// A file to show: a path on this computer, or a file the gateway presented
/// (`files.present`, a timeline image or file link) with the session folder
/// it is relative to.
#[derive(Debug, Clone)]
pub enum FileRef {
    Path(PathBuf),
    /// Built by the timeline's file links and `files.present` cards.
    #[allow(dead_code)]
    Asset {
        asset: Box<MessageImageAsset>,
        directory: Option<String>,
    },
}

impl From<PathBuf> for FileRef {
    fn from(path: PathBuf) -> Self {
        Self::Path(path)
    }
}

impl FileRef {
    /// The local file this names, if it is on this computer.
    pub fn resolve(&self) -> Option<PathBuf> {
        match self {
            Self::Path(path) => {
                let path = expand_home(path);
                path.is_file()
                    .then(|| std::fs::canonicalize(&path).unwrap_or(path))
            }
            Self::Asset { asset, directory } => [
                Some(asset.url.as_str()),
                asset.download_url.as_deref(),
                asset.preview_url.as_deref(),
                Some(asset.src.as_str()),
            ]
            .into_iter()
            .flatten()
            .filter(|src| !src.is_empty())
            .find_map(|src| resolve_host_media(src, directory.as_deref())),
        }
    }

    /// What to call it before (or without) a local file.
    pub fn display_name(&self) -> String {
        let raw = match self {
            Self::Path(path) => path.to_string_lossy().into_owned(),
            Self::Asset { asset, .. } => asset.src.clone(),
        };
        let trimmed = raw.trim_end_matches(['/', '\\']);
        let name = trimmed
            .rsplit(['/', '\\'])
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or(trimmed);
        name.split('?').next().unwrap_or(name).to_string()
    }

    /// The MIME type the gateway gave it, if any.
    pub fn mime(&self) -> Option<&str> {
        match self {
            Self::Path(_) => None,
            Self::Asset { asset, .. } => Some(asset.mime.as_str()).filter(|m| !m.is_empty()),
        }
    }
}

fn expand_home(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => dirs::home_dir().map_or_else(|| path.to_path_buf(), |home| home.join(rest)),
        Err(_) => path.to_path_buf(),
    }
}

/// How the viewer draws a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Text, highlighted when the language is one the kit colours.
    Text(Option<&'static str>),
    Markdown,
    Image,
    Video,
    Audio,
    Pdf,
    /// Anything else: metadata and the system app.
    Other,
}

impl FileKind {
    pub fn reads_text(self) -> bool {
        matches!(self, Self::Text(_) | Self::Markdown)
    }
}

/// The kind of `path`, from its name, else the gateway's MIME type, else
/// [`FileKind::Other`] (the loader still sniffs an unknown file for text).
pub fn kind_of(path: &Path, mime: Option<&str>) -> FileKind {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    if let Some(kind) = kind_by_extension(&name, ext) {
        return kind;
    }
    match mime.map(|m| m.split(';').next().unwrap_or(m).trim()) {
        Some(m) if m.starts_with("image/") => FileKind::Image,
        Some(m) if m.starts_with("video/") => FileKind::Video,
        Some(m) if m.starts_with("audio/") => FileKind::Audio,
        Some("application/pdf") => FileKind::Pdf,
        Some("text/markdown") => FileKind::Markdown,
        Some(m) if m.starts_with("text/") || m == "application/json" => FileKind::Text(None),
        _ => FileKind::Other,
    }
}

fn kind_by_extension(name: &str, ext: &str) -> Option<FileKind> {
    let code = |lang| Some(FileKind::Text(Some(lang)));
    match ext {
        "md" | "markdown" | "mdx" => Some(FileKind::Markdown),
        "png" | "jpg" | "jpeg" | "webp" | "svg" | "svgz" => Some(FileKind::Image),
        "mp4" | "mov" | "m4v" | "webm" | "mkv" | "avi" => Some(FileKind::Video),
        "mp3" | "wav" | "ogg" | "oga" | "opus" | "flac" | "m4a" | "aac" => Some(FileKind::Audio),
        "pdf" => Some(FileKind::Pdf),
        "rs" => code("rust"),
        "js" | "mjs" | "cjs" => code("javascript"),
        "jsx" => code("jsx"),
        "ts" | "mts" | "cts" => code("typescript"),
        "tsx" => code("tsx"),
        "json" | "jsonc" | "json5" => code("json"),
        "py" | "pyi" => code("python"),
        "go" => code("go"),
        "swift" => code("swift"),
        "sh" | "bash" | "zsh" | "fish" => code("shell"),
        "sql" => code("sql"),
        "html" | "htm" => code("html"),
        "xml" | "plist" | "xhtml" => code("xml"),
        "css" | "scss" => code("css"),
        "yaml" | "yml" => code("yaml"),
        "diff" | "patch" => code("diff"),
        "txt" | "log" | "toml" | "ini" | "cfg" | "conf" | "csv" | "tsv" | "c" | "h" | "cc"
        | "cpp" | "hpp" | "java" | "kt" | "rb" | "lua" | "zig" | "nix" | "lock" | "env"
        | "gitignore" => Some(FileKind::Text(None)),
        _ => match name {
            "dockerfile" | "makefile" | "justfile" | "license" | "readme" | "changelog" => {
                Some(FileKind::Text(None))
            }
            _ => None,
        },
    }
}

/// Size and modification time, when the file system answers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Meta {
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

impl Meta {
    pub fn of(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(m) => Self {
                size: Some(m.len()),
                modified: m.modified().ok(),
            },
            Err(_) => Self::default(),
        }
    }
}

/// A text file's contents as the loader read them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    Text {
        text: Arc<str>,
        /// Only the first [`MAX_TEXT_BYTES`] were read.
        truncated: bool,
    },
    /// Not text after all (NUL bytes, or not UTF-8): metadata and the system app.
    Binary,
    Failed(String),
}

/// Read `path` as text, at most [`MAX_TEXT_BYTES`]. Blocking.
pub fn load_text(path: &Path) -> Loaded {
    use std::io::Read as _;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => return Loaded::Failed(err.to_string()),
    };
    let mut bytes = Vec::new();
    if let Err(err) = file.take(MAX_TEXT_BYTES + 1).read_to_end(&mut bytes) {
        return Loaded::Failed(err.to_string());
    }
    let truncated = bytes.len() as u64 > MAX_TEXT_BYTES;
    bytes.truncate(MAX_TEXT_BYTES as usize);
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Loaded::Binary;
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        // A cut in the middle of a character at the cap is still text.
        Err(err) if truncated && err.utf8_error().error_len().is_none() => {
            let valid = err.utf8_error().valid_up_to();
            let mut bytes = err.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
        Err(_) => return Loaded::Binary,
    };
    Loaded::Text {
        text: Arc::from(text.replace("\r\n", "\n")),
        truncated,
    }
}

/// Read `path` on a thread of its own; the result is posted to the UI.
pub fn spawn_load(path: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("muqun-viewer".into())
        .spawn(move || {
            let loaded = load_text(&path);
            crate::platform::dialogs::post(UiAction::ViewerLoaded {
                path,
                loaded: Box::new(loaded),
            });
        });
    if let Err(err) = spawned {
        tracing::warn!("could not start the viewer's reader: {err}");
    }
}

/// `bytes` as people read a file size: `812 B`, `14.2 KB`, `3.1 MB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_drawn_by_what_they_are() {
        let cases: [(&str, Option<&str>, FileKind); 9] = [
            ("src/main.rs", None, FileKind::Text(Some("rust"))),
            ("README.md", None, FileKind::Markdown),
            ("Shot.PNG", None, FileKind::Image),
            ("clip.mov", None, FileKind::Video),
            ("talk.m4a", None, FileKind::Audio),
            ("spec.pdf", None, FileKind::Pdf),
            ("Makefile", None, FileKind::Text(None)),
            ("blob", Some("image/heic"), FileKind::Image),
            ("archive.tar.gz", None, FileKind::Other),
        ];
        for (name, mime, want) in cases {
            assert_eq!(kind_of(Path::new(name), mime), want, "{name}");
        }
    }

    #[test]
    fn text_is_read_up_to_the_cap_and_binary_is_refused() {
        let dir = std::env::temp_dir().join(format!("muqun-viewer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("a.txt");
        std::fs::write(&text, "one\r\ntwo\n").unwrap();
        assert_eq!(
            load_text(&text),
            Loaded::Text {
                text: Arc::from("one\ntwo\n"),
                truncated: false
            }
        );
        let big = dir.join("big.txt");
        std::fs::write(&big, "é".repeat(MAX_TEXT_BYTES as usize)).unwrap();
        match load_text(&big) {
            Loaded::Text { text, truncated } => {
                assert!(truncated);
                assert!(text.len() as u64 <= MAX_TEXT_BYTES);
            }
            other => panic!("{other:?}"),
        }
        let binary = dir.join("b.bin");
        std::fs::write(&binary, [0x7f, b'E', b'L', b'F', 0, 0, 1]).unwrap();
        assert_eq!(load_text(&binary), Loaded::Binary);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
