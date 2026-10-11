//! Boosters: specialist models that read a request's images before the
//! decision. Their readings are added to what the model sees as evidence and
//! come back in [`crate::Decision::evidence`].
//!
//! ```
//! use decisionnode::{truth, Booster, DecideRequest, Image};
//!
//! let request = DecideRequest::new()
//!     .image(Image::url("a1b2", "https://files.example.com/a1b2.jpg"))
//!     .image(Image::url("c3d4", "https://files.example.com/c3d4.jpg"))
//!     .boosters(["age", "ai_generated"])
//!     .question("adult", truth("Is the person in the first picture an adult?"))
//!     .question("synthetic", truth("Is the second picture AI-generated?"));
//!
//! // A booster with a setting: `ai_generated` reads only the second picture,
//! // and the request fails if it cannot answer.
//! let required = DecideRequest::new()
//!     .image(Image::url("c3d4", "https://files.example.com/c3d4.jpg"))
//!     .booster(Booster::new("ai_generated").image("c3d4").require(true))
//!     .question("synthetic", truth("Is the picture AI-generated?"));
//! ```

use indexmap::IndexMap;
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::{header, Error, Result};

// --------------------------------------------------------------------------- request

/// A booster the request names, in one of its two wire forms: a plain name
/// (`"age"`), or an object with the images it reads, whether it is required,
/// and its options.
///
/// Names are strings, not a closed list, so a booster added on the server
/// works with this version of the SDK. Nothing about boosters is checked
/// locally: an unknown name, a booster turned off for the workspace or a
/// malformed list is the server's answer, before any booster runs and before
/// any charge.
///
/// ```
/// use decisionnode::Booster;
///
/// let by_name = Booster::from("age"); // sent as "age"
/// let required = Booster::new("ai_generated").images(["c3d4"]).require(true);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Booster {
    form: Form,
}

#[derive(Clone, Debug, PartialEq)]
enum Form {
    /// A plain name, sent as a string.
    Name(String),
    /// The object form, each field left out when not given.
    Object {
        name: String,
        images: Option<Vec<String>>,
        require: Option<bool>,
        options: Option<std::result::Result<Value, String>>,
    },
    /// A wire form given as JSON, sent as it is.
    Wire(Value),
}

impl Booster {
    /// A booster by its name, to which the images, `require` and options may
    /// be added. Without any of them it is sent as the plain name, `"age"`;
    /// with one it is sent as the object `{"name": name, ...}`.
    pub fn new(name: impl Into<String>) -> Booster {
        Booster { form: Form::Object { name: name.into(), images: None, require: None, options: None } }
    }

    /// The object form of this booster, keeping what it already holds.
    fn object(self) -> Form {
        match self.form {
            Form::Name(name) => Form::Object { name, images: None, require: None, options: None },
            Form::Wire(Value::String(name)) => Form::Object { name, images: None, require: None, options: None },
            form => form,
        }
    }

    /// Adds an image the booster reads, by the image's id in the request.
    /// Without any, the booster reads every image of the request.
    pub fn image(self, id: impl Into<String>) -> Booster {
        self.images([id])
    }

