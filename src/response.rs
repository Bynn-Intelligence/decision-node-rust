//! Typed results: [`Decision`], the answers, and [`ModelInfo`].
//!
//! Numbers are passed through exactly as received: no rounding and no
//! renormalising. A `probabilities` list may sum to 0.99 or 1.01, and an entry
//! may carry more than two decimals (`0.8200000000000001`).

use std::collections::BTreeMap;
use std::ops::Index;

use indexmap::IndexMap;
use reqwest::header::HeaderMap;
use serde_json::{Map, Value};

use crate::booster::{evidence_from_body, Evidence};
use crate::error::{header, Error, ErrorKind, Result};
use crate::safety::Safety;

/// The answer to one question, by its type.
///
/// An answer of a type this version of the SDK does not know, or of a known
/// type with missing or wrongly typed fields, is returned as
/// [`Answer::Unknown`] with its raw fields, so a new question type on the
/// server does not break an existing client.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Answer {
    /// The answer to a `truth` question.
    Truth(TruthAnswer),
    /// The answer to a `choice` question.
    Choice(ChoiceAnswer),
    /// The answer to a `score` question.
    Score(ScoreAnswer),
    /// The answer to a `number` question.
    Number(NumberAnswer),
    /// The answer to a `point` question.
    Point(PointAnswer),
    /// The answer to a `box` question.
    Box(BoxAnswer),
    /// An answer of a type this version does not know, or a known type with
    /// missing or wrongly typed fields.
    Unknown(UnknownAnswer),
}

/// The answer to a `truth` question.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct TruthAnswer {
    /// The probability that the statement is true, 0 to 1.
    pub truth: f64,
    /// The answer object as received.
    pub raw: Value,
}

/// The answer to a `choice` question.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ChoiceAnswer {
    /// The chosen option name, exactly as sent.
    pub choice: String,
    /// 0 means no better than a uniform guess, 1 means certain.
    pub confidence: f64,
    /// The probability of each option, by option name exactly as sent.
    pub probabilities: IndexMap<String, f64>,
    /// The answer object as received.
    pub raw: Value,
}

/// The answer to a `score` question.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ScoreAnswer {
    /// The expected level index: the sum of index times probability, from 0 to
    /// the number of levels minus 1.
    pub score: f64,
    /// How concentrated the probabilities are around the most likely level.
    pub confidence: f64,
    /// The probability of each level, by level index.
    pub probabilities: BTreeMap<usize, f64>,
    /// Each level index with the level exactly as sent in the request.
    pub legend: BTreeMap<usize, Value>,
    /// The answer object as received.
    pub raw: Value,
}

/// The answer to a `number` question.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct NumberAnswer {
    /// The most likely value of the grid, exactly as sent (a whole number or
    /// the grid value's digits).
    pub number: f64,
    /// The expected value over the whole grid, as sent.
    pub expected: f64,
    /// The probability of `number`.
    pub confidence: f64,
    /// The probability of each likely value, keyed by the value exactly as the
    /// API writes it (`"7"`, `"3.5"`, `"-2.25"`), in ascending order of value.
    /// Values at or below 0.001 are left out, so the entries may sum to less
    /// than 1.
    pub probabilities: IndexMap<String, f64>,
    /// The answer object as received.
    pub raw: Value,
}

/// The answer to a `point` question: one spot in an image. Every number is
/// exactly as sent: whole pixels, or fractions of the width and the height
/// with `units` `"normalized"`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PointAnswer {
    /// The spot, from the top-left corner of the image, `x` to the right.
    pub x: f64,
    /// The spot, `y` down from the top.
    pub y: f64,
    /// The probability that what the instructions describe is in the image
    /// at all. Under a low `present` the spot is only the best guess.
    pub present: f64,
    /// The probability that the spot lies on the thing, if it is there.
    pub confidence: f64,
    /// The id of the image looked in.
    pub image: String,
    /// The image's width in pixels.
    pub width: f64,
    /// The image's height in pixels.
    pub height: f64,
    /// The answer object as received.
    pub raw: Value,
}

