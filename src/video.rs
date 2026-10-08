//! Videos sent beside the state as the frames the caller sampled, and the
//! local checks every video passes before a request leaves the client.

use std::fmt::Write as _;

use base64::Engine;
use bytes::Bytes;
use serde_json::{json, Map, Number, Value};

use crate::check::{entry, nesting_depth, Limits};
use crate::error::{Error, ErrorKind, Result};
use crate::image::{sniff_media_type, Image};

/// The most videos one request (or one batch line) may carry.
pub(crate) const MAX_VIDEOS: usize = 4;
/// The highest rate, in frames a second, a video may ask to be read at.
pub(crate) const MAX_FPS: f64 = 4.0;
/// The media types a frame may have.
const MEDIA_TYPES: [&str; 3] = ["image/jpeg", "image/png", "image/webp"];

/// A video sent beside the state as the frames the caller sampled, each with
/// its time in seconds.
///
/// Every question sees every video, the images and the state. The server
/// reads the frames as one video, in order and with their times, not as
/// separate images. Only frames are sent: no video file, no video URL and no
/// sound.
///
/// A video in its JSON wire form converts too (`Video::from(json!({...}))`)
/// and is checked by the same rules. Frames always travel inline as base64:
/// they are never uploaded, never cached and never resized or re-encoded.
///
/// ```no_run
/// use decisionnode::{Video, VideoFrame};
///
/// let mut dock = Video::new("dock").fps(2.0);
/// for (i, path) in ["dock-0000.jpg", "dock-0001.jpg"].iter().enumerate() {
///     dock = dock.frame(VideoFrame::bytes(i as f64 * 0.5, std::fs::read(path)?));
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Video {
    source: VideoSource,
}

#[derive(Clone, Debug, PartialEq)]
enum VideoSource {
    Typed { id: String, frames: Vec<VideoFrame>, fps: Option<f64>, start: Option<f64>, end: Option<f64> },
    Wire(Value),
}

/// One frame of a [`Video`]: an image (JPEG, PNG or WebP) and its time in
/// seconds. A frame in its JSON wire form converts too
/// (`VideoFrame::from(json!({"t": 0.5, "media_type": "image/jpeg", "data": "..."}))`).
#[derive(Clone, PartialEq)]
pub struct VideoFrame {
    source: FrameSource,
}

#[derive(Clone, PartialEq)]
enum FrameSource {
    Bytes { t: f64, data: Bytes, media_type: Option<String> },
    Text { t: f64, text: String, media_type: String },
    Wire(Value),
}

impl std::fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("VideoFrame");
        match &self.source {
            FrameSource::Bytes { t, data, .. } => d.field("t", t).field("bytes", &data.len()),
            FrameSource::Text { t, text, .. } => d.field("t", t).field("base64_chars", &text.len()),
            FrameSource::Wire(v) => d.field("t", &v.get("t")),
        };
        d.field("media_type", &self.media_type()).finish()
    }
}

impl VideoFrame {
    /// A frame from image bytes taken `t` seconds into the video. Its media
    /// type is read from the first bytes (JPEG, PNG or WebP); any other format
    /// is refused locally when the request is checked.
    pub fn bytes(t: f64, data: impl Into<Bytes>) -> VideoFrame {
        VideoFrame { source: FrameSource::Bytes { t, data: data.into(), media_type: None } }
    }

    /// A frame from image bytes with its media type: `image/jpeg`,
    /// `image/png` or `image/webp`.
    pub fn bytes_with_type(t: f64, data: impl Into<Bytes>, media_type: impl Into<String>) -> VideoFrame {
        VideoFrame { source: FrameSource::Bytes { t, data: data.into(), media_type: Some(media_type.into()) } }
    }

    /// A frame from image bytes given as base64 text, sent exactly as given,
    /// with its media type: `image/jpeg`, `image/png` or `image/webp`.
    pub fn base64(t: f64, text: impl Into<String>, media_type: impl Into<String>) -> VideoFrame {
        VideoFrame { source: FrameSource::Text { t, text: text.into(), media_type: media_type.into() } }
    }

