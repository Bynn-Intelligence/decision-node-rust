//! The API's fixed question rules, checked before any request.
//!
//! A request that breaks one would only come back refused, so the client
//! refuses it itself, with the API's own error kind and message and no
//! status, and saves the round trip. Limits that depend on the model, such as
//! the token budget, stay the server's.

use serde_json::{Map, Value};

use crate::error::{Error, ErrorKind, Result};

/// The most options of a choice.
pub(crate) const MAX_CHOICE_OPTIONS: usize = 255;
/// The most levels of a score.
pub(crate) const MAX_SCORE_LEVELS: usize = 10;
/// The most values of a number grid.
pub(crate) const MAX_GRID_VALUES: u128 = 4096;

fn bad_request(message: String) -> Error {
    Error::local(ErrorKind::BadRequest, None, message)
}

fn validation(message: String) -> Error {
    Error::local(ErrorKind::Validation, None, message)
}

/// Checks every question in the API's order: the faults the API answers with
/// a 422 first (a number grid that cannot be built, a score without levels),
/// then the 400 faults in question order (a choice of no or more than 255
/// options, a score of more than 10 levels, a truth with neither instructions
/// nor criteria, a number without instructions).
pub(crate) fn check_questions(questions: &Map<String, Value>) -> Result<()> {
    for (id, q) in questions {
        let Some(q) = q.as_object() else { continue };
        match q.get("type").and_then(Value::as_str) {
            Some("score") => {
                if levels(q) == Some(0) {
                    return Err(validation(format!("question {id}: a score needs at least one level")));
                }
            }
            Some("number") => check_grid(id, q)?,
            _ => {}
        }
    }
    for (id, q) in questions {
        let Some(q) = q.as_object() else { continue };
        match q.get("type").and_then(Value::as_str) {
            Some("choice") => {
                if let Some(Value::Object(options)) = q.get("criteria") {
                    if options.is_empty() {
                        return Err(bad_request(format!("Choice question must have at least one choice: {id}")));
                    }
                    if options.len() > MAX_CHOICE_OPTIONS {
                        return Err(bad_request(format!(
                            "Too many choices. Must have at most {MAX_CHOICE_OPTIONS} choices."
                        )));
                    }
                }
            }
            Some("score") => {
                if levels(q).is_some_and(|n| n > MAX_SCORE_LEVELS) {
                    return Err(bad_request(format!(
                        "Too many score levels. Must have at most {MAX_SCORE_LEVELS} levels."
                    )));
                }
            }
            Some("truth" | "noul") => {
                let criteria = q.get("criteria").and_then(Value::as_object);
                let side = |k: &str| criteria.and_then(|c| c.get(k)).is_some_and(|v| !v.is_null());
                if is_missing(q.get("instructions")) && !side("true") && !side("false") {
                    return Err(bad_request(format!("Truth question must have criteria or instructions: {id}")));
                }
            }
            Some("number") => {
                let instructions = q.get("instructions");
                if is_missing(instructions) || instructions == Some(&Value::String(String::new())) {
                    return Err(bad_request(format!("Number question must have instructions: {id}")));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_missing(v: Option<&Value>) -> bool {
    v.is_none_or(Value::is_null)
}

/// The number of levels of a score given as a list, or as an object keyed
/// `"0"` to `"K-1"`.
fn levels(q: &Map<String, Value>) -> Option<usize> {
    match q.get("criteria")? {
        Value::Array(a) => Some(a.len()),
        Value::Object(o) => Some(o.len()),
        _ => None,
    }
}

/// An exact decimal: `mantissa / 10^scale`, with no trailing zeros.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Decimal {
    mantissa: i128,
    scale: u32,
}

impl Decimal {
    fn int(n: i128) -> Decimal {
        Decimal { mantissa: n, scale: 0 }
    }

    /// Reads a JSON number exactly from its shortest text (`0.1` is one
    /// tenth, not the float nearest to it). `None` for a value too large to
    /// hold exactly, which the server then judges.
    fn parse(n: &serde_json::Number) -> Option<Decimal> {
        let text = n.to_string();
        let (negative, text) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.as_str()),
        };
        let (digits, exponent) = match text.find(['e', 'E']) {
            Some(at) => (&text[..at], text[at + 1..].parse::<i32>().ok()?),
            None => (text, 0),
        };
        let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
        let mut mantissa: i128 = 0;
        for c in int.chars().chain(frac.chars()) {
            mantissa = mantissa.checked_mul(10)?.checked_add(i128::from(c.to_digit(10)?))?;
        }
        let mut scale = i32::try_from(frac.len()).ok()?.checked_sub(exponent)?;
        if scale < 0 {
            mantissa = mantissa.checked_mul(10i128.checked_pow(scale.unsigned_abs())?)?;
            scale = 0;
        }
        let mut d = Decimal { mantissa: if negative { -mantissa } else { mantissa }, scale: scale as u32 };
        while d.scale > 0 && d.mantissa % 10 == 0 {
            d.mantissa /= 10;
            d.scale -= 1;
        }
        Some(d)
    }

    /// The value times `10^places`, rounded down; `places` is at least the
    /// scale for an exact result.
    fn scaled_floor(self, places: u32) -> Option<i128> {
        if places >= self.scale {
            self.mantissa.checked_mul(10i128.checked_pow(places - self.scale)?)
        } else {
            Some(self.mantissa.div_euclid(10i128.checked_pow(self.scale - places)?))
        }
    }
}

impl std::fmt::Display for Decimal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.scale == 0 {
            return write!(f, "{}", self.mantissa);
        }
        let digits = self.mantissa.unsigned_abs().to_string();
        let scale = self.scale as usize;
        let padded = format!("{digits:0>width$}", width = scale + 1);
        let (int, frac) = padded.split_at(padded.len() - scale);
        let sign = if self.mantissa < 0 { "-" } else { "" };
        write!(f, "{sign}{int}.{frac}")
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "a string",
        Value::Bool(_) => "a boolean",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
        _ => "a number",
    }
}

