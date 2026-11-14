//! Plugin discovery and external plugin runtime loader.
//!
//! Scans configured plugin search paths for folders containing `plugin.json`,
//! parses their manifests, and instantiates [`ExternalPlugin`] which implements
//! [`DesktopPlugin`].

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use muqun_gateway::platform::runtimes::{self, RuntimeKind};
use rune::{col, el, icon, row, text, El, Icon};
use rune_kit::{Button, ButtonVariant, ControlSize, Markdown};

use crate::config::Language;
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

/// An external file-based plugin loaded from a `plugin.json` manifest.
pub struct ExternalPlugin {
    id: &'static str,
    manifest: PluginManifest,
    dir: PathBuf,
    commands: Vec<PluginCommand>,
    dynamic_markdown: Mutex<Option<String>>,
    script_job: Mutex<Option<mpsc::Receiver<Result<String, String>>>>,
    script_title: Mutex<String>,
    /// Optional override for the Bun binary resolution:
    /// - `None`: resolve via `muqun_gateway::platform::runtimes::resolve_binary(RuntimeKind::Bun, None)`
    /// - `Some(Some(path))`: resolve with `configured = Some(path)`
    /// - `Some(None)`: force Bun unavailable (used for testing missing-runtime fallback)
    bun_binary_override: Mutex<Option<Option<PathBuf>>>,
}