    /// Adds images the booster reads, by their ids in the request.
    pub fn images<I, S>(self, ids: I) -> Booster
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut form = self.object();
        if let Form::Object { images, .. } = &mut form {
            images.get_or_insert_with(Vec::new).extend(ids.into_iter().map(Into::into));
        }
        Booster { form }
    }

    /// `true`: a booster that fails or does not answer in time fails the
    /// request with the `529` `booster_unavailable` (an
    /// [`crate::ErrorKind::Overloaded`] error, retried like any `529`).
    /// `false`, or not set: the decision runs without that reading, and its
    /// evidence says why.
    pub fn require(self, require: bool) -> Booster {
        let mut form = self.object();
        if let Form::Object { require: r, .. } = &mut form {
            *r = Some(require);
        }
        Booster { form }
    }

    /// The booster's settings, any value that serialises to a JSON object.
    /// The current boosters take none.
    pub fn options(self, options: impl Serialize) -> Booster {
        let value = serde_json::to_value(options)
            .map_err(|e| format!("the booster's options cannot be serialised to JSON: {e}"));
        let mut form = self.object();
        if let Form::Object { options: o, .. } = &mut form {
            *o = Some(value);
        }
        Booster { form }
    }

    /// The booster's name, when the booster was built from one (a wire form
    /// that is not a name or an object with `name` has none).
    pub fn name(&self) -> Option<&str> {
        match &self.form {
            Form::Name(name) | Form::Object { name, .. } => Some(name),
            Form::Wire(Value::String(name)) => Some(name),
            Form::Wire(v) => v.get("name").and_then(Value::as_str),
        }
    }

    /// The wire form. Options that cannot be serialised are an argument error.
    pub(crate) fn into_value(self) -> Result<Value> {
        Ok(match self.form {
            Form::Name(name) => Value::String(name),
            Form::Wire(value) => value,
            // No option set: the plain name, so the list stays a list of names.
            Form::Object { name, images: None, require: None, options: None } => Value::String(name),
            Form::Object { name, images, require, options } => {
                let mut m = Map::new();
                m.insert("name".into(), Value::String(name));
                if let Some(images) = images {
                    m.insert("images".into(), Value::Array(images.into_iter().map(Value::String).collect()));
                }
                if let Some(require) = require {
                    m.insert("require".into(), Value::Bool(require));
                }
                if let Some(options) = options {
                    m.insert("options".into(), options.map_err(Error::invalid)?);
                }
                Value::Object(m)
            }
        })
    }
}

/// A booster by its plain name, sent as the string.
impl From<&str> for Booster {
    fn from(name: &str) -> Booster {
        Booster { form: Form::Name(name.to_owned()) }
    }
}

/// A booster by its plain name, sent as the string.
impl From<String> for Booster {
    fn from(name: String) -> Booster {
        Booster { form: Form::Name(name) }
    }
}

/// A booster in its JSON wire form (a name, or an object with `booster`),
/// sent exactly as given.
impl From<Value> for Booster {
    fn from(value: Value) -> Booster {
        Booster { form: Form::Wire(value) }
    }
}

/// The `boosters` list of a body, in the caller's order; `None` when there
/// are none, so the body has no `boosters` key.
pub(crate) fn boosters_value(boosters: Vec<Booster>) -> Result<Option<Value>> {
    if boosters.is_empty() {
        return Ok(None);
    }
    Ok(Some(Value::Array(boosters.into_iter().map(Booster::into_value).collect::<Result<_>>()?)))
}

// --------------------------------------------------------------------------- response

/// What one booster read: an entry of [`crate::Decision::evidence`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Evidence {
    /// The version of the booster that read, for example `age-v3`.
    pub version: Option<String>,
    /// `ok`; `timeout` (no answer within its time budget); `error`; `skipped`
    /// (none of the request's images is one it reads). Any other value is kept
    /// as received.
    pub status: Option<String>,
    /// How long the booster took, in milliseconds. `None` when absent.
    pub latency_ms: Option<f64>,
    /// The calls of this booster billed on this request: one per image it
    /// read with status `ok`, 0 for a booster that failed or timed out and
    /// for a refused request.
    /// `None` when the server does not send it.
    pub billed_calls: Option<u64>,
    /// One entry per picture. A picture the booster did not read (it timed
    /// out or failed, or could not decode the picture) is a
    /// [`Reading::Unknown`] holding only its image, `status` saying why.
    pub results: Vec<Reading>,
    /// The evidence entry as received.
    pub raw: Value,
}

impl Evidence {
    /// Reads one entry of `evidence`, its readings by the booster's name.
    pub(crate) fn parse(booster: &str, raw: Value) -> Evidence {
        let obj = raw.as_object();
        let field = |k: &str| obj.and_then(|o| o.get(k));
        Evidence {
            version: field("version").and_then(Value::as_str).map(str::to_owned),
            status: field("status").and_then(Value::as_str).map(str::to_owned),
            latency_ms: field("latency_ms").and_then(Value::as_f64),
            billed_calls: field("billed_calls").and_then(Value::as_u64),
            results: field("results")
                .and_then(Value::as_array)
                .map(|list| list.iter().map(|r| Reading::parse(booster, r.clone())).collect())
                .unwrap_or_default(),
            raw,
        }
    }
}