/// The answer to a `box` question: a rectangle in an image, in the units of a
/// [`PointAnswer`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct BoxAnswer {
    /// The top-left corner, from the left.
    pub x1: f64,
    /// The top-left corner, from the top.
    pub y1: f64,
    /// The bottom-right corner, from the left.
    pub x2: f64,
    /// The bottom-right corner, from the top.
    pub y2: f64,
    /// The probability that what the instructions describe is in the image
    /// at all.
    pub present: f64,
    /// The probability that the box overlaps the thing's true box by an
    /// intersection over union of 0.5 or more, if it is there.
    pub confidence: f64,
    /// The id of the image looked in.
    pub image: String,
    /// The image's width in pixels.
    pub width: f64,
    /// The image's height in pixels.
    pub height: f64,
    /// The answer object as received.
    pub raw: Value,
}

/// An answer of a type this version of the SDK does not know, or of a known
/// type whose fields are missing or of the wrong type.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct UnknownAnswer {
    /// The `type` field of the answer, if any.
    pub answer_type: Option<String>,
    /// The answer object as received.
    pub raw: Value,
}

impl Answer {
    /// The type of the answer: `truth`, `choice`, `score`, `number`, `point`,
    /// `box` or, for an [`Answer::Unknown`], its `type` field.
    pub fn answer_type(&self) -> Option<&str> {
        match self {
            Answer::Truth(_) => Some("truth"),
            Answer::Choice(_) => Some("choice"),
            Answer::Score(_) => Some("score"),
            Answer::Number(_) => Some("number"),
            Answer::Point(_) => Some("point"),
            Answer::Box(_) => Some("box"),
            Answer::Unknown(o) => o.answer_type.as_deref(),
        }
    }

    /// The answer object as received.
    pub fn raw(&self) -> &Value {
        match self {
            Answer::Truth(a) => &a.raw,
            Answer::Choice(a) => &a.raw,
            Answer::Score(a) => &a.raw,
            Answer::Number(a) => &a.raw,
            Answer::Point(a) => &a.raw,
            Answer::Box(a) => &a.raw,
            Answer::Unknown(a) => &a.raw,
        }
    }

    /// The point answer, if this is one.
    pub fn as_point(&self) -> Option<&PointAnswer> {
        match self {
            Answer::Point(a) => Some(a),
            _ => None,
        }
    }

    /// The box answer, if this is one.
    pub fn as_box(&self) -> Option<&BoxAnswer> {
        match self {
            Answer::Box(a) => Some(a),
            _ => None,
        }
    }

    /// The number answer, if this is one.
    pub fn as_number(&self) -> Option<&NumberAnswer> {
        match self {
            Answer::Number(a) => Some(a),
            _ => None,
        }
    }

    /// The most likely value of a number answer.
    pub fn number(&self) -> Option<f64> {
        self.as_number().map(|a| a.number)
    }

    /// The truth answer, if this is one.
    pub fn as_truth(&self) -> Option<&TruthAnswer> {
        match self {
            Answer::Truth(a) => Some(a),
            _ => None,
        }
    }

    /// The choice answer, if this is one.
    pub fn as_choice(&self) -> Option<&ChoiceAnswer> {
        match self {
            Answer::Choice(a) => Some(a),
            _ => None,
        }
    }

    /// The score answer, if this is one.
    pub fn as_score(&self) -> Option<&ScoreAnswer> {
        match self {
            Answer::Score(a) => Some(a),
            _ => None,
        }
    }

    /// The probability of a truth answer.
    pub fn truth(&self) -> Option<f64> {
        self.as_truth().map(|a| a.truth)
    }

    /// The chosen option of a choice answer.
    pub fn choice(&self) -> Option<&str> {
        self.as_choice().map(|a| a.choice.as_str())
    }

    /// The expected level of a score answer.
    pub fn score(&self) -> Option<f64> {
        self.as_score().map(|a| a.score)
    }

    /// The confidence of a choice, score, number, point or box answer.
    pub fn confidence(&self) -> Option<f64> {
        match self {
            Answer::Choice(a) => Some(a.confidence),
            Answer::Score(a) => Some(a.confidence),
            Answer::Number(a) => Some(a.confidence),
            Answer::Point(a) => Some(a.confidence),
            Answer::Box(a) => Some(a.confidence),
            _ => None,
        }
    }

