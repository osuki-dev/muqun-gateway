//! Plugin discovery and external plugin runtime loader.
//!
//! Scans configured plugin search paths for native-view packages and standalone manifests,
//! parses their manifests, and instantiates [`ExternalPlugin`] which implements
//! [`DesktopPlugin`].

mod native_media;
mod native_selection;
mod native_view;
mod notifications;
mod panel;
mod web_surface;
pub(crate) use crate::platform::plugin_webview::webview_helper;

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};

use muqun_gateway::platform::runtimes::{self, RuntimeKind};
use rune::{col, el, icon, row, text, El, Icon};
use rune_kit::{Button, ButtonVariant, ControlSize, Markdown, ScrollBar};

use crate::config::Language;
use crate::diagnostics::{Attempt, EventKind, Outcome};
use crate::plugin::manifest::{
    PluginCategory, PluginCommandManifest, PluginManifest, PluginRenderKind, PluginSlot,
};
use crate::plugin::{DesktopPlugin, PluginCommand, PluginViewContext};
use crate::ui::copy::tr_ref;
use crate::ui::icons;
use crate::ui::terminal_panel;
use crate::ui::{ToastLevel, UiState};

/// Maximum execution time for a background Bun plugin command script.
const BUN_SCRIPT_TIMEOUT: Duration = Duration::from_secs(15);

/// Structured toast notification returned by a Bun plugin script on stdout.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BunToastResponse {
    pub title: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub level: Option<String>,
}

/// Structured JSON response returned by a Bun plugin script on stdout.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BunScriptResponse {
    #[serde(default)]
    pub toast: Option<BunToastResponse>,
    #[serde(default)]
    pub open_url: Option<String>,
    #[serde(default)]
    pub markdown: Option<String>,
    #[serde(default)]
    pub composer_text: Option<String>,
}

#[derive(Clone, Debug)]
struct ScriptOutput {
    media: native_media::Images,
    body: String,
    wrote_stderr: bool,
}

#[derive(Clone, Debug)]
struct ScriptFailure {
    reason: Outcome,
    detail: String,
}

struct ScriptJob {
    draft_target: Option<super::draft::Target>,
    receiver: mpsc::Receiver<Result<ScriptOutput, ScriptFailure>>,
    cancel: Arc<AtomicBool>,
    title: String,
    silent_on_success: bool,
    attempt: Attempt,
}

impl ScriptJob {
    fn cancel(self) {
        self.cancel.store(true, Ordering::Release);
        self.attempt.finish(Outcome::Cancelled);
    }
}

fn interner() -> &'static Mutex<HashSet<&'static str>> {
    static INTERNER: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    INTERNER.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Deduplicating static string interner for external plugin IDs, command IDs, and shortcuts.
/// Repeated package reloads reuse the same `'static` allocation instead of leaking memory.
fn intern_str(value: &str) -> &'static str {
    let mut set = interner()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(&existing) = set.get(value) {
        return existing;
    }
    let leaked: &'static str = Box::leak(value.to_owned().into_boxed_str());
    set.insert(leaked);
    leaked
}

/// An external file-based plugin loaded from a manifest.
pub struct ExternalPlugin {
    id: &'static str,
    manifest: PluginManifest,
    dir: PathBuf,
    commands: Vec<PluginCommand>,
    image_icon: Option<Arc<[u8]>>,
    dynamic_markdown: Mutex<Option<String>>,
    panels: Mutex<HashMap<String, panel::Panel>>,
    native_views: Mutex<HashMap<String, native_selection::Document>>,
    view_errors: Mutex<HashMap<String, String>>,
    tui_launched: Mutex<HashSet<String>>,
    jobs: Mutex<HashMap<Option<String>, ScriptJob>>,
    notifications: Mutex<notifications::Poll>,
    web_surfaces: Mutex<HashMap<String, super::webview_preview::Surface>>,
    /// Optional override for the Bun binary resolution:
    /// - `None`: resolve via `muqun_gateway::platform::runtimes::resolve_binary(RuntimeKind::Bun, None)`
    /// - `Some(Some(path))`: resolve with `configured = Some(path)`
    /// - `Some(None)`: force Bun unavailable (used for testing missing-runtime fallback)
    bun_binary_override: Mutex<Option<Option<PathBuf>>>,
}

impl Drop for ExternalPlugin {
    fn drop(&mut self) {
        for (_, job) in self.jobs.get_mut().unwrap().drain() {
            job.cancel();
        }
    }
}

impl ExternalPlugin {
    /// Construct an external plugin instance from a parsed manifest and directory.
    pub fn new(manifest: PluginManifest, dir: PathBuf) -> Self {
        let dir = if dir.is_absolute() {
            dir
        } else {
            std::env::current_dir().unwrap_or_default().join(dir)
        };
        let id: &'static str = intern_str(&manifest.id);
        let commands = manifest
            .commands
            .iter()
            .map(|c| {
                let cid: &'static str = intern_str(&c.id);
                let shortcut: Option<&'static str> = c.shortcut.as_deref().map(intern_str);
                PluginCommand {
                    id: cid,
                    title: c.title.clone(),
                    shortcut,
                }
            })
            .collect();

        let image_icon = manifest
            .icon
            .as_deref()
            .and_then(|path| load_image_icon(&dir, path));
        let notifications = Mutex::new(notifications::Poll::new(
            manifest.notification_entry.is_some(),
        ));
        Self {
            id,
            manifest,
            dir,
            commands,
            image_icon,
            dynamic_markdown: Mutex::new(None),
            native_views: Mutex::default(),
            view_errors: Mutex::default(),
            tui_launched: Mutex::default(),
            panels: Mutex::default(),
            jobs: Mutex::default(),
            notifications,
            web_surfaces: Mutex::default(),
            bun_binary_override: Mutex::new(None),
        }
    }

    /// Configure an explicit Bun binary path (or `None` to simulate missing Bun in tests).
    pub fn set_bun_binary_override(&self, override_bin: Option<Option<PathBuf>>) {
        *self.bun_binary_override.lock().unwrap() = override_bin;
    }

    /// Return the latest dynamic markdown produced by a Bun script execution, if any.
    pub fn dynamic_markdown(&self) -> Option<String> {
        self.dynamic_markdown.lock().unwrap().clone()
    }

    /// Load a plugin from an opted-in package or standalone manifest.
    pub fn from_dir(dir: &Path) -> anyhow::Result<Self> {
        Self::try_from_dir(dir)?.ok_or_else(|| anyhow::anyhow!("No plugin in {}", dir.display()))
    }

    fn try_from_dir(dir: &Path) -> anyhow::Result<Option<Self>> {
        let package_path = dir.join("package.json");
        let legacy_path = dir.join("plugin.json");
        let package = if package_path.is_file() {
            let data = std::fs::read(package_path)?;
            match serde_json::from_slice(&data) {
                Ok(value) => PluginManifest::from_package(value)?,
                Err(_) if legacy_path.is_file() => None,
                Err(err) => return Err(err.into()),
            }
        } else {
            None
        };
        let manifest = match package {
            Some(package) => package,
            None if legacy_path.is_file() => PluginManifest::from_file(legacy_path)?,
            None => return Ok(None),
        };
        anyhow::ensure!(
            !manifest.id.trim().is_empty(),
            "Empty plugin ID in {}",
            dir.display()
        );
        Ok(Some(Self::new(manifest, dir.to_path_buf())))
    }

