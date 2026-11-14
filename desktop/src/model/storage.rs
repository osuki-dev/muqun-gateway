//! What the desktop keeps on disk, for Settings → Storage: the state
//! directory's folders measured, and the rebuildable caches cleared.

use std::path::{Path, PathBuf};

/// Bytes per folder of the state directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageReport {
    /// Theme gallery card pictures, the library's thumbnails and timeline
    /// pictures fetched from the gateway (rebuilt on demand).
    pub previews: u64,
    /// The gallery index cache (refetched when stale).
    pub gallery: u64,
    /// Copies of files attached to prompts.
    pub uploads: u64,
    /// The session store and everything else under the state directory.
    pub other: u64,
    pub state_dir: Option<PathBuf>,
}

impl StorageReport {
    /// What [`clear_caches`] removes.
    pub fn cache(&self) -> u64 {
        self.previews + self.gallery
    }

    pub fn total(&self) -> u64 {
        self.previews + self.gallery + self.uploads + self.other
    }
}

/// `1.2 MB`, `640 KB`, `12 B`.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

const PREVIEWS: &str = "theme-previews";
const GALLERY: &str = "theme-gallery";
const UPLOADS: &str = "uploads";

/// Measure the state directory. Blocking: call off the UI thread.
pub fn scan() -> StorageReport {
    let Ok((_, state)) = crate::platform::directories() else {
        return StorageReport::default();
    };
    let previews = dir_size(&state.join(PREVIEWS)) + dir_size(&state.join(super::media::DIR));
    let gallery = dir_size(&state.join(GALLERY));
    let uploads = dir_size(&state.join(UPLOADS));
    let total = dir_size(&state);
    StorageReport {
        previews,
        gallery,
        uploads,
        other: total.saturating_sub(previews + gallery + uploads),
        state_dir: Some(state),
    }
}

/// Remove the rebuildable caches (previews, gallery index). Blocking.
pub fn clear_caches() {
    let Ok((_, state)) = crate::platform::directories() else {
        return;
    };
    for name in [PREVIEWS, GALLERY, super::media::DIR] {
        let dir = state.join(name);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let _ = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_as_people_write_them() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1_200), "1.2 KB");
        assert_eq!(human(20_000_000), "20 MB");
        assert_eq!(human(9_500_000_000), "9.5 GB");
    }
}
