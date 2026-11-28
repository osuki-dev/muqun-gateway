//! Dictation: record from the microphone, transcribe, and hand the text to
//! the composer's draft -- never sent on its own. The App's two services
//! (`lib/voice-input.ts`, `lib/voice-realtime-protocol.ts`):
//!
//! * **after recording** -- the whole recording, as WAV, to an
//!   OpenAI-compatible `/v1/audio/transcriptions`;
//! * **realtime** -- PCM16 mono 24 kHz streamed to an OpenAI-Realtime-style
//!   WebSocket in transcription mode, one manually committed turn: deltas are
//!   previews shown as subtitles, only the committed item's `completed`
//!   transcript is the result.
//!
//! The capture runs on the microphone thread; the session in a core task;
//! the level meter is shared with the UI so the waveform can be drawn at the
//! display's rate without a message per sample.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest as _, Message};

use crate::config::{VoiceConfig, VoiceMode};
use crate::model::inbox::{Inbox, Update};
use crate::platform::mic;

/// The secret-store name of the voice service's API key.
pub const KEY_NAME: &str = "voice-api-key";
/// The App's cap: a recording stops itself after two minutes.
pub const MAX_RECORDING: Duration = Duration::from_secs(120);
/// Samples per level reading: 20 ms at 24 kHz.
const METER_WINDOW: usize = 480;
/// Seconds of levels kept for the waveform.
const METER_KEEP: f64 = 12.0;

/// What a recording is doing, for the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    /// The microphone is live (and, in realtime mode, the service ready).
    Recording,
    /// Recording ended; waiting for the transcript.
    Transcribing,
    /// A realtime preview of what was said so far.
    Partial(String),
    /// The transcript, for the draft. Empty: nothing was heard.
    Text(String),
    Failed(VoiceFailure),
}

/// Why dictation failed, in the App's categories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceFailure {
    /// The microphone could not record.
    Microphone(String),
    /// The realtime socket closed or never opened.
    Connection,
    /// The service refused or failed the transcription.
    Transcription,
    /// The service answered something that is not a transcript.
    Response,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceControl {
    /// Stop recording and transcribe.
    Finish,
    /// Stop and throw the recording away.
    Cancel,
}

/// Levels (0..1) with the time they were measured, shared with the UI.
#[derive(Debug)]
pub struct Meter {
    pub started: Instant,
    levels: Mutex<VecDeque<(f64, f32)>>,
}

impl Default for Meter {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            levels: Mutex::new(VecDeque::new()),
        }
    }
}

impl Meter {
    /// A meter whose recording began at `started` (screenshot scenes).
    pub fn since(started: Instant) -> Self {
        Self {
            started,
            levels: Mutex::new(VecDeque::new()),
        }
    }

    pub fn push(&self, at: f64, level: f32) {
        let mut levels = self.levels.lock().unwrap();
        levels.push_back((at, level));
        while levels.front().is_some_and(|(t, _)| at - t > METER_KEEP) {
            levels.pop_front();
        }
    }

    /// Seconds since recording started.
    pub fn now(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// The level around `at`: a Gaussian-weighted blend of the readings
    /// within ~3σ, so neighbouring bars flow into each other.
    pub fn blended(&self, at: f64, sigma: f64) -> f32 {
        let levels = self.levels.lock().unwrap();
        let (mut sum, mut weight) = (0.0f64, 0.0f64);
        for (t, level) in levels.iter() {
            let d = (t - at) / sigma;
            if d.abs() > 3.0 {
                continue;
            }
            let w = (-0.5 * d * d).exp();
            sum += w * f64::from(*level);
            weight += w;
        }
        if weight > 0.0 {
            (sum / weight) as f32
        } else {
            0.0
        }
    }

    /// The newest reading.
    pub fn latest(&self) -> f32 {
        self.levels.lock().unwrap().back().map_or(0.0, |(_, l)| *l)
    }
}

/// A loudness reading for speech: RMS on a dB scale, -55 dB .. 0 dB to 0..1
/// (the App's meter range).
fn level_of(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    let db = 20.0 * rms.max(1e-6).log10();
    ((db + 55.0) / 55.0).clamp(0.0, 1.0)
}

/// PCM16 little-endian of `samples`.
pub fn pcm16(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// A mono PCM16 WAV file of `samples` at [`mic::RATE`].
pub fn wav(samples: &[f32]) -> Vec<u8> {
    let data = pcm16(samples);
    let rate = mic::RATE;
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

/// The App's `realtimeSessionUpdate`: transcription only, 24 kHz PCM, no
/// server turn detection (the turn is committed by hand).
pub fn session_update(model: &str, language: &str) -> Value {
    let mut transcription = serde_json::Map::new();
    if !model.is_empty() {
        transcription.insert("model".into(), json!(model));
    }
    if language != "auto" && !language.is_empty() {
        if model.starts_with("gpt-live-transcribe") {
            transcription.insert("languages".into(), json!([language.to_lowercase()]));
        } else {
            let short = language.split('-').next().unwrap_or(language);
            transcription.insert("language".into(), json!(short));
        }
    }
    json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": { "input": {
                "format": { "type": "audio/pcm", "rate": 24000 },
                "transcription": transcription,
                "turn_detection": null,
            }},
        },
    })
}