    /// Access the underlying manifest.
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Access the directory where the plugin is located.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn resolve_script_path(&self, raw: &str) -> PathBuf {
        let trimmed = raw.strip_prefix("bun:").unwrap_or(raw).trim();
        let p = Path::new(trimmed);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.dir.join(p)
        }
    }

    fn is_script_reference(s: &str) -> bool {
        let lower = s.trim().to_ascii_lowercase();
        lower.starts_with("bun:")
            || lower.ends_with(".ts")
            || lower.ends_with(".js")
            || lower.ends_with(".mjs")
            || lower.ends_with(".tsx")
            || lower.ends_with(".jsx")
    }

    /// Determine whether a command should execute a Bun script, returning its script path.
    fn script_for_command(&self, cmd: &PluginCommandManifest) -> Option<PathBuf> {
        if let Some(script) = &cmd.script {
            if !script.trim().is_empty() {
                return Some(self.resolve_script_path(script));
            }
        }
        if let Some(action) = &cmd.action {
            if Self::is_script_reference(action) {
                return Some(self.resolve_script_path(action));
            }
        }
        if let PluginRenderKind::BunScript { entry } = self.manifest.render_kind() {
            return Some(self.resolve_script_path(&entry));
        }
        None
    }

    fn resolve_bun_binary(&self) -> Option<PathBuf> {
        let guard = self.bun_binary_override.lock().unwrap().clone();
        match guard {
            Some(None) => None,
            Some(Some(cfg)) => runtimes::resolve_binary(RuntimeKind::Bun, Some(&cfg)),
            None => {
                let env_override = std::env::var("MUQUN_BUN_BINARY")
                    .ok()
                    .map(PathBuf::from)
                    .filter(|p| !p.as_os_str().is_empty());
                runtimes::resolve_binary(RuntimeKind::Bun, env_override.as_deref())
            }
        }
    }

    /// Execute a Bun `.ts` / `.js` script and apply its output to `UiState`.
    fn execute_bun_script(
        &self,
        s: &mut UiState,
        command_id: &str,
        command_title: &str,
        script_path: &Path,
        silent_on_success: bool,
    ) -> bool {
        self.start_script(
            s,
            command_id,
            command_title,
            script_path,
            None,
            silent_on_success,
        )
    }

    fn launch_terminal_tui(&self, s: &mut UiState, command: &[String], cwd: Option<&str>) {
        let dir = cwd
            .map(|c| {
                let p = Path::new(c);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    self.dir.join(p)
                }
            })
            .unwrap_or_else(|| s.terminal_cwd());
        let cmd_str = command.join(" ");
        let run_cmd = (!cmd_str.is_empty()).then(|| format!("{cmd_str}\n"));
        s.term_new_tab_in(dir, run_cmd);
    }

    fn start_script(
        &self,
        s: &mut UiState,
        command_id: &str,
        command_title: &str,
        script_path: &Path,
        view: Option<(&str, &str)>,
        silent_on_success: bool,
    ) -> bool {
        let instance = view.map(|(id, _)| id.to_owned());
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.contains_key(&instance) {
            return true;
        }
        if let Some(inst) = &instance {
            self.view_errors.lock().unwrap().remove(inst);
        }
        let kind = match view {
            Some((_, "open")) => EventKind::PluginOpen,
            Some(_) => EventKind::PluginAction,
            None => EventKind::PluginCommand,
        };
        let mut attempt = Attempt::new(kind, self.id, Some(&self.manifest.version));
        attempt.bun_version = s
            .runtime_statuses
            .iter()
            .find(|st| st.kind == RuntimeKind::Bun)
            .and_then(|st| st.version.clone());
        let Some(bun_bin) = self.resolve_bun_binary() else {
            s.notify_diagnostic(attempt.finish(Outcome::RuntimeMissing),
                ToastLevel::Warning,
                format!("{}: Bun runtime required", self.manifest.name),
                Some(
                    "Bun is not installed. Open System & Diagnostics to install the managed Bun runtime."
                        .to_string(),
                ),
            );
            return true;
        };

        if !script_path.is_file() {
            s.notify_diagnostic(
                attempt.finish(Outcome::EntryMissing),
                ToastLevel::Error,
                format!("{}: Script not found", self.manifest.name),
                Some(format!("Missing script file `{}`", script_path.display())),
            );
            return true;
        }

        let workspace_cwd = s
            .current()
            .and_then(|i| i.directory.clone())
            .unwrap_or_else(|| s.terminal_cwd().display().to_string());
        let session_id = s.selected.clone().unwrap_or_default();

        let media_dir = view.and_then(|_| native_media::invocation_dir().ok());
        let mut context = serde_json::json!(super::context::web_context(s, true));
        context["capabilities"] = serde_json::json!({"webviewEntry":webview_helper().is_some(), "nativeLayout":true, "nativePresentation":true, "nativeImages":media_dir.is_some(), "fillLayout":true, "markdownAnchors":true,"nativeSelect":true});
        if let Some((instance, action)) = view {
            if let Some(fields) = s
                .plugin_fields
                .get(&(self.id.to_owned(), instance.to_owned()))
            {
                let values: serde_json::Map<_, _> = fields
                    .iter()
                    .filter(|((form, _), _)| form == action)
                    .map(|((_, name), edit)| {
                        (name.clone(), serde_json::Value::String(edit.text.clone()))
                    })
                    .collect();
                context["form"] = values.into();
            }
        }
        let encoded = context.to_string();
        if encoded.len() > 60 * 1024 {
            s.toast_error(
                self.manifest.name.clone(),
                Some("Form exceeds the 60 KiB submission limit".into()),
            );
            return true;
        }
        let mut command = ProcessCommand::new(&bun_bin);
        command.env_remove("MUQUN_PLUGIN_MEDIA_DIR");
        if let Some(dir) = &media_dir {
            command.env("MUQUN_PLUGIN_MEDIA_DIR", dir.path());
        }
        if let Ok(exe) = std::env::current_exe() {
            command.env("MUQUN_DESKTOP_EXECUTABLE", exe);
        }
        command
            .arg("run")
            .arg(script_path)
            .current_dir(&self.dir)
            .env("MUQUN_PLUGIN_CONTEXT", encoded)
            .env("MUQUN_PLUGIN_ID", self.id)
            .env("MUQUN_PLUGIN_DIR", self.dir.as_os_str())
            .env("MUQUN_WORKSPACE_CWD", &workspace_cwd)
            .env("MUQUN_SESSION_ID", &session_id)
            .env("MUQUN_COMMAND_ID", command_id)
            .env("MUQUN_SCRIPT_PATH", script_path.as_os_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some((_, action)) = view {
            command.env("MUQUN_PLUGIN_ACTION", action);
        }
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        jobs.insert(
            instance,
            ScriptJob {
                draft_target: super::draft::Target::capture(s),
                receiver: rx,
                cancel: cancel.clone(),
                title: command_title.into(),
                silent_on_success,
                attempt,
            },
        );
        std::thread::spawn(move || {
            let result = run_script(command, cancel).map(|mut output| {
                if let Some(dir) = &media_dir {
                    output.media = native_media::read_view(&output.body, dir.path());
                }
                output
            });
            let _ = tx.send(result);
        });
        true
    }

    fn apply_script_output(
        &self,
        s: &mut UiState,
        command_title: &str,
        stdout_str: String,
        silent_on_success: bool,
    ) {
        // Parse structured JSON response from full stdout or the last JSON line.
        let parsed_json = serde_json::from_str::<BunScriptResponse>(&stdout_str).or_else(|_| {
            stdout_str
                .lines()
                .rev()
                .find(|line| line.trim_start().starts_with('{'))
                .ok_or_else(|| serde::de::Error::custom("no json line"))
                .and_then(serde_json::from_str::<BunScriptResponse>)
        });

        if let Ok(resp) = parsed_json {
            if let Some(md) = resp.markdown {
                *self.dynamic_markdown.lock().unwrap() = Some(md);
            }
            if let Some(url) = resp.open_url {
                if !url.trim().is_empty() {
                    crate::platform::shell::open_url(&url);
                }
            }
            if let Some(composer_text) = resp.composer_text {
                if !composer_text.trim().is_empty() {
                    s.insert_into_composer(&composer_text);
                }
            }
            if let Some(toast) = resp.toast {
                let level = match toast
                    .level
                    .as_deref()
                    .unwrap_or("info")
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "success" | "ok" => ToastLevel::Success,
                    "warning" | "warn" => ToastLevel::Warning,
                    "error" | "err" | "danger" => ToastLevel::Error,
                    _ => ToastLevel::Info,
                };
                s.notify(level, toast.title, toast.detail);
            } else if !silent_on_success {
                s.notify(
                    ToastLevel::Success,
                    format!("{}: {}", self.manifest.name, command_title),
                    None,
                );
            }
        } else if !stdout_str.is_empty() {
            *self.dynamic_markdown.lock().unwrap() = Some(stdout_str.clone());
            if !silent_on_success {
                s.notify(
                    ToastLevel::Success,
                    format!("{}: {}", self.manifest.name, command_title),
                    Some(stdout_str),
                );
            }
        } else if !silent_on_success {
            s.notify(
                ToastLevel::Success,
                format!("{}: {}", self.manifest.name, command_title),
                None,
            );
        }
    }

    /// Attempt to load markdown content from dynamic output, configured tile entry, or README.md.
    fn load_markdown_content(&self) -> Option<String> {
        if let Some(dynamic) = self.dynamic_markdown() {
            return Some(dynamic);
        }

        if let Some(tile) = &self.manifest.tile {
            if tile.kind == "markdown"
                || tile
                    .entry
                    .as_ref()
                    .is_some_and(|e| e.ends_with(".md") || e.ends_with(".markdown"))
            {
                if let Some(entry) = &tile.entry {
                    let entry_path = if Path::new(entry).is_absolute() {
                        PathBuf::from(entry)
                    } else {
                        self.dir.join(entry)
                    };
                    return match std::fs::read_to_string(&entry_path) {
                        Ok(content) => Some(content),
                        Err(err) => Some(format!(
                            "# {}\n\n*Failed to read markdown file `{}`: {}*",
                            tile.title,
                            entry_path.display(),
                            err
                        )),
                    };
                } else {
                    return Some(format!("# {}\n\n{}", tile.title, self.manifest.description));
                }
            }
        }

        let readme = self.dir.join("README.md");
        if readme.is_file() {
            if let Ok(content) = std::fs::read_to_string(&readme) {
                return Some(content);
            }
        }

        None
    }
}