/// The grid of a number question, built in exact decimals from `min`, `max`
/// and `step` (defaults 0, 255 and 1), as the API builds it.
fn check_grid(id: &str, q: &Map<String, Value>) -> Result<()> {
    let mut fields = [Decimal::int(0), Decimal::int(255), Decimal::int(1)];
    for (slot, name) in fields.iter_mut().zip(["min", "max", "step"]) {
        match q.get(name) {
            None | Some(Value::Null) => {}
            Some(Value::Number(n)) => match Decimal::parse(n) {
                Some(d) => *slot = d,
                // Beyond what the client can hold exactly: the server judges.
                None => return Ok(()),
            },
            Some(other) => {
                return Err(validation(format!("question '{id}': {name} must be a number, got {}", type_name(other))))
            }
        }
    }
    let [min, max, step] = fields;
    if step.mantissa <= 0 {
        return Err(validation(format!("question '{id}': step must be above 0, got {step}")));
    }
    let places = min.scale.max(step.scale);
    let (Some(m), Some(big_m), Some(s)) =
        (min.scaled_floor(places), max.scaled_floor(places), step.scaled_floor(places))
    else {
        return Ok(());
    };
    // min above max compares the values themselves (max need not be on the grid).
    let max_exact = max.scaled_floor(places.max(max.scale));
    let min_exact = min.scaled_floor(places.max(max.scale));
    if let (Some(lo), Some(hi)) = (min_exact, max_exact) {
        if lo > hi {
            return Err(validation(format!("question '{id}': min {min} is above max {max}")));
        }
    }
    let Some(span) = big_m.checked_sub(m) else { return Ok(()) };
    let values = (span / s) as u128 + 1;
    if values > MAX_GRID_VALUES {
        return Err(validation(format!(
            "question '{id}': the grid holds {values} values, the limit is {MAX_GRID_VALUES}; narrow the range or widen the step"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check(q: Value) -> Result<()> {
        check_questions(json!({"q": q}).as_object().unwrap())
    }

    fn kind(q: Value) -> Option<ErrorKind> {
        check(q).err().map(|e| {
            assert_eq!(e.status(), None);
            e.kind()
        })
    }

    #[test]
    fn decimals_are_exact() {
        let d = |v: Value| Decimal::parse(v.as_number().unwrap()).unwrap();
        assert_eq!(d(json!(0.1)), Decimal { mantissa: 1, scale: 1 });
        assert_eq!(d(json!(0.25)), Decimal { mantissa: 25, scale: 2 });
        assert_eq!(d(json!(1e-7)), Decimal { mantissa: 1, scale: 7 });
        assert_eq!(d(json!(1e21)), Decimal { mantissa: 1_000_000_000_000_000_000_000, scale: 0 });
        assert_eq!(d(json!(-2.5)), Decimal { mantissa: -25, scale: 1 });
        assert_eq!(d(json!(50)), Decimal::int(50));
        assert_eq!(d(json!(-0.000001)).to_string(), "-0.000001");
        assert_eq!(d(json!(3.5)).to_string(), "3.5");
        assert!(Decimal::parse(json!(1e300).as_number().unwrap()).is_none());
    }

    #[test]
    fn grids() {
        let n = |extra: Value| {
            let mut q = json!({"type": "number", "instructions": "How many?"});
            q.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            kind(q)
        };
        assert_eq!(n(json!({})), None); // 0 to 255: 256 values
        assert_eq!(n(json!({"min": 0, "max": 4095})), None);
        assert_eq!(n(json!({"min": 0, "max": 4096})), Some(ErrorKind::Validation));
        assert_eq!(n(json!({"min": 0, "max": 100, "step": 0.1})), None); // 1,001 values
        assert_eq!(n(json!({"min": 0, "max": 1000, "step": 0.1})), Some(ErrorKind::Validation));
        assert_eq!(n(json!({"min": 0, "max": 0.3, "step": 0.1})), None);
        assert_eq!(n(json!({"min": 60, "max": 50})), Some(ErrorKind::Validation));
        assert_eq!(n(json!({"min": 0.5, "max": 0.45})), Some(ErrorKind::Validation));
        assert_eq!(n(json!({"min": 5, "max": 5})), None);
        assert_eq!(n(json!({"step": 0})), Some(ErrorKind::Validation));
        assert_eq!(n(json!({"step": -1})), Some(ErrorKind::Validation));
        assert_eq!(n(json!({"max": "5"})), Some(ErrorKind::Validation));
        assert_eq!(n(json!({"min": null, "max": null, "step": null})), None);
        assert_eq!(n(json!({"min": -40, "max": 10})), None);
        assert_eq!(n(json!({"min": 1e300})), None); // the server's to judge
    }

    #[test]
    fn grid_faults_come_before_missing_instructions() {
        let e = check(json!({"type": "number", "min": 60, "max": 50})).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert_eq!(e.message(), "question 'q': min 60 is above max 50");
        let e = check(json!({"type": "number", "min": 0, "max": 10})).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::BadRequest);
        assert_eq!(e.message(), "Number question must have instructions: q");
        assert!(check(json!({"type": "number", "instructions": ""})).is_err());
    }

    #[test]
    fn the_422_phase_runs_over_every_question_first() {
        let options: Map<String, Value> = (0..256).map(|i| (format!("o{i}"), Value::Null)).collect();
        let qs = json!({
            "a": {"type": "choice", "criteria": options},
            "b": {"type": "score", "criteria": []},
        });
        assert_eq!(check_questions(qs.as_object().unwrap()).unwrap_err().kind(), ErrorKind::Validation);
    }

    #[test]
    fn choices_scores_and_truths() {
        let options =
            |n: usize| -> Value { (0..n).map(|i| (format!("o{i}"), Value::Null)).collect::<Map<_, _>>().into() };
        assert_eq!(kind(json!({"type": "choice", "criteria": options(255)})), None);
        assert_eq!(kind(json!({"type": "choice", "criteria": options(256)})), Some(ErrorKind::BadRequest));
        assert_eq!(kind(json!({"type": "choice", "criteria": {}})), Some(ErrorKind::BadRequest));
        assert_eq!(kind(json!({"type": "choice"})), None); // the server's 422
        assert_eq!(kind(json!({"type": "score", "criteria": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]})), None);
        assert_eq!(
            kind(json!({"type": "score", "criteria": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]})),
            Some(ErrorKind::BadRequest)
        );
        assert_eq!(kind(json!({"type": "score", "criteria": []})), Some(ErrorKind::Validation));
        assert_eq!(kind(json!({"type": "truth"})), Some(ErrorKind::BadRequest));
        assert_eq!(
            kind(json!({"type": "noul", "instructions": null, "criteria": {"true": null}})),
            Some(ErrorKind::BadRequest)
        );
        assert_eq!(kind(json!({"type": "truth", "criteria": {"false": "no"}})), None);
        assert_eq!(kind(json!({"type": "truth", "instructions": ""})), None);
        assert_eq!(kind(json!({"type": "ranking"})), None);
        let e = check(json!({"type": "choice", "criteria": options(300)})).unwrap_err();
        assert_eq!(e.message(), "Too many choices. Must have at most 255 choices.");
        assert_eq!(e.error_type(), None);
    }
}
