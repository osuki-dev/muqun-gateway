//! Theme artwork drawn through Rune's [`ImageCache`], placed as the pack asks
//! (`fit`, `opacity`, `focalPoint` of its decoration slots).

use rune::{el, El, ImageCache, SurfaceFit};

use crate::theme::apply;
use crate::theme::color_scheme::ColorScheme;
use crate::theme::pack::SlotLook;

/// Decode `bytes` through `images` into a fitted picture element.
pub fn picture(images: &ImageCache, bytes: impl Into<std::sync::Arc<[u8]>>, fit: SurfaceFit) -> El {
    let image = images.load(bytes);
    el().pixels_fit(image.pixels(), fit)
}

fn fit_of(look: SlotLook) -> SurfaceFit {
    if look.cover {
        SurfaceFit::Fill
    } else {
        SurfaceFit::Fit
    }
}

/// Full-bleed wallpaper behind the Home page, at the pack's opacity (scaled
/// by `dim` so copy over it stays readable).
pub fn wallpaper(images: &ImageCache, scheme: ColorScheme, dim: f32) -> Option<El> {
    let bytes = apply::current_wallpaper(scheme)?;
    let look = apply::slot_look("home.wallpaper", scheme);
    Some(
        picture(images, bytes, SurfaceFit::Fill)
            .key("theme-wallpaper")
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .opacity((look.opacity * dim).clamp(0.0, 1.0))
            .a11y_hidden(),
    )
}

/// The theme's Home figure, anchored to the stage's upper right (a
/// `contain` figure) or covering a band across the top (a `cover` scene).
pub fn figure(images: &ImageCache, scheme: ColorScheme, w: f32, h: f32) -> Option<El> {
    let bytes = apply::current_artwork(scheme)?;
    let look = apply::slot_look("home.artwork", scheme);
    let pic = picture(images, bytes, fit_of(look))
        .key("theme-figure")
        .opacity(look.opacity)
        .a11y_hidden();
    Some(if look.cover {
        pic.pin(Some(0.0), Some(0.0), Some(0.0), None).h(h)
    } else {
        pic.pin(None, Some(0.0), Some(0.0), None).w(w).h(h)
    })
}

/// Whether the Home figure is a full-width scene rather than a cut-out.
pub fn figure_covers(scheme: ColorScheme) -> bool {
    apply::current_artwork(scheme).is_some() && apply::slot_look("home.artwork", scheme).cover
}

/// The launch picture, at the pack's fit.
pub fn launch(images: &ImageCache, scheme: ColorScheme) -> Option<(El, SlotLook)> {
    let bytes = apply::current_launch(scheme)?;
    let look = apply::slot_look("launch.artwork", scheme);
    Some((picture(images, bytes, fit_of(look)), look))
}