impl DesktopPlugin for ExternalPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self, _lang: Language) -> String {
        self.manifest.name.clone()
    }

    fn icon(&self) -> &'static Icon {
        resolve_icon(self.manifest.icon.as_deref())
    }

    fn render_icon(&self, s: &UiState, size: f32, color: rune::Color) -> El {
        if let (Some(bytes), Some(images)) = (&self.image_icon, &s.images) {
            let image = crate::ui::art::image(images, bytes);
            return el()
                .size(size, size)
                .pixels_fit(image.pixels(), rune::SurfaceFit::Fit);
        }
        icon(self.icon(), size).color(color)
    }

    fn description(&self, _lang: Language) -> Option<String> {
        Some(self.manifest.description.clone())
    }

    fn category(&self) -> PluginCategory {
        self.manifest.category()
    }

    fn keywords(&self) -> &[String] {
        &self.manifest.keywords
    }

    fn repository(&self) -> Option<&super::PluginRepository> {
        self.manifest.repository.as_ref()
    }

    fn surfaces(&self) -> Vec<PluginSlot> {
        self.manifest.supported_surfaces()
    }

    fn render_kind(&self) -> PluginRenderKind {
        self.manifest.render_kind()
    }

    fn supports_tile(&self) -> bool {
        self.supports_surface(PluginSlot::Tile)
    }

    fn render_tile(&self, ctx: &PluginViewContext<'_>, layer: &mut Vec<El>) -> Option<El> {
        let k = ctx.k;
        let s = ctx.s;
        let look = ctx.look;
        let lang = s.cfg.general.language;
        let tile_id = ctx.tile_id;
        let pid = self.id.to_string();
        let render_kind = self.manifest.render_kind();
        if let Some(surface) = self.web_surfaces.lock().unwrap().get(ctx.tile_id) {
            return Some(surface.view(k, ctx.scale));
        }

        if matches!(render_kind, PluginRenderKind::BunScript { .. })
            && self
                .manifest
                .tile
                .as_ref()
                .is_some_and(|t| t.kind == "native-view")
        {
            return Some(self.render_native_view(ctx, layer));
        }

        if let PluginRenderKind::WebView { entry } = &render_kind {
            let entry_str = entry.as_str();
            let title = self
                .manifest
                .tile
                .as_ref()
                .map(|t| t.title.as_str())
                .filter(|t| !t.trim().is_empty())
                .unwrap_or(self.manifest.name.as_str());
            let is_local = web_surface::local_panel_url(entry_str);
            let can_embed = is_local && webview_helper().is_some();
            let view_error = self.view_errors.lock().unwrap().get(tile_id).cloned();

            let mut card_body = col()
                .w_full()
                .px(14.0)
                .py(14.0)
                .gap(10.0)
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(icon(&icons::OPEN_EXTERNAL, 13.0).color(k.c.text_accent))
                        .child(
                            text(title)
                                .font(k.ty.ui_medium)
                                .color(k.c.text_base)
                                .grow(1.0)
                                .shrink()
                                .min_w(0.0)
                                .truncate(),
                        ),
                )
                .child(
                    text(entry_str)
                        .selectable(true)
                        .font(k.ty.mono)
                        .color(k.c.text_muted)
                        .wrap(),
                );

            if let Some(error) = view_error {
                card_body = card_body.child(
                    text(error)
                        .selectable(true)
                        .font(k.ty.small)
                        .color(k.c.danger.fg)
                        .wrap(),
                );
            }

            let mut actions = row().w_full().wrap().gap(8.0).center_y();
            if can_embed {
                let pid_retry = pid.clone();
                let inst_retry = tile_id.to_owned();
                actions = actions.child(
                    Button::new(tr_ref(lang, "Retry"))
                        .variant(ButtonVariant::Outline)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| {
                            if let Some(p) =
                                s.plugins.get_enabled(&pid_retry, &s.cfg.plugins_disabled)
                            {
                                p.on_view_action(s, &inst_retry, "open");
                            }
                        })
                        .build(k)
                        .key(format!("webview-retry-{tile_id}")),
                );
            }
            actions = actions.child(
                Button::new(tr_ref(lang, "Open in Browser"))
                    .variant(if can_embed {
                        ButtonVariant::Ghost
                    } else {
                        ButtonVariant::Outline
                    })
                    .control_size(ControlSize::Small)
                    .disabled(entry_str == "about:blank" || entry_str.trim().is_empty())
                    .on_press({
                        let url = entry_str.to_string();
                        move |_s: &mut UiState| {
                            crate::platform::shell::open_url(&url);
                        }
                    })
                    .build(k)
                    .key(format!("webview-open-browser-{tile_id}")),
            );
            card_body = card_body.child(actions);

            let mut content_col = col().w_full().max_w(720.0).px(24.0).py(20.0).gap(14.0);
            content_col = content_col.child(
                look.row_card(k, card_body, k.radius.md)
                    .key(format!("webview-card-{tile_id}")),
            );

            if !self.manifest.description.trim().is_empty() {
                content_col = content_col.child(
                    look.row_card(
                        k,
                        col()
                            .w_full()
                            .px(14.0)
                            .py(12.0)
                            .gap(6.0)
                            .child(
                                text(tr_ref(lang, "About"))
                                    .font(k.ty.caption())
                                    .color(k.c.text_muted),
                            )
                            .child(
                                text(&self.manifest.description)
                                    .font(k.ty.body)
                                    .color(k.c.text_base)
                                    .wrap(),
                            ),
                        k.radius.md,
                    ),
                );
            }

            if !self.manifest.commands.is_empty() {
                let is_running = self.jobs.lock().unwrap().contains_key(&None);
                let mut commands_col = col().w_full().gap(8.0).child(
                    text(tr_ref(lang, "Commands"))
                        .font(k.ty.caption())
                        .color(k.c.text_muted),
                );
                for cmd in &self.manifest.commands {
                    let cmd_id = cmd.id.clone();
                    let pid_clone = pid.clone();
                    let mut cmd_row = row()
                        .w_full()
                        .h(36.0)
                        .px(10.0)
                        .center_y()
                        .gap(10.0)
                        .fill(k.alpha(k.c.layer_01, 0.3))
                        .radius(k.radius.sm)
                        .border(1.0, k.alpha(k.c.border_weak, 0.3))
                        .child(icon(&icons::KEYBOARD, 12.0).color(k.c.icon_muted))
                        .child(text(&cmd.title).font(k.ty.small).color(k.c.text_base));
                    if let Some(sc) = &cmd.shortcut {
                        cmd_row =
                            cmd_row.child(text(sc).font(k.ty.caption()).color(k.c.text_faint));
                    }
                    cmd_row = cmd_row.child(el().grow(1.0)).child(
                        Button::new(tr_ref(lang, "Run"))
                            .disabled(is_running)
                            .variant(ButtonVariant::Ghost)
                            .control_size(ControlSize::Small)
                            .on_press(move |s: &mut UiState| {
                                if let Some(p) = s.plugins.get(&pid_clone) {
                                    p.on_command(s, &cmd_id);
                                }
                            })
                            .build(k),
                    );
                    commands_col = commands_col.child(cmd_row);
                }
                content_col = content_col.child(commands_col);
            }

            return Some(
                col()
                    .key(format!("ext-plugin-webview-{}-{}", self.id, tile_id))
                    .grow(1.0)
                    .w_full()
                    .min_h(0.0)
                    .items_center()
                    .child(content_col)
                    .scroll(rune::ScrollSpec {
                        stick_bottom: false,
                    })
                    .scroll_bar(k),
            );
        }

        if let PluginRenderKind::TerminalTui { command, cwd } = &render_kind {
            let cmd_str = command.join(" ");
            let cwd_opt = cwd.clone();
            let pid_launch = pid.clone();
            let command_badge = row()
                .px(8.0)
                .py(3.0)
                .gap(6.0)
                .center_y()
                .shrink()
                .min_w(0.0)
                .fill(k.alpha(k.c.layer_02, 0.55))
                .radius(k.radius.sm)
                .border(1.0, k.alpha(k.c.border_weak, 0.35))
                .child(icon(&terminal_panel::TERMINAL_WINDOW, 12.0).color(k.c.text_accent))
                .child(
                    text(if cmd_str.is_empty() {
                        tr_ref(lang, "Interactive Terminal TUI").to_string()
                    } else {
                        format!("$ {cmd_str}")
                    })
                    .font(k.ty.mono)
                    .color(k.c.text_base)
                    .single_line()
                    .truncate(),
                );
            let mut tui_bar = row()
                .key(format!("tui-bar-{tile_id}"))
                .w_full()
                .h(36.0)
                .px(10.0)
                .gap(8.0)
                .center_y()
                .fill(k.alpha(k.c.layer_01, 0.28))
                .child(command_badge);
            if let Some(dir) = cwd_opt.as_deref().filter(|d| !d.trim().is_empty()) {
                tui_bar = tui_bar.child(
                    text(dir)
                        .font(k.ty.caption())
                        .color(k.c.text_muted)
                        .single_line()
                        .truncate()
                        .shrink()
                        .min_w(0.0),
                );
            }
            tui_bar = tui_bar.child(el().grow(1.0)).child(
                Button::new(tr_ref(lang, "Launch in Terminal"))
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| {
                        if let Some(p) = s.plugins.get(&pid_launch) {
                            p.on_command(s, "__run_entry__");
                        } else {
                            let dir = cwd_opt
                                .as_ref()
                                .map(PathBuf::from)
                                .unwrap_or_else(|| s.terminal_cwd());
                            let run_cmd = if cmd_str.is_empty() {
                                None
                            } else {
                                Some(format!("{cmd_str}\n"))
                            };
                            s.term_new_tab_in(dir, run_cmd);
                        }
                    })
                    .build(k)
                    .key(format!("tui-launch-{tile_id}")),
            );

            let scheme = if k.dark {
                crate::theme::color_scheme::ColorScheme::Dark
            } else {
                crate::theme::color_scheme::ColorScheme::Light
            };
            let term_view = terminal_panel::tile_body(s, k, scheme, ctx.scale, layer);
            return Some(
                col()
                    .key(format!("ext-plugin-tui-{}-{}", self.id, tile_id))
                    .grow(1.0)
                    .w_full()
                    .min_h(0.0)
                    .child(tui_bar)
                    .child(term_view),
            );
        }

        let is_running = self.jobs.lock().unwrap().contains_key(&None);
        let has_dynamic_md = self.dynamic_markdown.lock().unwrap().is_some();
        let initial_bun_loading = matches!(render_kind, PluginRenderKind::BunScript { .. })
            && is_running
            && !has_dynamic_md;
        let md_content = self.load_markdown_content();

        let body = if initial_bun_loading || md_content.is_some() {
            let mut col_inner = col().w_full().max_w(760.0).px(24.0).py(16.0).gap(12.0);

            if !self.manifest.commands.is_empty()
                || matches!(render_kind, PluginRenderKind::BunScript { .. })
            {
                let mut cmd_bar = row()
                    .w_full()
                    .wrap()
                    .gap(8.0)
                    .center_y()
                    .px(12.0)
                    .py(8.0)
                    .fill(k.alpha(k.c.layer_01, 0.3))
                    .radius(k.radius.sm)
                    .border(1.0, k.alpha(k.c.border_weak, 0.3));

                if let PluginRenderKind::BunScript { entry } = &render_kind {
                    let entry_path = self.resolve_script_path(entry);
                    let entry_covered_by_cmd = self.manifest.commands.iter().any(|c| {
                        self.script_for_command(c).as_deref() == Some(entry_path.as_path())
                    });
                    if !entry_covered_by_cmd {
                        let pid_run = pid.clone();
                        cmd_bar = cmd_bar.child(
                            Button::new(format!("{} ({entry})", tr_ref(lang, "Run Script")))
                                .disabled(is_running)
                                .variant(ButtonVariant::Outline)
                                .control_size(ControlSize::Small)
                                .on_press(move |s: &mut UiState| {
                                    if let Some(p) = s.plugins.get(&pid_run) {
                                        p.on_command(s, "__run_entry__");
                                    }
                                })
                                .build(k)
                                .key(format!("bun-run-entry-{tile_id}")),
                        );
                    }
                }

                for (index, cmd) in self.manifest.commands.iter().enumerate() {
                    let cmd_id = cmd.id.clone();
                    let pid_clone = pid.clone();
                    let variant = if index == 0
                        && matches!(render_kind, PluginRenderKind::BunScript { .. })
                    {
                        ButtonVariant::Outline
                    } else {
                        ButtonVariant::Ghost
                    };
                    let btn = Button::new(&cmd.title)
                        .disabled(is_running)
                        .variant(variant)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| {
                            if let Some(p) = s.plugins.get(&pid_clone) {
                                p.on_command(s, &cmd_id);
                            }
                        })
                        .build(k);
                    cmd_bar = cmd_bar.child(btn.key(format!("plugin-command-{}", cmd.id)));
                }
                if is_running {
                    cmd_bar = cmd_bar.child(
                        text(tr_ref(lang, "Loading…"))
                            .font(k.ty.small)
                            .color(k.c.text_muted),
                    );
                }
                col_inner = col_inner.child(cmd_bar);
            }

            if initial_bun_loading {
                let mut loading_card = col().w_full().px(16.0).py(16.0).gap(6.0).child(
                    row()
                        .gap(8.0)
                        .center_y()
                        .child(icon(&icons::REFRESH, 13.0).color(k.c.text_accent))
                        .child(
                            text(tr_ref(lang, "Loading…"))
                                .font(k.ty.ui_medium)
                                .color(k.c.text_base),
                        ),
                );
                if !self.manifest.description.trim().is_empty() {
                    loading_card = loading_card.child(
                        text(&self.manifest.description)
                            .font(k.ty.small)
                            .color(k.c.text_muted)
                            .wrap(),
                    );
                }
                col_inner = col_inner.child(
                    look.row_card(k, loading_card, k.radius.md)
                        .key(format!("bun-loading-{tile_id}")),
                );
            } else if let Some(content) = md_content {
                col_inner = col_inner.child(
                    Markdown::new(content)
                        .selectable(true)
                        .on_link_tx(|s: &mut UiState, url: &str, tx: &mut rune::Tx| {
                            crate::ui::open_markdown_link(s, url, tx);
                        })
                        .build(k),
                );
            }

            col()
                .key(format!("ext-plugin-md-{tile_id}"))
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(col().w_full().items_center().child(col_inner))
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_bar(k)
        } else {
            let mut content_col = col().w_full().max_w(720.0).px(24.0).py(20.0).gap(16.0);

            let desc_card = look.row_card(
                k,
                col()
                    .w_full()
                    .px(14.0)
                    .py(14.0)
                    .gap(6.0)
                    .child(
                        text(tr_ref(lang, "About"))
                            .font(k.ty.caption())
                            .color(k.c.text_muted),
                    )
                    .child(
                        text(&self.manifest.description)
                            .font(k.ty.body)
                            .color(k.c.text_base)
                            .wrap(),
                    ),
                k.radius.md,
            );
            content_col = content_col.child(desc_card);

            if let PluginRenderKind::BunScript { entry } = &render_kind {
                let pid_run = pid.clone();
                let bun_card = look.row_card(
                    k,
                    row()
                        .w_full()
                        .px(14.0)
                        .py(12.0)
                        .gap(10.0)
                        .center_y()
                        .child(icon(&icons::RUNNING, 13.0).color(k.c.text_accent))
                        .child(
                            col()
                                .grow(1.0)
                                .gap(2.0)
                                .child(
                                    text(tr_ref(lang, "Bun Script Host"))
                                        .font(k.ty.ui_medium)
                                        .color(k.c.text_base),
                                )
                                .child(
                                    text(format!("Entry: {entry}"))
                                        .font(k.ty.mono)
                                        .color(k.c.text_muted),
                                ),
                        )
                        .child(
                            Button::new(tr_ref(lang, "Run"))
                                .disabled(is_running)
                                .variant(ButtonVariant::Outline)
                                .control_size(ControlSize::Small)
                                .on_press(move |s: &mut UiState| {
                                    if let Some(p) = s.plugins.get(&pid_run) {
                                        p.on_command(s, "__run_entry__");
                                    }
                                })
                                .build(k)
                                .key(format!("bun-card-run-{tile_id}")),
                        ),
                    k.radius.md,
                );
                content_col = content_col.child(bun_card);
            }

            if !self.manifest.commands.is_empty() {
                let mut commands_col = col().w_full().gap(8.0).child(
                    text(tr_ref(lang, "Commands"))
                        .font(k.ty.caption())
                        .color(k.c.text_muted),
                );

                for cmd in &self.manifest.commands {
                    let cmd_id = cmd.id.clone();
                    let pid_clone = pid.clone();
                    let mut cmd_row = row()
                        .w_full()
                        .h(36.0)
                        .px(10.0)
                        .center_y()
                        .gap(10.0)
                        .fill(k.alpha(k.c.layer_01, 0.3))
                        .radius(k.radius.sm)
                        .border(1.0, k.alpha(k.c.border_weak, 0.3))
                        .child(icon(&icons::KEYBOARD, 12.0).color(k.c.icon_muted))
                        .child(text(&cmd.title).font(k.ty.small).color(k.c.text_base));

                    if let Some(sc) = &cmd.shortcut {
                        cmd_row =
                            cmd_row.child(text(sc).font(k.ty.caption()).color(k.c.text_faint));
                    }

                    cmd_row = cmd_row.child(el().grow(1.0));

                    let run_btn = Button::new(tr_ref(lang, "Run"))
                        .disabled(is_running)
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| {
                            if let Some(p) = s.plugins.get(&pid_clone) {
                                p.on_command(s, &cmd_id);
                            }
                        })
                        .build(k);

                    cmd_row = cmd_row.child(run_btn);
                    commands_col = commands_col.child(cmd_row);
                }

                content_col = content_col.child(commands_col);
            }

            col()
                .key(format!("ext-plugin-content-{tile_id}"))
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .items_center()
                .child(content_col)
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_bar(k)
        };

        Some(
            col()
                .key(format!("ext-plugin-tile-{}-{}", self.id, tile_id))
                .grow(1.0)
                .w_full()
                .min_h(0.0)
                .child(body),
        )
    }

    fn commands(&self) -> Vec<PluginCommand> {
        self.commands.clone()
    }

    fn enabled_changed(&self, enabled: bool) {
        if !enabled {
            *self.notifications.lock().unwrap() = Default::default();
            for (_, job) in self.jobs.lock().unwrap().drain() {
                job.cancel();
            }
            self.panels.lock().unwrap().clear();
            self.native_views.lock().unwrap().clear();
            self.view_errors.lock().unwrap().clear();
            self.tui_launched.lock().unwrap().clear();
            self.web_surfaces.lock().unwrap().clear();
        } else if self.manifest.notification_entry.is_some() {
            self.notifications.lock().unwrap().armed = true;
        }
    }

    fn poll(&self, s: &mut UiState, tx: &mut rune::Tx) -> bool {
        if !super::is_enabled(self.id, &s.cfg.plugins_disabled) {
            self.enabled_changed(false);
            return false;
        }
        let notified = self.poll_notifications(s, tx);
        let panel_changed = self.poll_panels(s, tx) || notified;
        let web_changed = self.poll_web_surfaces(s) || panel_changed;
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.is_empty() {
            return web_changed;
        }
        let mut completed = Vec::new();
        for (instance, job) in jobs.iter() {
            let result = match job.receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => continue,
                Err(mpsc::TryRecvError::Disconnected) => Err(ScriptFailure {
                    reason: Outcome::ProcessFailed,
                    detail: "Plugin worker stopped".into(),
                }),
            };
            completed.push((instance.clone(), result));
        }
        let completed: Vec<_> = completed
            .into_iter()
            .map(|(instance, result)| {
                let job = jobs.remove(&instance).unwrap();
                (instance, job, result)
            })
            .collect();
        if !jobs.is_empty() {
            tx.wake_after(Duration::from_millis(16));
        }
        drop(jobs);
        let changed = !completed.is_empty();
        for (instance, job, result) in completed {
            let mut attempt = job.attempt;
            let failed_instance = instance.clone();
            let outcome =
                result.and_then(|output| {
                    attempt.wrote_stderr = output.wrote_stderr;
                    let media = output.media;
                    let output = output.body;
                    if let Some(instance) = instance {
                        let parsed = serde_json::from_str::<native_view::Response>(&output)
                            .map_err(|_| ScriptFailure {
                                reason: Outcome::InvalidResponse,
                                detail: "Expected one native view response on stdout".into(),
                            })?;
                        if parsed.version != 1 {
                            return Err(ScriptFailure {
                                reason: Outcome::InvalidResponse,
                                detail: format!("Unsupported view version {}", parsed.version),
                            });
                        }
                        let fields = parsed.view.fields(&media)?;
                        if let native_view::Node::WebView { entry } = &parsed.view {
                            self.start_panel(s, &instance, entry, job.draft_target)
                                .map_err(|detail| ScriptFailure {
                                    reason: Outcome::ProcessFailed,
                                    detail,
                                })?;
                            tx.wake_after(Duration::from_millis(16));
                        } else if self.panels.lock().unwrap().remove(&instance).is_some() {
                            self.clear_web_instance(&instance);
                        }
                        s.plugin_fields
                            .insert((self.id.to_owned(), instance.clone()), fields);
                        self.view_errors.lock().unwrap().remove(&instance);
                        self.native_views
                            .lock()
                            .unwrap()
                            .insert(instance, native_selection::Document::new(parsed.view));
                    } else {
                        self.apply_script_output(s, &job.title, output, job.silent_on_success);
                    }
                    Ok(())
                });
            let event = attempt.finish(
                outcome
                    .as_ref()
                    .err()
                    .map_or(Outcome::Completed, |err| err.reason),
            );
            if let Err(err) = outcome {
                if let Some(instance) = failed_instance {
                    self.view_errors
                        .lock()
                        .unwrap()
                        .insert(instance, err.detail.clone());
                }
                s.notify_diagnostic(
                    event,
                    ToastLevel::Error,
                    self.manifest.name.clone(),
                    Some(err.detail),
                );
            }
        }
        web_changed || changed
    }

    fn open_terminal_entry(&self, s: &mut UiState, entry: &str) {
        let Some((root, path)) = self
            .dir
            .canonicalize()
            .ok()
            .zip(self.dir.join(entry).canonicalize().ok())
        else {
            s.toast_error(
                self.manifest.name.clone(),
                Some("Terminal entry is unavailable".into()),
            );
            return;
        };
        if !path.starts_with(&root)
            || !path.is_file()
            || path.to_string_lossy().chars().any(char::is_control)
        {
            s.toast_error(
                self.manifest.name.clone(),
                Some("Invalid terminal entry".into()),
            );
            return;
        }
        let Some(bun) = self.resolve_bun_binary() else {
            s.toast_error(
                self.manifest.name.clone(),
                Some("Bun runtime required".into()),
            );
            return;
        };
        let quote = crate::ui::terminal_panel::shell_quote_path;
        s.term_new_tab_in(
            root,
            Some(format!("{} run {}\n", quote(&bun), quote(&path))),
        );
    }

    fn can_move_view(&self, instance: &str) -> bool {
        self.web_surfaces.lock().unwrap().contains_key(instance)
            || self.has_panel_service(instance)
            || self.native_views.lock().unwrap().contains_key(instance)
            || self.view_errors.lock().unwrap().contains_key(instance)
            || self.tui_launched.lock().unwrap().contains(instance)
            || self
                .jobs
                .lock()
                .unwrap()
                .contains_key(&Some(instance.to_owned()))
    }

    fn move_view(&self, from: &str, to: &str) {
        let mut surfaces = self.web_surfaces.lock().unwrap();
        if let Some(surface) = surfaces.remove(from) {
            surfaces.insert(to.to_owned(), surface);
        }
        drop(surfaces);
        self.move_panel_service(from, to);
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.remove(&Some(from.to_owned())) {
            jobs.insert(Some(to.to_owned()), job);
        }
        drop(jobs);
        let mut views = self.native_views.lock().unwrap();
        if let Some(node) = views.remove(from) {
            views.insert(to.to_owned(), node);
        }
        drop(views);
        let mut errors = self.view_errors.lock().unwrap();
        if let Some(error) = errors.remove(from) {
            errors.insert(to.to_owned(), error);
        }
        drop(errors);
        let mut launched = self.tui_launched.lock().unwrap();
        if launched.remove(from) {
            launched.insert(to.to_owned());
        }
    }

    fn close_view(&self, instance: &str) {
        self.clear_web_instance(instance);
        if let Some(job) = self.jobs.lock().unwrap().remove(&Some(instance.to_owned())) {
            job.cancel();
        }
        self.panels.lock().unwrap().remove(instance);
        self.native_views.lock().unwrap().remove(instance);
        self.view_errors.lock().unwrap().remove(instance);
        self.tui_launched.lock().unwrap().remove(instance);
    }

    fn on_view_action(&self, s: &mut UiState, instance: &str, action: &str) -> bool {
        if !super::is_enabled(self.id, &s.cfg.plugins_disabled) {
            return false;
        }
        if self.manifest.notification_entry.is_some() {
            self.notifications.lock().unwrap().armed = true;
        }
        match self.manifest.render_kind() {
            PluginRenderKind::WebView { entry } => {
                if action == "open"
                    && webview_helper().is_some()
                    && web_surface::local_panel_url(&entry)
                {
                    self.view_errors.lock().unwrap().remove(instance);
                    if let Err(reason) = self.open_web_url(s, instance, &entry, false, None) {
                        self.view_errors
                            .lock()
                            .unwrap()
                            .insert(instance.to_owned(), reason.clone());
                        s.toast_error(self.manifest.name.clone(), Some(reason));
                    }
                    return true;
                }
                false
            }
            PluginRenderKind::TerminalTui { command, cwd } => {
                if action == "open"
                    && self
                        .tui_launched
                        .lock()
                        .unwrap()
                        .insert(instance.to_owned())
                {
                    self.launch_terminal_tui(s, &command, cwd.as_deref());
                    return true;
                }
                false
            }
            PluginRenderKind::BunScript { entry } => {
                if self
                    .manifest
                    .tile
                    .as_ref()
                    .is_some_and(|t| t.kind == "native-view")
                {
                    return self.start_script(
                        s,
                        "view",
                        &self.manifest.name,
                        &self.resolve_script_path(&entry),
                        Some((instance, action)),
                        false,
                    );
                }
                if action == "open"
                    && self.dynamic_markdown.lock().unwrap().is_none()
                    && !self.jobs.lock().unwrap().contains_key(&None)
                {
                    if let Some((cmd_id, cmd_title, script_path)) =
                        self.manifest.commands.iter().find_map(|cmd| {
                            self.script_for_command(cmd)
                                .map(|path| (cmd.id.as_str(), cmd.title.as_str(), path))
                        })
                    {
                        return self.execute_bun_script(s, cmd_id, cmd_title, &script_path, true);
                    }
                    let script_path = self.resolve_script_path(&entry);
                    return self.execute_bun_script(
                        s,
                        "__run_entry__",
                        &self.manifest.name,
                        &script_path,
                        true,
                    );
                }
                false
            }
            PluginRenderKind::Native { .. } => false,
        }
    }

    fn on_notification_action(&self, s: &mut UiState, action: &str) {
        if self.manifest.notification_entry.is_some() {
            s.open_plugin_page(self.id);
            self.notifications.lock().unwrap().pending = Some(action.to_owned());
        }
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        if !super::is_enabled(self.id, &s.cfg.plugins_disabled) {
            return false;
        }
        if command_id == "__run_entry__" {
            match self.manifest.render_kind() {
                PluginRenderKind::BunScript { entry } => {
                    let script_path = self.resolve_script_path(&entry);
                    return self.execute_bun_script(
                        s,
                        command_id,
                        &self.manifest.name,
                        &script_path,
                        false,
                    );
                }
                PluginRenderKind::TerminalTui { command, cwd } => {
                    self.launch_terminal_tui(s, &command, cwd.as_deref());
                    return true;
                }
                _ => return false,
            }
        }

        if let Some(cmd) = self.manifest.commands.iter().find(|c| c.id == command_id) {
            if let Some(script_path) = self.script_for_command(cmd) {
                return self.execute_bun_script(s, command_id, &cmd.title, &script_path, false);
            }
            if let Some(action) = &cmd.action {
                if action.starts_with("http://") || action.starts_with("https://") {
                    crate::platform::shell::open_url(action);
                }
            }
            s.toast(
                format!("{}: {}", self.manifest.name, cmd.title),
                cmd.action.clone(),
            );
            true
        } else {
            false
        }
    }
}

