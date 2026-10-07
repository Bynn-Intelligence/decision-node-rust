//! Images sent beside the state.

use std::path::Path;

use base64::Engine;
use serde_json::{json, Value};

use crate::error::{Error, Result};

/// An image every question in the request can see, together with the state.
///
/// The API takes up to 4 images a request, 10 MB each, as JPEG, PNG or WebP
/// bytes or an `https` URL. The server enforces these limits.
///
/// ```no_run
/// use decisionnode::Image;
///
/// let receipt = Image::path("receipt", "receipt.jpg")?;            // read now, type from the bytes
/// let photo = Image::bytes("photo", std::fs::read("photo.png")?)?; // type from the bytes
/// let scan = Image::bytes_with_type("scan", vec![0xFF, 0xD8, 0xFF], "image/jpeg");
/// let remote = Image::url("label", "https://example.com/label.webp");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    id: String,
    source: Source,
}

#[derive(Clone, Debug, PartialEq)]
enum Source {
    Bytes { data: Vec<u8>, media_type: String },
    Url(String),
}

impl Image {
    /// An image read from a file now, when the `Image` is made. Its media type
    /// is taken from the file's first bytes (JPEG, PNG or WebP). A file that
    /// cannot be read, or is none of the three formats, is an error of the
    /// base kind, before any request.
    pub fn path(id: impl Into<String>, path: impl AsRef<Path>) -> Result<Image> {
        let id = id.into();
        let path = path.as_ref();
        let data = std::fs::read(path)
            .map_err(|e| Error::invalid(format!("image {id:?}: cannot read {}: {e}", path.display())).with_source(e))?;
        let media_type = sniff_media_type(&data).ok_or_else(|| {
            Error::invalid(format!("image {id:?}: {} is not a JPEG, PNG or WebP file", path.display()))
        })?;
        Ok(Image { id, source: Source::Bytes { data, media_type: media_type.to_owned() } })
    }

    /// An image from bytes. Its media type is taken from the first bytes (JPEG,
    /// PNG or WebP); any other format is an error of the base kind, before any
    /// request. Use [`Image::bytes_with_type`] to name the type yourself.
    pub fn bytes(id: impl Into<String>, data: impl Into<Vec<u8>>) -> Result<Image> {
        let id = id.into();
        let data = data.into();
        let media_type = sniff_media_type(&data)
            .ok_or_else(|| Error::invalid(format!("image {id:?}: the bytes are not a JPEG, PNG or WebP image")))?;
        Ok(Image { id, source: Source::Bytes { data, media_type: media_type.to_owned() } })
    }

    /// An image from bytes with the given media type, for example `image/png`,
    /// sent as given.
    pub fn bytes_with_type(id: impl Into<String>, data: impl Into<Vec<u8>>, media_type: impl Into<String>) -> Image {
        Image { id: id.into(), source: Source::Bytes { data: data.into(), media_type: media_type.into() } }
    }

    /// An image the server fetches from an `https` URL.
    pub fn url(id: impl Into<String>, url: impl Into<String>) -> Image {
        Image { id: id.into(), source: Source::Url(url.into()) }
    }

    /// The image's id. Text in the state or the instructions can refer to the
    /// image by it.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The media type, for an image from a file or bytes.
    pub fn media_type(&self) -> Option<&str> {
        match &self.source {
            Source::Bytes { media_type, .. } => Some(media_type),
            Source::Url(_) => None,
        }
    }

    pub(crate) fn to_wire(&self) -> Value {
        match &self.source {
            Source::Url(url) => json!({"id": self.id, "url": url}),
            Source::Bytes { data, media_type } => json!({
                "id": self.id,
                "media_type": media_type,
                "data": base64::engine::general_purpose::STANDARD.encode(data),
            }),
        }
    }
}

/// The media type of JPEG, PNG and WebP bytes, from their signature.
pub(crate) fn sniff_media_type(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_the_three_formats() {
        assert_eq!(sniff_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_media_type(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff_media_type(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_media_type(b"GIF89a"), None);
        assert_eq!(sniff_media_type(b""), None);
    }

    #[test]
    fn bytes_and_urls() {
        let v = Image::bytes_with_type("a", vec![1u8, 2, 3], "image/png").to_wire();
        assert_eq!(v, json!({"id": "a", "media_type": "image/png", "data": "AQID"}));
        let v = Image::url("b", "https://example.com/r.jpg").to_wire();
        assert_eq!(v, json!({"id": "b", "url": "https://example.com/r.jpg"}));
    }

    #[test]
    fn bytes_without_a_type_are_detected() {
        let png = Image::bytes("p", b"\x89PNG\r\n\x1a\nrest".to_vec()).unwrap();
        assert_eq!(png.media_type(), Some("image/png"));
        let webp = Image::bytes("w", b"RIFF\x00\x00\x00\x00WEBPVP8 ".to_vec()).unwrap();
        assert_eq!(webp.to_wire()["media_type"], "image/webp");
        let gif = Image::bytes("g", b"GIF89a....".to_vec()).unwrap_err();
        assert!(gif.is_argument());
    }

    #[test]
    fn files_are_read_when_the_image_is_made() {
        let missing = Image::path("x", "/nonexistent/file.png").unwrap_err();
        assert!(missing.is_argument());

        let dir = std::env::temp_dir().join(format!("decisionnode-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gif = dir.join("a.gif");
        std::fs::write(&gif, b"GIF89a....").unwrap();
        assert!(Image::path("g", &gif).unwrap_err().is_argument());

        let png = dir.join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let image = Image::path("p", &png).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        // Already read: removing the file does not change the image.
        assert_eq!(image.to_wire()["media_type"], "image/png");
    }
}
