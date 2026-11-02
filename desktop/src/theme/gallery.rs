//! The published theme catalogue (`app/src/theme/gallery.ts`), read from
//! the same address the App and muqun.dev read.
//!
//! Network rules, all on the tokio side:
//!
//! * `index.json`: at most [`THEME_INDEX_MAX_BYTES`], [`INDEX_TIMEOUT`]; kept
//!   on disk under `<state>/theme-gallery/` and served from there for
//!   [`INDEX_TTL`]; after that revalidated with the server's `ETag`
//!   (`If-None-Match` → `304` keeps the copy). A failed fetch falls back to
//!   the kept copy, marked stale, and says why.
//! * A package: screened onto the catalogue's origin ([`theme_package_url`]),
//!   at most [`PACKAGE_BYTES`] (checked against the index's `bytes`, the
//!   `Content-Length` and the bytes actually read), [`PACKAGE_TIMEOUT`],
//!   redirects only within the catalogue's base. Its SHA-256 is compared with
//!   the index's before anything is unpacked; a mismatch installs nothing.
//! * A preview: screened the same way, at most [`PREVIEW_MAX_BYTES`],
//!   decoded and cut to a [`THUMB_W`]×[`THUMB_H`] JPEG under
//!   `<state>/theme-previews/`, named by content so it is fetched once.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{ETAG, IF_NONE_MATCH};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::pack::PACKAGE_BYTES;

/// `https://muqun.dev/api/themes/`, with the trailing slash joins need.
pub const THEME_GALLERY_BASE: &str = "https://muqun.dev/api/themes/";
/// Bounds the index itself. The packages it points at carry their own limit.
pub const THEME_INDEX_MAX_BYTES: usize = 512 * 1024;
pub const PREVIEW_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const INDEX_TIMEOUT: Duration = Duration::from_secs(10);
pub const INDEX_TTL: Duration = Duration::from_secs(60 * 60);
/// The App allows 30 s; a 25 MiB package on a slow link needs longer.
pub const PACKAGE_TIMEOUT: Duration = Duration::from_secs(120);
pub const PREVIEW_TIMEOUT: Duration = Duration::from_secs(15);
pub const REDIRECTS: usize = 5;
pub const THUMB_W: u32 = 640;
pub const THUMB_H: u32 = 400;

/// One catalogue row, with the field names the site's gallery uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: Option<String>,
    pub license: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    /// Relative to the catalogue base, e.g. `dist/<id>.muqun-theme`.
    pub package: String,
    pub bytes: u64,
    pub sha256: Option<String>,
    pub assets: Option<i64>,
    /// Relative to the base, e.g. `dist/previews/<id>.webp`.
    pub preview: Option<String>,
}

fn unreadable() -> anyhow::Error {
    anyhow::anyhow!("The theme catalogue could not be read")
}

/// `parseThemeIndex`: checked rather than trusted, and one bad row does not
/// lose the rest.
pub fn parse_theme_index(text: &str) -> anyhow::Result<Vec<IndexEntry>> {
    let document: serde_json::Value = serde_json::from_str(text).map_err(|_| unreadable())?;
    if document.get("format").and_then(|f| f.as_str()) != Some("muqun-themes-index") {
        return Err(unreadable());
    }
    let themes = document
        .get("themes")
        .and_then(|t| t.as_array())
        .ok_or_else(unreadable)?;
    let text = |row: &serde_json::Value, key: &str, max: usize| {
        row.get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty() && s.encode_utf16().count() <= max)
            .map(str::to_string)
    };
    let mut entries = Vec::new();
    for row in themes {
        if !row.is_object() {
            continue;
        }
        let (Some(id), Some(name), Some(version), Some(package)) = (
            text(row, "id", 64),
            text(row, "name", 64),
            text(row, "version", 32),
            text(row, "package", 512),
        ) else {
            continue;
        };
        let Some(bytes) = row
            .get("bytes")
            .and_then(|b| b.as_f64())
            .filter(|b| b.is_finite() && *b >= 0.0)
        else {
            continue;
        };
        let tags = row
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|t| {
                t.iter()
                    .filter_map(|tag| tag.as_str().map(str::to_string))
                    .take(12)
                    .collect()
            })
            .unwrap_or_default();
        entries.push(IndexEntry {
            id,
            name,
            version,
            author: text(row, "author", 100),
            license: text(row, "license", 100),
            description: text(row, "description", 280),
            tags,
            package,
            bytes: bytes as u64,
            sha256: row
                .get("sha256")
                .and_then(|s| s.as_str())
                .filter(|s| {
                    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                })
                .map(str::to_string),
            assets: row
                .get("assets")
                .and_then(|a| a.as_f64())
                .filter(|a| a.fract() == 0.0 && a.is_finite())
                .map(|a| a as i64),
            preview: text(row, "preview", 2048),
        });
    }
    Ok(entries)
}