/// Drain both pipes concurrently: a verbose child must never block on a full pipe.
fn run_script(
    mut command: ProcessCommand,
    cancel: Arc<AtomicBool>,
) -> Result<ScriptOutput, ScriptFailure> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| ScriptFailure {
        reason: Outcome::ProcessFailed,
        detail: e.to_string(),
    })?;
    fn drain(mut pipe: impl Read, cap: usize) -> String {
        let mut bytes = Vec::new();
        let _ = pipe.by_ref().take(cap as u64).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
        String::from_utf8_lossy(&bytes).trim().to_string()
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || drain(stdout, 64 * 1024));
    let err = std::thread::spawn(move || drain(stderr, 16 * 1024));
    let deadline = Instant::now() + BUN_SCRIPT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if !cancel.load(Ordering::Acquire) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            result => {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                let (reason, detail) = if let Err(err) = result {
                    (Outcome::ProcessFailed, err.to_string())
                } else if cancel.load(Ordering::Acquire) {
                    (Outcome::Cancelled, "Plugin command cancelled".into())
                } else {
                    (Outcome::TimedOut, "Plugin command timed out".into())
                };
                break Err(ScriptFailure { reason, detail });
            }
        }
    };
    // A completed script must not leave descendants holding its pipes open.
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if status?.success() {
        Ok(ScriptOutput {
            media: HashMap::new(),
            body: stdout,
            wrote_stderr: !stderr.is_empty(),
        })
    } else {
        Err(ScriptFailure {
            reason: Outcome::ProcessFailed,
            detail: if stderr.is_empty() {
                "Plugin process exited unsuccessfully".into()
            } else {
                stderr
            },
        })
    }
}