    /// Reads one answer. A known type whose fields are missing or of the wrong
    /// type is returned as [`Answer::Unknown`] with its raw fields, as an
    /// unknown type is, so one odd answer never fails the decision.
    pub(crate) fn parse(raw: Value) -> Answer {
        let answer_type = raw.get("type").and_then(Value::as_str).map(str::to_owned);
        let known = raw.as_object().and_then(|obj| match answer_type.as_deref() {
            Some("truth") => Self::parse_truth(obj, "truth"),
            Some("noul") => Self::parse_truth(obj, "noul"),
            Some("choice") => Self::parse_choice(obj),
            Some("score") => Self::parse_score(obj),
            Some("number") => Self::parse_number(obj),
            Some("point") => Self::parse_point(obj),
            Some("box") => Self::parse_box(obj),
            _ => None,
        });
        known.unwrap_or(Answer::Unknown(UnknownAnswer { answer_type, raw: Value::Null })).with_raw(raw)
    }

    fn parse_truth(obj: &Map<String, Value>, field: &str) -> Option<Answer> {
        Some(Answer::Truth(TruthAnswer { truth: obj.get(field)?.as_f64()?, raw: Value::Null }))
    }

    fn parse_number(obj: &Map<String, Value>) -> Option<Answer> {
        let number = obj.get("number")?.as_f64()?;
        let expected = obj.get("expected")?.as_f64()?;
        let confidence = obj.get("confidence")?.as_f64()?;
        let probabilities = obj
            .get("probabilities")?
            .as_object()?
            .iter()
            .map(|(k, v)| v.as_f64().map(|p| (k.clone(), p)))
            .collect::<Option<_>>()?;
        Some(Answer::Number(NumberAnswer { number, expected, confidence, probabilities, raw: Value::Null }))
    }

    fn parse_point(obj: &Map<String, Value>) -> Option<Answer> {
        let n = |k: &str| obj.get(k).and_then(Value::as_f64);
        Some(Answer::Point(PointAnswer {
            x: n("x")?,
            y: n("y")?,
            present: n("present")?,
            confidence: n("confidence")?,
            image: obj.get("image")?.as_str()?.to_owned(),
            width: n("width")?,
            height: n("height")?,
            raw: Value::Null,
        }))
    }

    fn parse_box(obj: &Map<String, Value>) -> Option<Answer> {
        let n = |k: &str| obj.get(k).and_then(Value::as_f64);
        Some(Answer::Box(BoxAnswer {
            x1: n("x1")?,
            y1: n("y1")?,
            x2: n("x2")?,
            y2: n("y2")?,
            present: n("present")?,
            confidence: n("confidence")?,
            image: obj.get("image")?.as_str()?.to_owned(),
            width: n("width")?,
            height: n("height")?,
            raw: Value::Null,
        }))
    }

    fn parse_choice(obj: &Map<String, Value>) -> Option<Answer> {
        let choice = obj.get("choice")?.as_str()?.to_owned();
        let confidence = obj.get("confidence")?.as_f64()?;
        let probabilities = obj
            .get("probabilities")?
            .as_object()?
            .iter()
            .map(|(k, v)| v.as_f64().map(|p| (k.clone(), p)))
            .collect::<Option<_>>()?;
        Some(Answer::Choice(ChoiceAnswer { choice, confidence, probabilities, raw: Value::Null }))
    }

    fn parse_score(obj: &Map<String, Value>) -> Option<Answer> {
        let score = obj.get("score")?.as_f64()?;
        let confidence = obj.get("confidence")?.as_f64()?;
        let probabilities = obj
            .get("probabilities")?
            .as_object()?
            .iter()
            .map(|(k, v)| Some((k.parse::<usize>().ok()?, v.as_f64()?)))
            .collect::<Option<_>>()?;
        let legend = obj
            .get("legend")?
            .as_object()?
            .iter()
            .map(|(k, v)| Some((k.parse::<usize>().ok()?, v.clone())))
            .collect::<Option<_>>()?;
        Some(Answer::Score(ScoreAnswer { score, confidence, probabilities, legend, raw: Value::Null }))
    }

