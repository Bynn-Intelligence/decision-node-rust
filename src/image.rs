//! Images sent beside the state, and the local checks every image passes
//! before a request leaves the client.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use base64::Engine;
use bytes::Bytes;
use serde_json::{Map, Value};
use sha2::Digest;

use crate::check::Limits;
use crate::error::{Error, ErrorKind, Result};

/// The most images one request (or one batch line) may carry, URLs included.
pub(crate) const MAX_IMAGES: usize = 16;
/// The largest image the API takes, in bytes.
pub(crate) const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// The most pixels the API decodes, read from the image header.
pub(crate) const MAX_IMAGE_PIXELS: u64 = 40_000_000;
/// The longest image URL the API takes, in characters.
pub(crate) const MAX_URL_CHARS: usize = 2048;
/// The media types the API takes.
const MEDIA_TYPES: [&str; 3] = ["image/jpeg", "image/png", "image/webp"];

/// An image every question in the request can see, together with the state.
///
/// Four forms, from fastest to slowest:
///
/// - [`Image::url`]: an `https` URL the API fetches. Nothing is uploaded and
///   the request stays small, so this is the form to use when the image is
///   already online.
/// - [`Image::upload_id`]: an image put earlier with
///   [`crate::DecisionNode::upload`], sent by its id.
/// - [`Image::path`], [`Image::bytes`], [`Image::base64`]: local bytes. They
///   are checked locally (JPEG, PNG or WebP by their first bytes, at most
///   10 MB, at most 40 megapixels read from the header), then sent inline
///   when the request's local images total under the client's inline
///   threshold ([`crate::DEFAULT_INLINE_THRESHOLD`], 512 KiB), and uploaded
///   otherwise. An image the client uploaded before is sent by its cached
///   upload id, whatever its size.
///
/// No image is resized or re-encoded. Cloning an `Image` is cheap: the bytes
/// are shared.
///
/// ```no_run
/// use decisionnode::Image;
///
/// let remote = Image::url("label", "https://example.com/label.webp"); // recommended
/// let receipt = Image::path("receipt", "receipt.jpg")?;              // read now
/// let photo = Image::bytes("photo", std::fs::read("photo.png")?);     // type from the bytes
/// let scan = Image::bytes_with_type("scan", std::fs::read("scan.jpg")?, "image/jpeg");
/// let pasted = Image::base64("pasted", "iVBORw0KGgo...")?;
/// let earlier = Image::upload_id("earlier", "upl_3f6c1a9e0b2d4c7f8a1e5d9b2c4f6a80");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct Image {
    id: String,
    source: Source,
}

#[derive(Clone)]
pub(crate) enum Source {
    Local(Arc<Local>),
    Url(String),
    Upload { upload_id: String, media_type: Option<String> },
}

/// Local bytes with what is known about them.
pub(crate) struct Local {
    pub(crate) data: Bytes,
    /// The media type the caller declared, if any; it must match the bytes.
    declared: Option<String>,
    /// The media type read from the first bytes.
    sniffed: Option<&'static str>,
    sha256: OnceLock<[u8; 32]>,
}

impl Local {
    fn new(data: Bytes, declared: Option<String>) -> Local {
        Local { sniffed: sniff_media_type(&data), data, declared, sha256: OnceLock::new() }
    }

    /// The type sent with the bytes: the one read from them (which a declared
    /// type must equal, see [`check_images`]), else the declared one.
    pub(crate) fn media_type(&self) -> &str {
        self.sniffed.or(self.declared.as_deref()).unwrap_or("application/octet-stream")
    }

    /// The SHA-256 of the bytes, computed once.
    pub(crate) fn sha256(&self) -> &[u8; 32] {
        self.sha256.get_or_init(|| sha2::Sha256::digest(&self.data).into())
    }
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Image");
        d.field("id", &self.id);
        match &self.source {
            Source::Local(l) => d.field("media_type", &l.media_type()).field("bytes", &l.data.len()),
            Source::Url(u) => d.field("url", u),
            Source::Upload { upload_id, media_type } => d.field("upload_id", upload_id).field("media_type", media_type),
        };
        d.finish()
    }
}

