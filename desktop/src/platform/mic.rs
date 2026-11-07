//! The microphone, for dictation: mono 24 kHz samples, whatever the device
//! records natively (cpal; ALSA on Linux, Core Audio, WASAPI).
//!
//! On Linux the PulseAudio-compatible `pulse` ALSA device is preferred when it
//! exists (PipeWire provides it): it records from the desktop's chosen input
//! and honours `PULSE_SOURCE`, which plain `default` does not on every setup.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};

/// The rate every consumer gets: what OpenAI-Realtime-style services take.
pub const RATE: u32 = 24_000;

/// A running capture; dropping it (or [`Capture::stop`]) ends it.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.halt();
    }
}

/// Why the microphone did not start, in words for the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicError(pub String);

impl std::fmt::Display for MicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn input_device(host: &cpal::Host) -> Option<cpal::Device> {
    if cfg!(target_os = "linux") {
        if let Ok(mut devices) = host.input_devices() {
            #[allow(deprecated)]
            if let Some(pulse) = devices.find(|d| d.name().is_ok_and(|n| n == "pulse")) {
                return Some(pulse);
            }
        }
    }
    host.default_input_device()
}

/// Start recording. `on_samples` gets mono samples in -1..1 at [`RATE`], in
/// chunks as the device delivers them, on the capture thread.
pub fn start(on_samples: impl FnMut(&[f32]) + Send + 'static) -> Result<Capture, MicError> {
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), MicError>>();
    let halt = stop.clone();
    let thread = std::thread::Builder::new()
        .name("muqun-mic".into())
        .spawn(move || {
            // The stream lives on this thread (it is not `Send` everywhere).
            let stream = match open(on_samples) {
                Ok(stream) => stream,
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            while !halt.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(stream);
        })
        .map_err(|err| MicError(err.to_string()))?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(Capture {
            stop,
            thread: Some(thread),
        }),
        Ok(Err(err)) => Err(err),
        Err(_) => {
            stop.store(true, Ordering::Release);
            Err(MicError("the microphone did not start".into()))
        }
    }
}

fn open(mut on_samples: impl FnMut(&[f32]) + Send + 'static) -> Result<cpal::Stream, MicError> {
    let host = cpal::default_host();
    let device = input_device(&host).ok_or_else(|| MicError("no microphone found".into()))?;
    let supported = device
        .default_input_config()
        .map_err(|err| MicError(format!("the microphone cannot record: {err}")))?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = config.channels.max(1) as usize;
    let mut resampler = Resampler::new(config.sample_rate, RATE);
    let mut mono = Vec::new();
    let mut out = Vec::new();
    let stream = device
        .build_input_stream_raw(
            &config,
            format,
            move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
                mono.clear();
                downmix(data, channels, &mut mono);
                out.clear();
                resampler.push(&mono, &mut out);
                if !out.is_empty() {
                    on_samples(&out);
                }
            },
            |err| tracing::warn!("microphone stream: {err}"),
            None,
        )
        .map_err(|err| MicError(format!("the microphone cannot record: {err}")))?;
    stream
        .play()
        .map_err(|err| MicError(format!("the microphone did not start: {err}")))?;
    Ok(stream)
}

fn downmix(data: &cpal::Data, channels: usize, out: &mut Vec<f32>) {
    fn mix<T: Copy>(samples: &[T], channels: usize, out: &mut Vec<f32>, f: impl Fn(T) -> f32) {
        for frame in samples.chunks(channels) {
            out.push(frame.iter().map(|s| f(*s)).sum::<f32>() / channels as f32);
        }
    }
    if let Some(s) = data.as_slice::<f32>() {
        mix(s, channels, out, |x| x);
    } else if let Some(s) = data.as_slice::<i16>() {
        mix(s, channels, out, |x| x as f32 / 32768.0);
    } else if let Some(s) = data.as_slice::<i32>() {
        mix(s, channels, out, |x| x as f32 / 2_147_483_648.0);
    } else if let Some(s) = data.as_slice::<u16>() {
        mix(s, channels, out, |x| (x as f32 - 32768.0) / 32768.0);
    } else if let Some(s) = data.as_slice::<f64>() {
        mix(s, channels, out, |x| x as f32);
    }
}

/// Linear-interpolating rate converter, streaming across chunks. Speech at
/// 24 kHz loses nothing a transcriber listens for.
pub struct Resampler {
    step: f64,
    pos: f64,
    last: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            step: from.max(1) as f64 / to.max(1) as f64,
            pos: 0.0,
            last: 0.0,
        }
    }

    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        // Position is measured from `last` (index -1) through `input`.
        let n = input.len() as f64;
        while self.pos < n {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let a = if i < 1.0 {
                self.last
            } else {
                input[i as usize - 1]
            };
            let b = input[i as usize];
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        self.pos -= n;
        if let Some(last) = input.last() {
            self.last = *last;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_the_duration() {
        let mut r = Resampler::new(48_000, RATE);
        let mut out = Vec::new();
        for _ in 0..100 {
            r.push(&[0.5; 480], &mut out);
        }
        // One second at 48 kHz becomes one second at 24 kHz.
        assert!((out.len() as i64 - 24_000).abs() <= 1, "{}", out.len());
        assert!(out[10..].iter().all(|s| (*s - 0.5).abs() < 1e-6));
    }
}