    fn with_raw(mut self, raw: Value) -> Answer {
        match &mut self {
            Answer::Truth(a) => a.raw = raw,
            Answer::Choice(a) => a.raw = raw,
            Answer::Score(a) => a.raw = raw,
            Answer::Number(a) => a.raw = raw,
            Answer::Point(a) => a.raw = raw,
            Answer::Box(a) => a.raw = raw,
            Answer::Unknown(a) => a.raw = raw,
        }
        self
    }
}

/// Token counts of a decision and, for a request with videos or boosters,
/// what was read of them. A value the response does not carry is `None`,
/// never 0.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Usage {
    /// The tokens of the state, the questions, the images and the videos.
    pub input_tokens: Option<u64>,
    /// The output tokens.
    pub output_tokens: Option<u64>,
    /// The frames read over all the videos. `None` for a request without
    /// videos.
    pub video_frames: Option<u64>,
    /// The seconds of the videos' windows read, summed, as sent. `None` for
    /// a request without videos.
    pub video_seconds: Option<f64>,
    /// The share of `input_tokens` that the boosters' readings take. `None`
    /// for a request without boosters.
    pub booster_tokens: Option<u64>,
    /// The booster calls billed. `None` for a request without boosters.
    pub boosters: Option<u64>,
}

impl Usage {
    /// Reads a `usage` object; a value it does not carry is `None`.
    pub(crate) fn from_body(usage: Option<&Value>) -> Usage {
        let usage = usage.and_then(Value::as_object);
        let field = |k: &str| usage.and_then(|u| u.get(k));
        Usage {
            input_tokens: field("input_tokens").and_then(Value::as_u64),
            output_tokens: field("output_tokens").and_then(Value::as_u64),
            video_frames: field("video_frames").and_then(Value::as_u64),
            video_seconds: field("video_seconds").and_then(Value::as_f64),
            booster_tokens: field("booster_tokens").and_then(Value::as_u64),
            boosters: field("boosters").and_then(Value::as_u64),
        }
    }
}

/// The result of one `POST /v1/decide`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Decision {
    /// The answers by question id, in the order the server sent them. Read them
    /// by id, never by position.
    pub answers: IndexMap<String, Answer>,
    /// The pinned id of the model that answered, `decisionnode-<family>-<version>`;
    /// for a fine-tuned model its pinned name, `decisionnode-flash@acme/fraud:3`.
    pub model: Option<String>,
    /// Token counts, each `None` when the response does not carry it.
    pub usage: Usage,
    /// The request id, from the `x-request-id` header.
    pub request_id: Option<String>,
    /// The server's time in milliseconds, from the `x-decisionnode-latency-ms`
    /// header. `None` when the header is missing, never 0.
    pub latency_ms: Option<f64>,
    /// What the safety check found, from the `x-decisionnode-safety` header.
    /// `None` on a server without the check.
    pub safety: Option<Safety>,
    /// The server's notes on a request with videos, exactly as sent: one when
    /// it read a video at a lower rate or resolution than asked. Empty when
    /// there are none.
    pub warnings: Vec<String>,
    /// What each booster the request named read, by booster name in the
    /// request's order. `None` when the request named none (or the response
    /// carries no evidence, as for a request the refusal check refused).
    pub evidence: Option<IndexMap<String, Evidence>>,
    /// Set when another model answered than the one asked for: a request for
    /// `decisionnode-moe` that no ready worker could take at once is answered
    /// by `decisionnode-flash`, unless it was sent with `fallback(false)`.
    /// Read from the `x-decisionnode-fallback` and
    /// `x-decisionnode-requested-model` headers; `None` on an ordinary answer.
    /// [`Decision::model`] is the model that answered.
    pub fallback: Option<Fallback>,
    /// The parsed response body.
    pub raw: Value,
}

/// A fallback answer: the model that answered in place of the one asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Fallback {
    /// The model that answered, from the `x-decisionnode-fallback` header.
    pub served_model: String,
    /// The model the request asked for, from the
    /// `x-decisionnode-requested-model` header.
    pub requested_model: Option<String>,
}

impl Fallback {
    /// Reads the two headers; `None` without `x-decisionnode-fallback`.
    pub(crate) fn from_headers(headers: &HeaderMap) -> Option<Fallback> {
        Some(Fallback {
            served_model: header(headers, "x-decisionnode-fallback")?,
            requested_model: header(headers, "x-decisionnode-requested-model"),
        })
    }
}