    /// The frame's time in seconds (`None` for a wire-form frame without a
    /// numeric `t`).
    pub fn t(&self) -> Option<f64> {
        match &self.source {
            FrameSource::Bytes { t, .. } | FrameSource::Text { t, .. } => Some(*t),
            FrameSource::Wire(v) => v.get("t").and_then(Value::as_f64),
        }
    }

    /// The media type: the declared one, else the one read from the frame's
    /// first bytes.
    pub fn media_type(&self) -> Option<&str> {
        match &self.source {
            FrameSource::Bytes { data, media_type, .. } => media_type.as_deref().or_else(|| sniff_media_type(data)),
            FrameSource::Text { media_type, .. } => Some(media_type),
            FrameSource::Wire(v) => v.get("media_type").and_then(Value::as_str),
        }
    }

    /// The frame in its wire form, `{"t", "media_type", "data"}`.
    fn wire(&self) -> Value {
        match &self.source {
            FrameSource::Wire(v) => v.clone(),
            FrameSource::Bytes { t, data, .. } => frame_object(
                *t,
                self.media_type().unwrap_or_default(),
                base64::engine::general_purpose::STANDARD.encode(data),
            ),
            FrameSource::Text { t, text, media_type } => frame_object(*t, media_type, text.clone()),
        }
    }
}

fn frame_object(t: f64, media_type: &str, data: String) -> Value {
    let mut out = Map::new();
    out.insert("t".into(), number(t));
    out.insert("media_type".into(), Value::String(media_type.to_owned()));
    out.insert("data".into(), Value::String(data));
    Value::Object(out)
}

/// A finite number as JSON; the checks refuse any other before it is sent.
fn number(x: f64) -> Value {
    Number::from_f64(x).map_or(Value::Null, Value::Number)
}

/// A frame in its JSON wire form, checked by the same rules as a built one.
impl From<Value> for VideoFrame {
    fn from(value: Value) -> VideoFrame {
        VideoFrame { source: FrameSource::Wire(value) }
    }
}

impl Video {
    /// A video with this id and no frames yet. The id must be unique across
    /// the request's images and videos. The model sees the video as
    /// `"Video <n>"` (its place in the request's videos, from 1) with this id
    /// beside it, so text in the state or the instructions can refer to it by
    /// either.
    pub fn new(id: impl Into<String>) -> Video {
        Video { source: VideoSource::Typed { id: id.into(), frames: Vec::new(), fps: None, start: None, end: None } }
    }

    /// Adds a frame. Frame times must be at least 0 and strictly increasing.
    pub fn frame(self, frame: impl Into<VideoFrame>) -> Video {
        self.frames([frame])
    }

    /// Adds several frames, in time order.
    pub fn frames<F: Into<VideoFrame>>(mut self, frames: impl IntoIterator<Item = F>) -> Video {
        match &mut self.source {
            VideoSource::Typed { frames: list, .. } => list.extend(frames.into_iter().map(Into::into)),
            VideoSource::Wire(Value::Object(map)) => {
                let list = map.entry("frames").or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(list) = list {
                    list.extend(frames.into_iter().map(|f| f.into().wire()));
                }
            }
            VideoSource::Wire(_) => {}
        }
        self
    }

    /// The most frames a second the server reads: above 0 and at most 4.
    /// Without it the server reads up to 2 a second.
    pub fn fps(self, fps: f64) -> Video {
        self.set(Field::Fps, fps)
    }

    /// The start of the window read, on the frames' own times: a frame is
    /// read when `start <= t < end`. Without it the window starts at 0.
    pub fn start(self, seconds: f64) -> Video {
        self.set(Field::Start, seconds)
    }

    /// The end of the window read (not included); it must be after the start.
    /// Without it every frame from the start on is read.
    pub fn end(self, seconds: f64) -> Video {
        self.set(Field::End, seconds)
    }

