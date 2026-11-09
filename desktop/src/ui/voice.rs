//! Dictation in the composer, and Settings → Voice.
//!
//! The mic button (or Ctrl/Cmd+Shift+Space) starts a recording; while it
//! runs the composer shows a waveform -- a ribbon of bars scrolling at the
//! display's rate, each a Gaussian blend of the levels around its moment,
//! with an eased live edge -- and, in realtime mode, the words as the
//! service hears them, fading in like subtitles. Done (or the shortcut
//! again) transcribes; the text lands in the draft and is never sent.
//! Escape or Cancel throws the recording away.

use std::sync::{Arc, Mutex};

use crate::model::inbox::UiAction;

use rune::{col, el, row, text, El, Spring, TextEdit, Transition, Tx};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, SegmentedControl,
    TextInput,
};
use tokio::sync::mpsc;

use super::copy::{tr, tr_ref};
use super::UiState;
use crate::config::{ConfigEdit, Language, VoiceConfig, VoiceMode};
use crate::model::commands::{Command, VoiceStart};
use crate::model::voice::{Meter, VoiceControl, VoiceEvent, VoiceFailure, KEY_NAME};
use crate::platform::secrets::Store;

/// The App's microphone glyph (`assets/glyphs/microphone.svg`).
pub static MIC: rune::Icon = rune::Icon {
    name: "microphone",
    view: 24.0,
    body: concat!(
        r#"<rect x="9" y="3" width="6" height="11" rx="3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
        r#"<path d="M5 11a7 7 0 0 0 14 0M12 18v3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
    ),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Opening the microphone (and the realtime socket).
    Starting,
    Recording,
    Transcribing,
}

/// A recording in progress.
pub struct Dictation {
    pub phase: Phase,
    pub mode: VoiceMode,
    pub meter: Arc<Meter>,
    control: mpsc::UnboundedSender<VoiceControl>,
    /// The realtime preview so far.
    pub partial: String,
    /// The live edge's level, eased toward the newest reading every frame.
    pub live: f32,
    /// When the live edge was last eased (meter seconds).
    eased_at: f64,
}

impl Dictation {
    /// A recording that is only drawn: the screenshot scenes' state.
    pub(super) fn preview(mode: VoiceMode, meter: Arc<Meter>, partial: &str) -> Self {
        Self {
            phase: Phase::Recording,
            mode,
            live: meter.latest(),
            meter,
            control: mpsc::unbounded_channel().0,
            partial: partial.to_string(),
            eased_at: 0.0,
        }
    }
}

/// Settings → Voice's fields.
pub struct VoiceForm {
    pub url: TextEdit,
    pub model: TextEdit,
    pub key: TextEdit,
    /// Where the saved key lives; `None` when there is none (or before the
    /// lookup answered).
    pub key_saved: Option<Store>,
}

impl VoiceForm {
    pub fn new(cfg: &VoiceConfig) -> Self {
        Self {
            url: TextEdit::new(cfg.url.clone()),
            model: TextEdit::new(cfg.model.clone()),
            key: TextEdit::new(""),
            key_saved: None,
        }
    }
}

impl UiState {
    /// The mic button and the shortcut: start, or finish what is recording.
    pub fn toggle_voice(&mut self) {
        match self.voice.as_ref().map(|v| v.phase) {
            Some(Phase::Recording) => self.finish_voice(),
            Some(_) => {}
            None => self.start_voice(),
        }
    }

    fn start_voice(&mut self) {
        let config = self.cfg.voice.clone();
        if !config.usable() {
            let language = self.cfg.general.language;
            self.toast(
                tr(language, "Set up voice input first", "请先设置语音输入"),
                Some(tr(
                    language,
                    "Settings → Voice: the speech service's address.",
                    "设置 → 语音：填写语音服务地址。",
                )),
            );
            self.go(super::Mode::Settings);
            self.settings_tab = super::SettingsTab::Voice;
            return;
        }
        let (control, rx) = mpsc::unbounded_channel();
        let meter = Arc::new(Meter::default());
        self.voice = Some(Dictation {
            phase: Phase::Starting,
            mode: config.mode,
            meter: meter.clone(),
            control,
            partial: String::new(),
            live: 0.0,
            eased_at: 0.0,
        });
        self.send(Command::Voice(Arc::new(VoiceStart {
            config,
            meter,
            control: Mutex::new(Some(rx)),
        })));
    }