impl PartialEq for Image {
    fn eq(&self, other: &Image) -> bool {
        self.id == other.id
            && match (&self.source, &other.source) {
                (Source::Local(a), Source::Local(b)) => a.data == b.data && a.declared == b.declared,
                (Source::Url(a), Source::Url(b)) => a == b,
                (Source::Upload { upload_id: a, media_type: ma }, Source::Upload { upload_id: b, media_type: mb }) => {
                    a == b && ma == mb
                }
                _ => false,
            }
    }
}

impl Image {
    /// An image the API fetches from an `https` URL: the fastest form, since
    /// nothing is uploaded. Checked locally before any request: `https` and at
    /// most 2,048 characters.
    pub fn url(id: impl Into<String>, url: impl Into<String>) -> Image {
        Image { id: id.into(), source: Source::Url(url.into()) }
    }

    /// An image read from a file now, when the `Image` is made. A file that
    /// cannot be read is an error of the base kind ([`Error::is_argument`]).
    /// Its type is read from the first bytes when the request is checked.
    pub fn path(id: impl Into<String>, path: impl AsRef<Path>) -> Result<Image> {
        let id = id.into();
        let path = path.as_ref();
        let data = std::fs::read(path)
            .map_err(|e| Error::invalid(format!("image {id:?}: cannot read {}: {e}", path.display())).with_source(e))?;
        Ok(Image::local(id, data.into(), None))
    }

    /// An image from bytes. Its media type is read from the first bytes (JPEG,
    /// PNG or WebP); any other format is refused locally with a
    /// [`ErrorKind::BadRequest`] error when the request is checked, before
    /// anything is sent.
    pub fn bytes(id: impl Into<String>, data: impl Into<Bytes>) -> Image {
        Image::local(id.into(), data.into(), None)
    }

    /// An image from bytes with a declared media type, for example
    /// `image/png`. The type must match the bytes; a mismatch is refused
    /// locally like an unknown format.
    pub fn bytes_with_type(id: impl Into<String>, data: impl Into<Bytes>, media_type: impl Into<String>) -> Image {
        Image::local(id.into(), data.into(), Some(media_type.into()))
    }

    /// An image from base64 text (the standard alphabet, padded or not,
    /// without a `data:` prefix). Text that is not base64 is an error of the
    /// base kind.
    pub fn base64(id: impl Into<String>, text: impl AsRef<str>) -> Result<Image> {
        let id = id.into();
        let data = decode_base64(text.as_ref())
            .map_err(|e| Error::invalid(format!("image {id:?}: the text is not base64: {e}")).with_source(e))?;
        Ok(Image::local(id, data.into(), None))
    }

    /// An image put earlier with [`crate::DecisionNode::upload`] (or any
    /// upload of the same workspace less than an hour old), sent as given.
    pub fn upload_id(id: impl Into<String>, upload_id: impl Into<String>) -> Image {
        Image { id: id.into(), source: Source::Upload { upload_id: upload_id.into(), media_type: None } }
    }

    /// [`Image::upload_id`] with the upload's media type, which the API checks
    /// against the type declared at upload.
    pub fn upload_id_with_type(
        id: impl Into<String>,
        upload_id: impl Into<String>,
        media_type: impl Into<String>,
    ) -> Image {
        Image {
            id: id.into(),
            source: Source::Upload { upload_id: upload_id.into(), media_type: Some(media_type.into()) },
        }
    }

    fn local(id: String, data: Bytes, declared: Option<String>) -> Image {
        Image { id, source: Source::Local(Arc::new(Local::new(data, declared))) }
    }

    /// The image's id. Text in the state or the instructions can refer to the
    /// image by it.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The media type: read from the bytes of a local image (or the declared
    /// one when the bytes are of no known format), the given one of an upload,
    /// none for a URL.
    pub fn media_type(&self) -> Option<&str> {
        match &self.source {
            Source::Local(l) => l.sniffed.or(l.declared.as_deref()),
            Source::Upload { media_type, .. } => media_type.as_deref(),
            Source::Url(_) => None,
        }
    }

    /// The size in bytes of a local image.
    pub fn len(&self) -> Option<usize> {
        match &self.source {
            Source::Local(l) => Some(l.data.len()),
            _ => None,
        }
    }

    /// True for a local image of no bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }

    pub(crate) fn as_local(&self) -> Option<&Arc<Local>> {
        match &self.source {
            Source::Local(l) => Some(l),
            _ => None,
        }
    }

    pub(crate) fn is_url(&self) -> bool {
        matches!(self.source, Source::Url(_))
    }