/// Reads a body's `evidence`; `None` when it has none.
pub(crate) fn evidence_from_body(evidence: Option<&Value>) -> Option<IndexMap<String, Evidence>> {
    evidence
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(name, e)| (name.clone(), Evidence::parse(name, e.clone()))).collect())
}

/// One booster reading of one image.
///
/// A reading of a booster this version of the SDK does not know, or a reading
/// with a field missing or of the wrong type, is a [`Reading::Unknown`] with
/// its raw fields, so the decision never fails on one.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Reading {
    /// A reading of the `age` booster.
    Age(AgeReading),
    /// A reading of the `ai_generated` booster.
    AiGenerated(AiGeneratedReading),
    /// A reading of the `ocr` booster.
    Ocr(OcrReading),
    /// A reading of the `face_detection` booster.
    FaceDetection(FaceDetectionReading),
    /// A reading of the `wanted_person` booster.
    WantedPerson(WantedPersonReading),
    /// A reading of a booster this version does not know, or a malformed
    /// reading.
    Unknown(UnknownReading),
}

/// A reading of the `age` booster: how old the person in a picture is (the
/// largest face is read). With `faces` 0 the picture shows no face and the
/// age fields are `None`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct AgeReading {
    /// The id of the image read.
    pub image: String,
    /// How many faces were found.
    pub faces: u64,
    /// The age in years.
    pub age_years: Option<f64>,
    /// The low end of the range, in years.
    pub low: Option<f64>,
    /// The high end of the range, in years.
    pub high: Option<f64>,
    /// How sure the reading is, 0 to 1.
    pub confidence: Option<f64>,
    /// The reading as received.
    pub raw: Value,
}

/// A reading of the `ai_generated` booster: how likely a picture was made by
/// an image generator.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct AiGeneratedReading {
    /// The id of the image read.
    pub image: String,
    /// The probability that the picture was made by a generator, 0 to 1.
    pub generated: f64,
    /// How sure the reading is, 0 to 1.
    pub confidence: f64,
    /// `inference` (read from the pixels) or `c2pa` (the file's Content
    /// Credentials say so); any other value is kept as received.
    pub method: String,
    /// The likeliest generator. `None` unless the reading says the picture
    /// is likely generated, and on every `c2pa` reading.
    pub top_generator: Option<TopGenerator>,
    /// Who signed the file's Content Credentials; `None` when it has none.
    pub c2pa_signer: Option<String>,
    /// `"untrusted"` when the file's Content Credentials validate but their
    /// signer is not trusted, any other value as received, or `None` when the
    /// reading has none.
    pub c2pa: Option<String>,
    /// The reading as received.
    pub raw: Value,
}

/// A reading of the `face_detection` booster, its fields as sent.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct FaceDetectionReading {
    /// The id of the image read.
    pub image: String,
    /// The `faces` count as sent. It may be more than `detections` lists.
    pub faces: u64,
    /// The `detections`, in the order sent.
    pub detections: Vec<FaceDetection>,
    /// The reading as received.
    pub raw: Value,
}

/// One entry of a [`FaceDetectionReading`]'s detections.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct FaceDetection {
    /// Its box.
    pub bbox: FaceBox,
    /// Its `confidence`, as sent.
    pub confidence: f64,
}

/// A reading of the `wanted_person` booster, its fields as sent.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct WantedPersonReading {
    /// The id of the image read.
    pub image: String,
    /// The `faces` count as sent. It may be more than `matches` lists.
    pub faces: u64,
    /// The `matches`, in the order sent.
    pub matches: Vec<WantedMatch>,
    /// The reading as received.
    pub raw: Value,
}

/// One entry of a [`WantedPersonReading`]'s matches.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct WantedMatch {
    /// Its box.
    pub bbox: FaceBox,
    /// Its `score`, as sent.
    pub score: f64,
    /// Its `reference`, as sent.
    pub reference: String,
}

/// A face's box: its top-left and bottom-right corners, exactly as sent (in
/// pixels of the image, or fractions of its width and height when the
/// request asks for normalized units).
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct FaceBox {
    /// The top-left corner, from the left.
    pub x1: f64,
    /// The top-left corner, from the top.
    pub y1: f64,
    /// The bottom-right corner, from the left.
    pub x2: f64,
    /// The bottom-right corner, from the top.
    pub y2: f64,
}

