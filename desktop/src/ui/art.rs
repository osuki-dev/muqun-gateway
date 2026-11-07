//! Theme artwork drawn through Rune's [`ImageCache`].

use rune::{el, El, ImageCache, SurfaceFit};

use crate::theme::apply;
use crate::theme::color_scheme::ColorScheme;

/// Decode `bytes` through `images` into a fitted picture element.
pub fn picture(images: &ImageCache, bytes: impl Into<std::sync::Arc<[u8]>>, fit: SurfaceFit) -> El {
    let image = images.load(bytes);
    el().pixels_fit(image.pixels(), fit)
}

/// Soft full-bleed wallpaper behind the Home page.
pub fn wallpaper(images: &ImageCache, scheme: ColorScheme, dark: bool) -> Option<El> {
    let bytes = apply::current_wallpaper(scheme)?;
    let opacity = if dark { 0.20 } else { 0.14 };
    Some(
        picture(images, bytes, SurfaceFit::Fill)
            .key("theme-wallpaper")
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .opacity(opacity)
            .a11y_hidden(),
    )
}

/// The theme's Home figure, anchored upper-right.
pub fn figure(images: &ImageCache, scheme: ColorScheme, dark: bool) -> Option<El> {
    let bytes = apply::current_artwork(scheme).or_else(|| apply::current_cover(scheme))?;
    let opacity = if dark { 0.92 } else { 0.88 };
    Some(
        picture(images, bytes, SurfaceFit::Fit)
            .key("theme-figure")
            .pin(None, Some(12.0), Some(8.0), None)
            .w(460.0)
            .h(520.0)
            .opacity(opacity)
            .a11y_hidden(),
    )
}
