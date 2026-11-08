//! The tokio side of themes: every `Command::Theme*` lands here, does its
//! disk and network work off the UI thread, and answers through the inbox
//! with [`ThemeUpdate`]s. The main thread applies
//! the updates (`ui::UiState::on_theme`, `apply.rs` `activate`).
//!
//! Concurrency: one install per theme id at a time (a second request while
//! one runs is ignored); installs of different themes run side by side.
//! Each install is atomic on disk (`store.rs`), so a window closed mid-way
//! leaves the previous state.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::apply::AppliedTheme;
use super::color_scheme::ColorScheme;
use super::gallery::{digest16, thumbnail, write_atomic, Gallery, IndexEntry, IndexSource};
use super::pack::{unpack, PACKAGE_BYTES};
use super::store::{read_bounded, Loaded, ThemeEntry, ThemeStore};
use crate::config::Scheme;
use crate::model::commands::Command;
use crate::model::inbox::{Inbox, Update};
use crate::model::projection::ConfigStore;

/// Previews fetched at once.
const PREVIEW_CONCURRENCY: usize = 4;
/// Progress is reported at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(80);

/// Where the gallery is read from: [`super::gallery::THEME_GALLERY_BASE`],
/// or `MUQUN_THEME_GALLERY` (an `https://` base ending in `/`) for testing
/// against a mirror.
fn gallery_base() -> String {
    std::env::var("MUQUN_THEME_GALLERY")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| super::gallery::THEME_GALLERY_BASE.to_string())
}

/// An install in progress, or how it ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    Downloading {
        read: u64,
        total: u64,
    },
    /// Downloaded; the SHA-256 is being compared / the package checked.
    Verifying,
    Installing,
    /// Installed. `verified`: the catalogue's SHA-256 matched.
    Done {
        verified: bool,
    },
    Failed {
        message: String,
    },
}

/// The gallery tab's state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GalleryView {
    pub entries: Vec<IndexEntry>,
    pub loading: bool,
    /// From the kept copy because the network failed: why.
    pub stale: Option<String>,
    /// Nothing to show, and why.
    pub error: Option<String>,
}

/// A one-line message for the panel.
#[derive(Debug, Clone)]
pub struct Notice {
    pub message: String,
    pub error: bool,
    /// What a Retry button re-sends, if anything.
    #[allow(dead_code)]
    pub retry: Option<Command>,
}

#[derive(Debug, Clone)]
// Boxed by `Update::Theme` already; one rare update is not worth boxing each field.
#[allow(clippy::large_enum_variant)]
pub enum ThemeUpdate {
    Library(Arc<Vec<ThemeEntry>>),
    Gallery(Arc<GalleryView>),
    Preview { id: String, path: PathBuf },
    Job { id: String, job: Job },
    Applied(AppliedTheme),
    Notice(Notice),
}

struct Service {
    store: ThemeStore,
    gallery: Option<Gallery>,
    gallery_error: Option<String>,
    previews_dir: PathBuf,
    index: Mutex<Vec<IndexEntry>>,
    busy: Mutex<HashSet<String>>,
    /// Gallery thumbnails already announced, by id.
    previews: Mutex<HashMap<String, PathBuf>>,
    /// A round of preview fetches is running; a second refresh does not
    /// start another.
    previews_running: AtomicBool,
}

static PATHS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();

/// Pin `<config>` and `<state>` for the service. Called once, at startup.
pub fn set_paths(config_dir: &Path, state_dir: &Path) {
    let _ = PATHS.set((config_dir.to_path_buf(), state_dir.to_path_buf()));
}

pub fn store_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("themes")
}

fn service() -> Option<&'static Arc<Service>> {
    static SERVICE: OnceLock<Arc<Service>> = OnceLock::new();
    if let Some(s) = SERVICE.get() {
        return Some(s);
    }
    let (config, state) = PATHS.get()?;
    let (gallery, gallery_error) = match Gallery::new(&gallery_base(), state) {
        Ok(g) => (Some(g), None),
        Err(err) => (None, Some(format!("{err:#}"))),
    };
    let _ = SERVICE.set(Arc::new(Service {
        store: ThemeStore::open(store_dir(config)),
        gallery,
        gallery_error,
        previews_dir: state.join("theme-previews"),
        index: Mutex::new(Vec::new()),
        busy: Mutex::new(HashSet::new()),
        previews: Mutex::new(HashMap::new()),
        previews_running: AtomicBool::new(false),
    }));
    SERVICE.get()
}