    pub fn finish_voice(&mut self) {
        if let Some(v) = self.voice.as_mut() {
            if v.phase == Phase::Recording {
                let _ = v.control.send(VoiceControl::Finish);
                v.phase = Phase::Transcribing;
            }
        }
    }

    pub fn cancel_voice(&mut self) {
        if let Some(v) = self.voice.take() {
            let _ = v.control.send(VoiceControl::Cancel);
        }
    }

    pub fn on_voice(&mut self, event: VoiceEvent) {
        let language = self.cfg.general.language;
        let Some(v) = self.voice.as_mut() else {
            return;
        };
        match event {
            VoiceEvent::Recording => {
                if v.phase == Phase::Starting {
                    v.phase = Phase::Recording;
                }
            }
            VoiceEvent::Transcribing => v.phase = Phase::Transcribing,
            VoiceEvent::Partial(text) => v.partial = text,
            VoiceEvent::Text(text) => {
                self.voice = None;
                if text.trim().is_empty() {
                    self.toast(
                        tr(
                            language,
                            "No speech detected. Please try again.",
                            "没有听到语音，请再试一次。",
                        ),
                        None,
                    );
                } else {
                    self.insert_dictation(text.trim());
                }
            }
            VoiceEvent::Failed(failure) => {
                self.voice = None;
                let message = match failure {
                    VoiceFailure::Microphone(why) => {
                        return self.toast(tr(language, "Recording failed.", "录音失败。"), Some(why));
                    }
                    VoiceFailure::Connection => tr(
                        language,
                        "Realtime transcription disconnected. Check your connection and speech service, then try again.",
                        "实时转写已断开。请检查网络和语音服务后重试。",
                    ),
                    VoiceFailure::Response => tr(
                        language,
                        "The speech service returned an invalid transcript.",
                        "语音服务返回的转写结果无效。",
                    ),
                    VoiceFailure::Transcription => tr(
                        language,
                        "Transcription failed. Check your speech service and try again.",
                        "转写失败。请检查语音服务后重试。",
                    ),
                };
                self.toast(message, None);
            }
        }
    }

    /// Put dictated text in the session draft, after what is there.
    fn insert_dictation(&mut self, text: &str) {
        let mut draft = self.draft.text.clone();
        if !draft.is_empty() && !draft.ends_with(char::is_whitespace) {
            draft.push(' ');
        }
        draft.push_str(text);
        let mut edit = TextEdit::new(draft);
        edit.focused = true;
        self.draft = edit;
        self.persist_draft(super::Slot::Session);
    }

    /// Each frame while recording: ease the live edge and keep drawing.
    pub fn poll_voice(&mut self) -> bool {
        let reduced = self.cfg.appearance.reduced_motion;
        let Some(v) = self.voice.as_mut() else {
            return false;
        };
        let now = v.meter.now();
        let dt = (now - v.eased_at).clamp(0.0, 0.1) as f32;
        v.eased_at = now;
        let target = v.meter.latest();
        // Critically damped-ish: ~70 ms to settle, never a jump.
        let k = if reduced {
            1.0
        } else {
            1.0 - (-dt / 0.07).exp()
        };
        v.live += (target - v.live) * k;
        v.phase != Phase::Transcribing
    }

    /// Look up whether a key is saved (off the UI thread); the answer comes
    /// back as [`UiAction::VoiceKey`].
    pub fn check_voice_key(&mut self) {
        std::thread::spawn(|| {
            let found = crate::platform::secrets::locate(KEY_NAME);
            crate::platform::dialogs::post(UiAction::VoiceKey(found));
        });
    }