impl ExternalPlugin {
    /// Construct an external plugin instance from a parsed manifest and directory.
    pub fn new(manifest: PluginManifest, dir: PathBuf) -> Self {
        let dir = if dir.is_absolute() {
            dir
        } else {
            std::env::current_dir().unwrap_or_default().join(dir)
        };
        let id: &'static str = Box::leak(manifest.id.clone().into_boxed_str());
        let commands = manifest
            .commands
            .iter()
            .map(|c| {
                let cid: &'static str = Box::leak(c.id.clone().into_boxed_str());
                let shortcut: Option<&'static str> = c
                    .shortcut
                    .as_ref()
                    .map(|s| &*Box::leak(s.clone().into_boxed_str()));
                PluginCommand {
                    id: cid,
                    title: c.title.clone(),
                    shortcut,
                }
            })
            .collect();

        Self {
            id,
            manifest,
            dir,
            commands,
            dynamic_markdown: Mutex::new(None),
            script_job: Mutex::new(None),
            script_title: Mutex::new(String::new()),
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

    /// Load an external plugin from a directory containing `plugin.json`.
    pub fn from_dir(dir: &Path) -> anyhow::Result<Self> {
        let manifest_path = dir.join("plugin.json");
        let manifest = PluginManifest::from_file(&manifest_path)?;
        if manifest.id.trim().is_empty() {
            anyhow::bail!("Plugin manifest in {} has an empty id", dir.display());
        }
        Ok(Self::new(manifest, dir.to_path_buf()))
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
    ) -> bool {
        let Some(bun_bin) = self.resolve_bun_binary() else {
            s.notify(
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
            s.notify(
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

        let mut command = ProcessCommand::new(&bun_bin);
        command
            .arg("run")
            .arg(script_path)
            .current_dir(&self.dir)
            .env("MUQUN_PLUGIN_ID", self.id)
            .env("MUQUN_PLUGIN_DIR", self.dir.as_os_str())
            .env("MUQUN_WORKSPACE_CWD", &workspace_cwd)
            .env("MUQUN_SESSION_ID", &session_id)
            .env("MUQUN_COMMAND_ID", command_id)
            .env("MUQUN_SCRIPT_PATH", script_path.as_os_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut job = self.script_job.lock().unwrap();
        if job.is_some() {
            return true;
        }
        let (tx, rx) = mpsc::channel();
        *job = Some(rx);
        *self.script_title.lock().unwrap() = command_title.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(run_script(command));
        });
        true
    }

    fn apply_script_output(&self, s: &mut UiState, command_title: &str, stdout_str: String) {
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
            } else {
                s.notify(
                    ToastLevel::Success,
                    format!("{}: {}", self.manifest.name, command_title),
                    None,
                );
            }
        } else if !stdout_str.is_empty() {
            *self.dynamic_markdown.lock().unwrap() = Some(stdout_str.clone());
            s.notify(
                ToastLevel::Success,
                format!("{}: {}", self.manifest.name, command_title),
                Some(stdout_str),
            );
        } else {
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

    fn description(&self, _lang: Language) -> Option<String> {
        Some(self.manifest.description.clone())
    }

    fn category(&self) -> PluginCategory {
        self.manifest.category()
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

        if let PluginRenderKind::TerminalTui { command, cwd } = &render_kind {
            let cmd_str = command.join(" ");
            let cwd_opt = cwd.clone();
            let pid_launch = pid.clone();
            let tui_bar = row()
                .w_full()
                .h(36.0)
                .px(12.0)
                .gap(8.0)
                .center_y()
                .fill(k.alpha(k.c.layer_01, 0.3))
                .child(icon(&terminal_panel::TERMINAL_WINDOW, 13.0).color(k.c.text_accent))
                .child(
                    text(if cmd_str.is_empty() {
                        tr_ref(lang, "Interactive Terminal TUI").to_string()
                    } else {
                        format!("$ {cmd_str}")
                    })
                    .font(k.ty.mono)
                    .color(k.c.text_base)
                    .truncate(),
                )
                .child(el().grow(1.0))
                .child(
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

        let md_content = self.load_markdown_content();

        let body = if let Some(content) = md_content {
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
                    let pid_run = pid.clone();
                    cmd_bar = cmd_bar.child(
                        Button::new(format!("{} ({entry})", tr_ref(lang, "Run Script")))
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

                for cmd in &self.manifest.commands {
                    let cmd_id = cmd.id.clone();
                    let pid_clone = pid.clone();
                    let btn = Button::new(&cmd.title)
                        .disabled(self.script_job.lock().unwrap().is_some())
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| {
                            if let Some(p) = s.plugins.get(&pid_clone) {
                                p.on_command(s, &cmd_id);
                            }
                        })
                        .build(k);
                    cmd_bar = cmd_bar.child(btn.key(format!("plugin-command-{}", cmd.id)));
                }
                if self.script_job.lock().unwrap().is_some() {
                    cmd_bar = cmd_bar.child(
                        text(tr_ref(lang, "Loading…"))
                            .font(k.ty.small)
                            .color(k.c.text_muted),
                    );
                }
                col_inner = col_inner.child(cmd_bar);
            }

            col_inner = col_inner.child(
                Markdown::new(content)
                    .selectable(true)
                    .on_link_tx(|s: &mut UiState, url: &str, tx: &mut rune::Tx| {
                        crate::ui::open_markdown_link(s, url, tx);
                    })
                    .build(k),
            );

            col()
                .key(format!("ext-plugin-md-{tile_id}"))
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(col().w_full().items_center().child(col_inner))
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
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
                            .color(k.c.text_base),
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

            if let Some(tile) = &self.manifest.tile {
                if tile.kind == "webview" {
                    let entry_str = tile.entry.as_deref().unwrap_or("about:blank");
                    let webview_card = look.row_card(
                        k,
                        col()
                            .w_full()
                            .px(14.0)
                            .py(14.0)
                            .gap(8.0)
                            .child(
                                row()
                                    .gap(6.0)
                                    .center_y()
                                    .child(icon(&icons::OPEN_EXTERNAL, 12.0).color(k.c.text_accent))
                                    .child(
                                        text(&tile.title).font(k.ty.ui_medium).color(k.c.text_base),
                                    ),
                            )
                            .child(
                                text(format!("Entry: {entry_str}"))
                                    .font(k.ty.mono)
                                    .color(k.c.text_muted),
                            )
                            .child(
                                Button::new(tr_ref(lang, "Open in Browser"))
                                    .variant(ButtonVariant::Outline)
                                    .control_size(ControlSize::Small)
                                    .on_press({
                                        let url = entry_str.to_string();
                                        move |_s: &mut UiState| {
                                            crate::platform::shell::open_url(&url);
                                        }
                                    })
                                    .build(k),
                            ),
                        k.radius.md,
                    );
                    content_col = content_col.child(webview_card);
                }
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

    fn poll(&self, s: &mut UiState, tx: &mut rune::Tx) -> bool {
        if !super::is_enabled(self.id, &s.cfg.plugins_disabled) {
            self.script_job.lock().unwrap().take();
            return false;
        }
        let result = {
            let mut job = self.script_job.lock().unwrap();
            let Some(rx) = job.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => {
                    *job = None;
                    Some(result)
                }
                Err(mpsc::TryRecvError::Empty) => {
                    tx.wake_after(Duration::from_millis(16));
                    None
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    *job = None;
                    Some(Err("Plugin worker stopped".into()))
                }
            }
        };
        match result {
            Some(Ok(output)) => {
                self.apply_script_output(s, &self.script_title.lock().unwrap(), output)
            }
            Some(Err(error)) => s.notify(
                ToastLevel::Error,
                format!("{}: command failed", self.manifest.name),
                Some(error),
            ),
            None => return false,
        }
        true
    }

    fn on_command(&self, s: &mut UiState, command_id: &str) -> bool {
        if command_id == "__run_entry__" {
            match self.manifest.render_kind() {
                PluginRenderKind::BunScript { entry } => {
                    let script_path = self.resolve_script_path(&entry);
                    return self.execute_bun_script(
                        s,
                        command_id,
                        &self.manifest.name,
                        &script_path,
                    );
                }
                PluginRenderKind::TerminalTui { command, cwd } => {
                    let dir = cwd
                        .as_ref()
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
                    return true;
                }
                _ => return false,
            }
        }

        if let Some(cmd) = self.manifest.commands.iter().find(|c| c.id == command_id) {
            if let Some(script_path) = self.script_for_command(cmd) {
                return self.execute_bun_script(s, command_id, &cmd.title, &script_path);
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
fn run_script(mut command: ProcessCommand) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
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
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            result => {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break Err(result
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "Plugin command timed out".into()));
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
        Ok(stdout)
    } else {
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
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

    for base_dir in plugin_dirs {
        if !base_dir.is_dir() {
            continue;
        }

        // Direct directory check
        let direct_manifest = base_dir.join("plugin.json");
        if direct_manifest.is_file() {
            match ExternalPlugin::from_dir(base_dir) {
                Ok(plugin) => {
                    let id = plugin.manifest().id.clone();
                    if seen_ids.insert(id) {
                        plugins.push(plugin);
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        path = %base_dir.display(),
                        error = %err,
                        "Failed to load external plugin"
                    );
                }
            }
            continue;
        }

        // Subdirectories check
        let read_dir = match std::fs::read_dir(base_dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };

        let mut entries: Vec<_> = read_dir.flatten().collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("plugin.json");
            if manifest_path.is_file() {
                match ExternalPlugin::from_dir(&path) {
                    Ok(plugin) => {
                        let id = plugin.manifest().id.clone();
                        if seen_ids.insert(id) {
                            plugins.push(plugin);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "Failed to load external plugin"
                        );
                    }
                }
            }
        }
    }

    plugins
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn verbose_scripts_drain_both_pipes_without_deadlocking() {
        let mut command = ProcessCommand::new("/bin/sh");
        command.args(["-c", "i=0; while [ $i -lt 9000 ]; do printf 0123456789; printf 0123456789 >&2; i=$((i+1)); done"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let output =
            run_script(command).expect("a full stdout/stderr pipe must not stall the worker");
        assert_eq!(output.len(), 64 * 1024);
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
            icon: Some("terminal".to_string()),
            runtime: None,
            entry: None,
            surfaces: vec![],
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
        plugin.set_bun_binary_override(Some(Some(mock_bun)));
        s.toasts.clear();
        s.draft.text.clear();
        assert!(plugin.on_command(&mut s, "bun.analyze"));
        let result = plugin
            .script_job
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        plugin.apply_script_output(&mut s, "Analyze workspace", result);
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
}