fn push(inbox: &Inbox, update: ThemeUpdate) {
    inbox.push(Update::Theme(Box::new(update)));
}

fn notice(inbox: &Inbox, message: String, retry: Option<Command>) {
    push(
        inbox,
        ThemeUpdate::Notice(Notice {
            message,
            error: true,
            retry,
        }),
    );
}

/// Run one theme command to completion. Called on the tokio runtime.
pub async fn run(command: Command, inbox: Arc<Inbox>, config: Arc<ConfigStore>) {
    let Some(service) = service() else {
        tracing::error!("theme command before the theme paths were set");
        return;
    };
    let service = service.clone();
    match command {
        Command::ThemeRefresh => {
            service.library(&inbox).await;
            service.refresh_gallery(&inbox).await;
        }
        Command::ThemeInstall(id) => service.install(&inbox, id).await,
        Command::ThemeRemove(id) => service.remove(&inbox, &config, id).await,
        Command::ThemeApply(id, scheme) => service.apply(&inbox, &config, id, scheme).await,
        Command::ThemeImportFile(path) => service.import(&inbox, path).await,
        Command::ThemeImportUrl(url) => service.import_url(&inbox, url).await,
        other => tracing::warn!(?other, "not a theme command"),
    }
}

/// The thumbnail for a library theme, made once per distinct preview.
fn library_thumbnail(previews_dir: &Path, loaded: &Loaded) -> Option<PathBuf> {
    let preview = loaded.preview()?;
    let path = previews_dir.join(format!("t-{}.jpg", digest16(&[&preview])));
    if path.is_file() {
        return Some(path);
    }
    match thumbnail(&preview).and_then(|jpeg| Ok(write_atomic(&path, &jpeg)?)) {
        Ok(()) => Some(path),
        Err(err) => {
            tracing::warn!(id = %loaded.manifest().id, "no thumbnail: {err:#}");
            None
        }
    }
}

