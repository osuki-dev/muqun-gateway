//! Theme artwork drawn through Rune's [`ImageCache`], placed as the pack asks
//! (`fit`, `opacity`, `focalPoint` of its decoration slots).
//!
//! The cache shares a decoded texture by content, so a pack that names one
//! picture in several slots (Home and the window backdrop, say) uploads it
//! once, and a slot that stops drawing releases it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use rune::{el, Color, El, ImageCache, SurfaceFit};

use crate::theme::apply::{self, Art};

/// Decode `bytes` through `images` into a fitted picture element.
pub fn picture(images: &ImageCache, bytes: impl Into<Arc<[u8]>>, fit: SurfaceFit) -> El {
    let image = images.load(bytes);
    el().pixels_fit(image.pixels(), fit)
}

/// The decoded handle for a theme picture. Looking a picture up in the
/// cache hashes all its bytes, megabytes for a wallpaper, and the view runs
/// on every keystroke; this remembers the handle per picture instead. A
/// picture the current theme no longer holds is let go on the next miss.
pub fn image(images: &ImageCache, bytes: &Arc<[u8]>) -> rune::Image {
    type Handles = HashMap<usize, (Arc<[u8]>, rune::Image)>;
    thread_local! {
        static HANDLES: std::cell::RefCell<Handles> = Default::default();
    }
    let key = Arc::as_ptr(bytes) as *const u8 as usize;
    HANDLES.with_borrow_mut(|handles| {
        if let Some((_, image)) = handles.get(&key) {
            return image.clone();
        }
        handles.retain(|_, (held, _)| Arc::strong_count(held) > 1);
        let image = images.load(bytes.clone());
        handles.insert(key, (bytes.clone(), image.clone()));
        image
    })
}

/// `art` placed in a `w`×`h` box as the pack asks: covering it (cropped)
/// or contained (whole), either way positioned by the focal point, so the
/// subject stays in view (CSS `object-fit` with `object-position` at the
/// focal point). Clipped hard; a caller softens any edge with `clip_fade`.
pub fn place(images: &ImageCache, art: &Art, key: &str, w: f32, h: f32) -> El {
    let image = image(images, &art.bytes);
    let [iw, ih] = art.size;
    let scale = if art.look.cover {
        (w / iw).max(h / ih)
    } else {
        (w / iw).min(h / ih)
    };
    let (dw, dh) = (iw * scale, ih * scale);
    let [fx, fy] = art.look.focal;
    let picture = el()
        .w(dw)
        .h(dh)
        .pixels_fit(image.pixels(), SurfaceFit::Fill);
    // A contained picture ends inside the box: its edges dissolve rather
    // than stop on a line.
    let picture = if art.look.cover {
        picture.absolute((w - dw) * fx, (h - dh) * fy)
    } else {
        let fade = dw.min(dh) * 0.14;
        el().absolute((w - dw) * fx, (h - dh) * fy)
            .w(dw)
            .h(dh)
            .clip_fade(fade, fade, fade, fade)
            .child(picture)
    };
    el().key(key.to_string())
        .w(w)
        .h(h)
        .clip_fade(0.0, 0.0, 0.0, 0.0)
        .a11y_hidden()
        .child(picture)
}

/// [`place`] as a cover whatever the pack's fit: for a band a painting
/// must fill edge to edge.
pub fn cover(images: &ImageCache, art: &Art, key: &str, w: f32, h: f32) -> El {
    let mut art = art.clone();
    art.look.cover = true;
    place(images, &art, key, w, h)
}

/// `art` fitted whole inside `w`×`h`, and its drawn size. The picture is
/// a child, so a `clip_fade` on the returned box softens its edges.
pub fn contained(images: &ImageCache, art: &Art, w: f32, h: f32) -> (El, [f32; 2]) {
    let [iw, ih] = art.size;
    let scale = (w / iw).min(h / ih);
    let (dw, dh) = (iw * scale, ih * scale);
    let image = image(images, &art.bytes);
    (
        el().w(dw).h(dh).a11y_hidden().child(
            el().w(dw)
                .h(dh)
                .pixels_fit(image.pixels(), SurfaceFit::Fill),
        ),
        [dw, dh],
    )
}

/// The pack's glyph `name` at `size` points, drawn in `color` when it is a
/// template (its alpha is the shape), as it is otherwise. `None` keeps the
/// built-in icon.
pub fn glyph(images: &ImageCache, name: &str, size: f32, color: Color) -> Option<El> {
    glyph_toward(images, name, size, color, None)
}

/// [`glyph`] turned to point `toward` (radians clockwise from right). A
/// directional glyph whose drawn direction the pack does not state keeps
/// the built-in icon rather than point the wrong way.
pub fn glyph_toward(
    images: &ImageCache,
    name: &str,
    size: f32,
    color: Color,
    toward: Option<f32>,
) -> Option<El> {
    let g = apply::glyph(name)?;
    let turn = match toward {
        Some(want) => want - g.direction?,
        None => 0.0,
    };
    // Named, not called inline: the colour-literal lint reads a call to it
    // as a CSS colour function.
    let from_pixels = ImageCache::rgba;
    let image = if g.template {
        from_pixels(images, g.width, g.height, tinted(&g, color))
    } else {
        from_pixels(images, g.width, g.height, g.rgba.clone())
    };
    Some(
        el().key(format!("glyph-{name}"))
            .size(size, size)
            .pixels_fit(image.pixels(), SurfaceFit::Fit)
            .rotate(turn)
            .a11y_hidden(),
    )
}

/// A template glyph recoloured, cached by glyph and colour so a frame does
/// not repaint it.
fn tinted(g: &apply::Glyph, color: Color) -> Arc<[u8]> {
    type Key = (usize, [u8; 4]);
    static CACHE: OnceLock<Mutex<HashMap<Key, Arc<[u8]>>>> = OnceLock::new();
    let enc = |c: f32| (rune::color::srgb_encode(c.clamp(0.0, 1.0)) * 255.0).round() as u8;
    let rgba = [
        enc(color.r),
        enc(color.g),
        enc(color.b),
        (color.a.clamp(0.0, 1.0) * 255.0).round() as u8,
    ];
    let key = (Arc::as_ptr(&g.rgba) as *const u8 as usize, rgba);
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some(bytes) = cache.get(&key) {
        return bytes.clone();
    }
    // A theme switch leaves old glyphs behind; a handful of colours per glyph
    // is all a session ever asks for.
    if cache.len() > 64 {
        cache.clear();
    }
    let bytes: Arc<[u8]> = g
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| {
            let a = (p[3] as u16 * rgba[3] as u16 / 255) as u8;
            [rgba[0], rgba[1], rgba[2], a]
        })
        .collect();
    cache.insert(key, bytes.clone());
    bytes
}
