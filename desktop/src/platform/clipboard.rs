//! The system clipboard's image, for pasting a screenshot into the composer.
//! Text goes through Blinc's own `clipboard_read` (the text area handles it);
//! Blinc has no image path, so this asks the same crate Blinc uses.

/// The clipboard's image as PNG bytes, or `None` when it holds none (or the
/// clipboard cannot be opened). PNG because the gateway's upload route
/// accepts it as is and it is lossless.
pub fn read_image_png() -> Option<Vec<u8>> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let image = clipboard.get_image().ok()?;
    encode_png(image.width as u32, image.height as u32, &image.bytes)
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    use image::ImageEncoder as _;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_pixels_become_a_png() {
        let png = encode_png(2, 1, &[255, 0, 0, 255, 0, 255, 0, 255]).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
