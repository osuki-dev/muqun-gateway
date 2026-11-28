//! Where themes live: a built-in set compiled into the binary, and the ones
//! the owner installed under `<config>/themes/<id>/`.
//!
//! An installed theme is its package extracted: `manifest.json` (the pack's
//! `theme.json`, byte for byte) and `assets/<file>` at the paths the manifest
//! declares. It is re-read through the same checks as a package
//! ([`parse_manifest`], [`check_image`]), so a hand-edited directory cannot
//! smuggle in anything a package could not. Installing is atomic: the new
//! directory is written beside the old one and renamed over it, so a crash
//! leaves either the previous theme or the new one, never half of either.
//!
//! An installed theme with a built-in's id (a newer download of it) shadows
//! the built-in; removing it brings the built-in back.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::color_scheme::ColorScheme;

use super::pack::{
    check_image, is_identifier, parse_manifest, unpack, Manifest, ThemePack, ASSET_BYTES,
    MANIFEST_BYTES,
};

const MANIFEST_FILE: &str = "manifest.json";

/// A pack compiled into the binary (`assets/themes/builtin/`, built by
/// `assets/themes/build-builtin.py`; all CC0-1.0).
pub struct Builtin {
    pub id: &'static str,
    pub bytes: &'static [u8],
}

macro_rules! builtin {
    ($id:literal) => {
        Builtin {
            id: $id,
            bytes: include_bytes!(concat!("../../assets/themes/builtin/", $id, ".muqun-theme")),
        }
    };
}

/// In the order the Installed tab lists them; the first is the default.
pub const BUILTIN: &[Builtin] = &[
    builtin!("cover-courier"),
    builtin!("akiba-overdrive"),
    builtin!("jade-tempest"),
    builtin!("eclipse-archivist"),
    builtin!("vanguard-drifter"),
    builtin!("scene-voyager"),
];

/// A built-in pack, unpacked once on first use.
pub fn builtin(id: &str) -> Option<Arc<ThemePack>> {
    static CACHE: OnceLock<Mutex<BTreeMap<&'static str, Arc<ThemePack>>>> = OnceLock::new();
    let entry = BUILTIN.iter().find(|b| b.id == id)?;
    let cache = CACHE.get_or_init(Default::default);
    if let Some(pack) = cache.lock().unwrap().get(entry.id) {
        return Some(pack.clone());
    }
    match unpack(entry.bytes) {
        Ok(pack) => {
            let pack = Arc::new(pack);
            cache.lock().unwrap().insert(entry.id, pack.clone());
            Some(pack)
        }
        Err(err) => {
            tracing::error!(id, "a built-in theme does not unpack: {err:#}");
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Builtin,
    Installed,
    /// Installed, with the id of a built-in it shadows.
    InstalledOverBuiltin,
}

impl Origin {
    pub fn removable(self) -> bool {
        self != Origin::Builtin
    }
}

/// Four colours of a variant, for a card without a picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swatch {
    pub background: String,
    pub surface: String,
    pub primary: String,
    pub text: String,
}

/// What a library card shows.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: Option<String>,
    pub license: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub origin: Origin,
    pub light: Swatch,
    pub dark: Swatch,
    /// The card picture, once a thumbnail exists (`service.rs`).
    pub thumbnail: Option<PathBuf>,
}

impl ThemeEntry {
    pub fn from_manifest(m: &Manifest, origin: Origin) -> Self {
        let swatch = |v: &super::pack::Variant| Swatch {
            background: v.colors.background.clone(),
            surface: v.colors.surface.clone(),
            primary: v.colors.primary.clone(),
            text: v.colors.text.clone(),
        };
        Self {
            id: m.id.clone(),
            name: m.name.clone(),
            version: m.version.clone(),
            author: m.author.clone(),
            license: m.license.clone(),
            description: m.description.clone(),
            tags: m.tags.clone(),
            origin,
            light: swatch(&m.variants.light),
            dark: swatch(&m.variants.dark),
            thumbnail: None,
        }
    }
}

/// A theme ready to apply: its manifest, and its images on demand.
#[derive(Clone)]
pub enum Loaded {
    Builtin(Arc<ThemePack>),
    Installed {
        manifest: Arc<Manifest>,
        dir: PathBuf,
    },
}

impl Loaded {
    pub fn manifest(&self) -> &Manifest {
        match self {
            Loaded::Builtin(pack) => &pack.manifest,
            Loaded::Installed { manifest, .. } => manifest,
        }
    }

    /// Asset `id`'s bytes, re-checked (digest, format, size) when read from disk.
    pub fn asset(&self, id: &str) -> anyhow::Result<Option<Arc<[u8]>>> {
        match self {
            Loaded::Builtin(pack) => Ok(pack.asset(id)),
            Loaded::Installed { manifest, dir } => {
                let Some(asset) = manifest.assets.get(id) else {
                    return Ok(None);
                };
                // Validated by `parse_manifest`: `assets/<plain name>.<ext>`.
                let Some(path) = &asset.path else {
                    return Ok(None);
                };
                let bytes = read_bounded(&dir.join(path), ASSET_BYTES)?;
                check_image(path, asset.sha256.as_deref(), &bytes)?;
                Ok(Some(bytes.into()))
            }
        }
    }