/// `publicThemeUrl`: an absolute public `https://` address, nothing else.
pub fn public_theme_url(value: &str, base: Option<&str>) -> anyhow::Result<Url> {
    let refuse = || anyhow::anyhow!("Use a public HTTPS theme download link");
    if value.len() > 2048
        || value
            .chars()
            .any(|c| c.is_whitespace() || c == '\\' || c < ' ' || c == '\x7f')
    {
        return Err(refuse());
    }
    let mut url = match base {
        Some(base) => Url::parse(base).and_then(|b| b.join(value)),
        None => Url::parse(value),
    }
    .map_err(|_| refuse())?;
    let host = url
        .host_str()
        .unwrap_or("")
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_string();
    let label_ok = |l: &str| {
        !l.is_empty()
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let reserved = [
        "localhost",
        "local",
        "internal",
        "home",
        "lan",
        "onion",
        "invalid",
        "test",
    ];
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|p| p != 443)
        || !host.contains('.')
        || host.contains(':')
        || host.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        || !host.split('.').all(label_ok)
        || reserved
            .iter()
            .any(|r| host == *r || host.ends_with(&format!(".{r}")))
    {
        return Err(refuse());
    }
    url.set_fragment(None);
    Ok(url)
}

/// `themePackageUrl`: the package's address, which must stay on `base`.
pub fn theme_package_url(entry: &IndexEntry, base: &str) -> anyhow::Result<Url> {
    let url = public_theme_url(&entry.package, Some(base))?;
    anyhow::ensure!(
        url.as_str().starts_with(base),
        "Theme is not on the catalogue"
    );
    Ok(url)
}

/// `themePreviewUrl`: the same screen, but a cover that fails it costs a
/// placeholder, not the row.
pub fn theme_preview_url(entry: &IndexEntry, base: &str) -> Option<Url> {
    let url = public_theme_url(entry.preview.as_deref()?, Some(base)).ok()?;
    url.as_str().starts_with(base).then_some(url)
}

/// `Ok(true)` when `expected` is given and matches; `Ok(false)` when the
/// catalogue gave none (installable, but unverified); an error on mismatch.
pub fn verify_sha256(bytes: &[u8], expected: Option<&str>) -> anyhow::Result<bool> {
    let Some(expected) = expected else {
        return Ok(false);
    };
    let actual = hex::encode(sha2::Sha256::digest(bytes));
    anyhow::ensure!(
        actual == expected,
        "The download does not match the catalogue's SHA-256 (expected {}…, got {}…); nothing was installed",
        &expected[..12.min(expected.len())],
        &actual[..12]
    );
    Ok(true)
}

/// A card picture: `bytes` decoded, cut to fill 8:5, flattened onto white,
/// as a JPEG -- small enough that drawing a grid of them costs the UI thread
/// nothing noticeable.
pub fn thumbnail(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let image = image::load_from_memory(bytes)?;
    let image = image.resize_to_fill(THUMB_W, THUMB_H, image::imageops::FilterType::Triangle);
    let rgba = image.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (out, px) in rgb.pixels_mut().zip(rgba.pixels()) {
        let a = px[3] as u32;
        let blend = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
        *out = image::Rgb([blend(px[0]), blend(px[1]), blend(px[2])]);
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&rgb)?;
    Ok(out)
}

