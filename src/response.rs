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
    /// The `type` of the answer: `truth`, `choice`, `score` or another type.
    pub fn answer_type(&self) -> Option<&str> {
        match self {
            Answer::Truth(_) => Some("truth"),
            Answer::Choice(_) => Some("choice"),
            Answer::Score(_) => Some("score"),
            Answer::Unknown(o) => o.answer_type.as_deref(),
        }
    }

    /// The answer object as received.
    pub fn raw(&self) -> &Value {
        match self {
            Answer::Truth(a) => &a.raw,
            Answer::Choice(a) => &a.raw,
            Answer::Score(a) => &a.raw,
            Answer::Unknown(a) => &a.raw,
        }
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

    /// The confidence of a choice or score answer.
    pub fn confidence(&self) -> Option<f64> {
        match self {
            Answer::Choice(a) => Some(a.confidence),
            Answer::Score(a) => Some(a.confidence),
            _ => None,
        }
    }

    /// Reads one answer. A known type whose fields are missing or of the wrong
    /// type is returned as [`Answer::Unknown`] with its raw fields, as an
    /// unknown type is, so one odd answer never fails the decision.
    pub(crate) fn parse(raw: Value) -> Answer {
        let answer_type = raw.get("type").and_then(Value::as_str).map(str::to_owned);
        let known = raw.as_object().and_then(|obj| match answer_type.as_deref() {
            Some("truth") => Self::parse_truth(obj),
            Some("choice") => Self::parse_choice(obj),
            Some("score") => Self::parse_score(obj),
            _ => None,
        });
        known.unwrap_or(Answer::Unknown(UnknownAnswer { answer_type, raw: Value::Null })).with_raw(raw)
    }

    fn parse_truth(obj: &Map<String, Value>) -> Option<Answer> {
        Some(Answer::Truth(TruthAnswer { truth: obj.get("truth")?.as_f64()?, raw: Value::Null }))
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
            Answer::Unknown(a) => a.raw = raw,
        }
        self
    }
}

/// Token counts of a decision. A count the response does not carry is
/// `None`, never 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Usage {
    /// The tokens of the state, the questions and the images.
    pub input_tokens: Option<u64>,
    /// The output tokens.
    pub output_tokens: Option<u64>,
}

/// The result of one `POST /v1/decide`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Decision {
    /// The answers by question id, in the order the server sent them. Read them
    /// by id, never by position.
    pub answers: IndexMap<String, Answer>,
    /// The resolved model version, for example `decisionnode-1.0`.
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
    /// The parsed response body.
    pub raw: Value,
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
        let usage = obj.get("usage").and_then(Value::as_object);
        let tokens = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64);
        Ok(Decision {
            model: obj.get("model").and_then(Value::as_str).map(str::to_owned),
            usage: Usage { input_tokens: tokens("input_tokens"), output_tokens: tokens("output_tokens") },
            answers,
            latency_ms: header(headers, "x-decisionnode-latency-ms").and_then(|v| v.trim().parse::<f64>().ok()),
            safety: header(headers, "x-decisionnode-safety").map(|v| Safety::parse(&v)),
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
    /// The model id to send, for example `decisionnode-1.0`.
    pub name: String,
    /// What the model is.
    pub description: String,
    /// The release date, `YYYY-MM-DD`.
    pub release_date: String,
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
            r#"{"model":"decisionnode-1.0","answers":{
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
        assert_eq!(d.model.as_deref(), Some("decisionnode-1.0"));
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
        assert_eq!(d.usage, Usage { input_tokens: None, output_tokens: None });
        let d = Decision::from_response(json!({"answers": {}, "usage": {"input_tokens": 5}}), 200, &HeaderMap::new())
            .unwrap();
        assert_eq!(d.usage.input_tokens, Some(5));
        assert_eq!(d.usage.output_tokens, None);
        assert_eq!(d.latency_ms, None);
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
    fn models_split_limits() {
        let body = json!({"models": [{"name": "decisionnode-1.0", "description": "d", "release_date": "2026-10-01", "max_images": 4}]});
        let m = ModelInfo::list_from(body, 200, &HeaderMap::new()).unwrap();
        assert_eq!(m[0].name, "decisionnode-1.0");
        assert_eq!(m[0].limits.get("max_images"), Some(&json!(4)));
        assert!(!m[0].limits.contains_key("name"));
    }
}