/// A face's box from its wire form: an object with four number corners.
fn face_box(value: Option<&Value>) -> Option<FaceBox> {
    let b = value?.as_object()?;
    let n = |k: &str| b.get(k).and_then(Value::as_f64);
    Some(FaceBox { x1: n("x1")?, y1: n("y1")?, x2: n("x2")?, y2: n("y2")? })
}

/// A reading of the `ocr` booster, its fields as sent.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct OcrReading {
    /// The id of the image read.
    pub image: String,
    /// The `text`, exactly as sent, `""` included.
    pub text: String,
    /// The `truncated` flag, as sent.
    pub truncated: bool,
    /// The reading as received.
    pub raw: Value,
}

/// The likeliest generator of an [`AiGeneratedReading`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct TopGenerator {
    /// The generator's name.
    pub name: String,
    /// Its probability, 0 to 1.
    pub probability: f64,
}

/// A reading of a booster this version of the SDK does not know, or a reading
/// with a field missing or of the wrong type.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct UnknownReading {
    /// The name of the booster that read.
    pub booster: String,
    /// The reading as received.
    pub raw: Value,
}

/// A number field: absent or `null` is `Ok(None)`, anything but a number is
/// malformed.
fn optional_number(obj: &Map<String, Value>, key: &str) -> Option<Option<f64>> {
    match obj.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(v) => v.as_f64().map(Some),
    }
}

impl Reading {
    /// Reads one reading of the named booster.
    pub(crate) fn parse(booster: &str, raw: Value) -> Reading {
        let known = raw.as_object().and_then(|obj| match booster {
            "age" => Self::parse_age(obj, &raw),
            "ai_generated" => Self::parse_ai_generated(obj, &raw),
            "ocr" => Self::parse_ocr(obj, &raw),
            "face_detection" => Self::parse_face_detection(obj, &raw),
            "wanted_person" => Self::parse_wanted_person(obj, &raw),
            _ => None,
        });
        known.unwrap_or_else(|| Reading::Unknown(UnknownReading { booster: booster.to_owned(), raw }))
    }

