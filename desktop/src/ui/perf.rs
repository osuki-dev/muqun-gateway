//! On-screen frame meter so stutter is measurable while polishing.
//!
//! A permanent animation tick keeps the loop running and samples every
//! frame; the label refreshes a few times a second so the overlay itself
//! does not thrash the tree.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use blinc_core::State;
use blinc_layout::prelude::*;

use super::text::mono_text;

const SAMPLE: std::time::Duration = std::time::Duration::from_millis(250);

struct Meter {
    frames: u32,
    sample_start: Instant,
    /// Smoothed frame time in milliseconds.
    ema_ms: f32,
    label: State<String>,
}

/// Install once (main thread, Blinc up). Keeps a tick callback for the
/// life of the process so FPS stays current while idle or busy.
pub fn install() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(scheduler) = blinc_animation::try_get_scheduler() else {
        DONE.store(false, Ordering::Relaxed);
        return;
    };
    let label = blinc_core::BlincContextState::get()
        .use_state_keyed("mq.perf.fps", || "— FPS".to_string());
    let meter = Arc::new(Mutex::new(Meter {
        frames: 0,
        sample_start: Instant::now(),
        ema_ms: 16.7,
        label: label.clone(),
    }));
    let _ = scheduler.register_tick_callback(move |dt| {
        let Ok(mut m) = meter.lock() else {
            return;
        };
        m.frames += 1;
        // `dt` is seconds since the last scheduler tick.
        if dt.is_finite() && dt > 0.000_1 {
            let ms = dt * 1000.0;
            m.ema_ms = if m.ema_ms <= 0.0 {
                ms
            } else {
                m.ema_ms * 0.85 + ms * 0.15
            };
        }
        if m.sample_start.elapsed() < SAMPLE {
            return;
        }
        let secs = m.sample_start.elapsed().as_secs_f32().max(0.001);
        let fps = m.frames as f32 / secs;
        m.frames = 0;
        m.sample_start = Instant::now();
        let text = format!("{fps:4.0} FPS · {:>4.1} ms", m.ema_ms);
        if m.label.get() != text {
            m.label.signal().set(text);
        }
    });
    // Keep the loop alive so idle FPS is still measured.
    scheduler.request_redraw();
}

/// Corner overlay; reads the shared label state.
pub fn overlay() -> Div {
    let label = blinc_core::BlincContextState::get()
        .use_state_keyed("mq.perf.fps", || "— FPS".to_string());
    div()
        .class("mq-fps")
        .absolute()
        .right(12.0)
        .top(8.0)
        .child(
            stateful_with_key::<NoState>("mq-fps-label")
                .deps([label.signal_id()])
                .on_state(move |_| {
                    div()
                        .class("mq-fps-label")
                        .child(mono_text(label.get()).size(11.0).no_wrap().no_cursor())
                }),
        )
}