/// Start dictation: the session runs until `control` says finish or cancel
/// (or two minutes pass), reporting through `inbox`.
pub fn spawn(
    config: VoiceConfig,
    meter: Arc<Meter>,
    inbox: Arc<Inbox>,
    control: mpsc::UnboundedReceiver<VoiceControl>,
) {
    tokio::spawn(async move {
        let push = |event| inbox.push(Update::Voice(event));
        let key = tokio::task::spawn_blocking(|| crate::platform::secrets::load(KEY_NAME))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        match run(&config, &key, meter, control, &push).await {
            Ok(Some(text)) => push(VoiceEvent::Text(text.trim().to_string())),
            Ok(None) => {}
            Err(failure) => push(VoiceEvent::Failed(failure)),
        }
    });
}

/// One dictation: `Ok(None)` when cancelled, else the transcript.
pub async fn run(
    config: &VoiceConfig,
    key: &str,
    meter: Arc<Meter>,
    control: mpsc::UnboundedReceiver<VoiceControl>,
    push: &impl Fn(VoiceEvent),
) -> Result<Option<String>, VoiceFailure> {
    match config.mode {
        VoiceMode::File => after_recording(config, key, meter, control, push).await,
        VoiceMode::Realtime => realtime(config, key, meter, control, push).await,
    }
}

/// Start the microphone: samples come back on a channel, levels go to `meter`.
fn listen(
    meter: &Arc<Meter>,
) -> Result<(mic::Capture, mpsc::UnboundedReceiver<Vec<f32>>), VoiceFailure> {
    let (tx, rx) = mpsc::unbounded_channel();
    let meter = meter.clone();
    let mut pending: Vec<f32> = Vec::new();
    let mut heard = 0usize;
    let capture = mic::start(move |samples| {
        pending.extend_from_slice(samples);
        while pending.len() >= METER_WINDOW {
            let window: Vec<f32> = pending.drain(..METER_WINDOW).collect();
            heard += METER_WINDOW;
            meter.push(heard as f64 / f64::from(mic::RATE), level_of(&window));
            let _ = tx.send(window);
        }
    })
    .map_err(|err| VoiceFailure::Microphone(err.0))?;
    Ok((capture, rx))
}

async fn after_recording(
    config: &VoiceConfig,
    key: &str,
    meter: Arc<Meter>,
    mut control: mpsc::UnboundedReceiver<VoiceControl>,
    push: &impl Fn(VoiceEvent),
) -> Result<Option<String>, VoiceFailure> {
    let (capture, mut samples) = listen(&meter)?;
    push(VoiceEvent::Recording);
    let mut recording: Vec<f32> = Vec::new();
    let deadline = tokio::time::sleep(MAX_RECORDING);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            chunk = samples.recv() => match chunk {
                Some(chunk) => recording.extend_from_slice(&chunk),
                None => break,
            },
            c = control.recv() => match c {
                Some(VoiceControl::Finish) => break,
                Some(VoiceControl::Cancel) | None => {
                    capture.stop();
                    return Ok(None);
                }
            },
            _ = &mut deadline => break,
        }
    }
    capture.stop();
    while let Ok(chunk) = samples.try_recv() {
        recording.extend_from_slice(&chunk);
    }
    push(VoiceEvent::Transcribing);
    if recording.is_empty() {
        return Ok(Some(String::new()));
    }
    let mut form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(wav(&recording))
                .file_name("dictation.wav")
                .mime_str("audio/wav")
                .map_err(|_| VoiceFailure::Transcription)?,
        )
        .text("response_format", "json");
    if !config.model.is_empty() {
        form = form.text("model", config.model.clone());
    }
    if config.language != "auto" && !config.language.is_empty() {
        let short = config
            .language
            .split('-')
            .next()
            .unwrap_or_default()
            .to_string();
        form = form.text("language", short);
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|_| VoiceFailure::Transcription)?;
    let mut request = http.post(config.url.trim()).multipart(form);
    if !key.is_empty() {
        request = request.bearer_auth(key);
    }
    let answer = request.send().await.map_err(|err| {
        tracing::warn!("transcription request failed: {err}");
        VoiceFailure::Transcription
    })?;
    if !answer.status().is_success() {
        tracing::warn!(status = %answer.status(), "transcription refused");
        return Err(VoiceFailure::Transcription);
    }
    let body: Value = answer.json().await.map_err(|_| VoiceFailure::Response)?;
    body.get("text")
        .and_then(Value::as_str)
        .map(|t| Some(t.to_string()))
        .ok_or(VoiceFailure::Response)
}