    /// The wire form when the image is sent as it is: a URL, an upload id or
    /// inline base64 data. `with_id` is false for a session frame, whose image
    /// carries no id.
    pub(crate) fn wire(&self, with_id: bool) -> Value {
        let mut out = Map::new();
        if with_id {
            out.insert("id".into(), Value::String(self.id.clone()));
        }
        match &self.source {
            Source::Url(url) => {
                out.insert("url".into(), Value::String(url.clone()));
            }
            Source::Upload { upload_id, media_type } => {
                out.insert("upload_id".into(), Value::String(upload_id.clone()));
                if let Some(t) = media_type {
                    out.insert("media_type".into(), Value::String(t.clone()));
                }
            }
            Source::Local(l) => {
                out.insert("media_type".into(), Value::String(l.media_type().to_owned()));
                out.insert("data".into(), Value::String(base64::engine::general_purpose::STANDARD.encode(&l.data)));
            }
        }
        Value::Object(out)
    }

    /// The image as a `422` entry lists it: its wire form, a local image's
    /// bytes left out (`{"id", "media_type"}`).
    pub(crate) fn input(&self) -> Value {
        match &self.source {
            Source::Local(l) => {
                let mut out = Map::new();
                out.insert("id".into(), Value::String(self.id.clone()));
                if let Some(t) = l.sniffed.or(l.declared.as_deref()) {
                    out.insert("media_type".into(), Value::String(t.to_owned()));
                }
                Value::Object(out)
            }
            _ => self.wire(true),
        }
    }

    /// The wire form naming an upload.
    pub(crate) fn wire_upload(&self, upload_id: &str, with_id: bool) -> Value {
        let mut out = Map::new();
        if with_id {
            out.insert("id".into(), Value::String(self.id.clone()));
        }
        out.insert("upload_id".into(), Value::String(upload_id.to_owned()));
        Value::Object(out)
    }
}

fn decode_base64(text: &str) -> std::result::Result<Vec<u8>, base64::DecodeError> {
    let text = text.trim();
    let engine = if text.ends_with('=') {
        &base64::engine::general_purpose::STANDARD
    } else {
        &base64::engine::general_purpose::STANDARD_NO_PAD
    };
    engine.decode(text)
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

/// Width and height in pixels, read from the image header without decoding:
/// PNG `IHDR`, the JPEG start-of-frame segment, and the WebP `VP8 `, `VP8L`
/// and `VP8X` chunks. `None` when the header cannot be read.
pub(crate) fn dimensions(data: &[u8], media_type: &str) -> Option<(u32, u32)> {
    match media_type {
        "image/png" => png_dimensions(data),
        "image/jpeg" => jpeg_dimensions(data),
        "image/webp" => webp_dimensions(data),
        _ => None,
    }
}

fn be16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn le16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn le24(data: &[u8], at: usize) -> Option<u32> {
    let b = data.get(at..at + 3)?;
    Some(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
}

fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // Signature (8), chunk length (4), "IHDR", then width and height.
    if data.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be32(data, 16)?, be32(data, 20)?))
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2; // after SOI
    loop {
        if *data.get(i)? != 0xFF {
            return None;
        }
        let marker = *data.get(i + 1)?;
        match marker {
            // Fill byte before a marker.
            0xFF => i += 1,
            // Markers without a length: TEM and RSTn.
            0x01 | 0xD0..=0xD7 => i += 2,
            // A second SOI, the end of the image or the start of the scan
            // before any frame header: no dimensions to read.
            0xD8..=0xDA => return None,
            // Start of frame, every coding process (not DHT C4, JPG C8, DAC CC).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                let height = be16(data, i + 5)?;
                let width = be16(data, i + 7)?;
                return Some((u32::from(width), u32::from(height)));
            }
            _ => {
                let length = usize::from(be16(data, i + 2)?);
                if length < 2 {
                    return None;
                }
                i += 2 + length;
            }
        }
    }
}