    fn save_voice_key(&mut self) {
        let key = std::mem::take(&mut self.voice_form.key.text);
        self.voice_form.key = TextEdit::new("");
        std::thread::spawn(move || {
            let saved = match crate::platform::secrets::save(KEY_NAME, key.trim()) {
                Ok(_) if key.trim().is_empty() => None,
                Ok(store) => Some(store),
                Err(err) => {
                    tracing::warn!("the voice key was not saved: {err}");
                    None
                }
            };
            crate::platform::dialogs::post(UiAction::VoiceKey(saved));
        });
    }

    fn edit_voice(&mut self, f: impl FnOnce(&mut VoiceConfig)) {
        let mut voice = self.cfg.voice.clone();
        f(&mut voice);
        self.edit(ConfigEdit::Voice(voice));
    }
}

// ---- composer --------------------------------------------------------------------

/// The composer's mic button.
pub fn mic_button(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let recording = s.voice.as_ref().map(|v| v.phase);
    let label = match recording {
        Some(Phase::Recording) => tr(language, "Finish dictation", "结束听写"),
        Some(_) => tr(language, "Transcribing…", "转写中…"),
        None => tr(language, "Dictate", "语音输入"),
    };
    IconButton::new(&MIC)
        .variant(if recording.is_some() {
            IconButtonVariant::Contrast
        } else {
            IconButtonVariant::Ghost
        })
        .label(label.clone())
        .help(format!(
            "{label} · {}",
            crate::platform::keys::label("Primary+Shift+Space")
        ))
        .disabled(matches!(
            recording,
            Some(Phase::Starting | Phase::Transcribing)
        ))
        .on_press(|s: &mut UiState| s.toggle_voice())
        .build(k)
        .key("mic")
}

const WAVE_H: f32 = 34.0;
const BAR_W: f32 = 3.0;
const BAR_GAP: f32 = 3.0;
/// Seconds per bar slot: the ribbon moves 6 pt every 1/14 s (84 pt/s).
const SLOT: f64 = 1.0 / 14.0;
const BARS: usize = 120;

/// The waveform ribbon: bars on a fixed time grid scroll left continuously;
/// each is the Gaussian-blended level around its slot; the newest eases.
fn waveform(k: &Kit, v: &Dictation) -> El {
    let now = v.meter.now();
    let speed = f64::from(BAR_W + BAR_GAP) / SLOT;
    let newest = (now / SLOT).floor();
    let colour = k.c.bg_accent;
    // Anchored to the right edge (the live edge); whatever is older than the
    // box is wide is clipped on the left.
    let mut wave = el()
        .key("voice-wave")
        .w_full()
        .h(WAVE_H)
        .clip_rounded(0.0)
        .a11y_hidden();
    for i in 0..BARS {
        let slot_t = (newest - i as f64) * SLOT;
        if slot_t < 0.0 {
            break;
        }
        let right = ((now - slot_t) * speed) as f32;
        // The two newest slots follow the eased live edge, the rest the
        // blended history: the ribbon's head moves smoothly, its body is calm.
        let level = if i < 2 {
            v.live.max(v.meter.blended(slot_t, 0.05) * 0.6)
        } else {
            v.meter.blended(slot_t, 0.05)
        };
        let h = (WAVE_H * level.powf(0.85)).clamp(3.0, WAVE_H);
        // Older bars fade out toward the left.
        let age = (now - slot_t) / (BARS as f64 * SLOT);
        let alpha = (1.0 - age.powf(1.6) as f32).clamp(0.08, 1.0);
        wave = wave.child(
            el().pin(None, Some((WAVE_H - h) / 2.0), Some(right), None)
                .size(BAR_W, h)
                .radius(BAR_W / 2.0)
                .fill(colour.alpha(alpha)),
        );
    }
    wave
}