/// Decode once at discovery. Names keep their vector meaning; image paths stay inside the package.
fn load_image_icon(dir: &Path, value: &str) -> Option<Arc<[u8]>> {
    let path = Path::new(value);
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp") {
        return None;
    }
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    let root = dir.canonicalize().ok()?;
    let path = root.join(path).canonicalize().ok()?;
    if !path.starts_with(&root) {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > 1_048_576 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 1_048_576 {
        return None;
    }
    let dimensions = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if dimensions.0 == 0 || dimensions.1 == 0 || dimensions.0 > 1024 || dimensions.1 > 1024 {
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1024);
    limits.max_image_height = Some(1024);
    limits.max_alloc = Some(8 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().ok()?.thumbnail(256, 256);
    let mut encoded = std::io::Cursor::new(Vec::new());
    decoded
        .write_to(&mut encoded, image::ImageFormat::Png)
        .ok()?;
    Some(encoded.into_inner().into())
}

/// Resolve icon string from manifest to static Rune Icon.
pub fn resolve_icon(name: Option<&str>) -> &'static Icon {
    match name.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("terminal") | Some("shell") | Some("console") => &terminal_panel::TERMINAL_WINDOW,
        Some("task") | Some("notes") | Some("todo") | Some("note") => &icons::TASK,
        Some("settings") | Some("config") | Some("preferences") => &icons::SETTINGS,
        Some("branch") | Some("diff") | Some("git") | Some("vcs") => &icons::BRANCH,
        Some("running") | Some("play") | Some("run") => &icons::RUNNING,
        Some("keyboard") | Some("command") | Some("shortcuts") => &icons::KEYBOARD,
        Some("agent") | Some("bot") | Some("ai") => &icons::AGENT,
        Some("info") | Some("information") | Some("help") => &icons::INFORMATION,
        Some("external") | Some("open_external") | Some("link") | Some("browser") => {
            &icons::OPEN_EXTERNAL
        }
        Some("controls") | Some("sliders") => &icons::CONTROLS,
        Some("appearance") | Some("theme") | Some("paint") => &icons::APPEARANCE,
        Some("storage") | Some("database") | Some("disk") => &icons::STORAGE,
        Some("refresh") | Some("reload") | Some("sync") => &icons::REFRESH,
        Some("grid") | Some("dashboard") | Some("table") => &icons::GRID,
        Some("fork") => &icons::FORK,
        Some("inbox") => &icons::INBOX,
        _ => &icons::GRID,
    }
}