impl Service {
    async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Service) -> T + Send + 'static,
    ) -> anyhow::Result<T> {
        let me = self.clone();
        Ok(tokio::task::spawn_blocking(move || f(&me)).await?)
    }

    /// The Installed tab: every theme, with thumbnails.
    async fn library(self: &Arc<Self>, inbox: &Inbox) {
        let list = self
            .blocking(|s| {
                let mut list = s.store.list();
                for entry in &mut list {
                    if let Ok(loaded) = s.store.load(&entry.id) {
                        entry.thumbnail = library_thumbnail(&s.previews_dir, &loaded);
                    }
                }
                list
            })
            .await;
        match list {
            Ok(list) => push(inbox, ThemeUpdate::Library(Arc::new(list))),
            Err(err) => tracing::error!("theme list failed: {err:#}"),
        }
    }

    async fn refresh_gallery(self: &Arc<Self>, inbox: &Arc<Inbox>) {
        let Some(gallery) = &self.gallery else {
            push(
                inbox,
                ThemeUpdate::Gallery(Arc::new(GalleryView {
                    error: self.gallery_error.clone(),
                    ..GalleryView::default()
                })),
            );
            return;
        };
        let current = self.index.lock().unwrap().clone();
        push(
            inbox,
            ThemeUpdate::Gallery(Arc::new(GalleryView {
                entries: current,
                loading: true,
                ..GalleryView::default()
            })),
        );
        let started = Instant::now();
        match gallery.index().await {
            Ok(index) => {
                tracing::info!(
                    themes = index.entries.len(),
                    source = ?index.source,
                    ms = started.elapsed().as_millis() as u64,
                    "theme gallery index"
                );
                *self.index.lock().unwrap() = index.entries.clone();
                let stale = match index.source {
                    IndexSource::Stale(why) => Some(why),
                    _ => None,
                };
                let entries = index.entries;
                push(
                    inbox,
                    ThemeUpdate::Gallery(Arc::new(GalleryView {
                        entries: entries.clone(),
                        loading: false,
                        stale,
                        error: None,
                    })),
                );
                self.clone().previews(inbox.clone(), entries).await;
            }
            Err(err) => {
                tracing::warn!("theme gallery unavailable: {err:#}");
                push(
                    inbox,
                    ThemeUpdate::Gallery(Arc::new(GalleryView {
                        entries: Vec::new(),
                        loading: false,
                        stale: None,
                        error: Some(format!("{err:#}")),
                    })),
                );
            }
        }
    }

    /// Fetch every missing card picture, [`PREVIEW_CONCURRENCY`] at a time,
    /// announcing each as it lands.
    async fn previews(self: Arc<Self>, inbox: Arc<Inbox>, mut entries: Vec<IndexEntry>) {
        if self.previews_running.swap(true, Ordering::AcqRel) {
            return;
        }
        // Popped from the end: the first cards on screen go first.
        entries.reverse();
        let queue = Arc::new(Mutex::new(entries));
        let mut workers = Vec::new();
        for _ in 0..PREVIEW_CONCURRENCY {
            let (me, queue, inbox) = (self.clone(), queue.clone(), inbox.clone());
            workers.push(tokio::spawn(async move {
                loop {
                    let Some(entry) = queue.lock().unwrap().pop() else {
                        return;
                    };
                    if me.previews.lock().unwrap().contains_key(&entry.id) {
                        continue;
                    }
                    let Some(gallery) = &me.gallery else { return };
                    match gallery.preview(&entry).await {
                        Ok(Some(path)) => {
                            me.previews
                                .lock()
                                .unwrap()
                                .insert(entry.id.clone(), path.clone());
                            push(&inbox, ThemeUpdate::Preview { id: entry.id, path });
                        }
                        Ok(None) => {}
                        Err(err) => tracing::debug!(id = %entry.id, "preview: {err:#}"),
                    }
                }
            }));
        }
        for worker in workers {
            let _ = worker.await;
        }
        self.previews_running.store(false, Ordering::Release);
    }

    async fn install(self: &Arc<Self>, inbox: &Arc<Inbox>, id: String) {
        if !self.busy.lock().unwrap().insert(id.clone()) {
            return;
        }
        let result = self.install_inner(inbox, &id).await;
        self.busy.lock().unwrap().remove(&id);
        let job = match result {
            Ok(verified) => Job::Done { verified },
            Err(err) => {
                tracing::warn!(%id, "theme install failed: {err:#}");
                Job::Failed {
                    message: format!("{err:#}"),
                }
            }
        };
        push(inbox, ThemeUpdate::Job { id, job });
        self.library(inbox).await;
    }

    async fn install_inner(self: &Arc<Self>, inbox: &Arc<Inbox>, id: &str) -> anyhow::Result<bool> {
        let gallery = self
            .gallery
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("The theme gallery is not available"))?;
        let entry = self
            .index
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("{id} is not in the catalogue; refresh and try again")
            })?;
        let job = |job| ThemeUpdate::Job {
            id: id.to_string(),
            job,
        };
        push(
            inbox,
            job(Job::Downloading {
                read: 0,
                total: entry.bytes,
            }),
        );
        let mut last = Instant::now();
        let declared = entry.bytes;
        let progress_inbox = inbox.clone();
        let progress_id = id.to_string();
        let started = Instant::now();
        let (bytes, verified) = gallery
            .download(&entry, move |read, total| {
                if last.elapsed() >= PROGRESS_EVERY {
                    last = Instant::now();
                    push(
                        &progress_inbox,
                        ThemeUpdate::Job {
                            id: progress_id.clone(),
                            job: Job::Downloading {
                                read,
                                total: if total > 0 { total } else { declared },
                            },
                        },
                    );
                }
            })
            .await?;
        tracing::info!(
            %id,
            bytes = bytes.len(),
            verified,
            ms = started.elapsed().as_millis() as u64,
            "theme downloaded"
        );
        push(inbox, job(Job::Verifying));
        let expected = id.to_string();
        let pack = self
            .blocking(move |_| -> anyhow::Result<_> {
                let pack = unpack(&bytes)?;
                anyhow::ensure!(
                    pack.manifest.id == expected,
                    "the package holds {} instead of {expected}",
                    pack.manifest.id
                );
                Ok(pack)
            })
            .await??;
        push(inbox, job(Job::Installing));
        self.blocking(move |s| s.store.install(&pack)).await??;
        Ok(verified)
    }

    async fn import(self: &Arc<Self>, inbox: &Arc<Inbox>, path: PathBuf) {
        let shown = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let result = self
            .blocking({
                let path = path.clone();
                move |s| -> anyhow::Result<String> {
                    let bytes = read_bounded(&path, PACKAGE_BYTES)?;
                    let pack = unpack(&bytes)?;
                    s.store.install(&pack)?;
                    Ok(pack.manifest.name.clone())
                }
            })
            .await
            .and_then(|r| r);
        match result {
            Ok(name) => {
                tracing::info!(file = %path.display(), %name, "theme imported");
                push(
                    inbox,
                    ThemeUpdate::Notice(Notice {
                        message: format!("Installed {name} from {shown}"),
                        error: false,
                        retry: None,
                    }),
                );
            }
            Err(err) => notice(
                inbox,
                format!("Could not import {shown}: {err:#}"),
                Some(Command::ThemeImportFile(path)),
            ),
        }
        self.library(inbox).await;
    }

    /// App-style link import: a public HTTPS package URL, not a git repo.
    async fn import_url(self: &Arc<Self>, inbox: &Arc<Inbox>, url: String) {
        let shown = url.clone();
        let job_id = format!("import:{}", digest16(&[url.as_bytes()]));
        if !self.busy.lock().unwrap().insert(job_id.clone()) {
            return;
        }
        push(
            inbox,
            ThemeUpdate::Job {
                id: job_id.clone(),
                job: Job::Downloading { read: 0, total: 0 },
            },
        );
        let progress_inbox = inbox.clone();
        let progress_id = job_id.clone();
        let mut last = Instant::now();
        let downloaded = Gallery::download_public(&url, move |read, total| {
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                push(
                    &progress_inbox,
                    ThemeUpdate::Job {
                        id: progress_id.clone(),
                        job: Job::Downloading { read, total },
                    },
                );
            }
        })
        .await;
        let result = match downloaded {
            Ok(bytes) => {
                push(
                    inbox,
                    ThemeUpdate::Job {
                        id: job_id.clone(),
                        job: Job::Verifying,
                    },
                );
                self.blocking(move |s| -> anyhow::Result<String> {
                    let pack = unpack(&bytes)?;
                    let name = pack.manifest.name.clone();
                    s.store.install(&pack)?;
                    Ok(name)
                })
                .await
                .and_then(|r| r)
            }
            Err(err) => Err(err),
        };
        self.busy.lock().unwrap().remove(&job_id);
        match result {
            Ok(name) => {
                tracing::info!(%url, %name, "theme imported from link");
                push(
                    inbox,
                    ThemeUpdate::Job {
                        id: job_id,
                        job: Job::Done { verified: false },
                    },
                );
                push(
                    inbox,
                    ThemeUpdate::Notice(Notice {
                        message: format!("Installed {name} from link"),
                        error: false,
                        retry: None,
                    }),
                );
            }
            Err(err) => {
                push(
                    inbox,
                    ThemeUpdate::Job {
                        id: job_id,
                        job: Job::Failed {
                            message: format!("{err:#}"),
                        },
                    },
                );
                notice(
                    inbox,
                    format!("Could not import from link: {err:#}"),
                    Some(Command::ThemeImportUrl(shown)),
                );
            }
        }
        self.library(inbox).await;
    }

    async fn remove(self: &Arc<Self>, inbox: &Arc<Inbox>, config: &Arc<ConfigStore>, id: String) {
        let removed = {
            let id = id.clone();
            self.blocking(move |s| s.store.remove(&id))
                .await
                .and_then(|r| r)
        };
        match removed {
            Ok(()) => {
                // The current theme went away: back to the default (or the
                // built-in it was shadowing).
                if super::apply::current_theme_id().as_deref() == Some(id.as_str()) {
                    let scheme = config.get().appearance.scheme;
                    let fallback = if super::store::builtin(&id).is_some() {
                        id.clone()
                    } else {
                        super::DEFAULT_THEME_ID.to_string()
                    };
                    self.apply(inbox, config, fallback, scheme).await;
                }
            }
            Err(err) => notice(inbox, format!("Could not remove {id}: {err:#}"), None),
        }
        self.library(inbox).await;
    }

    async fn apply(
        self: &Arc<Self>,
        inbox: &Arc<Inbox>,
        config: &Arc<ConfigStore>,
        id: String,
        scheme: Scheme,
    ) {
        let requested = Instant::now();
        let loaded = {
            let id = id.clone();
            self.blocking(move |s| s.load_applied(&id, scheme, requested))
                .await
                .and_then(|r| r)
        };
        match loaded {
            Ok(applied) => {
                push(inbox, ThemeUpdate::Applied(applied));
                // The write blocks on the disk: keep it off the async threads.
                let config = config.clone();
                let saved = tokio::task::spawn_blocking(move || {
                    config.change(|c| {
                        c.appearance.theme_id = id;
                        c.appearance.scheme = scheme;
                    });
                    config.flush();
                })
                .await;
                if let Err(err) = saved {
                    tracing::warn!("saving the theme choice failed: {err}");
                }
            }
            Err(err) => notice(
                inbox,
                format!("Could not apply {id}: {err:#}"),
                Some(Command::ThemeApply(id, scheme)),
            ),
        }
    }

    fn load_applied(
        &self,
        id: &str,
        scheme: Scheme,
        requested: Instant,
    ) -> anyhow::Result<AppliedTheme> {
        load_applied(&self.store, id, scheme, requested)
    }
}