/// The App's `VoiceRealtimeProtocol`, one manually committed turn.
#[derive(Debug, Default)]
pub struct Turn {
    item: Option<String>,
    transcripts: Vec<(String, String, bool)>,
}

/// What one server event means for the turn.
#[derive(Debug, PartialEq, Eq)]
pub enum TurnStep {
    Ready,
    Partial(String),
    Done(String),
    Failed(VoiceFailure),
    Nothing,
}

impl Turn {
    pub fn receive(&mut self, raw: &str, committing: bool) -> TurnStep {
        let Ok(event) = serde_json::from_str::<Value>(raw) else {
            return TurnStep::Failed(VoiceFailure::Response);
        };
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "session.updated" => TurnStep::Ready,
            "error" | "conversation.item.input_audio_transcription.failed" => {
                TurnStep::Failed(VoiceFailure::Transcription)
            }
            "input_audio_buffer.committed" => {
                let Some(item) = event.get("item_id").and_then(Value::as_str) else {
                    return TurnStep::Failed(VoiceFailure::Response);
                };
                if !committing || self.item.is_some() {
                    return TurnStep::Failed(VoiceFailure::Response);
                }
                self.item = Some(item.to_string());
                self.done().unwrap_or(TurnStep::Nothing)
            }
            "conversation.item.input_audio_transcription.delta"
            | "conversation.item.input_audio_transcription.completed" => {
                let Some(item) = event.get("item_id").and_then(Value::as_str) else {
                    return TurnStep::Failed(VoiceFailure::Response);
                };
                let last = kind.ends_with("completed");
                let text = event
                    .get(if last { "transcript" } else { "delta" })
                    .and_then(Value::as_str);
                let Some(text) = text else {
                    return TurnStep::Failed(VoiceFailure::Response);
                };
                let at = match self.transcripts.iter().position(|(id, _, _)| id == item) {
                    Some(at) => at,
                    None => {
                        if self.transcripts.len() >= 8 {
                            return TurnStep::Failed(VoiceFailure::Response);
                        }
                        self.transcripts
                            .push((item.to_string(), String::new(), false));
                        self.transcripts.len() - 1
                    }
                };
                let entry = &mut self.transcripts[at];
                if entry.2 {
                    return TurnStep::Nothing;
                }
                if last {
                    entry.1 = text.to_string();
                    entry.2 = true;
                } else {
                    entry.1.push_str(text);
                }
                if entry.1.len() > 64_000 {
                    return TurnStep::Failed(VoiceFailure::Response);
                }
                let partial = entry.1.clone();
                if committing {
                    if let Some(done) = self.done() {
                        return done;
                    }
                }
                TurnStep::Partial(partial)
            }
            _ => TurnStep::Nothing,
        }
    }

    fn done(&self) -> Option<TurnStep> {
        let item = self.item.as_deref()?;
        self.transcripts
            .iter()
            .find(|(id, _, last)| id == item && *last)
            .map(|(_, text, _)| TurnStep::Done(text.clone()))
    }
}