    fn set(mut self, field: Field, x: f64) -> Video {
        match &mut self.source {
            VideoSource::Typed { fps, start, end, .. } => {
                *match field {
                    Field::Fps => fps,
                    Field::Start => start,
                    Field::End => end,
                } = Some(x);
            }
            VideoSource::Wire(Value::Object(map)) => {
                map.insert(field.name().into(), number(x));
            }
            VideoSource::Wire(_) => {}
        }
        self
    }

    /// The video's id (`None` for a wire-form video without a string id).
    pub fn id(&self) -> Option<&str> {
        match &self.source {
            VideoSource::Typed { id, .. } => Some(id),
            VideoSource::Wire(v) => v.get("id").and_then(Value::as_str),
        }
    }

    /// How deep the video's wire form nests objects and arrays.
    pub(crate) fn depth(&self) -> usize {
        match &self.source {
            // {"frames": [{...}]}
            VideoSource::Typed { .. } => 3,
            VideoSource::Wire(v) => nesting_depth(v),
        }
    }

    /// The video's wire form: `{"id", "fps", "start", "end", "frames"}`, the
    /// optional numbers only when given.
    pub(crate) fn wire(&self) -> Value {
        match &self.source {
            VideoSource::Wire(v) => v.clone(),
            VideoSource::Typed { id, frames, fps, start, end } => {
                let mut out = Map::new();
                out.insert("id".into(), Value::String(id.clone()));
                for (name, value) in [("fps", fps), ("start", start), ("end", end)] {
                    if let Some(x) = value {
                        out.insert(name.into(), number(*x));
                    }
                }
                out.insert("frames".into(), Value::Array(frames.iter().map(VideoFrame::wire).collect()));
                Value::Object(out)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Field {
    Fps,
    Start,
    End,
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Field::Fps => "fps",
            Field::Start => "start",
            Field::End => "end",
        }
    }
}

/// A video in its JSON wire form, checked by the same rules as a built one.
impl From<Value> for Video {
    fn from(value: Value) -> Video {
        Video { source: VideoSource::Wire(value) }
    }
}

fn refuse(message: String, reason: String) -> Error {
    Error::local(ErrorKind::BadRequest, Some("api_usage_error"), message).with_reason(reason)
}

fn problem(id: &str, what: String) -> Error {
    let id = py_repr(id);
    refuse(format!("Video {id}: {what}."), format!("video {id}: {what}"))
}

/// A number as the API's messages write it, as printf's `%g`: six
/// significant digits without trailing zeros (`1.5`, `4`, `1.69679e+09`).
fn decimal(x: f64) -> String {
    if x == 0.0 || !x.is_finite() {
        return format!("{x}");
    }
    // Rounded to six significant digits first, so the exponent is the
    // rounded number's: 999999.5 is 1e+06.
    let sci = format!("{x:.5e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let trim = |t: &str| -> String {
        if t.contains('.') {
            t.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            t.to_owned()
        }
    };
    if !(-4..6).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{}e{sign}{:02}", trim(mantissa), exp.abs());
    }
    trim(&format!("{x:.*}", (5 - exp) as usize))
}

/// The API's `422` `too_long` entries for more images (16, or the held limit)
/// or more videos (4, or the held `videos.max_videos`) than a request takes,
/// listed after the request's other entries, the images' first. Each entry's
/// input is the list in its wire form, a local image without its bytes.
pub(crate) fn count_entries(images: &[Image], videos: &[Video], limits: &Limits) -> Vec<Value> {
    let mut out = Vec::new();
    let most_videos = limits.count(&["videos", "max_videos"]).map_or(MAX_VIDEOS, |n| n as usize);
    for (name, n, most) in
        [("images", images.len(), crate::image::max_images(limits)), ("videos", videos.len(), most_videos)]
    {
        if n > most {
            let given: Vec<Value> = match name {
                "images" => images.iter().map(Image::input).collect(),
                _ => videos.iter().map(Video::wire).collect(),
            };
            out.push(entry(
                "too_long",
                &[json!("body"), json!(name)],
                format!("List should have at most {most} items after validation, not {n}"),
                &Value::Array(given),
                Some(json!({"field_type": "List", "max_length": most, "actual_length": n})),
            ));
        }
    }
    out
}

/// The API's video rules, checked before any request, after the images (their
/// count is one of the request's `422` entries, [`count_entries`]): each video
/// in turn: an id present and unique across the images and the videos,
/// at least one frame, frame times at least 0 and strictly increasing, a JPEG,
/// PNG or WebP media type for every frame, `fps` above 0 and at most 4,
/// `start` and `end` at least 0 and `end` after `start`. Frame counts, bytes
/// and pixels are the server's to judge. A fault is a
/// [`ErrorKind::BadRequest`] error with no status, as the API would answer; a
/// wire-form video that is not an object is an argument error. Returns the
/// videos' wire forms.
pub(crate) fn check_videos(videos: &[Video], images: &[Image]) -> Result<Vec<Value>> {
    let mut seen: Vec<String> = images.iter().map(|i| i.id().to_owned()).collect();
    let mut wires = Vec::with_capacity(videos.len());
    for (i, video) in videos.iter().enumerate() {
        let wire = video.wire();
        let Value::Object(v) = &wire else {
            return Err(Error::invalid(format!("video {i} must be a JSON object")));
        };
        let id = match v.get("id") {
            Some(Value::String(id)) if !id.is_empty() => id.as_str(),
            _ => return Err(refuse(format!("Video {i} needs an id."), format!("video {i} has no id"))),
        };
        if seen.iter().any(|s| s == id) {
            let q = py_repr(id);
            return Err(refuse(
                format!("Video id {q} is used twice (ids are unique across images and videos)."),
                format!("video id {q} is used twice"),
            ));
        }
        seen.push(id.to_owned());
        let present = |k: &str| v.get(k).is_some_and(|x| !x.is_null());
        if ["data", "url", "frames"].iter().filter(|k| present(k)).count() > 1 {
            return Err(problem(id, "needs exactly one of data, url and frames".into()));
        }
        let frames = match v.get("frames") {
            Some(Value::Array(frames)) if !frames.is_empty() => frames,
            _ => return Err(problem(id, "frames must be a non-empty list of frames".into())),
        };
        let mut before: Option<f64> = None;
        for (n, frame) in frames.iter().enumerate() {
            let Value::Object(f) = frame else {
                return Err(problem(id, format!("frame {n} must be an object with t, media_type and data")));
            };
            let t = match f.get("t").and_then(finite) {
                Some(t) if t >= 0.0 => t,
                _ => return Err(problem(id, format!("frame {n}: t must be a number of seconds, at least 0"))),
            };
            if let Some(b) = before.filter(|&b| t <= b) {
                return Err(problem(
                    id,
                    format!("frame {n}: t must be larger than the frame before it ({})", decimal(b)),
                ));
            }
            before = Some(t);
            if !f.get("data").is_some_and(Value::is_string) {
                return Err(problem(id, format!("frame {n}: data must be base64 text")));
            }
            if !f.get("media_type").and_then(Value::as_str).is_some_and(|m| MEDIA_TYPES.contains(&m)) {
                return Err(problem(id, format!("frame {n}: media_type must be one of {}", MEDIA_TYPES.join(", "))));
            }
        }
        // A bound present but not a number is refused as one out of range.
        let bound = |k: &str| v.get(k).filter(|x| !x.is_null()).map(|x| finite(x).unwrap_or(f64::NAN));
        if bound("fps").is_some_and(|f| f.is_nan() || f <= 0.0 || f > MAX_FPS) {
            return Err(problem(id, format!("fps must be a number above 0 and at most {}", decimal(MAX_FPS))));
        }
        for name in ["start", "end"] {
            if bound(name).is_some_and(|x| x.is_nan() || x < 0.0) {
                return Err(problem(id, format!("{name} must be a number of seconds, at least 0")));
            }
        }
        if bound("end").is_some_and(|end| end <= bound("start").unwrap_or(0.0)) {
            return Err(problem(id, "end must be after start".into()));
        }
        wires.push(wire);
    }
    Ok(wires)
}

/// A JSON number (never a boolean or a string) that is finite.
fn finite(v: &Value) -> Option<f64> {
    v.as_f64().filter(|x| x.is_finite())
}

/// An id quoted as the API's messages quote it: in single quotes, or in
/// double quotes when it holds a single quote and no double quote, with
/// backslashes, the quote and characters that do not print escaped.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || (0x7f..0xa0).contains(&(c as u32)) => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];