    fn parse_face_detection(obj: &Map<String, Value>, raw: &Value) -> Option<Reading> {
        let detections = obj
            .get("detections")?
            .as_array()?
            .iter()
            .map(|d| {
                let d = d.as_object()?;
                Some(FaceDetection { bbox: face_box(d.get("box"))?, confidence: d.get("confidence")?.as_f64()? })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Reading::FaceDetection(FaceDetectionReading {
            image: obj.get("image")?.as_str()?.to_owned(),
            faces: obj.get("faces")?.as_u64()?,
            detections,
            raw: raw.clone(),
        }))
    }

    fn parse_wanted_person(obj: &Map<String, Value>, raw: &Value) -> Option<Reading> {
        let matches = obj
            .get("matches")?
            .as_array()?
            .iter()
            .map(|m| {
                let m = m.as_object()?;
                Some(WantedMatch {
                    bbox: face_box(m.get("box"))?,
                    score: m.get("score")?.as_f64()?,
                    reference: m.get("reference")?.as_str()?.to_owned(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Reading::WantedPerson(WantedPersonReading {
            image: obj.get("image")?.as_str()?.to_owned(),
            faces: obj.get("faces")?.as_u64()?,
            matches,
            raw: raw.clone(),
        }))
    }

    fn parse_ocr(obj: &Map<String, Value>, raw: &Value) -> Option<Reading> {
        // `text` and `truncated` are always sent.
        Some(Reading::Ocr(OcrReading {
            image: obj.get("image")?.as_str()?.to_owned(),
            text: obj.get("text")?.as_str()?.to_owned(),
            truncated: obj.get("truncated")?.as_bool()?,
            raw: raw.clone(),
        }))
    }

    fn parse_age(obj: &Map<String, Value>, raw: &Value) -> Option<Reading> {
        let image = obj.get("image")?.as_str()?.to_owned();
        let faces = obj.get("faces")?.as_u64()?;
        let age_years = optional_number(obj, "age_years")?;
        let low = optional_number(obj, "low")?;
        let high = optional_number(obj, "high")?;
        let confidence = optional_number(obj, "confidence")?;
        if faces > 0 && [age_years, low, high, confidence].iter().any(Option::is_none) {
            return None;
        }
        Some(Reading::Age(AgeReading { image, faces, age_years, low, high, confidence, raw: raw.clone() }))
    }

    fn parse_ai_generated(obj: &Map<String, Value>, raw: &Value) -> Option<Reading> {
        let n = |k: &str| obj.get(k).and_then(Value::as_f64);
        // The `top_generator` and `c2pa_signer` keys are always sent, `null`
        // when unset; `c2pa` only when the file carries Content Credentials.
        let top_generator = match obj.get("top_generator")? {
            Value::Null => None,
            Value::Object(g) => Some(TopGenerator {
                name: g.get("name")?.as_str()?.to_owned(),
                probability: g.get("probability")?.as_f64()?,
            }),
            _ => return None,
        };
        let text = |v: &Value| match v {
            Value::Null => Some(None),
            v => v.as_str().map(|s| Some(s.to_owned())),
        };
        let c2pa_signer = text(obj.get("c2pa_signer")?)?;
        let c2pa = obj.get("c2pa").map_or(Some(None), text)?;
        Some(Reading::AiGenerated(AiGeneratedReading {
            image: obj.get("image")?.as_str()?.to_owned(),
            generated: n("generated")?,
            confidence: n("confidence")?,
            method: obj.get("method")?.as_str()?.to_owned(),
            top_generator,
            c2pa_signer,
            c2pa,
            raw: raw.clone(),
        }))
    }

    /// The age reading, if this is one.
    pub fn as_age(&self) -> Option<&AgeReading> {
        match self {
            Reading::Age(r) => Some(r),
            _ => None,
        }
    }

    /// The AI-generated reading, if this is one.
    pub fn as_ai_generated(&self) -> Option<&AiGeneratedReading> {
        match self {
            Reading::AiGenerated(r) => Some(r),
            _ => None,
        }
    }

    /// The face detection reading, if this is one.
    pub fn as_face_detection(&self) -> Option<&FaceDetectionReading> {
        match self {
            Reading::FaceDetection(r) => Some(r),
            _ => None,
        }
    }

    /// The wanted person reading, if this is one.
    pub fn as_wanted_person(&self) -> Option<&WantedPersonReading> {
        match self {
            Reading::WantedPerson(r) => Some(r),
            _ => None,
        }
    }

    /// The OCR reading, if this is one.
    pub fn as_ocr(&self) -> Option<&OcrReading> {
        match self {
            Reading::Ocr(r) => Some(r),
            _ => None,
        }
    }

    /// The id of the image read, when the reading names one.
    pub fn image(&self) -> Option<&str> {
        match self {
            Reading::Age(r) => Some(&r.image),
            Reading::AiGenerated(r) => Some(&r.image),
            Reading::Ocr(r) => Some(&r.image),
            Reading::FaceDetection(r) => Some(&r.image),
            Reading::WantedPerson(r) => Some(&r.image),
            Reading::Unknown(r) => r.raw.get("image").and_then(Value::as_str),
        }
    }

    /// The reading as received.
    pub fn raw(&self) -> &Value {
        match self {
            Reading::Age(r) => &r.raw,
            Reading::AiGenerated(r) => &r.raw,
            Reading::Ocr(r) => &r.raw,
            Reading::FaceDetection(r) => &r.raw,
            Reading::WantedPerson(r) => &r.raw,
            Reading::Unknown(r) => &r.raw,
        }
    }
}

// --------------------------------------------------------------------------- header

/// One entry of the `x-decisionnode-boosters` header: what a booster of the
/// request did. See [`crate::Error::boosters`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct BoosterStatus {
    /// `ok`, `timeout`, `error`, `skipped`, or any other value as received.
    pub status: String,
    /// How long the booster took, in milliseconds. `None` when the header
    /// gives no whole number.
    pub latency_ms: Option<u64>,
}

/// Parses `x-decisionnode-boosters` (`age=ok:212,ai_generated=timeout:800`)
/// into booster name to status, in the header's order. Empty when the header
/// is absent; an entry without `=` is skipped.
pub(crate) fn parse_boosters_header(headers: &HeaderMap) -> IndexMap<String, BoosterStatus> {
    let Some(value) = header(headers, "x-decisionnode-boosters") else {
        return IndexMap::new();
    };
    value
        .split(',')
        .filter_map(|entry| {
            let (name, rest) = entry.trim().split_once('=')?;
            let (status, latency) = match rest.split_once(':') {
                Some((s, ms)) => (s, ms.trim().parse::<u64>().ok()),
                None => (rest, None),
            };
            Some((name.trim().to_owned(), BoosterStatus { status: status.trim().to_owned(), latency_ms: latency }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use serde_json::json;

    #[test]
    fn wire_forms() {
        assert_eq!(Booster::from("age").into_value().unwrap(), json!("age"));
        assert_eq!(Booster::from("age".to_owned()).into_value().unwrap(), json!("age"));
        assert_eq!(Booster::new("age").into_value().unwrap(), json!("age"));
        assert_eq!(
            Booster::new("age").images(Vec::<String>::new()).into_value().unwrap(),
            json!({"name": "age", "images": []})
        );
        assert_eq!(
            Booster::new("ai_generated")
                .images(["c3d4"])
                .image("e5")
                .require(true)
                .options(json!({}))
                .into_value()
                .unwrap(),
            json!({"name": "ai_generated", "images": ["c3d4", "e5"], "require": true, "options": {}})
        );
        assert_eq!(Booster::from("age").require(false).into_value().unwrap(), json!({"name": "age", "require": false}));
        let wire = json!({"name": "age", "unknown": 1});
        assert_eq!(Booster::from(wire.clone()).into_value().unwrap(), wire);
        assert_eq!(Booster::from(wire).name(), Some("age"));
        assert_eq!(Booster::from(json!(3)).name(), None);
        assert_eq!(boosters_value(Vec::new()).unwrap(), None);
    }

    #[test]
    fn options_that_cannot_be_serialised_are_an_argument_error() {
        let mut bad = std::collections::HashMap::new();
        bad.insert(vec![1u8], 1);
        let e = Booster::new("age").options(bad).into_value().unwrap_err();
        assert!(e.is_argument());
    }

    #[test]
    fn face_readings() {
        let b = json!({"x1": 10, "y1": 20.5, "x2": 0.4, "y2": 0.6});
        let body = json!({
            "face_detection": {"version": "face_detection-v1", "status": "ok", "latency_ms": 40, "billed_calls": 6, "results": [
                {"image": "crowd", "faces": 3, "detections": [{"box": b, "confidence": 0.98}, {"box": b, "confidence": 0.71}], "extra": 1},
                {"image": "empty", "faces": 0, "detections": []},
                {"image": "a", "faces": -1, "detections": []},
                {"image": "b", "faces": 1.5, "detections": []},
                {"image": "c", "faces": 1},
                {"image": "d", "faces": 1, "detections": {}},
                {"image": "e", "faces": 1, "detections": [{"box": {"x1": 1, "y1": 2, "x2": 3}, "confidence": 0.5}]},
                {"image": "f", "faces": 1, "detections": [{"box": {"x1": 1, "y1": 2, "x2": 3, "y2": "4"}, "confidence": 0.5}]},
                {"image": "g", "faces": 1, "detections": [{"box": b}]},
                {"image": 7, "faces": 1, "detections": []}
            ]},
            "wanted_person": {"version": "wanted_person-v1", "status": "ok", "latency_ms": 60, "billed_calls": 1, "results": [
                {"image": "door", "faces": 2, "matches": [{"box": b, "score": 0.91, "reference": "case-17"}]},
                {"image": "x", "faces": 1, "matches": [{"box": b, "score": "0.9", "reference": "r"}]},
                {"image": "y", "faces": 1, "matches": [{"box": b, "score": 0.9}]},
                {"image": "z", "faces": 1, "matches": [{"box": b, "score": 0.9, "reference": 5}]},
                {"image": "w", "faces": 1}
            ]}
        });
        let ev = evidence_from_body(Some(&body)).unwrap();
        let fd = &ev["face_detection"].results;
        let r = fd[0].as_face_detection().unwrap();
        assert_eq!((r.image.as_str(), r.faces, r.detections.len()), ("crowd", 3, 2));
        assert_eq!(r.detections[0].bbox, FaceBox { x1: 10.0, y1: 20.5, x2: 0.4, y2: 0.6 });
        assert_eq!((r.detections[0].confidence, r.detections[1].confidence), (0.98, 0.71));
        assert_eq!(fd[1].as_face_detection().unwrap().faces, 0);
        for (i, reading) in fd.iter().enumerate().skip(2) {
            assert!(matches!(reading, Reading::Unknown(u) if u.booster == "face_detection"), "{i}");
        }
        let wp = &ev["wanted_person"].results;
        let w = wp[0].as_wanted_person().unwrap();
        assert_eq!(
            (w.image.as_str(), w.faces, w.matches[0].score, w.matches[0].reference.as_str()),
            ("door", 2, 0.91, "case-17")
        );
        assert_eq!(w.matches[0].bbox.y1, 20.5);
        for (i, reading) in wp.iter().enumerate().skip(1) {
            assert!(matches!(reading, Reading::Unknown(u) if u.booster == "wanted_person"), "{i}");
        }
        assert_eq!(wp[0].image(), Some("door"));
    }

    #[test]
    fn ocr_readings() {
        let body = json!({"ocr": {"version": "ocr-v1", "status": "ok", "latency_ms": 90, "billed_calls": 6, "results": [
            {"image": "receipt", "text": "TOTAL 42.00 EUR", "truncated": false, "region": {"x": 1}},
            {"image": "blank", "text": "", "truncated": false},
            {"image": "long", "text": "page", "truncated": true},
            {"image": "n", "text": "x", "truncated": null},
            {"image": "m", "text": "x"},
            {"image": "a", "truncated": false},
            {"image": "b", "text": 7, "truncated": false},
            {"image": 3, "text": "x", "truncated": false},
            {"image": "c", "text": "x", "truncated": "yes"}
        ]}});
        let r = &evidence_from_body(Some(&body)).unwrap()["ocr"].results;
        let o = r[0].as_ocr().unwrap();
        assert_eq!((o.image.as_str(), o.text.as_str(), o.truncated), ("receipt", "TOTAL 42.00 EUR", false));
        assert_eq!(r[1].as_ocr().unwrap().text, "");
        assert!(r[2].as_ocr().unwrap().truncated);
        // A truncated that is null, missing or not a boolean; a missing or
        // non-string text; a non-string image.
        for (i, reading) in r.iter().enumerate().skip(3) {
            assert!(matches!(reading, Reading::Unknown(u) if u.booster == "ocr"), "{i}");
        }
        assert_eq!(r[0].image(), Some("receipt"));
    }

    #[test]
    fn readings() {
        let body = json!({
            "age": {"version": "age-v3", "status": "ok", "latency_ms": 212, "billed_calls": 2, "results": [
                {"image": "a1b2", "age_years": 27, "low": 22, "high": 33, "confidence": 0.81, "faces": 1},
                {"image": "c3d4", "faces": 0},
                {"image": "e5", "faces": 1},
                {"image": 7, "faces": 0}
            ]},
            "ai_generated": {"version": "ai_generated-v2", "status": "ok", "latency_ms": 340, "results": [
                {"image": "c3d4", "generated": 0.93, "confidence": 0.86, "method": "inference",
                 "top_generator": {"name": "gen-a", "probability": 0.71}, "c2pa_signer": null},
                {"image": "e5", "generated": 0.99, "confidence": 0.99, "method": "c2pa",
                 "top_generator": null, "c2pa_signer": "Example Signer"},
                {"image": "f6", "generated": "high", "confidence": 0.5, "method": "inference"},
                {"image": "g7", "generated": 0.1, "confidence": 0.5, "method": "inference",
                 "top_generator": {"name": "gen-a", "probability": 0.1}},
                {"image": "h8", "generated": 0.1, "confidence": 0.5, "method": "inference", "top_generator": null,
                 "c2pa_signer": null, "c2pa": "untrusted", "region": {"x": 4}},
                {"image": "i9", "generated": 0.1, "confidence": 0.5, "method": "inference", "c2pa_signer": null},
                {"image": "j0", "generated": 0.1, "confidence": 0.5, "method": "inference", "top_generator": "gen-a",
                 "c2pa_signer": null},
                {"image": "k1", "generated": 0.1, "confidence": 0.5, "method": "inference", "top_generator": null,
                 "c2pa_signer": null, "c2pa": true}
            ]},
            "slow": {"version": "slow-v1", "status": "timeout", "latency_ms": 800, "billed_calls": null, "results": []},
            "colour": {"version": "colour-v1", "status": "ok", "latency_ms": 5, "results": [{"image": "a1b2", "hue": 3}]}
        });
        let ev = evidence_from_body(Some(&body)).unwrap();
        assert_eq!(ev.keys().collect::<Vec<_>>(), ["age", "ai_generated", "slow", "colour"]);
        let age = &ev["age"];
        assert_eq!(
            (age.version.as_deref(), age.status.as_deref(), age.latency_ms),
            (Some("age-v3"), Some("ok"), Some(212.0))
        );
        let a = age.results[0].as_age().unwrap();
        assert_eq!(
            (a.faces, a.age_years, a.low, a.high, a.confidence),
            (1, Some(27.0), Some(22.0), Some(33.0), Some(0.81))
        );
        let none = age.results[1].as_age().unwrap();
        assert_eq!((none.faces, none.age_years, none.confidence), (0, None, None));
        assert!(matches!(&age.results[2], Reading::Unknown(r) if r.booster == "age"));
        assert!(matches!(age.results[3], Reading::Unknown(_)));
        let g = ev["ai_generated"].results[0].as_ai_generated().unwrap();
        assert_eq!((g.generated, g.method.as_str(), g.c2pa_signer.as_deref()), (0.93, "inference", None));
        assert_eq!(g.top_generator.as_ref().unwrap().probability, 0.71);
        assert_eq!(g.c2pa, None);
        let c = ev["ai_generated"].results[1].as_ai_generated().unwrap();
        assert_eq!((c.method.as_str(), c.c2pa_signer.as_deref()), ("c2pa", Some("Example Signer")));
        assert_eq!(c.top_generator, None);
        assert!(matches!(ev["ai_generated"].results[2], Reading::Unknown(_)));
        assert_eq!(ev["ai_generated"].results[2].image(), Some("f6"));
        // A missing c2pa_signer or top_generator key is a field missing.
        assert!(matches!(ev["ai_generated"].results[3], Reading::Unknown(_)));
        // A photograph: no top generator; untrusted Content Credentials; an
        // unknown field ignored.
        let photo = ev["ai_generated"].results[4].as_ai_generated().unwrap();
        assert_eq!((photo.top_generator.as_ref(), photo.c2pa.as_deref()), (None, Some("untrusted")));
        for i in 5..8 {
            assert!(matches!(ev["ai_generated"].results[i], Reading::Unknown(_)), "{i}");
        }
        assert!(ev["slow"].results.is_empty());
        assert_eq!((age.billed_calls, ev["slow"].billed_calls, ev["colour"].billed_calls), (Some(2), None, None));
        assert_eq!(ev["slow"].status.as_deref(), Some("timeout"));
        assert!(matches!(&ev["colour"].results[0], Reading::Unknown(r) if r.booster == "colour" && r.raw["hue"] == 3));
        assert_eq!(evidence_from_body(None), None);
    }

    #[test]
    fn header() {
        let mut h = HeaderMap::new();
        assert!(parse_boosters_header(&h).is_empty());
        h.insert(
            "x-decisionnode-boosters",
            HeaderValue::from_static("age=ok:212, ai_generated=timeout:800,odd,x=error"),
        );
        let m = parse_boosters_header(&h);
        assert_eq!(m.keys().collect::<Vec<_>>(), ["age", "ai_generated", "x"]);
        assert_eq!(m["age"], BoosterStatus { status: "ok".into(), latency_ms: Some(212) });
        assert_eq!(m["ai_generated"].status, "timeout");
        assert_eq!(m["ai_generated"].latency_ms, Some(800));
        assert_eq!(m["x"].latency_ms, None);
    }
}
