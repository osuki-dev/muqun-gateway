use gtk::{cairo, gdk, gio, glib};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc::SyncSender,
    },
    time::{Duration, Instant},
};
use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};

/// Owns a single scheduled capture or a single WebKit request. Hidden tabs
/// stop scheduling entirely; an in-flight request may finish and be discarded.
pub struct Frames {
    web: webkit2gtk::WebView,
    sink: SyncSender<Vec<u8>>,
    pub size: Cell<(i32, i32)>,
    visible: Cell<bool>,
    pending: Cell<bool>,
    timer: RefCell<Option<glib::SourceId>>,
    idle: Arc<AtomicU8>,
}

impl Frames {
    pub fn new(
        web: webkit2gtk::WebView,
        sink: SyncSender<Vec<u8>>,
        size: (i32, i32),
        idle: Arc<AtomicU8>,
    ) -> Rc<Self> {
        let this = Rc::new(Self {
            web,
            sink,
            size: Cell::new(size),
            visible: Cell::new(true),
            pending: Cell::new(false),
            timer: RefCell::new(None),
            idle,
        });
        this.schedule(Duration::ZERO);
        this
    }
    pub fn set_visible(self: &Rc<Self>, visible: bool) {
        self.visible.set(visible);
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        if visible {
            self.idle.store(0, Ordering::Relaxed);
            self.schedule(Duration::ZERO);
        }
    }
    pub fn input(self: &Rc<Self>) {
        if self.idle.swap(0, Ordering::Relaxed) >= 4 {
            if let Some(timer) = self.timer.take() {
                timer.remove();
            }
            self.schedule(Duration::ZERO);
        }
    }
    fn schedule(self: &Rc<Self>, delay: Duration) {
        if !self.visible.get() || self.pending.get() || self.timer.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(delay, move || {
            if let Some(this) = weak.upgrade() {
                this.timer.take();
                this.capture();
            }
        });
        self.timer.replace(Some(id));
    }
    fn capture(self: &Rc<Self>) {
        self.pending.set(true);
        let started = Instant::now();
        let weak = Rc::downgrade(self);
        let size = self.size.get();
        self.web.snapshot(
            SnapshotRegion::Visible,
            SnapshotOptions::INCLUDE_SELECTION_HIGHLIGHTING,
            None::<&gio::Cancellable>,
            move |res| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.pending.set(false);
                if !this.visible.get() {
                    return;
                }
                if this.size.get() == size
                    && let Ok(surface) = res
                    && let Some(frame) = pack_frame(surface, size)
                {
                    let _ = this.sink.try_send(frame);
                }
                let interval = match this.idle.load(Ordering::Relaxed) {
                    // Budget raw transport by pixel count. Count snapshot work
                    // toward the interval rather than sleeping a whole frame after it.
                    0..=3 => {
                        let pixels = size.0 as u64 * size.1 as u64;
                        (pixels * 1000 / 90_000_000).clamp(16, 160)
                    }
                    4..=7 => 240,
                    _ => 800,
                };
                this.schedule(Duration::from_millis(interval).saturating_sub(started.elapsed()));
            },
        );
    }
}

fn pack_frame(surface: cairo::Surface, (width, height): (i32, i32)) -> Option<Vec<u8>> {
    let image = cairo::ImageSurface::try_from(surface).ok()?;
    // The snapshot is already in device pixels. Ignore a stale allocation
    // during resize instead of stretching an old frame into the new viewport.
    if (image.width(), image.height()) != (width, height) {
        return None;
    }
    let format = image.format();
    if matches!(format, cairo::Format::Rgb24 | cairo::Format::ARgb32) {
        // WebKit paints native-endian, premultiplied ARGB. Write straight RGBA
        // directly into the wire frame without allocating a full-size GdkPixbuf.
        let mut frame = vec![0; 8 + width as usize * height as usize * 4];
        frame[..4].copy_from_slice(&(width as u32).to_le_bytes());
        frame[4..8].copy_from_slice(&(height as u32).to_le_bytes());
        let stride = image.stride() as usize;
        image
            .with_data(|bytes| {
                for (src, dst) in bytes
                    .chunks_exact(stride)
                    .zip(frame[8..].chunks_exact_mut(width as usize * 4))
                {
                    for (pixel, out) in src
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .zip(dst.as_chunks_mut::<4>().0)
                    {
                        let [a, r, g, b] = u32::from_ne_bytes(*pixel).to_be_bytes();
                        *out = if format == cairo::Format::Rgb24 || a == 255 {
                            [r, g, b, 255]
                        } else if a == 0 {
                            [0; 4]
                        } else {
                            let alpha = u16::from(a);
                            let straight = |v| ((u16::from(v) * 255 + alpha / 2) / alpha) as u8;
                            [straight(r), straight(g), straight(b), a]
                        };
                    }
                }
            })
            .ok()?;
        return Some(frame);
    }
    let pix = gdk::pixbuf_get_from_surface(&image, 0, 0, width, height)?;
    let bytes = pix.read_pixel_bytes();
    let channels = pix.n_channels();
    let mut frame = Vec::with_capacity(8 + width as usize * height as usize * 4);
    frame.extend_from_slice(&(width as u32).to_le_bytes());
    frame.extend_from_slice(&(height as u32).to_le_bytes());
    for row in bytes.chunks(pix.rowstride() as usize).take(height as usize) {
        if channels == 4 {
            frame.extend_from_slice(&row[..width as usize * 4]);
        } else {
            for rgb in row.as_chunks::<3>().0.iter().take(width as usize) {
                frame.extend_from_slice(rgb);
                frame.push(255);
            }
        }
    }
    Some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_cairo_rows_preserve_straight_rgba_and_skip_padding() {
        let pixels = [
            0x80402010_u32,
            0xff123456,
            0xdeadbeef,
            0,
            0x40102030,
            0xdeadbeef,
        ];
        let data: Vec<u8> = pixels
            .iter()
            .flat_map(|pixel| pixel.to_ne_bytes())
            .collect();
        let img =
            cairo::ImageSurface::create_for_data(data, cairo::Format::ARgb32, 2, 2, 12).unwrap();
        let frame = pack_frame(img.as_ref().clone(), (2, 2)).unwrap();
        assert_eq!(
            &frame[8..],
            &[
                128, 64, 32, 128, 18, 52, 86, 255, 0, 0, 0, 0, 64, 128, 191, 64
            ]
        );
    }
}