    fn refused(videos: &[Video], images: &[Image]) -> String {
        let e = check_videos(videos, images).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::BadRequest);
        assert_eq!(e.error_type(), Some("api_usage_error"));
        assert_eq!(e.status(), None);
        e.message().to_owned()
    }

    fn dock() -> Video {
        Video::new("dock").frame(VideoFrame::bytes(0.0, JPEG))
    }

    #[test]
    fn wire_form() {
        let v = Video::new("dock")
            .fps(2.0)
            .start(30.0)
            .frame(VideoFrame::bytes(30.0, vec![0xFF, 0xD8, 0xFF]))
            .frame(VideoFrame::base64(30.5, "AAEC", "image/webp"));
        let wires = check_videos(&[v], &[]).unwrap();
        assert_eq!(
            wires[0],
            json!({"id": "dock", "fps": 2.0, "start": 30.0, "frames": [
                {"t": 30.0, "media_type": "image/jpeg", "data": "/9j/"},
                {"t": 30.5, "media_type": "image/webp", "data": "AAEC"},
            ]})
        );
        let raw = json!({"id": "yard", "frames": [{"t": 1, "media_type": "image/png", "data": "AA=="}]});
        assert_eq!(check_videos(&[Video::from(raw.clone())], &[]).unwrap()[0], raw);
        let built =
            Video::from(json!({"id": "yard"})).fps(1.0).frame(json!({"t": 0, "media_type": "image/png", "data": ""}));
        assert_eq!(
            built.wire(),
            json!({"id": "yard", "fps": 1.0, "frames": [{"t": 0, "media_type": "image/png", "data": ""}]})
        );
    }

    #[test]
    fn local_checks() {
        let five: Vec<Video> =
            ["a", "b", "c", "d", "e"].iter().map(|id| Video::new(*id).frame(VideoFrame::bytes(0.0, JPEG))).collect();
        // Five videos are a 422 entry of the request (count_entries), not a 400 here.
        assert!(check_videos(&five, &[]).is_ok());
        let counts = count_entries(&[], &five, &Limits::default());
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0]["loc"], json!(["body", "videos"]));
        assert_eq!(counts[0]["msg"], "List should have at most 4 items after validation, not 5");
        assert_eq!(counts[0]["ctx"], json!({"field_type": "List", "max_length": 4, "actual_length": 5}));
        assert_eq!(counts[0]["input"][4]["id"], "e");
        assert!(count_entries(&[], &five[..4], &Limits::default()).is_empty());
        assert_eq!(refused(&[Video::new("").frame(VideoFrame::bytes(0.0, JPEG))], &[]), "Video 0 needs an id.");
        let twice = "Video id 'dock' is used twice (ids are unique across images and videos).";
        assert_eq!(refused(&[dock(), dock()], &[]), twice);
        assert_eq!(refused(&[dock()], &[Image::bytes("dock", JPEG)]), twice);
        assert_eq!(refused(&[Video::new("dock")], &[]), "Video 'dock': frames must be a non-empty list of frames.");
        assert_eq!(refused(&[Video::new("it's")], &[]), "Video \"it's\": frames must be a non-empty list of frames.");
        assert_eq!(
            refused(&[Video::new("dock").frame(VideoFrame::bytes(-0.5, JPEG))], &[]),
            "Video 'dock': frame 0: t must be a number of seconds, at least 0."
        );
        assert_eq!(
            refused(&[Video::new("dock").frame(VideoFrame::bytes(f64::NAN, JPEG))], &[]),
            "Video 'dock': frame 0: t must be a number of seconds, at least 0."
        );
        let back = Video::new("dock").frames([0.5, 1.5, 1.5].map(|t| VideoFrame::bytes(t, JPEG)));
        assert_eq!(refused(&[back], &[]), "Video 'dock': frame 2: t must be larger than the frame before it (1.5).");
        let types = "Video 'dock': frame 0: media_type must be one of image/jpeg, image/png, image/webp.";
        assert_eq!(refused(&[Video::new("dock").frame(VideoFrame::bytes(0.0, &b"GIF89a"[..]))], &[]), types);
        assert_eq!(refused(&[Video::new("dock").frame(VideoFrame::base64(0.0, "AAAA", "image/gif"))], &[]), types);
        let fps = "Video 'dock': fps must be a number above 0 and at most 4.";
        assert_eq!(refused(&[dock().fps(4.5)], &[]), fps);
        assert_eq!(refused(&[dock().fps(0.0)], &[]), fps);
        assert!(check_videos(&[dock().fps(4.0)], &[]).is_ok());
        assert_eq!(refused(&[dock().start(-1.0)], &[]), "Video 'dock': start must be a number of seconds, at least 0.");
        assert_eq!(refused(&[dock().end(0.0)], &[]), "Video 'dock': end must be after start.");
        assert_eq!(refused(&[dock().start(3.0).end(2.0)], &[]), "Video 'dock': end must be after start.");
        // Wire forms, by the same rules and the server's own messages.
        let wire = |v: Value| refused(&[Video::from(v)], &[]);
        assert_eq!(
            wire(json!({"id": "cam", "url": "https://example.com/a.mp4"})),
            "Video 'cam': frames must be a non-empty list of frames."
        );
        assert_eq!(
            wire(json!({"id": "cam", "url": "https://example.com/a.mp4", "frames": []})),
            "Video 'cam': needs exactly one of data, url and frames."
        );
        assert_eq!(
            wire(json!({"id": "cam", "frames": [1]})),
            "Video 'cam': frame 0 must be an object with t, media_type and data."
        );
        assert_eq!(
            wire(json!({"id": "cam", "frames": [{"t": true, "media_type": "image/png", "data": ""}]})),
            "Video 'cam': frame 0: t must be a number of seconds, at least 0."
        );
        assert_eq!(
            wire(json!({"id": "cam", "frames": [{"t": 0, "media_type": "image/png", "data": 1}]})),
            "Video 'cam': frame 0: data must be base64 text."
        );
        assert_eq!(
            wire(json!({"id": "cam", "fps": "2", "frames": [{"t": 0, "media_type": "image/png", "data": ""}]})),
            "Video 'cam': fps must be a number above 0 and at most 4."
        );
        assert!(check_videos(&[Video::from(json!("dock.mp4"))], &[]).unwrap_err().is_argument());
        // Not checked locally: the frame count and the bytes against their type.
        let many = Video::new("dock")
            .frames((0..65).map(|i| VideoFrame::bytes_with_type(f64::from(i) / 2.0, &b"x"[..], "image/png")));
        assert!(check_videos(&[many], &[]).is_ok());
    }

    #[test]
    fn numbers_and_quotes() {
        assert_eq!(decimal(1.5), "1.5");
        assert_eq!(decimal(4.0), "4");
        assert_eq!(decimal(1_696_789_012.5), "1.69679e+09");
        for (x, g) in [(0.333_333_333, "0.333333"), (1e6, "1e+06"), (1_234_567.5, "1.23457e+06"), (999_999.5, "1e+06")]
        {
            assert_eq!(decimal(x), g);
        }
        for (x, g) in
            [(0.0001, "0.0001"), (0.00001, "1e-05"), (30.123_456, "30.1235"), (100_000.0, "100000"), (0.0, "0")]
        {
            assert_eq!(decimal(x), g);
        }
        assert_eq!(py_repr("dock"), "'dock'");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("a\\b\n"), "'a\\\\b\\n'");
    }
}