fn webp_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    match data.get(12..16)? {
        b"VP8 " => {
            // Frame tag (3 bytes) at 20, the start code 9D 01 2A at 23, then
            // 14-bit width and height with 2 scale bits each.
            if data.get(23..26)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            Some((u32::from(le16(data, 26)? & 0x3FFF), u32::from(le16(data, 28)? & 0x3FFF)))
        }
        b"VP8L" => {
            // Signature 0x2F at 20, then 14 bits of width - 1 and of height - 1.
            if *data.get(20)? != 0x2F {
                return None;
            }
            let bits = u32::from_le_bytes(data.get(21..25)?.try_into().ok()?);
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        b"VP8X" => Some((le24(data, 24)? + 1, le24(data, 27)? + 1)),
        _ => None,
    }
}

fn refuse(message: String) -> Error {
    Error::local(ErrorKind::BadRequest, Some("api_usage_error"), message)
}

/// A literal IPv4 address in a range the API never fetches from.
fn blocked_v4(ip: Ipv4Addr) -> bool {
    let n = u32::from(ip);
    const RANGES: [(u32, u32); 16] = [
        (0x0000_0000, 8),  // 0.0.0.0/8
        (0x0A00_0000, 8),  // 10.0.0.0/8
        (0x6440_0000, 10), // 100.64.0.0/10
        (0x7F00_0000, 8),  // 127.0.0.0/8
        (0xA9FE_0000, 16), // 169.254.0.0/16
        (0xAC10_0000, 12), // 172.16.0.0/12
        (0xC000_0000, 24), // 192.0.0.0/24
        (0xC000_0200, 24), // 192.0.2.0/24
        (0xC058_6300, 24), // 192.88.99.0/24
        (0xC0A8_0000, 16), // 192.168.0.0/16
        (0xC612_0000, 15), // 198.18.0.0/15
        (0xC633_6400, 24), // 198.51.100.0/24
        (0xCB00_7100, 24), // 203.0.113.0/24
        (0xE000_0000, 4),  // 224.0.0.0/4
        (0xF000_0000, 4),  // 240.0.0.0/4
        (0xFFFF_FFFF, 32), // 255.255.255.255/32
    ];
    RANGES.iter().any(|&(net, bits)| n >> (32 - bits) == net >> (32 - bits))
}

/// A literal IPv6 address in a range the API never fetches from; an
/// IPv4-mapped or NAT64 address by the IPv4 address it embeds.
fn blocked_v6(ip: Ipv6Addr) -> bool {
    let n = u128::from(ip);
    if n >> 32 == 0xFFFF || n >> 32 == 0x0064_FF9B_0000_0000_0000_0000 {
        return blocked_v4(Ipv4Addr::from(n as u32));
    }
    const RANGES: [(u128, u32); 7] = [
        (0, 128),                           // ::/128
        (1, 128),                           // ::1/128
        (0xFC00 << 112, 7),                 // fc00::/7
        (0xFE80 << 112, 10),                // fe80::/10
        (0xFF00 << 112, 8),                 // ff00::/8
        (0x2001_0DB8 << 96, 32),            // 2001:db8::/32
        ((0xFD00_0EC2 << 96) | 0x254, 128), // fd00:ec2::254/128
    ];
    RANGES.iter().any(|&(net, bits)| n >> (128 - bits) == net >> (128 - bits))
}

/// The URL rules that need no network: `https` (else the API's
/// `api_usage_error`), then `image_url_refused` for over 2,048 characters,
/// user information, a port other than 443, no host or a literal address in
/// a blocked range. A DNS name is the server's to resolve and judge.
fn check_url(id: &str, url: &str) -> Result<()> {
    if !url.starts_with("https://") {
        return Err(refuse(format!("Image {id}: only https URLs are accepted.")));
    }
    let reason = if url.chars().count() > MAX_URL_CHARS {
        Some(format!("the URL is {} characters, over the limit of {MAX_URL_CHARS}", url.chars().count()))
    } else {
        match reqwest::Url::parse(url) {
            Err(_) => Some("the URL has no host".to_owned()),
            Ok(u) if !u.username().is_empty() || u.password().is_some() => {
                Some("the URL carries user information".to_owned())
            }
            Ok(u) if u.port().is_some_and(|p| p != 443) => Some("the URL uses a port other than 443".to_owned()),
            Ok(u) => match u.host_str().filter(|h| !h.is_empty()) {
                None => Some("the URL has no host".to_owned()),
                Some(host) => match host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
                    Ok(IpAddr::V4(ip)) if blocked_v4(ip) => Some("the URL resolves to a private address".to_owned()),
                    Ok(IpAddr::V6(ip)) if blocked_v6(ip) => Some("the URL resolves to a private address".to_owned()),
                    _ => None,
                },
            },
        }
    };
    match reason {
        Some(r) => Err(Error::local(ErrorKind::BadRequest, Some("image_url_refused"), format!("Image '{id}': {r}."))),
        None => Ok(()),
    }
}