impl Decision {
    pub(crate) fn from_response(body: Value, status: u16, headers: &HeaderMap) -> Result<Decision> {
        let request_id = header(headers, "x-request-id");
        let invalid = |what: &str| {
            Error::new(ErrorKind::Server, format!("the response is not a valid decision: {what}"))
                .with_response(status, headers)
        };
        let obj = body.as_object().ok_or_else(|| invalid("the body is not an object"))?;
        let answers = match obj.get("answers") {
            Some(Value::Object(map)) => map.iter().map(|(id, raw)| (id.clone(), Answer::parse(raw.clone()))).collect(),
            None | Some(Value::Null) => IndexMap::new(),
            Some(_) => return Err(invalid("answers is not an object")),
        };
        Ok(Decision {
            model: obj.get("model").and_then(Value::as_str).map(str::to_owned),
            usage: Usage::from_body(obj.get("usage")),
            answers,
            latency_ms: header(headers, "x-decisionnode-latency-ms").and_then(|v| v.trim().parse::<f64>().ok()),
            safety: header(headers, "x-decisionnode-safety").map(|v| Safety::parse(&v)),
            warnings: obj
                .get("warnings")
                .and_then(Value::as_array)
                .map(|w| w.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
            evidence: evidence_from_body(obj.get("evidence")),
            fallback: Fallback::from_headers(headers),
            request_id,
            raw: body,
        })
    }

    /// The answer to a question, by its id.
    pub fn get(&self, id: &str) -> Option<&Answer> {
        self.answers.get(id)
    }
}

/// `decision["route"]` reads the answer to the question `route`.
///
/// Panics when there is no such question, like indexing a map. Use
/// [`Decision::get`] to handle a missing id.
impl Index<&str> for Decision {
    type Output = Answer;

    fn index(&self, id: &str) -> &Answer {
        match self.answers.get(id) {
            Some(a) => a,
            None => panic!("no answer for question {id:?}"),
        }
    }
}

/// The result of one request of [`crate::DecisionNode::decide_many`]: the
/// decision, or the error that request ended with.
pub type DecisionResult = Result<Decision>;

/// A model a key may use: one entry of `GET /v1/models`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ModelInfo {
    /// The model id to send: a family id such as `decisionnode-moe`, or the
    /// pinned id `decisionnode-<family>-<version>` it resolves to.
    pub name: String,
    /// What the model is.
    pub description: String,
    /// The release date, `YYYY-MM-DD`; for a fine-tuned model, the day the
    /// version finished training.
    pub release_date: String,
    /// `Some(true)` for one of the workspace's fine-tuned models; `None` on a
    /// catalogue entry.
    pub fine_tuned: Option<bool>,
    /// For a fine-tuned model: the pinned id of the model it was trained from.
    pub base: Option<String>,
    /// For a fine-tuned model: its version number; on a plain name, the
    /// version it resolves to.
    pub adapter_version: Option<u64>,
    /// For a fine-tuned model: `deployed` (it answers decisions) or `ready`
    /// (trained, not deployed); any other value as received.
    pub status: Option<String>,
    /// The model's published limits: every other field of the entry, for
    /// example `max_input_tokens`.
    pub limits: Map<String, Value>,
    /// The entry as received.
    pub raw: Value,
}