/// Returns the standard directories scanned for external plugins.
pub fn default_plugin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(config_dir) = dirs::config_dir() {
        dirs.push(config_dir.join("muqun-desktop").join("plugins"));
    }
    if let Some(data_dir) = dirs::data_dir() {
        dirs.push(data_dir.join("muqun-desktop").join("plugins"));
    }
    dirs.extend(crate::platform::bundled_plugin_dirs());

    let local_assets = PathBuf::from("desktop/assets/plugins");
    if local_assets.is_dir() {
        dirs.push(local_assets);
    }
    let crate_assets = PathBuf::from("assets/plugins");
    if crate_assets.is_dir() && !dirs.contains(&crate_assets) {
        dirs.push(crate_assets);
    }

    dirs
}

/// Discover external plugins located in the specified search directories.
pub fn discover_plugins(plugin_dirs: &[PathBuf]) -> Vec<ExternalPlugin> {
    let mut plugins = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    let mut load = |path: &Path| match ExternalPlugin::try_from_dir(path) {
        Ok(Some(plugin)) => {
            if seen_ids.insert(plugin.manifest().id.clone()) {
                plugins.push(plugin);
            }
            true
        }
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "Failed to load external plugin");
            false
        }
    };

    for base_dir in plugin_dirs {
        if load(base_dir) {
            continue;
        }
        if let Ok(children) = std::fs::read_dir(base_dir) {
            let mut entries: Vec<_> = children.flatten().collect();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                if entry.path().is_dir() {
                    load(&entry.path());
                }
            }
        }
    }

    plugins
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rediscovery_reads_native_plugin_version_from_package() {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let path = root.join("example");
        std::fs::create_dir_all(&path).unwrap();
        let package = path.join("package.json");
        std::fs::write(&package, r#"{"name":"ordinary"}"#).unwrap();
        let paths = [root.clone()];
        assert!(discover_plugins(&paths).is_empty());
        std::fs::write(
            path.join("plugin.json"),
            r#"{"id":"legacy","name":"Legacy","version":"0.1.0","description":""}"#,
        )
        .unwrap();
        for version in ["1.2.3", "2.0.0"] {
            let value = serde_json::json!({"name":"example","main":"src/main.ts","version":version,"muqun":{}});
            std::fs::write(&package, value.to_string()).unwrap();
            let plugins = discover_plugins(&paths);
            assert_eq!(plugins.len(), 1);
            assert_eq!(plugins[0].manifest().id, "example");
            assert_eq!(plugins[0].manifest().version, version);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn image_icons_are_bounded_local_assets_and_cached_after_discovery() {
        let root = std::env::temp_dir().join(format!("muqun-plugin-icon-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("assets")).unwrap();
        let icon_path = root.join("assets/icon.png");
        image::RgbaImage::from_pixel(512, 256, image::Rgba([30, 120, 220, 255]))
            .save(&icon_path)
            .unwrap();
        let icon = load_image_icon(&root, "assets/icon.png").unwrap();
        let decoded = image::load_from_memory(&icon).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (256, 128));
        let manifest = PluginManifest::parse(r#"{"id":"image-icon","name":"Image","version":"1","description":"","icon":"assets/icon.png"}"#).unwrap();
        let plugin = ExternalPlugin::new(manifest, root.clone());
        std::fs::remove_file(&icon_path).unwrap();
        assert!(
            plugin.image_icon.is_some(),
            "discovery must cache the image instead of reading files on UI frames"
        );
        assert!(load_image_icon(&root, "assets/icon.png").is_none());
        assert!(load_image_icon(&root, "../outside.png").is_none());
        assert!(load_image_icon(&root, &root.join("outside.png").to_string_lossy()).is_none());
        std::fs::write(&icon_path, b"not an image").unwrap();
        assert!(load_image_icon(&root, "assets/icon.png").is_none());
        image::RgbaImage::new(2048, 1).save(&icon_path).unwrap();
        assert!(
            load_image_icon(&root, "assets/icon.png").is_none(),
            "dimensions must be checked before decoding"
        );
        std::fs::write(&icon_path, vec![0; 1_048_577]).unwrap();
        assert!(load_image_icon(&root, "assets/icon.png").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn an_image_icon_symlink_cannot_escape_the_plugin_directory() {
        let root =
            std::env::temp_dir().join(format!("muqun-plugin-icon-link-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("plugin/assets")).unwrap();
        image::RgbaImage::new(16, 16)
            .save(root.join("outside.png"))
            .unwrap();
        std::os::unix::fs::symlink(
            root.join("outside.png"),
            root.join("plugin/assets/icon.png"),
        )
        .unwrap();
        assert!(load_image_icon(&root.join("plugin"), "assets/icon.png").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn closing_one_view_discards_its_worker_while_another_submits_its_own_form() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"name":"test-views","version":"1","main":"entry.ts","muqun":{"requires":["forms"]}}"#).unwrap();
        std::fs::write(dir.join("entry.ts"), "").unwrap();
        let bun = dir.join("bun");
        std::fs::write(
            &bun,
            r#"#!/bin/sh
if [ "$MUQUN_PLUGIN_ACTION" = slow ]; then exec sleep 30; fi
printf '%s' "$MUQUN_PLUGIN_CONTEXT" > submitted.json
printf '%s' '{"version":1,"view":{"type":"text","text":"saved"}}'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&bun, std::fs::Permissions::from_mode(0o755)).unwrap();
        let plugin = ExternalPlugin::from_dir(&dir).unwrap();
        plugin.set_bun_binary_override(Some(Some(bun)));
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.plugin_fields.insert(
            ("test-views".into(), "drawer".into()),
            HashMap::from([
                (
                    ("save".into(), "body".into()),
                    rune::TextEdit::new("drawer draft"),
                ),
                (
                    ("other".into(), "secret".into()),
                    rune::TextEdit::new("other form"),
                ),
            ]),
        );
        assert!(plugin.on_view_action(&mut s, "page", "slow"));
        assert!(plugin.on_view_action(&mut s, "drawer", "save"));
        assert_eq!(plugin.jobs.lock().unwrap().len(), 2);
        plugin.close_view("page");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !plugin.poll(&mut s, &mut rune::Tx::default()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("submitted.json")).unwrap()).unwrap();
        assert_eq!(data["form"], serde_json::json!({"body":"drawer draft"}));
        let views = plugin.native_views.lock().unwrap();
        assert!(views.contains_key("drawer"));
        assert!(!views.contains_key("page"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn disabling_a_plugin_cancels_its_running_process() {
        let dir =
            std::env::temp_dir().join(format!("muqun-plugin-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        let mut command = ProcessCommand::new("/bin/sh");
        command
            .args(["-c", "echo $$ > pid; exec sleep 30"])
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let manifest: PluginManifest = serde_json::from_str(
            r#"{"id":"cancel-test","name":"Cancel","version":"1","description":""}"#,
        )
        .unwrap();
        let plugin = Arc::new(ExternalPlugin::new(manifest, dir.clone()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        plugin.jobs.lock().unwrap().insert(
            None,
            ScriptJob {
                draft_target: None,
                receiver,
                cancel: cancel.clone(),
                title: String::new(),
                silent_on_success: false,
                attempt: Attempt::new(EventKind::PluginCommand, "cancel-test", None),
            },
        );
        let worker = std::thread::spawn(move || {
            let result = run_script(command, cancel);
            let _ = sender.send(result.clone());
            result
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !pid_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let (tx, _) = crate::model::commands::channel();
        let mut state = UiState::new(tx);
        state.plugins.register(plugin.clone());
        state.edit(crate::config::ConfigEdit::PluginEnabled {
            plugin: "cancel-test".into(),
            on: false,
        });
        assert_eq!(
            worker.join().unwrap().unwrap_err().detail,
            "Plugin command cancelled"
        );
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert!(plugin.jobs.lock().unwrap().is_empty());
        assert!(!plugin.on_command(&mut state, "__run_entry__"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn verbose_scripts_drain_both_pipes_without_deadlocking() {
        let mut command = ProcessCommand::new("/bin/sh");
        command.args(["-c", "i=0; while [ $i -lt 9000 ]; do printf 0123456789; printf 0123456789 >&2; i=$((i+1)); done"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = run_script(command, Arc::new(AtomicBool::new(false)))
            .expect("a full stdout/stderr pipe must not stall the worker");
        assert_eq!(output.body.len(), 64 * 1024);
        assert!(output.wrote_stderr);
    }

    #[test]
    fn discover_plugins_scans_and_filters() {
        let temp_dir =
            std::env::temp_dir().join(format!("muqun_test_plugins_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // 1. Valid plugin A
        let p_a = temp_dir.join("plugin-a");
        std::fs::create_dir_all(&p_a).unwrap();
        std::fs::write(
            p_a.join("plugin.json"),
            r#"{
                "id": "plugin-a",
                "name": "Plugin A",
                "version": "1.0.0",
                "description": "First test plugin"
            }"#,
        )
        .unwrap();

        // 2. Valid plugin B with README
        let p_b = temp_dir.join("plugin-b");
        std::fs::create_dir_all(&p_b).unwrap();
        std::fs::write(
            p_b.join("plugin.json"),
            r#"{
                "id": "plugin-b",
                "name": "Plugin B",
                "version": "2.0.0",
                "description": "Second test plugin",
                "tile": {
                    "kind": "markdown",
                    "title": "B Guide",
                    "entry": "README.md"
                }
            }"#,
        )
        .unwrap();
        std::fs::write(p_b.join("README.md"), "# Plugin B Content\n\nHello world").unwrap();

        // 3. Invalid plugin (malformed JSON)
        let p_c = temp_dir.join("plugin-invalid");
        std::fs::create_dir_all(&p_c).unwrap();
        std::fs::write(p_c.join("plugin.json"), "{ invalid json").unwrap();

        // 4. Directory without plugin.json
        let p_d = temp_dir.join("not-a-plugin");
        std::fs::create_dir_all(&p_d).unwrap();

        let discovered = discover_plugins(std::slice::from_ref(&temp_dir));
        assert_eq!(discovered.len(), 2);

        let ids: Vec<_> = discovered.iter().map(|p| p.id()).collect();
        assert!(ids.contains(&"plugin-a"));
        assert!(ids.contains(&"plugin-b"));

        // Clean up
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn discover_plugins_deduplicates_by_id() {
        let root = std::env::temp_dir().join(format!("muqun_dedup_{}", uuid::Uuid::new_v4()));
        let dir1 = root.join("dir1");
        let dir2 = root.join("dir2");
        std::fs::create_dir_all(dir1.join("same-plugin")).unwrap();
        std::fs::create_dir_all(dir2.join("same-plugin")).unwrap();

        let manifest = r#"{
            "id": "duplicate-id",
            "name": "Same Plugin",
            "version": "1.0.0",
            "description": "Dedup test"
        }"#;

        std::fs::write(dir1.join("same-plugin").join("plugin.json"), manifest).unwrap();
        std::fs::write(dir2.join("same-plugin").join("plugin.json"), manifest).unwrap();

        let plugins = discover_plugins(&[dir1, dir2]);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id(), "duplicate-id");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sample_plugin_loads_successfully() {
        let manifest_path =
            if Path::new("desktop/assets/plugins/sample-plugin/plugin.json").is_file() {
                PathBuf::from("desktop/assets/plugins/sample-plugin")
            } else {
                PathBuf::from("assets/plugins/sample-plugin")
            };

        let plugin = ExternalPlugin::from_dir(&manifest_path).expect("sample plugin must load");
        assert_eq!(plugin.id(), "sample-plugin");
        assert_eq!(plugin.title(Language::En), "Sample Workspace Guide");
        assert_eq!(plugin.category(), PluginCategory::Extensions);
        assert!(plugin.supports_tile());
        assert_eq!(plugin.commands().len(), 2);

        let md = plugin.load_markdown_content();
        assert!(md.is_some());
        assert!(md.unwrap().contains("Sample Desktop Plugin"));
    }

    #[test]
    fn external_plugin_trait_and_command_handling() {
        use crate::plugin::manifest::{PluginCommandManifest, PluginTileManifest};

        let manifest = PluginManifest {
            id: "ext-test".to_string(),
            name: "Ext Test".to_string(),
            version: "0.2.0".to_string(),
            description: "Test description".to_string(),
            author: Some("Tester".to_string()),
            category: Some(PluginCategory::Tools),
            keywords: vec![],
            repository: None,
            icon: Some("terminal".to_string()),
            runtime: None,
            entry: None,
            surfaces: vec![],
            composer_draft: false,
            notification_entry: None,
            commands: vec![PluginCommandManifest {
                id: "ext.run".to_string(),
                title: "Run Command".to_string(),
                shortcut: Some("Ctrl+R".to_string()),
                action: Some("Executed test".to_string()),
                script: None,
            }],
            tile: Some(PluginTileManifest {
                kind: "command".to_string(),
                title: "Command Tile".to_string(),
                entry: None,
                command: None,
                cwd: None,
            }),
        };

        let plugin = ExternalPlugin::new(manifest, PathBuf::from("/tmp"));
        assert_eq!(plugin.id(), "ext-test");
        assert_eq!(plugin.title(Language::En), "Ext Test");
        assert_eq!(plugin.category(), PluginCategory::Tools);
        assert_eq!(
            plugin.description(Language::En).as_deref(),
            Some("Test description")
        );
        assert!(plugin.supports_tile());

        let cmds = plugin.commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].id, "ext.run");
        assert_eq!(cmds[0].shortcut, Some("Ctrl+R"));

        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        assert!(plugin.on_command(&mut s, "ext.run"));
        assert!(!s.toasts.is_empty());
        assert!(!plugin.on_command(&mut s, "unknown.command"));
    }

    #[test]
    fn external_plugin_bun_script_execution_and_missing_runtime_toast() {
        let temp_dir =
            std::env::temp_dir().join(format!("muqun_test_bun_plugin_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let script_file = temp_dir.join("index.ts");
        std::fs::write(&script_file, "// mock ts script\n").unwrap();

        // Create a mock `bun` binary in the temp directory that verifies env vars and outputs JSON.
        let mock_bun = temp_dir.join("mock-bun");
        std::fs::write(
            &mock_bun,
            r##"#!/bin/sh
printf '%s' "$MUQUN_PLUGIN_CONTEXT" > observed-context.json
printf '{"toast":{"title":"Bun Executed %s","detail":"session=%s cmd=%s","level":"success"},"markdown":"# Dynamic Output from %s","composer_text":"generated prompt from bun"}\n' "$MUQUN_PLUGIN_ID" "$MUQUN_SESSION_ID" "$MUQUN_COMMAND_ID" "$MUQUN_PLUGIN_ID"
"##,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&mock_bun, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let manifest_json = r#"{
            "id": "bun-demo",
            "name": "Bun Demo Plugin",
            "version": "1.0.0",
            "description": "Demonstrates Bun script execution",
            "runtime": "bun",
            "entry": "index.ts",
            "surfaces": ["page", "tile", "drawer"],
            "commands": [
                {
                    "id": "bun.analyze",
                    "title": "Analyze Workspace",
                    "shortcut": "Ctrl+Shift+Y",
                    "script": "index.ts"
                }
            ]
        }"#;
        std::fs::write(temp_dir.join("plugin.json"), manifest_json).unwrap();

        let plugin = ExternalPlugin::from_dir(&temp_dir).expect("bun plugin loads");
        assert_eq!(
            plugin.render_kind(),
            PluginRenderKind::BunScript {
                entry: "index.ts".to_string()
            }
        );

        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.selected = Some("sess-42".to_string());

        // 1. Simulate missing Bun runtime -> shows informative warning toast
        plugin.set_bun_binary_override(Some(None));
        s.toasts.clear();
        assert!(plugin.on_command(&mut s, "bun.analyze"));
        assert_eq!(s.toasts.len(), 1);
        assert_eq!(s.toasts[0].level, ToastLevel::Warning);
        assert!(s.toasts[0].title.contains("Bun runtime required"));
        assert!(s.toasts[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("System & Diagnostics"));

        // 2. Configure mock Bun binary -> executes script, parses JSON stdout, updates markdown & composer & toast
        s.cfg.general.language = Language::ZhTw;
        s.cfg.appearance.scheme = crate::config::Scheme::System;
        s.system_dark.set(Some(false));
        s.cfg.appearance.reduced_motion = true;
        plugin.set_bun_binary_override(Some(Some(mock_bun)));
        s.toasts.clear();
        s.draft.text.clear();
        assert!(plugin.on_command(&mut s, "bun.analyze"));
        let result = plugin
            .jobs
            .lock()
            .unwrap()
            .remove(&None)
            .unwrap()
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let context: serde_json::Value =
            serde_json::from_slice(&std::fs::read(temp_dir.join("observed-context.json")).unwrap())
                .unwrap();
        assert_eq!(context["locale"], "zh-TW");
        assert_eq!(
            context["workspace"]["path"],
            s.terminal_cwd().display().to_string()
        );
        let url_page = serde_json::json!(crate::plugin::context::web_context(&s, false));
        assert!(url_page.get("workspace").is_none());
        assert_eq!(url_page["theme"]["colors"], context["theme"]["colors"]);
        assert_eq!(context["theme"]["preference"], "system");
        assert_eq!(context["theme"]["colorScheme"], "light");
        assert_eq!(context["theme"]["reducedMotion"], true);
        plugin.apply_script_output(&mut s, "Analyze workspace", result.body, false);
        assert_eq!(s.toasts.len(), 1);
        assert_eq!(s.toasts[0].level, ToastLevel::Success);
        assert_eq!(s.toasts[0].title, "Bun Executed bun-demo");
        assert_eq!(
            s.toasts[0].detail.as_deref(),
            Some("session=sess-42 cmd=bun.analyze")
        );
        assert_eq!(
            plugin.dynamic_markdown().as_deref(),
            Some("# Dynamic Output from bun-demo")
        );
        assert!(s.draft.text.contains("generated prompt from bun"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn webview_plugin_with_readme_renders_webview_card_instead_of_readme() {
        let dir =
            std::env::temp_dir().join(format!("muqun-webview-readme-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{
                "id": "web-docs",
                "name": "Web Docs",
                "version": "1.0.0",
                "description": "Remote docs dashboard",
                "tile": {
                    "kind": "webview",
                    "title": "Docs Portal",
                    "entry": "https://example.com/docs"
                }
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("README.md"),
            "# Shadowing README\n\nThis markdown must not hide the WebView card.",
        )
        .unwrap();

        let plugin = Arc::new(ExternalPlugin::from_dir(&dir).unwrap());
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.plugins.register(plugin);

        fn render_web(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let k = rune_kit::Kit::neutral(true);
            let look = s.look();
            let ctx = PluginViewContext::new(s, &k, &look, "page", 1.0, PluginSlot::Page);
            let p = s.plugins.get("web-docs").unwrap();
            col()
                .w_full()
                .h_full()
                .child(p.render_tile(&ctx, &mut vec![]).unwrap())
        }

        let mut rt = rune::Runtime::new(s, render_web, (640.0, 480.0), rune::Env::new(true, 1.0));
        rt.frame(0.0);
        let elements = rt.elements();
        assert!(
            elements
                .iter()
                .any(|e| e.key.as_deref() == Some("webview-card-page")),
            "expected WebView card to render even when README.md exists"
        );
        assert!(
            elements
                .iter()
                .any(|e| e.key.as_deref() == Some("webview-open-browser-page")),
            "expected Open in Browser button to render"
        );
        assert!(
            !elements.iter().any(|e| e
                .text
                .as_deref()
                .is_some_and(|t| t.contains("Shadowing README"))),
            "README.md must never shadow a WebView tile"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repeated_plugin_reloads_intern_static_strings_without_unbounded_leaks() {
        let unique = format!("intern-test-{}", uuid::Uuid::new_v4());
        let cmd_id = format!("{unique}.run");
        let shortcut = format!("Ctrl+Alt+{unique}");
        let manifest = PluginManifest {
            id: unique.clone(),
            name: "Intern Test".to_string(),
            version: "1.0.0".to_string(),
            description: "Reload interning test".to_string(),
            commands: vec![PluginCommandManifest {
                id: cmd_id.clone(),
                title: "Run".to_string(),
                shortcut: Some(shortcut.clone()),
                action: None,
                script: None,
            }],
            ..Default::default()
        };

        let first = ExternalPlugin::new(manifest.clone(), PathBuf::from("/tmp"));
        let count_matching = || {
            interner()
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.contains(&unique))
                .count()
        };
        assert_eq!(count_matching(), 3);
        let first_id_ptr = first.id().as_ptr();
        let first_cmd_ptr = first.commands()[0].id.as_ptr();
        let first_sc_ptr = first.commands()[0].shortcut.unwrap().as_ptr();

        for _ in 0..100 {
            let reloaded = ExternalPlugin::new(manifest.clone(), PathBuf::from("/tmp"));
            assert_eq!(reloaded.id().as_ptr(), first_id_ptr);
            assert_eq!(reloaded.commands()[0].id.as_ptr(), first_cmd_ptr);
            assert_eq!(
                reloaded.commands()[0].shortcut.unwrap().as_ptr(),
                first_sc_ptr
            );
        }
        assert_eq!(
            count_matching(),
            3,
            "reloading identical plugin manifests must allocate zero new leaked strings"
        );
    }

    #[cfg(unix)]
    #[test]
    fn panel_runtime_error_retains_error_card_with_retry_instead_of_blanking() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("muqun-panel-err-card-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"panel-err-test","version":"1.0.0","main":"entry.ts","muqun":{}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("entry.ts"), "").unwrap();
        let bun = dir.join("mock-bun");
        std::fs::write(
            &bun,
            r#"#!/bin/sh
printf '{"version":1,"view":{"type":"text","text":"Recovered after retry"}}'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&bun, std::fs::Permissions::from_mode(0o755)).unwrap();

        let plugin = Arc::new(ExternalPlugin::from_dir(&dir).unwrap());
        plugin.set_bun_binary_override(Some(Some(bun)));

        // Start a panel whose process immediately fails with a structured startup error.
        let mut fail_cmd = ProcessCommand::new("sh");
        fail_cmd.args([
            "-c",
            "echo '!muqun:webview:error:backend-unavailable'; exit 1",
        ]);
        plugin.panels.lock().unwrap().insert(
            "page".into(),
            panel::Panel::start_for_test(PathBuf::from("panel.ts"), fail_cmd),
        );

        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.plugins.register(plugin.clone());

        let deadline = Instant::now() + Duration::from_secs(5);
        while !plugin.poll(&mut s, &mut rune::Tx::default()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            plugin.view_errors.lock().unwrap().contains_key("page"),
            "panel failure must retain an explicit error state on the instance"
        );

        fn render_panel_err(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let k = rune_kit::Kit::neutral(true);
            let look = s.look();
            let ctx = PluginViewContext::new(s, &k, &look, "page", 1.0, PluginSlot::Page);
            let p = s.plugins.get("panel-err-test").unwrap();
            col()
                .w_full()
                .h_full()
                .child(p.render_tile(&ctx, &mut vec![]).unwrap())
        }

        let mut rt = rune::Runtime::new(
            s,
            render_panel_err,
            (640.0, 480.0),
            rune::Env::new(true, 1.0),
        );
        rt.set_system_reduced_motion(true);
        rt.frame(0.0);
        let elements = rt.elements();
        assert!(
            elements
                .iter()
                .any(|e| e.key.as_deref() == Some("plugin-native-error-panel-err-test-page")),
            "expected explicit error card after panel failure"
        );
        assert!(
            elements.iter().any(|e| e
                .text
                .as_deref()
                .is_some_and(|t| t.contains("could not open its WebView panel"))),
            "expected error detail text on panel error card"
        );
        let retry = elements
            .iter()
            .find(|e| e.key.as_deref() == Some("plugin-native-retry-panel-err-test-page"))
            .expect("expected Retry button on panel error card");
        let [x, y, w, h] = retry.rect;
        rt.pointer_down(x + w / 2.0, y + h / 2.0);
        rt.pointer_up(x + w / 2.0, y + h / 2.0);
        rt.frame(0.0);

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let mut done = false;
            rt.update(|s, tx| {
                done = plugin.poll(s, tx);
            });
            if done {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        rt.frame(0.0);
        assert!(
            !plugin.view_errors.lock().unwrap().contains_key("page"),
            "retry recovery must clear the error state"
        );
        assert!(
            rt.elements()
                .iter()
                .any(|e| e.text.as_deref() == Some("Recovered after retry")),
            "expected recovered native view after clicking Retry"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn notification_entry_is_armed_on_load_for_enabled_plugins_and_respects_disable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("muqun-notif-armed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"notif-auto","version":"1.0.0","main":"entry.ts","muqun":{"notificationEntry":"notify.ts"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("entry.ts"), "").unwrap();
        std::fs::write(dir.join("notify.ts"), "").unwrap();
        let bun = dir.join("mock-bun");
        std::fs::write(
            &bun,
            r#"#!/bin/sh
printf '{"notices":[{"title":"Background update","body":"Polled without manual open","action":"open-item"}]}'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&bun, std::fs::Permissions::from_mode(0o755)).unwrap();

        let plugin = ExternalPlugin::from_dir(&dir).unwrap();
        plugin.set_bun_binary_override(Some(Some(bun)));
        assert!(
            plugin.notifications.lock().unwrap().armed,
            "notificationEntry must start armed on load without requiring manual open"
        );

        // Disabled plugins must not poll notifications.
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.cfg.notifications.enabled = true;
        s.cfg.plugins_disabled.insert("notif-auto".into());
        assert!(!plugin.poll(&mut s, &mut rune::Tx::default()));
        assert!(!plugin.notifications.lock().unwrap().armed);

        // Re-enabling re-arms notifications and polling delivers notices without manual open.
        s.cfg.plugins_disabled.remove("notif-auto");
        plugin.enabled_changed(true);
        assert!(plugin.notifications.lock().unwrap().armed);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut delivered = false;
        while Instant::now() < deadline {
            if plugin.poll(&mut s, &mut rune::Tx::default()) {
                delivered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            delivered,
            "enabled plugin should poll notifications automatically"
        );
        assert!(
            s.toasts
                .iter()
                .any(|t| t.title.contains("Background update")),
            "expected toast from background notification poll"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn review_center_auto_refreshes_on_open_and_shows_loading_state() {
        use std::os::unix::fs::PermissionsExt;
        let review_dir = if Path::new("desktop/assets/plugins/review-center/plugin.json").is_file()
        {
            PathBuf::from("desktop/assets/plugins/review-center")
        } else {
            PathBuf::from("assets/plugins/review-center")
        };
        let plugin = ExternalPlugin::from_dir(&review_dir).expect("review-center must load");
        assert_eq!(
            plugin.render_kind(),
            PluginRenderKind::BunScript {
                entry: "review.ts".to_string()
            }
        );

        let temp_dir =
            std::env::temp_dir().join(format!("muqun-review-auto-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let mock_bun = temp_dir.join("mock-bun");
        std::fs::write(
            &mock_bun,
            r##"#!/bin/sh
printf '{"markdown":"# owner/repo\\n\\n## Pull requests (1)"}'
"##,
        )
        .unwrap();
        std::fs::set_permissions(&mock_bun, std::fs::Permissions::from_mode(0o755)).unwrap();
        plugin.set_bun_binary_override(Some(Some(mock_bun)));

        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        assert!(plugin.dynamic_markdown().is_none());
        assert!(
            plugin.on_view_action(&mut s, "page", "open"),
            "opening review-center with no cached output must auto-run its refresh script"
        );

        // While running, render_tile displays a Loading… card rather than stale README.
        let k = rune_kit::Kit::neutral(true);
        let look = s.look();
        let ctx = PluginViewContext::new(&s, &k, &look, "page", 1.0, PluginSlot::Page);
        assert!(plugin.render_tile(&ctx, &mut vec![]).is_some());

        let deadline = Instant::now() + Duration::from_secs(5);
        while !plugin.poll(&mut s, &mut rune::Tx::default()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(plugin
            .dynamic_markdown()
            .as_deref()
            .unwrap_or_default()
            .contains("## Pull requests (1)"));
        // Auto-refresh on open should not spam a generic success toast when script didn't send one.
        assert!(s.toasts.is_empty());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn terminal_tui_auto_launches_once_per_instance_and_survives_move() {
        let manifest = PluginManifest {
            id: "tui-auto".to_string(),
            name: "Lazygit".to_string(),
            version: "1.0.0".to_string(),
            description: "Git TUI".to_string(),
            tile: Some(crate::plugin::manifest::PluginTileManifest {
                kind: "terminal_tui".to_string(),
                title: "Lazygit".to_string(),
                entry: None,
                command: Some(vec!["lazygit".to_string()]),
                cwd: None,
            }),
            ..Default::default()
        };
        let plugin = ExternalPlugin::new(manifest, PathBuf::from("/tmp"));
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);

        assert!(plugin.on_view_action(&mut s, "tile-1", "open"));
        assert!(!plugin.on_view_action(&mut s, "tile-1", "open"));
        assert!(plugin.can_move_view("tile-1"));
        plugin.move_view("tile-1", "page");
        assert!(!plugin.on_view_action(&mut s, "page", "open"));
        plugin.close_view("page");
        assert!(plugin.on_view_action(&mut s, "page", "open"));
    }
}