async fn realtime(
    config: &VoiceConfig,
    key: &str,
    meter: Arc<Meter>,
    mut control: mpsc::UnboundedReceiver<VoiceControl>,
    push: &impl Fn(VoiceEvent),
) -> Result<Option<String>, VoiceFailure> {
    let mut request = config
        .url
        .trim()
        .into_client_request()
        .map_err(|_| VoiceFailure::Connection)?;
    if !key.is_empty() {
        let value = format!("Bearer {key}")
            .parse()
            .map_err(|_| VoiceFailure::Connection)?;
        request.headers_mut().insert("Authorization", value);
    }
    let connect = tokio_tungstenite::connect_async(request);
    let (socket, _) = tokio::time::timeout(Duration::from_secs(15), connect)
        .await
        .map_err(|_| VoiceFailure::Connection)?
        .map_err(|err| {
            tracing::warn!("realtime transcription did not connect: {err}");
            VoiceFailure::Connection
        })?;
    let (mut tx, mut rx) = socket.split();
    let send = |v: Value| Message::text(v.to_string());
    tx.send(send(session_update(&config.model, &config.language)))
        .await
        .map_err(|_| VoiceFailure::Connection)?;
    let mut turn = Turn::default();
    // Wait for the service to take the session before the microphone opens.
    let ready = async {
        while let Some(message) = rx.next().await {
            let Ok(Message::Text(text)) = message else {
                continue;
            };
            match turn.receive(&text, false) {
                TurnStep::Ready => return Ok(()),
                TurnStep::Failed(failure) => return Err(failure),
                _ => {}
            }
        }
        Err(VoiceFailure::Connection)
    };
    tokio::time::timeout(Duration::from_secs(15), ready)
        .await
        .map_err(|_| VoiceFailure::Connection)??;

    let (capture, mut samples) = listen(&meter)?;
    push(VoiceEvent::Recording);
    let encode = |chunk: &[f32]| base64::engine::general_purpose::STANDARD.encode(pcm16(chunk));
    let mut frames = 0usize;
    let mut batch: Vec<f32> = Vec::new();
    let deadline = tokio::time::sleep(MAX_RECORDING);
    tokio::pin!(deadline);
    let mut cancelled = false;
    loop {
        tokio::select! {
            chunk = samples.recv() => match chunk {
                Some(chunk) => {
                    batch.extend_from_slice(&chunk);
                    // ~100 ms per append, as the App's encoder batches.
                    if batch.len() >= 2400 {
                        frames += batch.len();
                        let audio = encode(&batch);
                        batch.clear();
                        tx.send(send(json!({"type": "input_audio_buffer.append", "audio": audio})))
                            .await
                            .map_err(|_| VoiceFailure::Connection)?;
                    }
                }
                None => break,
            },
            message = rx.next() => match message {
                Some(Ok(Message::Text(text))) => match turn.receive(&text, false) {
                    TurnStep::Partial(partial) => push(VoiceEvent::Partial(partial)),
                    TurnStep::Failed(failure) => {
                        capture.stop();
                        return Err(failure);
                    }
                    _ => {}
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                    capture.stop();
                    return Err(VoiceFailure::Connection);
                }
                Some(Ok(_)) => {}
            },
            c = control.recv() => match c {
                Some(VoiceControl::Finish) => break,
                Some(VoiceControl::Cancel) | None => {
                    cancelled = true;
                    break;
                }
            },
            _ = &mut deadline => break,
        }
    }
    capture.stop();
    if cancelled {
        let _ = tx.send(Message::Close(None)).await;
        return Ok(None);
    }
    while let Ok(chunk) = samples.try_recv() {
        batch.extend_from_slice(&chunk);
    }
    push(VoiceEvent::Transcribing);
    if frames + batch.len() == 0 {
        let _ = tx.send(Message::Close(None)).await;
        return Ok(Some(String::new()));
    }
    // A commit needs at least 100 ms of audio: pad with silence.
    if frames + batch.len() < 2400 {
        batch.resize(2400 - frames, 0.0);
    }
    if !batch.is_empty() {
        tx.send(send(
            json!({"type": "input_audio_buffer.append", "audio": encode(&batch)}),
        ))
        .await
        .map_err(|_| VoiceFailure::Connection)?;
    }
    tx.send(send(json!({"type": "input_audio_buffer.commit"})))
        .await
        .map_err(|_| VoiceFailure::Connection)?;
    let finish = async {
        while let Some(message) = rx.next().await {
            match message {
                Ok(Message::Text(text)) => match turn.receive(&text, true) {
                    TurnStep::Done(text) => return Ok(text),
                    TurnStep::Partial(partial) => push(VoiceEvent::Partial(partial)),
                    TurnStep::Failed(failure) => return Err(failure),
                    _ => {}
                },
                Ok(Message::Close(_)) | Err(_) => return Err(VoiceFailure::Connection),
                Ok(_) => {}
            }
        }
        Err(VoiceFailure::Connection)
    };
    let text = tokio::time::timeout(Duration::from_secs(30), finish)
        .await
        .map_err(|_| VoiceFailure::Transcription)??;
    let _ = tx.send(Message::Close(None)).await;
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deltas preview; only the committed item's `completed` is the result.
    #[test]
    fn a_realtime_turn_resolves_on_the_committed_items_completion() {
        let mut turn = Turn::default();
        let ev = |v: Value| v.to_string();
        assert_eq!(
            turn.receive(&ev(json!({"type": "session.updated"})), false),
            TurnStep::Ready
        );
        let delta = |text: &str| {
            ev(
                json!({"type": "conversation.item.input_audio_transcription.delta", "item_id": "a", "delta": text}),
            )
        };
        assert_eq!(
            turn.receive(&delta("Hel"), false),
            TurnStep::Partial("Hel".into())
        );
        assert_eq!(
            turn.receive(&delta("lo"), false),
            TurnStep::Partial("Hello".into())
        );
        assert_eq!(
            turn.receive(
                &ev(json!({"type": "input_audio_buffer.committed", "item_id": "a"})),
                true
            ),
            TurnStep::Nothing
        );
        assert_eq!(
            turn.receive(
                &ev(json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": "a", "transcript": "Hello."})),
                true
            ),
            TurnStep::Done("Hello.".into())
        );
        assert_eq!(
            Turn::default().receive(&ev(json!({"type": "error"})), false),
            TurnStep::Failed(VoiceFailure::Transcription)
        );
    }

    /// The whole path with a real microphone and a real service: run with a
    /// virtual mic playing speech, e.g.
    /// `PULSE_SOURCE=mqmic MQ_VOICE_WAV=… MQ_VOICE_PLAY_TARGET=mqvoice
    ///  MQ_VOICE_RT_URL=wss://… MQ_VOICE_FILE_URL=https://… MQ_VOICE_KEY_FILE=…
    ///  cargo test -p muqun-desktop dictation_end_to_end -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a microphone, speech playing into it, and a speech service"]
    async fn dictation_end_to_end() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
        let key = std::fs::read_to_string(env("MQ_VOICE_KEY_FILE")).unwrap();
        let key = key.trim().to_string();
        for (mode, url) in [
            (VoiceMode::Realtime, env("MQ_VOICE_RT_URL")),
            (VoiceMode::File, env("MQ_VOICE_FILE_URL")),
        ] {
            let config = VoiceConfig {
                mode,
                url,
                model: String::new(),
                language: "auto".into(),
            };
            let (control, rx) = mpsc::unbounded_channel();
            let meter = Arc::new(Meter::default());
            let events = Arc::new(Mutex::new(Vec::new()));
            let seen = events.clone();
            let push = move |e: VoiceEvent| seen.lock().unwrap().push(e);
            let player = {
                let (wav, target) = (env("MQ_VOICE_WAV"), env("MQ_VOICE_PLAY_TARGET"));
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let _ = tokio::process::Command::new("pw-play")
                        .args(["--target", &target, &wav])
                        .status()
                        .await;
                })
            };
            let session = {
                let (meter, key) = (meter.clone(), key.clone());
                tokio::spawn(async move { run(&config, &key, meter, rx, &push).await })
            };
            player.await.unwrap();
            tokio::time::sleep(Duration::from_millis(800)).await;
            control.send(VoiceControl::Finish).unwrap();
            let text = session
                .await
                .unwrap()
                .expect("transcribed")
                .expect("not cancelled");
            let partials = events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, VoiceEvent::Partial(_)))
                .count();
            let loudest = (0..200)
                .map(|i| meter.blended(f64::from(i) * 0.1, 0.05))
                .fold(0.0f32, f32::max);
            println!(
                "{mode:?}: {} chars, {partials} previews, peak level {loudest:.2}: {text}",
                text.chars().count()
            );
            assert!(text.chars().count() > 10, "{mode:?}: {text:?}");
            assert!(loudest > 0.3, "the meter heard the speech");
            if mode == VoiceMode::Realtime {
                assert!(partials > 0, "realtime previews arrived");
            }
        }
    }

    #[test]
    fn the_session_asks_for_24k_pcm_and_the_language() {
        let update = session_update("gpt-4o-transcribe", "zh-CN");
        let input = &update["session"]["audio"]["input"];
        assert_eq!(input["format"]["rate"], 24000);
        assert_eq!(input["transcription"]["language"], "zh");
        assert!(input["turn_detection"].is_null());
        let live = session_update("gpt-live-transcribe", "zh-CN");
        assert_eq!(
            live["session"]["audio"]["input"]["transcription"]["languages"][0],
            "zh-cn"
        );
    }
}