/// The most images one held `/v1/models` entry lets a request carry: its
/// `max_images`, else its `images.max_images`, else 16.
pub(crate) fn max_images(limits: &Limits) -> usize {
    limits.count(&["max_images"]).or_else(|| limits.count(&["images", "max_images"])).map_or(MAX_IMAGES, |n| n as usize)
}

/// The API's image rules, checked before any request (their count is one of
/// the request's `422` entries, see `video::count_entries`): ids present and
/// unique; a URL by [`check_url`]; an upload reference's media type one of
/// the three; local bytes JPEG, PNG or WebP by their first bytes (a declared
/// type must match), at most 10 MB (or the held `max_image_bytes`) and at
/// most 40 megapixels by their header. A fault is a
/// [`ErrorKind::BadRequest`] error with no status, as the API would answer.
pub(crate) fn check_images(images: &[Image], limits: &Limits) -> Result<()> {
    let held = |key: &str| limits.count(&[key]).or_else(|| limits.count(&["images", key]));
    let biggest = held("max_image_bytes").map_or(MAX_IMAGE_BYTES, |n| n as usize);
    for (n, image) in images.iter().enumerate() {
        if image.id.is_empty() {
            return Err(refuse(format!("Image {n} needs an id.")));
        }
        if images[..n].iter().any(|other| other.id == image.id) {
            return Err(refuse(format!("Duplicate image id: {}", image.id)));
        }
        check_image(image, biggest)?;
    }
    Ok(())
}