impl ModelInfo {
    pub(crate) fn list_from(body: Value, status: u16, headers: &HeaderMap) -> Result<Vec<ModelInfo>> {
        let invalid =
            || Error::new(ErrorKind::Server, "the response is not a valid model list").with_response(status, headers);
        let models = body.get("models").and_then(Value::as_array).ok_or_else(invalid)?;
        models
            .iter()
            .map(|entry| {
                let obj = entry.as_object().ok_or_else(invalid)?;
                let text = |k: &str| obj.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                Ok(ModelInfo {
                    name: text("name"),
                    description: text("description"),
                    release_date: text("release_date"),
                    fine_tuned: obj.get("fine_tuned").and_then(Value::as_bool),
                    base: obj.get("base").and_then(Value::as_str).map(str::to_owned),
                    adapter_version: obj.get("adapter_version").and_then(Value::as_u64),
                    status: obj.get("status").and_then(Value::as_str).map(str::to_owned),
                    limits: obj
                        .iter()
                        .filter(|(k, _)| !matches!(k.as_str(), "name" | "description" | "release_date"))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    raw: entry.clone(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        map
    }

    #[test]
    fn parses_the_three_types_and_keeps_long_floats() {
        let body: Value = serde_json::from_str(
            r#"{"model":"decisionnode-moe-0.9.0-rc1","answers":{
                "route":{"type":"choice","choice":"billing","confidence":0.81,"probabilities":{"billing":0.8200000000000001,"bug":0.08,"account":0.1}},
                "urgency":{"type":"score","score":2.31,"confidence":0.31,"legend":{"0":"routine","1":{"what":"today"}},"probabilities":{"0":0.060000000000000005,"1":0.94}},
                "escalate":{"type":"truth","truth":0.94}},
                "usage":{"input_tokens":62,"output_tokens":0}}"#,
        )
        .unwrap();
        let h = headers(&[
            ("x-request-id", "req_1"),
            ("x-decisionnode-latency-ms", "23"),
            ("x-decisionnode-safety", "harm=0.031, self_harm=0.004"),
        ]);
        let d = Decision::from_response(body, 200, &h).unwrap();
        assert_eq!(d["route"].choice(), Some("billing"));
        assert_eq!(d["route"].as_choice().unwrap().probabilities["billing"], 0.8200000000000001);
        assert_eq!(d["urgency"].as_score().unwrap().probabilities[&0], 0.060000000000000005);
        assert_eq!(d["urgency"].as_score().unwrap().legend[&1], json!({"what": "today"}));
        assert_eq!(d["escalate"].truth(), Some(0.94));
        assert_eq!(d.usage.input_tokens, Some(62));
        assert_eq!(d.usage.output_tokens, Some(0));
        assert_eq!(d.request_id.as_deref(), Some("req_1"));
        assert_eq!(d.latency_ms, Some(23.0));
        assert_eq!(d.safety.as_ref().unwrap().harm, Some(0.031));
        assert_eq!(d.model.as_deref(), Some("decisionnode-moe-0.9.0-rc1"));
    }

    #[test]
    fn unknown_types_are_kept() {
        let body = json!({"answers": {"f": {"type": "ranking", "order": ["b", "a"]}}, "usage": {"input_tokens": 1, "output_tokens": 0}});
        let d = Decision::from_response(body, 200, &HeaderMap::new()).unwrap();
        let a = d.get("f").unwrap();
        assert_eq!(a.answer_type(), Some("ranking"));
        assert_eq!(a.raw()["order"], json!(["b", "a"]));
        assert!(d.safety.is_none());
        assert!(d.latency_ms.is_none());
        assert!(matches!(a, Answer::Unknown(_)));
    }

    #[test]
    fn missing_counts_are_none() {
        let d = Decision::from_response(json!({"answers": {}}), 200, &HeaderMap::new()).unwrap();
        assert_eq!(d.usage, Usage::default());
        assert!(d.warnings.is_empty());
        let d = Decision::from_response(json!({"answers": {}, "usage": {"input_tokens": 5}}), 200, &HeaderMap::new())
            .unwrap();
        assert_eq!(d.usage.input_tokens, Some(5));
        assert_eq!(d.usage.output_tokens, None);
        assert_eq!(d.latency_ms, None);
    }

    #[test]
    fn video_usage_and_warnings() {
        let body = json!({"answers": {}, "usage": {"input_tokens": 1300, "output_tokens": 0, "video_frames": 20, "video_seconds": 10.000000000000002},
            "warnings": ["Video 'dock' was read at 1 frame a second."]});
        let d = Decision::from_response(body, 200, &HeaderMap::new()).unwrap();
        assert_eq!(d.usage.video_frames, Some(20));
        assert_eq!(d.usage.video_seconds, Some(10.000000000000002));
        assert_eq!(d.warnings, vec!["Video 'dock' was read at 1 frame a second.".to_owned()]);
    }

    #[test]
    fn point_and_box_answers() {
        let body = json!({"answers": {
            "pick": {"type": "point", "x": 812, "y": 455, "present": 0.97, "confidence": 0.91, "image": "belt", "width": 1920, "height": 1080},
            "damage": {"type": "box", "x1": 0.1563, "y1": 0.1111, "x2": 0.2708, "y2": 0.3796, "present": 0.95, "confidence": 0.88,
                "image": "belt", "width": 1920, "height": 1080},
            "odd": {"type": "point", "x": true, "y": 455, "present": 0.97, "confidence": 0.91, "image": "belt", "width": 1920, "height": 1080},
            "no_image": {"type": "box", "x1": 1, "y1": 1, "x2": 2, "y2": 2, "present": 0.5, "confidence": 0.5, "image": 3, "width": 4, "height": 4},
        }});
        let d = Decision::from_response(body, 200, &HeaderMap::new()).unwrap();
        let p = d["pick"].as_point().unwrap();
        assert_eq!(
            (p.x, p.y, p.present, p.confidence, p.image.as_str(), p.width, p.height),
            (812.0, 455.0, 0.97, 0.91, "belt", 1920.0, 1080.0)
        );
        let b = d["damage"].as_box().unwrap();
        assert_eq!((b.x1, b.y1, b.x2, b.y2), (0.1563, 0.1111, 0.2708, 0.3796));
        assert_eq!(d["damage"].answer_type(), Some("box"));
        assert!(matches!(d["odd"], Answer::Unknown(_)));
        assert!(matches!(d["no_image"], Answer::Unknown(_)));
    }

    #[test]
    fn a_malformed_known_answer_is_unknown() {
        let body = json!({"answers": {
            "t": {"type": "truth"},
            "c": {"type": "choice", "choice": 3, "confidence": 0.5, "probabilities": {}},
            "s": {"type": "score", "score": 1.0, "confidence": 0.5, "probabilities": {"x": 0.1}, "legend": {}},
            "n": 7,
            "ok": {"type": "truth", "truth": 0.5}
        }});
        let d = Decision::from_response(body, 200, &HeaderMap::new()).unwrap();
        for id in ["t", "c", "s"] {
            assert!(matches!(d[id], Answer::Unknown(_)), "{id}");
        }
        assert_eq!(d["t"].answer_type(), Some("truth"));
        assert_eq!(d["c"].raw()["choice"], json!(3));
        assert_eq!(d["n"].answer_type(), None);
        assert_eq!(d["n"].raw(), &json!(7));
        assert_eq!(d["ok"].truth(), Some(0.5));
    }

    #[test]
    fn noul_reads_as_truth_and_number_answers_parse() {
        let body = json!({"answers": {
            "n": {"type": "noul", "noul": 0.25},
            "bad_noul": {"type": "noul", "truth": 0.25},
            "cars": {"type": "number", "number": 7, "expected": 7.0, "confidence": 0.78,
                     "probabilities": {"6": 0.105, "7": 0.783, "8": 0.098}},
            "fine": {"type": "number", "number": -2.5, "expected": -2.35, "confidence": 0.5,
                     "probabilities": {"-2.5": 0.5, "0.000001": 0.002}},
            "bad_number": {"type": "number", "number": "7", "expected": 7.0, "confidence": 0.78, "probabilities": {}}
        }});
        let d = Decision::from_response(body, 200, &HeaderMap::new()).unwrap();
        assert_eq!(d["n"].truth(), Some(0.25));
        assert_eq!(d["n"].answer_type(), Some("truth"));
        assert_eq!(d["n"].raw()["type"], "noul");
        assert!(matches!(d["bad_noul"], Answer::Unknown(_)));
        assert_eq!(d["bad_noul"].answer_type(), Some("noul"));
        let cars = d["cars"].as_number().unwrap();
        assert_eq!(cars.number, 7.0);
        assert_eq!(cars.probabilities.keys().collect::<Vec<_>>(), ["6", "7", "8"]);
        assert_eq!(d["fine"].as_number().unwrap().probabilities["0.000001"], 0.002);
        assert_eq!(d["fine"].number(), Some(-2.5));
        assert!(matches!(d["bad_number"], Answer::Unknown(_)));
    }

    #[test]
    fn evidence_and_booster_usage() {
        let body = json!({"model": "decisionnode-flash@acme/fraud:3", "answers": {"adult": {"type": "truth", "truth": 0.97}},
            "evidence": {"age": {"version": "age-v3", "status": "ok", "latency_ms": 212,
                "results": [{"image": "a1b2", "age_years": 27, "low": 22, "high": 33, "confidence": 0.81, "faces": 1}]}},
            "usage": {"input_tokens": 341, "output_tokens": 0, "booster_tokens": 72, "boosters": 3}});
        let d = Decision::from_response(body, 200, &HeaderMap::new()).unwrap();
        assert_eq!(d.model.as_deref(), Some("decisionnode-flash@acme/fraud:3"));
        assert_eq!((d.usage.booster_tokens, d.usage.boosters), (Some(72), Some(3)));
        let ev = d.evidence.as_ref().unwrap();
        assert_eq!(ev["age"].results[0].as_age().unwrap().age_years, Some(27.0));
        let plain =
            Decision::from_response(json!({"answers": {}, "usage": {"input_tokens": 3}}), 200, &HeaderMap::new())
                .unwrap();
        assert_eq!((plain.usage.booster_tokens, plain.usage.boosters), (None, None));
        assert!(plain.evidence.is_none());
    }

    #[test]
    fn fine_tuned_models() {
        let body = json!({"models": [
            {"name": "decisionnode-flash", "description": "d", "release_date": "2026-10-01", "version": "decisionnode-flash-0.9.0-rc1"},
            {"name": "decisionnode-flash@acme/fraud:3", "description": "Claims fraud", "release_date": "2026-10-09",
             "fine_tuned": true, "base": "decisionnode-flash-0.9.0-rc1", "adapter_version": 3, "status": "deployed",
             "version": "decisionnode-flash@acme/fraud:3"}]});
        let m = ModelInfo::list_from(body, 200, &HeaderMap::new()).unwrap();
        assert_eq!(
            (m[0].fine_tuned, m[0].base.as_deref(), m[0].adapter_version, m[0].status.as_deref()),
            (None, None, None, None)
        );
        assert_eq!(m[1].fine_tuned, Some(true));
        assert_eq!(m[1].base.as_deref(), Some("decisionnode-flash-0.9.0-rc1"));
        assert_eq!(m[1].adapter_version, Some(3));
        assert_eq!(m[1].status.as_deref(), Some("deployed"));
        assert_eq!(m[1].limits.get("version"), Some(&json!("decisionnode-flash@acme/fraud:3")));
    }

    #[test]
    fn fallback_from_the_headers() {
        let body = json!({"model": "decisionnode-flash-0.9.0-rc1", "answers": {}});
        let h = headers(&[
            ("x-decisionnode-fallback", "decisionnode-flash-0.9.0-rc1"),
            ("x-decisionnode-requested-model", "decisionnode-moe"),
        ]);
        let d = Decision::from_response(body.clone(), 200, &h).unwrap();
        let f = d.fallback.unwrap();
        assert_eq!(
            (f.served_model.as_str(), f.requested_model.as_deref()),
            ("decisionnode-flash-0.9.0-rc1", Some("decisionnode-moe"))
        );
        assert_eq!(d.model.as_deref(), Some("decisionnode-flash-0.9.0-rc1"));
        let d = Decision::from_response(body.clone(), 200, &HeaderMap::new()).unwrap();
        assert_eq!(d.fallback, None);
        // The requested model alone is not a fallback answer.
        let d = Decision::from_response(body, 200, &headers(&[("x-decisionnode-requested-model", "decisionnode-moe")]))
            .unwrap();
        assert_eq!(d.fallback, None);
    }

    #[test]
    fn models_split_limits() {
        let body = json!({"models": [{"name": "decisionnode-moe-0.9.0-rc1", "description": "d", "release_date": "2026-10-01", "max_images": 4}]});
        let m = ModelInfo::list_from(body, 200, &HeaderMap::new()).unwrap();
        assert_eq!(m[0].name, "decisionnode-moe-0.9.0-rc1");
        assert_eq!(m[0].limits.get("max_images"), Some(&json!(4)));
        assert!(!m[0].limits.contains_key("name"));
    }
}