/// A short, stable name for content.
pub fn digest16(parts: &[&[u8]]) -> String {
    let mut hasher = sha2::Sha256::new();
    for part in parts {
        hasher.update(part);
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())[..16].to_string()
}

/// Write `bytes` to `path` through a sibling temporary file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CacheMeta {
    etag: Option<String>,
    fetched_at: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where the list came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexSource {
    /// Fetched (or revalidated) just now.
    Network,
    /// The kept copy, younger than [`INDEX_TTL`].
    Cache,
    /// The kept copy, because the network failed with this.
    Stale(String),
}

pub struct Index {
    pub entries: Vec<IndexEntry>,
    pub source: IndexSource,
}

pub struct Gallery {
    base: String,
    client: reqwest::Client,
    cache_dir: PathBuf,
    previews_dir: PathBuf,
}

impl Gallery {
    /// `state_dir` holds `theme-gallery/` and `theme-previews/`.
    pub fn new(base: &str, state_dir: &Path) -> anyhow::Result<Self> {
        let base = public_theme_url(base, None)?.to_string();
        anyhow::ensure!(base.ends_with('/'), "the gallery base must end with /");
        let allowed = base.clone();
        let client = reqwest::Client::builder()
            .connect_timeout(INDEX_TIMEOUT)
            .user_agent(concat!("muqun-desktop/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= REDIRECTS {
                    attempt.error("too many redirects")
                } else if attempt.url().as_str().starts_with(&allowed) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()?;
        Ok(Self {
            base,
            client,
            cache_dir: state_dir.join("theme-gallery"),
            previews_dir: state_dir.join("theme-previews"),
        })
    }

    fn read_cache(&self) -> Option<(Vec<IndexEntry>, CacheMeta)> {
        let text = std::fs::read_to_string(self.cache_dir.join("index.json")).ok()?;
        let meta = std::fs::read(self.cache_dir.join("index.meta.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Some((parse_theme_index(&text).ok()?, meta))
    }

    fn write_meta(&self, meta: &CacheMeta) {
        if let Ok(json) = serde_json::to_vec(meta) {
            let _ = write_atomic(&self.cache_dir.join("index.meta.json"), &json);
        }
    }

    /// The catalogue: the kept copy while it is fresh, else the network,
    /// else the kept copy marked stale.
    pub async fn index(&self) -> anyhow::Result<Index> {
        let cached = self.read_cache();
        if let Some((entries, meta)) = &cached {
            if now_secs().saturating_sub(meta.fetched_at) < INDEX_TTL.as_secs() {
                return Ok(Index {
                    entries: entries.clone(),
                    source: IndexSource::Cache,
                });
            }
        }
        let etag = cached.as_ref().and_then(|(_, m)| m.etag.clone());
        match self.fetch_index(etag.as_deref()).await {
            Ok(Some((text, etag))) => {
                let entries = parse_theme_index(&text)?;
                let _ = write_atomic(&self.cache_dir.join("index.json"), text.as_bytes());
                self.write_meta(&CacheMeta {
                    etag,
                    fetched_at: now_secs(),
                });
                Ok(Index {
                    entries,
                    source: IndexSource::Network,
                })
            }
            Ok(None) => {
                let (entries, mut meta) = cached.ok_or_else(unreadable)?;
                meta.fetched_at = now_secs();
                self.write_meta(&meta);
                Ok(Index {
                    entries,
                    source: IndexSource::Network,
                })
            }
            Err(err) => match cached {
                Some((entries, _)) => Ok(Index {
                    entries,
                    source: IndexSource::Stale(format!("{err:#}")),
                }),
                None => Err(err),
            },
        }
    }

    /// `Ok(None)` is `304 Not Modified`.
    async fn fetch_index(
        &self,
        etag: Option<&str>,
    ) -> anyhow::Result<Option<(String, Option<String>)>> {
        let url = format!("{}index.json", self.base);
        let mut request = self.client.get(&url).timeout(INDEX_TIMEOUT);
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request.send().await.map_err(network)?;
        if response.status() == StatusCode::NOT_MODIFIED && etag.is_some() {
            return Ok(None);
        }
        if response.status() != StatusCode::OK {
            anyhow::bail!(
                "The theme catalogue is not available right now (HTTP {})",
                response.status().as_u16()
            );
        }
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = read_body(response, THEME_INDEX_MAX_BYTES, |_, _| {}).await?;
        let text = String::from_utf8(bytes).map_err(|_| unreadable())?;
        Ok(Some((text, etag)))
    }

    /// Download `entry`'s package, reporting `(read, total)` as it goes.
    /// Returns the bytes and whether a catalogue SHA-256 vouched for them.
    pub async fn download(
        &self,
        entry: &IndexEntry,
        progress: impl FnMut(u64, u64),
    ) -> anyhow::Result<(Vec<u8>, bool)> {
        anyhow::ensure!(
            entry.bytes <= PACKAGE_BYTES,
            "{} is {} MiB; themes are at most 25 MiB",
            entry.name,
            entry.bytes / (1024 * 1024)
        );
        let url = theme_package_url(entry, &self.base)?;
        let response = self
            .client
            .get(url)
            .timeout(PACKAGE_TIMEOUT)
            .send()
            .await
            .map_err(network)?;
        if response.status() != StatusCode::OK {
            anyhow::bail!(
                "The theme download failed (HTTP {})",
                response.status().as_u16()
            );
        }
        let bytes = read_body(response, PACKAGE_BYTES as usize, progress).await?;
        let verified = verify_sha256(&bytes, entry.sha256.as_deref())?;
        Ok((bytes, verified))
    }

    /// The card picture for `entry`, from disk when already made.
    pub async fn preview(&self, entry: &IndexEntry) -> anyhow::Result<Option<PathBuf>> {
        let Some(url) = theme_preview_url(entry, &self.base) else {
            return Ok(None);
        };
        let key = digest16(&[
            url.as_str().as_bytes(),
            entry.version.as_bytes(),
            entry.sha256.as_deref().unwrap_or("").as_bytes(),
        ]);
        let path = self.previews_dir.join(format!("g-{}-{key}.jpg", entry.id));
        if path.is_file() {
            return Ok(Some(path));
        }
        let response = self
            .client
            .get(url)
            .timeout(PREVIEW_TIMEOUT)
            .send()
            .await
            .map_err(network)?;
        anyhow::ensure!(
            response.status() == StatusCode::OK,
            "preview HTTP {}",
            response.status().as_u16()
        );
        let bytes = read_body(response, PREVIEW_MAX_BYTES, |_, _| {}).await?;
        let out = path.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            write_atomic(&out, &thumbnail(&bytes)?)?;
            Ok(())
        })
        .await??;
        Ok(Some(path))
    }
}

/// The reader-facing sentence for a transport failure.
fn network(err: reqwest::Error) -> anyhow::Error {
    if err.is_timeout() {
        anyhow::anyhow!("The theme server did not answer in time")
    } else if err.is_connect() {
        anyhow::anyhow!("Could not reach the theme server ({err})")
    } else {
        anyhow::anyhow!("The theme download failed ({err})")
    }
}

/// The body, refused as soon as it passes `max` (whatever `Content-Length`
/// claimed).
async fn read_body(
    mut response: reqwest::Response,
    max: usize,
    mut progress: impl FnMut(u64, u64),
) -> anyhow::Result<Vec<u8>> {
    let declared = response.content_length();
    if declared.is_some_and(|n| n > max as u64) {
        anyhow::bail!("The download is larger than {} KiB", max / 1024);
    }
    let total = declared.unwrap_or(0);
    let mut body = Vec::with_capacity(declared.unwrap_or(0).min(max as u64) as usize);
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if body.len() + chunk.len() > max {
            anyhow::bail!("The download is larger than {} KiB", max / 1024);
        }
        body.extend_from_slice(&chunk);
        progress(body.len() as u64, total);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_INDEX: &str = include_str!("testdata/index.json");

    #[test]
    fn the_real_index_parses_every_row() {
        let entries = parse_theme_index(REAL_INDEX).unwrap();
        assert_eq!(entries.len(), 97);
        let first = &entries[0];
        assert_eq!(first.id, "acacia-afterglow");
        assert_eq!(first.package, "dist/acacia-afterglow.muqun-theme");
        assert!(first.sha256.as_deref().is_some_and(|s| s.len() == 64));
        assert!(first.bytes > 0);
        for entry in &entries {
            let url = theme_package_url(entry, THEME_GALLERY_BASE).unwrap();
            assert!(url.as_str().starts_with(THEME_GALLERY_BASE), "{url}");
            if entry.preview.is_some() {
                assert!(theme_preview_url(entry, THEME_GALLERY_BASE).is_some());
            }
        }
    }

    #[test]
    fn bad_rows_are_dropped_and_bad_documents_refused() {
        let text = r#"{"format":"muqun-themes-index","themes":[
            {"id":"ok","name":"Ok","version":"1.0.0","package":"dist/ok.muqun-theme","bytes":10,
             "sha256":"NOTHEX","tags":["a",1,"b"],"assets":2.5},
            {"id":"no-bytes","name":"x","version":"1","package":"p"},
            {"id":"neg","name":"x","version":"1","package":"p","bytes":-1},
            {"name":"no-id","version":"1","package":"p","bytes":1},
            7
        ]}"#;
        let entries = parse_theme_index(text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sha256, None, "malformed digest dropped");
        assert_eq!(entries[0].tags, ["a", "b"]);
        assert_eq!(entries[0].assets, None);
        assert!(parse_theme_index("{}").is_err());
        assert!(parse_theme_index("[").is_err());
        assert!(parse_theme_index(r#"{"format":"muqun-themes-index"}"#).is_err());
    }

    fn entry(package: &str, preview: Option<&str>) -> IndexEntry {
        let mut e = parse_theme_index(REAL_INDEX).unwrap().remove(0);
        e.package = package.into();
        e.preview = preview.map(Into::into);
        e
    }

    #[test]
    fn packages_and_previews_stay_on_the_catalogue() {
        for bad in [
            "https://evil.example.com/x.muqun-theme",
            "//evil.example.com/x",
            "http://muqun.dev/api/themes/x",
            "../../other/x",
            "https://muqun.dev:8443/api/themes/x",
            "https://user@muqun.dev/api/themes/x",
            "dist/a b.muqun-theme",
        ] {
            assert!(
                theme_package_url(&entry(bad, None), THEME_GALLERY_BASE).is_err(),
                "{bad}"
            );
            assert!(
                theme_preview_url(&entry("dist/x", Some(bad)), THEME_GALLERY_BASE).is_none(),
                "{bad}"
            );
        }
        let ok =
            theme_package_url(&entry("dist/x.muqun-theme#frag", None), THEME_GALLERY_BASE).unwrap();
        assert_eq!(
            ok.as_str(),
            "https://muqun.dev/api/themes/dist/x.muqun-theme"
        );
    }

    #[test]
    fn public_urls_refuse_private_hosts() {
        for bad in [
            "https://localhost/x",
            "https://10.0.0.2/x",
            "https://nas.home/x",
            "https://router.lan/x",
            "https://a.local/x",
            "https://[::1]/x",
            "https://example/x",
            "file:///etc/passwd",
        ] {
            assert!(public_theme_url(bad, None).is_err(), "{bad}");
        }
        assert!(public_theme_url("https://muqun.dev/api/themes/", None).is_ok());
    }

    #[test]
    fn a_sha256_mismatch_is_refused_and_a_match_is_verified() {
        let bytes = b"package bytes";
        let right = hex::encode(sha2::Sha256::digest(bytes));
        assert!(verify_sha256(bytes, Some(&right)).unwrap());
        assert!(
            !verify_sha256(bytes, None).unwrap(),
            "unverified, not refused"
        );
        let err = verify_sha256(bytes, Some(&"0".repeat(64)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing was installed"), "{err}");
    }

    #[test]
    fn thumbnails_are_8_by_5_jpegs() {
        let png = crate::theme::pack::tests::png();
        let jpeg = thumbnail(&png).unwrap();
        assert_eq!(
            image::guess_format(&jpeg).unwrap(),
            image::ImageFormat::Jpeg
        );
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (THUMB_W, THUMB_H));
    }
}