fn check_image(image: &Image, biggest: usize) -> Result<()> {
    let id = &image.id;
    match &image.source {
        Source::Url(url) => check_url(id, url)?,
        Source::Upload { media_type, .. } => {
            if media_type.as_deref().is_some_and(|t| !MEDIA_TYPES.contains(&t)) {
                return Err(refuse(format!("Image {id}: media_type must be image/jpeg, image/png or image/webp.")));
            }
        }
        Source::Local(local) => {
            let Some(found) = local.sniffed else {
                return Err(refuse(format!("Image {id} is not a JPEG, PNG or WebP image.")));
            };
            if let Some(declared) = &local.declared {
                if declared != found {
                    return Err(refuse(format!("Image {id} could not be decoded as {declared}.")));
                }
            }
            if local.data.len() > biggest {
                return Err(refuse(format!("Image {id} is larger than 10 MB.")));
            }
            let Some((w, h)) = dimensions(&local.data, found) else {
                return Err(refuse(format!("Image {id}: its header cannot be read.")));
            };
            if u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
                return Err(refuse(format!("Image {id} is {w}x{h} pixels, over 40 megapixels.")));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Headers built byte for byte, each with the dimensions in its name.
    const PNG_2X2: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xFD, 0xD4, 0x9A, 0x73,
    ];
    const PNG_8000X6000: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x1F, 0x40, 0x00, 0x00, 0x17, 0x70, 0x08, 0x02, 0x00, 0x00, 0x00, 0xBE, 0x06, 0x73,
    ];
    // SOI, APP0 (JFIF), a DQT, then SOF0 for 300 wide by 200 high.
    const JPEG_300X200: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01,
        0x00, 0x00, 0xFF, 0xDB, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0xC8, 0x01, 0x2C, 0x03,
        0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01,
    ];
    // A progressive frame (SOF2) after a fill byte, 4000 by 3000.
    const JPEG_PROGRESSIVE_4000X3000: &[u8] =
        &[0xFF, 0xD8, 0xFF, 0xFF, 0xC2, 0x00, 0x11, 0x08, 0x0B, 0xB8, 0x0F, 0xA0, 0x03, 0x01, 0x22, 0x00];
    // A Huffman table (C4) is not a frame header: the reader skips it.
    const JPEG_DHT_THEN_SOF1_10X20: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xC4, 0x00, 0x03, 0x00, 0xFF, 0xC1, 0x00, 0x0B, 0x08, 0x00, 0x14, 0x00, 0x0A, 0x01, 0x01,
        0x11, 0x00,
    ];
    const JPEG_SCAN_BEFORE_FRAME: &[u8] = &[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02];
    // A 2 by 2 lossy WebP.
    const WEBP_VP8_2X2: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x3A, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x20, 0x2E, 0x00,
        0x00, 0x00, 0xB0, 0x01, 0x00, 0x9D, 0x01, 0x2A, 0x02, 0x00, 0x02, 0x00, 0x01, 0x40,
    ];
    // Lossless: width - 1 = 99 and height - 1 = 49 packed in 14 bits each.
    const WEBP_VP8L_100X50: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x00, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x4C, 0x00, 0x00,
        0x00, 0x00, 0x2F, 0x63, 0x40, 0x0C, 0x00,
    ];
    // Extended: canvas width - 1 = 9999 and height - 1 = 4999, 24 bits each.
    const WEBP_VP8X_10000X5000: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x00, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x58, 0x0A, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0F, 0x27, 0x00, 0x87, 0x13, 0x00,
    ];

    #[test]
    fn sniffs_the_three_formats() {
        assert_eq!(sniff_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_media_type(PNG_2X2), Some("image/png"));
        assert_eq!(sniff_media_type(WEBP_VP8_2X2), Some("image/webp"));
        assert_eq!(sniff_media_type(b"GIF89a"), None);
        assert_eq!(sniff_media_type(b""), None);
    }

    #[test]
    fn png_header() {
        assert_eq!(dimensions(PNG_2X2, "image/png"), Some((2, 2)));
        assert_eq!(dimensions(PNG_8000X6000, "image/png"), Some((8000, 6000)));
        assert_eq!(dimensions(&PNG_2X2[..20], "image/png"), None);
        let mut no_ihdr = PNG_2X2.to_vec();
        no_ihdr[12] = b'X';
        assert_eq!(dimensions(&no_ihdr, "image/png"), None);
    }

    #[test]
    fn jpeg_start_of_frame() {
        assert_eq!(dimensions(JPEG_300X200, "image/jpeg"), Some((300, 200)));
        assert_eq!(dimensions(JPEG_PROGRESSIVE_4000X3000, "image/jpeg"), Some((4000, 3000)));
        assert_eq!(dimensions(JPEG_DHT_THEN_SOF1_10X20, "image/jpeg"), Some((10, 20)));
        assert_eq!(dimensions(JPEG_SCAN_BEFORE_FRAME, "image/jpeg"), None);
        assert_eq!(dimensions(&JPEG_300X200[..30], "image/jpeg"), None);
        assert_eq!(dimensions(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x00], "image/jpeg"), None);
    }

    #[test]
    fn webp_chunks() {
        assert_eq!(dimensions(WEBP_VP8_2X2, "image/webp"), Some((2, 2)));
        assert_eq!(dimensions(WEBP_VP8L_100X50, "image/webp"), Some((100, 50)));
        assert_eq!(dimensions(WEBP_VP8X_10000X5000, "image/webp"), Some((10000, 5000)));
        assert_eq!(dimensions(&WEBP_VP8_2X2[..27], "image/webp"), None);
        let mut bad_start = WEBP_VP8_2X2.to_vec();
        bad_start[23] = 0;
        assert_eq!(dimensions(&bad_start, "image/webp"), None);
        let mut bad_signature = WEBP_VP8L_100X50.to_vec();
        bad_signature[20] = 0;
        assert_eq!(dimensions(&bad_signature, "image/webp"), None);
    }

    fn check_images_default(images: &[Image]) -> Result<()> {
        check_images(images, &Limits::default())
    }

    fn refused(images: &[Image]) -> Error {
        let e = check_images(images, &Limits::default()).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::BadRequest);
        assert_eq!(e.status(), None);
        assert_eq!(e.request_id(), None);
        e
    }

    #[test]
    fn local_checks() {
        assert!(check_images_default(&[Image::bytes("a", PNG_2X2)]).is_ok());
        assert!(check_images_default(&[Image::bytes_with_type("a", JPEG_300X200, "image/jpeg")]).is_ok());
        refused(&[Image::bytes("g", &b"GIF89a...."[..])]);
        refused(&[Image::bytes_with_type("p", JPEG_300X200, "image/png")]);
        refused(&[Image::bytes("big", PNG_8000X6000)]);
        refused(&[Image::bytes("w", WEBP_VP8X_10000X5000)]);
        assert!(check_images_default(&[Image::bytes("w", WEBP_VP8L_100X50)]).is_ok());
        let mut padded = PNG_2X2.to_vec();
        padded.resize(MAX_IMAGE_BYTES + 1, 0);
        refused(&[Image::bytes("over", padded)]);
        let many: Vec<Image> = (0..17).map(|i| Image::url(format!("u{i}"), "https://example.com/x.png")).collect();
        assert!(check_images_default(&many).is_ok(), "the count is a 422 entry of the request, not checked here");
        assert_eq!(Image::bytes("p", PNG_2X2).input(), json!({"id": "p", "media_type": "image/png"}));
        assert_eq!(Image::bytes("g", &b"GIF89a"[..]).input(), json!({"id": "g"}));
        assert_eq!(refused(&[Image::url("u", "http://example.com/x.png")]).error_type(), Some("api_usage_error"));
        for bad in [
            format!("https://example.com/{}", "a".repeat(2040)),
            "https://user:pw@example.com/x.png".to_owned(),
            "https://example.com:8443/x.png".to_owned(),
            "https://127.0.0.1/x.png".to_owned(),
            "https://169.254.169.254/latest".to_owned(),
            "https://[::1]/x.png".to_owned(),
            "https://[::ffff:10.0.0.1]/x.png".to_owned(),
            "https://[64:ff9b::a00:1]/x.png".to_owned(),
            "https://[fd00:ec2::254]/x.png".to_owned(),
        ] {
            assert_eq!(refused(&[Image::url("u", bad.clone())]).error_type(), Some("image_url_refused"), "{bad}");
        }
        for good in ["https://example.com:443/x.png", "https://8.8.8.8/x.png", "https://[2606:4700::1]/x.png"] {
            assert!(check_images_default(&[Image::url("u", good)]).is_ok(), "{good}");
        }
        assert_eq!(refused(&[Image::url("", "https://example.com/x.png")]).message(), "Image 0 needs an id.");
        refused(&[Image::upload_id_with_type("u", "upl_0", "image/gif")]);
        assert!(check_images_default(&[Image::url("u", format!("https://e.co/{}", "a".repeat(2035)))]).is_ok());
        refused(&[Image::bytes("a", PNG_2X2), Image::url("a", "https://example.com/x.png")]);
        assert!(check_images_default(&[Image::upload_id("u", "upl_0")]).is_ok());
    }

    #[test]
    fn wire_forms() {
        let v = Image::bytes_with_type("a", vec![1u8, 2, 3], "image/png").wire(true);
        assert_eq!(v, json!({"id": "a", "media_type": "image/png", "data": "AQID"}));
        let v = Image::url("b", "https://example.com/r.jpg").wire(true);
        assert_eq!(v, json!({"id": "b", "url": "https://example.com/r.jpg"}));
        let v = Image::upload_id_with_type("c", "upl_1", "image/jpeg").wire(false);
        assert_eq!(v, json!({"upload_id": "upl_1", "media_type": "image/jpeg"}));
        assert_eq!(Image::bytes("d", PNG_2X2).wire_upload("upl_2", false), json!({"upload_id": "upl_2"}));
    }

    #[test]
    fn base64_text() {
        let a = Image::base64("a", "AQID").unwrap();
        assert_eq!(a.len(), Some(3));
        assert_eq!(Image::base64("b", "AQI").unwrap().len(), Some(2));
        assert!(Image::base64("c", "not base64!").unwrap_err().is_argument());
    }

    #[test]
    fn files_are_read_when_the_image_is_made() {
        let missing = Image::path("x", "/nonexistent/file.png").unwrap_err();
        assert!(missing.is_argument());

        let dir = std::env::temp_dir().join(format!("decisionnode-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, PNG_2X2).unwrap();
        let image = Image::path("p", &png).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        // Already read: removing the file does not change the image.
        assert_eq!(image.media_type(), Some("image/png"));
        assert_eq!(image.wire(true)["media_type"], "image/png");
    }

    #[test]
    fn the_hash_is_computed_once_and_shared_by_clones() {
        let a = Image::bytes("a", PNG_2X2);
        let b = a.clone();
        let ha = a.as_local().unwrap().sha256() as *const _;
        let hb = b.as_local().unwrap().sha256() as *const _;
        assert_eq!(ha, hb);
    }
}