/// The realtime preview as words that fade in as they arrive; CJK text has
/// no spaces, so each character is a word.
fn subtitles(k: &Kit, partial: &str, motion: bool) -> El {
    let mut words: Vec<String> = Vec::new();
    for piece in partial.split_inclusive(char::is_whitespace) {
        let mut current = String::new();
        for c in piece.chars() {
            let cjk = matches!(c as u32, 0x3000..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF);
            if cjk {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                words.push(c.to_string());
            } else {
                current.push(c);
            }
        }
        if !current.is_empty() {
            words.push(current);
        }
    }
    // The last two lines' worth; older words have already been read.
    let skip = words.len().saturating_sub(48);
    let mut line = row().key("voice-words").w_full().flex_wrap().items_end();
    for (i, word) in words.iter().enumerate().skip(skip) {
        let mut w = text(word.clone())
            .key(format!("w{i}"))
            .font(k.ty.body)
            .color(k.c.text_base);
        if motion {
            w = w
                .transition(Transition::opacity().combined(Transition::offset(0.0, 6.0)))
                .motion(Spring::new(0.35, 0.2));
        }
        line = line.child(w);
    }
    line
}

/// What the composer shows while dictating, above the draft.
pub fn strip(s: &UiState, k: &Kit) -> Option<El> {
    let v = s.voice.as_ref()?;
    let language = s.cfg.general.language;
    let secs = v.meter.now() as u64;
    let status = match v.phase {
        Phase::Starting => tr(language, "Starting…", "准备中…"),
        Phase::Recording => format!("{}:{:02}", secs / 60, secs % 60),
        Phase::Transcribing => tr(language, "Transcribing…", "转写中…"),
    };
    let dot =
        el().key("voice-dot")
            .size(8.0, 8.0)
            .radius(4.0)
            .fill(if v.phase == Phase::Recording {
                k.c.danger.icon
            } else {
                k.c.icon_faint
            });
    let dot = if v.phase == Phase::Recording {
        s.look().pulse(dot)
    } else {
        dot
    };
    let mut head = row()
        .key("voice-head")
        .w_full()
        .gap(10.0)
        .center_y()
        .child(dot)
        .child(
            text(status)
                .font(k.ty.small_medium)
                .color(k.c.text_muted)
                .min_w(44.0),
        )
        .child(
            row()
                .key("voice-wave-box")
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .child(waveform(k, v)),
        )
        .child(
            Button::new(tr_ref(language, "Cancel", "取消"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .shortcut("Escape")
                .on_press(|s: &mut UiState| s.cancel_voice())
                .build(k)
                .key("voice-cancel"),
        );
    if v.phase == Phase::Recording {
        head = head.child(
            Button::new(tr_ref(language, "Done", "完成"))
                .variant(ButtonVariant::Submit)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| s.finish_voice())
                .build(k)
                .key("voice-done"),
        );
    }
    let mut strip = col()
        .key("voice-strip")
        .w_full()
        .gap(8.0)
        .px(4.0)
        .py(4.0)
        .child(head);
    if v.mode == VoiceMode::Realtime {
        let words = if v.partial.trim().is_empty() {
            text(match v.phase {
                Phase::Starting => tr(language, "Connecting…", "连接中…"),
                _ => tr(language, "Listening…", "正在听…"),
            })
            .key("voice-words")
            .font(k.ty.body)
            .color(k.c.text_faint)
        } else {
            subtitles(k, &v.partial, !s.cfg.appearance.reduced_motion)
        };
        strip = strip.child(words);
    }
    Some(strip)
}

// ---- Settings → Voice --------------------------------------------------------------

const LANGUAGES: [(&str, &str); 6] = [
    ("auto", "Auto"),
    ("en", "English"),
    ("zh-CN", "简体中文"),
    ("zh-TW", "繁體中文"),
    ("ja", "日本語"),
    ("ko", "한국어"),
];

pub fn settings(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Vec<El> {
    use super::settings::{
        button, button_w, control_w, field, field_button, group, label, row, row_with, select,
        status, Tone,
    };
    let language = s.cfg.general.language;
    let voice = &s.cfg.voice;
    let mode_at = usize::from(voice.mode == VoiceMode::Realtime);
    let (preset_url, preset_model) = VoiceConfig::preset(voice.mode);
    let mode = SegmentedControl::new(
        &[
            tr_ref(language, "After recording", "录完再转写"),
            tr_ref(language, "Realtime", "实时"),
        ],
        Some(mode_at),
    )
    .fit()
    .label(tr_ref(language, "Mode", "模式"))
    .on_pick(|s: &mut UiState, i, _: &mut Tx| {
        let to = [VoiceMode::File, VoiceMode::Realtime][i];
        let from = s.cfg.voice.mode;
        if from == to {
            return;
        }
        // Follow the built-in preset only; a custom endpoint stays.
        let (from_url, from_model) = VoiceConfig::preset(from);
        let (to_url, to_model) = VoiceConfig::preset(to);
        s.edit_voice(|v| {
            v.mode = to;
            if v.url == from_url {
                v.url = to_url.into();
                if v.model == from_model {
                    v.model = to_model.into();
                }
            }
        });
        s.voice_form.url = TextEdit::new(s.cfg.voice.url.clone());
        s.voice_form.model = TextEdit::new(s.cfg.voice.model.clone());
    })
    .build(k)
    .key("seg-voice-mode");
    let lang_at = LANGUAGES
        .iter()
        .position(|(code, _)| *code == voice.language)
        .unwrap_or(0);
    let labels: Vec<&str> = LANGUAGES
        .iter()
        .map(|(id, l)| {
            if *id == "auto" {
                tr_ref(language, "Auto", "自动")
            } else {
                *l
            }
        })
        .collect();
    let lang = select(s, k, layer, "voice-language", &labels, lang_at, |s, i| {
        s.edit_voice(|v| v.language = LANGUAGES[i].0.into())
    });

    let url_ok = voice.url.trim().is_empty() || voice.usable();
    let url = TextInput::new(&s.voice_form.url)
        .placeholder(preset_url)
        .label(tr_ref(language, "Service URL", "服务地址"))
        .invalid(!url_ok)
        .on_edit(|s: &mut UiState| &mut s.voice_form.url)
        .on_change(|s: &mut UiState, _: &mut Tx| {
            let url = s.voice_form.url.text.trim().to_string();
            s.edit_voice(|v| v.url = url);
        });
    let model = TextInput::new(&s.voice_form.model)
        .placeholder(preset_model)
        .label(tr_ref(language, "Model", "模型"))
        .on_edit(|s: &mut UiState| &mut s.voice_form.model)
        .on_change(|s: &mut UiState, _: &mut Tx| {
            let model = s.voice_form.model.text.trim().to_string();
            s.edit_voice(|v| v.model = model);
        });
    let url_detail = match voice.mode {
        VoiceMode::File => tr_ref(
            language,
            "An OpenAI-compatible /v1/audio/transcriptions endpoint (https).",
            "兼容 OpenAI 的 /v1/audio/transcriptions 地址（https）。",
        ),
        VoiceMode::Realtime => tr_ref(
            language,
            "An OpenAI Realtime-compatible WebSocket in transcription mode (wss).",
            "兼容 OpenAI Realtime 的转写 WebSocket（wss）。",
        ),
    };
    let url_control = if url_ok {
        field(url, &s.voice_form.url, k, "voice-url")
    } else {
        col()
            .key("voice-url-field")
            .w(control_w())
            .gap(6.0)
            .child(field(url, &s.voice_form.url, k, "voice-url"))
            .child(status(
                k,
                "voice-url-error",
                Tone::Bad,
                tr(
                    language,
                    "Needs https:// (after recording) or wss:// (realtime), with no credentials in it.",
                    "需要 https://（录完再转写）或 wss://（实时），且地址中不能包含凭据。",
                ),
            ))
    };

    // The key: typed, saved to the keyring, then shown only as saved. One
    // button beside the field: Save what was typed, else Remove what is
    // saved.
    let typed = !s.voice_form.key.text.trim().is_empty();
    let saved = s.voice_form.key_saved;
    let (save, remove) = (
        tr_ref(language, "Save", "保存"),
        tr_ref(language, "Remove", "删除"),
    );
    let key_button = if typed || saved.is_none() {
        button(save)
            .variant(ButtonVariant::Submit)
            .disabled(!typed)
            .on_press(|s: &mut UiState| s.save_voice_key())
    } else {
        button(remove).on_press(|s: &mut UiState| {
            s.voice_form.key = TextEdit::new("");
            s.save_voice_key();
        })
    };
    let key_field = field_button(
        TextInput::new(&s.voice_form.key)
            .secure()
            .placeholder(if saved.is_some() {
                "••••••••"
            } else {
                "sk-…"
            })
            .label(tr_ref(language, "API key", "API 密钥"))
            .on_edit(|s: &mut UiState| &mut s.voice_form.key)
            .on_submit(|s: &mut UiState, _: &mut Tx| s.save_voice_key()),
        &s.voice_form.key,
        k,
        "voice-key",
        key_button,
        button_w(k, save).max(button_w(k, remove)),
    );
    let mut key_label = label(
        k,
        tr_ref(language, "API key", "API 密钥"),
        tr_ref(
            language,
            "Sent as a Bearer token. Kept in the system keyring, never in config.json.",
            "以 Bearer 令牌发送。保存在系统钥匙串中，不写入 config.json。",
        ),
    );
    if let Some(store) = saved {
        let words = match store {
            Store::Keyring => tr(
                language,
                "Saved in the system keyring",
                "已保存在系统钥匙串",
            ),
            Store::File => tr(
                language,
                "Saved in a file only you can read",
                "已保存在仅你可读的文件中",
            ),
        };
        key_label = key_label.child(status(k, "voice-key-saved", Tone::Good, words).pt(4.0));
    }
    // The keymap's own chord, drawn as keycaps the way this platform writes it.
    let caps = super::keymap::entry(super::keymap::Action::Dictation)
        .keycaps_on(rune::KeyPlatform::current())
        .into_iter()
        .next()
        .unwrap_or_default();
    vec![
        group(
            k,
            "voice-service",
            tr_ref(language, "Speech service", "语音服务"),
            vec![
                row(
                    k,
                    "voice-mode",
                    tr_ref(language, "Mode", "模式"),
                    tr_ref(
                        language,
                        "Realtime shows the words as you speak; after recording sends the whole clip when you finish.",
                        "实时模式边说边显示文字；录完再转写会在结束后上传整段录音。",
                    ),
                    mode,
                ),
                row(
                    k,
                    "voice-url",
                    tr_ref(language, "Service URL", "服务地址"),
                    url_detail,
                    url_control,
                ),
                row(
                    k,
                    "voice-model",
                    tr_ref(language, "Model", "模型"),
                    tr_ref(
                        language,
                        "Optional; empty uses the service's default.",
                        "可选；留空使用服务默认模型。",
                    ),
                    field(model, &s.voice_form.model, k, "voice-model"),
                ),
                row_with(k, "voice-key", key_label, key_field),
            ],
        ),
        group(
            k,
            "voice-dictation",
            tr_ref(language, "Dictation", "听写"),
            vec![
                row(
                    k,
                    "voice-lang",
                    tr_ref(language, "Spoken language", "说话语言"),
                    tr_ref(
                        language,
                        "Auto lets the service decide.",
                        "自动由服务判断。",
                    ),
                    lang,
                ),
                row(
                    k,
                    "voice-shortcut",
                    tr_ref(language, "Start and finish", "开始与结束"),
                    tr_ref(
                        language,
                        "Or the mic button. Escape cancels. The words land in the draft and are never sent on their own.",
                        "也可用麦克风按钮，Esc 取消。文字只放进草稿，不会自动发送。",
                    ),
                    super::shortcuts::keycaps(k, &caps),
                ),
            ],
        ),
    ]
}

/// What Settings → Voice says in its subtitle.
pub fn subtitle(language: Language) -> &'static str {
    tr_ref(
        language,
        "Dictation into the composer: the speech service and its key.",
        "在输入框中语音听写：语音服务与密钥。",
    )
}
