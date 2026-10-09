//! Detects supported image payloads once for tool reads and Task attachments.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageMetadata {
    pub mime_type: &'static str,
    pub width: i32,
    pub height: i32,
}

pub(crate) fn image_metadata(data: &[u8]) -> Option<ImageMetadata> {
    let reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    let (width, height) = reader.into_dimensions().ok()?;
    let width = i32::try_from(width).ok()?;
    let height = i32::try_from(height).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let mime_type = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => return None,
    };
    Some(ImageMetadata {
        mime_type,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    #[test]
    fn detects_supported_images_and_rejects_truncated_data() {
        let png = STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
            .unwrap();
        assert_eq!(
            image_metadata(&png),
            Some(ImageMetadata {
                mime_type: "image/png",
                width: 1,
                height: 1,
            })
        );
        assert_eq!(image_metadata(b"\x89PNG\r\n\x1a\n"), None);
    }
}