    /// The Home hero image for `scheme` ([`Manifest::cover_asset`]).
    #[allow(dead_code)]
    pub fn cover(&self, scheme: ColorScheme) -> Option<Arc<[u8]>> {
        let id = self.manifest().cover_asset(scheme == ColorScheme::Dark)?;
        self.asset(id)
            .inspect_err(|err| tracing::warn!(id, "theme cover unreadable: {err:#}"))
            .ok()
            .flatten()
    }

    /// The gallery picture (`preview`), if the pack has one.
    pub fn preview(&self) -> Option<Arc<[u8]>> {
        let id = self.manifest().preview.as_deref()?;
        self.asset(id).ok().flatten()
    }
}

/// Read at most `limit` bytes; a longer file is an error, not a truncation.
pub fn read_bounded(path: &Path, limit: u64) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::File::open(path)
        .map_err(|err| anyhow::anyhow!("cannot read {}: {err}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= limit,
        "{} is larger than {} MiB",
        path.display(),
        limit / (1024 * 1024)
    );
    Ok(bytes)
}

pub struct ThemeStore {
    dir: PathBuf,
}

impl ThemeStore {
    /// `dir` is `<config>/themes`. Leftovers of an interrupted install are
    /// removed.
    pub fn open(dir: PathBuf) -> Self {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(".tmp-") || name.starts_with(".old-") {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        Self { dir }
    }

    fn installed_dir(&self, id: &str) -> Option<PathBuf> {
        is_identifier(id).then(|| self.dir.join(id))
    }

    pub fn is_installed(&self, id: &str) -> bool {
        self.installed_dir(id)
            .is_some_and(|d| d.join(MANIFEST_FILE).is_file())
    }

    fn installed_manifest(&self, id: &str) -> anyhow::Result<Manifest> {
        let dir = self
            .installed_dir(id)
            .ok_or_else(|| anyhow::anyhow!("not a theme id: {id}"))?;
        let bytes = read_bounded(&dir.join(MANIFEST_FILE), MANIFEST_BYTES as u64)?;
        let manifest = parse_manifest(&bytes)?;
        anyhow::ensure!(
            manifest.id == id,
            "{} holds theme {}",
            dir.display(),
            manifest.id
        );
        Ok(manifest)
    }

    /// The theme `id`, installed first, else built-in.
    pub fn load(&self, id: &str) -> anyhow::Result<Loaded> {
        if self.is_installed(id) {
            let manifest = self.installed_manifest(id)?;
            return Ok(Loaded::Installed {
                manifest: Arc::new(manifest),
                dir: self.dir.join(id),
            });
        }
        builtin(id)
            .map(Loaded::Builtin)
            .ok_or_else(|| anyhow::anyhow!("theme {id} is not installed"))
    }

    /// Every theme: built-ins in their order, then installed ones by name.
    /// An installed directory that no longer reads is skipped with a warning.
    pub fn list(&self) -> Vec<ThemeEntry> {
        let mut installed: BTreeMap<String, Manifest> = BTreeMap::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let id = entry.file_name().to_string_lossy().to_string();
                if !is_identifier(&id) || !self.is_installed(&id) {
                    continue;
                }
                match self.installed_manifest(&id) {
                    Ok(m) => {
                        installed.insert(id, m);
                    }
                    Err(err) => tracing::warn!(id, "installed theme skipped: {err:#}"),
                }
            }
        }
        let mut list = Vec::new();
        for b in BUILTIN {
            if let Some(m) = installed.remove(b.id) {
                list.push(ThemeEntry::from_manifest(&m, Origin::InstalledOverBuiltin));
            } else if let Some(pack) = builtin(b.id) {
                list.push(ThemeEntry::from_manifest(&pack.manifest, Origin::Builtin));
            }
        }
        let mut rest: Vec<ThemeEntry> = installed
            .values()
            .map(|m| ThemeEntry::from_manifest(m, Origin::Installed))
            .collect();
        rest.sort_by_key(|e| e.name.to_lowercase());
        list.extend(rest);
        list
    }

    /// Install `pack` as `<dir>/<id>`, replacing an installed copy atomically.
    pub fn install(&self, pack: &ThemePack) -> anyhow::Result<()> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = &pack.manifest.id;
        let target = self
            .installed_dir(id)
            .ok_or_else(|| anyhow::anyhow!("not a theme id: {id}"))?;
        std::fs::create_dir_all(&self.dir)?;
        let unique = format!(
            "{id}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let tmp = self.dir.join(format!(".tmp-{unique}"));
        let result = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(tmp.join("assets"))?;
            write_synced(&tmp.join(MANIFEST_FILE), &pack.manifest_json)?;
            for (asset_id, asset) in &pack.manifest.assets {
                let (Some(path), Some(bytes)) = (&asset.path, pack.assets.get(asset_id)) else {
                    anyhow::bail!("asset {asset_id} has no bytes");
                };
                write_synced(&tmp.join(path), bytes)?;
            }
            if target.exists() {
                let old = self.dir.join(format!(".old-{unique}"));
                std::fs::rename(&target, &old)?;
                if let Err(err) = std::fs::rename(&tmp, &target) {
                    let _ = std::fs::rename(&old, &target);
                    return Err(err.into());
                }
                let _ = std::fs::remove_dir_all(&old);
            } else {
                std::fs::rename(&tmp, &target)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        result.map_err(|err| anyhow::anyhow!("could not install {id}: {err:#}"))
    }

    /// Remove an installed theme. A built-in (not shadowed) cannot be.
    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        if !self.is_installed(id) {
            anyhow::bail!("{id} is built in and cannot be removed");
        }
        let target = self.dir.join(id);
        let old = self
            .dir
            .join(format!(".old-{id}-{}-rm", std::process::id()));
        std::fs::rename(&target, &old)?;
        std::fs::remove_dir_all(&old)?;
        Ok(())
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::pack::tests::valid_package;

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muqun-desktop-themes-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_builtin_unpacks_and_the_set_stays_small() {
        let total: usize = BUILTIN.iter().map(|b| b.bytes.len()).sum();
        assert!(total < 6 * 1024 * 1024, "{total} bytes embedded");
        for b in BUILTIN {
            let pack = builtin(b.id).unwrap_or_else(|| panic!("{}", b.id));
            assert_eq!(pack.manifest.id, b.id);
            assert_eq!(pack.manifest.license.as_deref(), Some("CC0-1.0"));
            assert!(pack.manifest.preview.is_some());
            for dark in [false, true] {
                let cover = pack.manifest.cover_asset(dark).unwrap();
                assert!(pack.asset(cover).is_some(), "{} {cover}", b.id);
            }
        }
    }

    #[test]
    fn install_list_load_and_remove() {
        let dir = scratch("install");
        let store = ThemeStore::open(dir.clone());
        let pack = unpack(&valid_package("demo-one")).unwrap();
        store.install(&pack).unwrap();
        assert!(store.is_installed("demo-one"));
        let list = store.list();
        assert_eq!(list.len(), BUILTIN.len() + 1);
        let entry = list.iter().find(|e| e.id == "demo-one").unwrap();
        assert_eq!(entry.origin, Origin::Installed);

        let loaded = store.load("demo-one").unwrap();
        assert_eq!(loaded.manifest().id, "demo-one");
        assert!(loaded.preview().is_some());

        // Reinstall replaces in place and leaves nothing beside it.
        store.install(&pack).unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["demo-one"]);

        store.remove("demo-one").unwrap();
        assert!(!store.is_installed("demo-one"));
        assert!(store.load("demo-one").is_err());
        assert!(store.remove("cover-courier").is_err(), "built in");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_installed_copy_shadows_its_builtin_until_removed() {
        let dir = scratch("shadow");
        let store = ThemeStore::open(dir.clone());
        let pack = unpack(&valid_package("cover-courier")).unwrap();
        store.install(&pack).unwrap();
        let list = store.list();
        assert_eq!(list.len(), BUILTIN.len());
        assert_eq!(list[0].origin, Origin::InstalledOverBuiltin);
        assert!(matches!(
            store.load("cover-courier").unwrap(),
            Loaded::Installed { .. }
        ));
        store.remove("cover-courier").unwrap();
        assert!(matches!(
            store.load("cover-courier").unwrap(),
            Loaded::Builtin(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tampered_installed_asset_is_refused_on_read() {
        let dir = scratch("tamper");
        let store = ThemeStore::open(dir.clone());
        use sha2::Digest as _;
        let png = crate::theme::pack::tests::png();
        let sha = hex::encode(sha2::Sha256::digest(&png));
        let manifest = crate::theme::pack::tests::manifest_with_preview("demo-two", Some(&sha));
        let bytes = crate::theme::pack::tests::zip(
            &[("theme.json", &manifest), ("assets/preview.png", &png)],
            false,
        );
        store.install(&unpack(&bytes).unwrap()).unwrap();
        std::fs::write(dir.join("demo-two/assets/preview.png"), b"swapped").unwrap();
        let loaded = store.load("demo-two").unwrap();
        assert!(loaded.asset("preview").is_err());
        assert!(loaded.preview().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn interrupted_installs_are_cleaned_up_and_odd_directories_ignored() {
        let dir = scratch("leftovers");
        std::fs::create_dir_all(dir.join(".tmp-demo-1-0/assets")).unwrap();
        std::fs::create_dir_all(dir.join("Not An Id")).unwrap();
        std::fs::create_dir_all(dir.join("empty-dir")).unwrap();
        let store = ThemeStore::open(dir.clone());
        assert!(!dir.join(".tmp-demo-1-0").exists());
        assert_eq!(store.list().len(), BUILTIN.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