/// Read theme `id` (and both covers) for [`super::apply::activate`]; the
/// palettes are checked here so the main thread cannot fail on them.
pub fn load_applied(
    store: &ThemeStore,
    id: &str,
    scheme: Scheme,
    requested: Instant,
) -> anyhow::Result<AppliedTheme> {
    let loaded = store.load(id)?;
    applied_from(&loaded, scheme, requested)
}

/// [`AppliedTheme`] for a theme already read: its palettes checked and the
/// images its slots draw.
pub fn applied_from(
    loaded: &super::store::Loaded,
    scheme: Scheme,
    requested: Instant,
) -> anyhow::Result<AppliedTheme> {
    let manifest = loaded.manifest();
    manifest.palette(ColorScheme::Light)?;
    manifest.palette(ColorScheme::Dark)?;
    let mut wanted: Vec<&str> = super::apply::DRAWN_SLOTS
        .iter()
        .flat_map(|slot| [false, true].map(|dark| manifest.slot_asset(slot, dark)))
        .flatten()
        .collect();
    if let Some(logo) = manifest
        .home_identity
        .as_ref()
        .and_then(|h| h.logo.as_ref())
        .filter(|l| l.mode == "custom")
        .and_then(|l| l.asset.as_deref())
    {
        wanted.push(logo);
    }
    let mut images = std::collections::BTreeMap::new();
    let mut sizes = std::collections::BTreeMap::new();
    for id in wanted {
        if images.contains_key(id) {
            continue;
        }
        match loaded.asset(id) {
            Ok(Some(bytes)) => {
                // Checked at unpack, so the header reads; a size of nothing
                // keeps the image out rather than mis-placed.
                let size = image::ImageReader::new(std::io::Cursor::new(&bytes[..]))
                    .with_guessed_format()
                    .ok()
                    .and_then(|r| r.into_dimensions().ok());
                if let Some((w, h)) = size.filter(|(w, h)| *w > 0 && *h > 0) {
                    sizes.insert(id.to_string(), [w, h]);
                    images.insert(id.to_string(), bytes);
                }
            }
            Ok(None) => {}
            Err(err) => tracing::warn!(id, "theme image unreadable: {err:#}"),
        }
    }
    let mut glyphs = std::collections::BTreeMap::new();
    let icons = manifest.icons.as_ref();
    for name in super::apply::DRAWN_ICONS {
        let Some(icon) = icons.and_then(|i| i.get(*name)).and_then(|i| i.as_ref()) else {
            continue;
        };
        let decoded = loaded
            .asset(&icon.asset)
            .and_then(|b| b.ok_or_else(|| anyhow::anyhow!("not in the pack")))
            .and_then(|bytes| {
                // A glyph draws at ~20 pt: 256 px covers it at any scale.
                let img = image::load_from_memory(&bytes)?;
                Ok(if img.width() > 256 || img.height() > 256 {
                    img.thumbnail(256, 256)
                } else {
                    img
                }
                .into_rgba8())
            });
        match decoded {
            Ok(rgba) => {
                glyphs.insert(
                    name.to_string(),
                    super::apply::Glyph {
                        width: rgba.width(),
                        height: rgba.height(),
                        rgba: rgba.into_raw().into(),
                        template: icon.template(),
                        direction: manifest
                            .icon_directions
                            .as_ref()
                            .and_then(|d| d.get(*name))
                            .and_then(|w| super::pack::direction_angle(w)),
                    },
                );
            }
            Err(err) => tracing::warn!(name, "theme icon unreadable: {err:#}"),
        }
    }
    Ok(AppliedTheme {
        manifest: Arc::new(manifest.clone()),
        scheme,
        images,
        sizes,
        glyphs,
        requested,
    })
}
