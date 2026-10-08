//! The API's fixed rules, checked before any request.
//!
//! The server never sees a request the client could have refused: every rule
//! the API documents is checked here, in the server's order (the nesting
//! limit, then every `422` entry, then the first `400` in question order),
//! and a fault is what the server would answer: the same kind, the same
//! `error_type`, the documented message, and for a `422` exactly the
//! server's entries. A local error has no status and no request id. Limits
//! that depend on the model and are counted in tokens stay the server's.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::error::{Error, ErrorKind, Result};

/// The most options of a choice.
pub(crate) const MAX_CHOICE_OPTIONS: u64 = 255;
/// The most levels of a score.
pub(crate) const MAX_SCORE_LEVELS: u64 = 10;
/// The most values of a number grid.
pub(crate) const MAX_GRID_VALUES: u64 = 4096;
/// The most objects and arrays inside each other in a body.
pub(crate) const MAX_NESTING: usize = 128;
/// The largest session window.
#[cfg(feature = "sessions")]
pub(crate) const MAX_WINDOW: i64 = 64;
/// The longest session lifetime, in seconds.
#[cfg(feature = "sessions")]
pub(crate) const MAX_TTL_SECONDS: i64 = 3600;
/// 2^53, the bound of a number grid's `min` and `max`.
const TWO_53: i128 = 1 << 53;
/// The most decimals of a grid's `min` and `step`.
const MAX_NUMBER_DECIMALS: u32 = 6;

/// The path a request goes to: `/v1/systemone` takes fewer question types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    Decide,
    Systemone,
}

/// The `/v1/models` entry of the model a request names, when the client holds
/// it from an earlier `models()` call. A rule whose value only `/v1/models`
/// states is checked against it, and skipped without it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Limits {
    pub(crate) model: String,
    entry: Option<Map<String, Value>>,
}

impl Limits {
    /// The entry of the model a request names: the entry of that name, else
    /// the one stating the name as its `alias` or its pinned `version`. A
    /// name the list does not state holds nothing.
    pub(crate) fn of(model: &str, held: &HashMap<String, Map<String, Value>>) -> Limits {
        let entry = held.get(model).or_else(|| {
            held.values()
                .find(|m| ["alias", "version"].iter().any(|k| m.get(*k).and_then(Value::as_str) == Some(model)))
        });
        Limits { model: model.to_owned(), entry: entry.cloned() }
    }

    /// A field of the entry, or of one of its objects (`sessions`, `batch`).
    fn value(&self, path: &[&str]) -> Option<&Value> {
        let (first, rest) = path.split_first()?;
        let mut cur = self.entry.as_ref()?.get(*first)?;
        for p in rest {
            cur = cur.get(*p)?;
        }
        Some(cur)
    }

    /// A positive whole-number limit; 0 or missing counts as not held.
    pub(crate) fn count(&self, path: &[&str]) -> Option<u64> {
        self.value(path).and_then(Value::as_u64).filter(|&n| n > 0)
    }

    fn is_zero(&self, path: &[&str]) -> bool {
        self.value(path).and_then(Value::as_u64) == Some(0)
    }

    #[cfg(feature = "sessions")]
    fn is_false(&self, path: &[&str]) -> bool {
        self.value(path).and_then(Value::as_bool) == Some(false)
    }
}

pub(crate) fn bad_request(error_type: Option<&str>, message: impl Into<String>) -> Error {
    Error::local(ErrorKind::BadRequest, error_type, message)
}

pub(crate) fn usage_error(message: impl Into<String>) -> Error {
    bad_request(Some("api_usage_error"), message)
}

fn entry(kind: &str, loc: &[Value], msg: impl Into<String>, input: &Value, ctx: Option<Value>) -> Value {
    let mut e = Map::new();
    e.insert("type".into(), kind.into());
    e.insert("loc".into(), Value::Array(loc.to_vec()));
    e.insert("msg".into(), Value::String(msg.into()));
    e.insert("input".into(), input.clone());
    if let Some(c) = ctx {
        e.insert("ctx".into(), c);
    }
    Value::Object(e)
}

fn with(loc: &[Value], more: impl Into<Value>) -> Vec<Value> {
    let mut v = loc.to_vec();
    v.push(more.into());
    v
}

/// A string, an object or an array: the three forms a text value may take.
fn is_text(v: &Value) -> bool {
    matches!(v, Value::String(_) | Value::Object(_) | Value::Array(_))
}

/// The three entries the API lists for a value that is none of the three.
fn text_entries(loc: &[Value], value: &Value, entries: &mut Vec<Value>) {
    entries.push(entry("string_type", &with(loc, "str"), "Input should be a valid string", value, None));
    entries.push(entry("dict_type", &with(loc, "dict[any,any]"), "Input should be a valid dictionary", value, None));
    entries.push(entry("list_type", &with(loc, "list[any]"), "Input should be a valid list", value, None));
}

/// How many objects and arrays sit inside each other, the outermost
/// included.
pub(crate) fn nesting_depth(value: &Value) -> usize {
    let mut deepest = 0;
    let mut stack = vec![(value, 1usize)];
    while let Some((node, depth)) = stack.pop() {
        let children: Box<dyn Iterator<Item = &Value>> = match node {
            Value::Object(m) => Box::new(m.values()),
            Value::Array(a) => Box::new(a.iter()),
            _ => continue,
        };
        deepest = deepest.max(depth);
        stack.extend(children.filter(|c| c.is_object() || c.is_array()).map(|c| (c, depth + 1)));
    }
    deepest
}

/// The nesting depth of a body held as a map.
pub(crate) fn object_depth(m: &Map<String, Value>) -> usize {
    1 + m.values().map(nesting_depth).max().unwrap_or(0)
}

/// The 128-level nesting limit of a body.
pub(crate) fn check_nesting(depth: usize) -> Result<()> {
    if depth > MAX_NESTING {
        return Err(usage_error("Invalid request.").with_reason(format!(
            "the request nests {depth} objects and arrays inside each other; the limit is {MAX_NESTING}"
        )));
    }
    Ok(())
}

/// Checks a request body (its images already counted in `depth` by the
/// caller): the nesting limit, every `422` entry, then the first `400`.
/// `extra` holds `422` entries found before the questions. The point and box
/// rules on the request's images are left out: [`check_body`] runs them.
#[cfg(any(feature = "sessions", test))]
pub(crate) fn check_request(
    body: &Map<String, Value>,
    depth: usize,
    profile: Profile,
    limits: &Limits,
    extra: Vec<Value>,
) -> Result<()> {
    check_body(body, depth, profile, limits, extra, None)
}

/// [`check_request`] with the request's image ids, which a point or box
/// question must name among; `None` where a request carries no images of its
/// own (a session's open).
pub(crate) fn check_body(
    body: &Map<String, Value>,
    depth: usize,
    profile: Profile,
    limits: &Limits,
    extra: Vec<Value>,
    image_ids: Option<&[&str]>,
) -> Result<()> {
    check_nesting(depth)?;
    let mut entries = extra;
    let mut deferred: Vec<Error> = Vec::new();
    if let Some(state) = body.get("state").filter(|s| !s.is_null()) {
        if !is_text(state) {
            text_entries(&[json!("body"), json!("state")], state, &mut entries);
        }
    }
    for (id, q) in body.get("questions").and_then(Value::as_object).into_iter().flatten() {
        check_question(id, q, &mut entries, &mut deferred, profile, limits, image_ids);
    }
    if !entries.is_empty() {
        return Err(Error::local_entries(entries));
    }
    match deferred.into_iter().next() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A point or box question: its image among the request's (the `422`
/// entries), its units (a `422` entry), then its instructions (the plain
/// `400`). Without image ids (a session's open) the image rules are skipped.
#[allow(clippy::too_many_arguments)]
fn check_place(
    id: &str,
    qtype: &str,
    obj: &Map<String, Value>,
    q: &Value,
    loc: &[Value],
    entries: &mut Vec<Value>,
    deferred: &mut Vec<Error>,
    image_ids: Option<&[&str]>,
) {
    if let Some(ids) = image_ids {
        let image = obj.get("image").filter(|v| !v.is_null());
        match image {
            _ if ids.is_empty() => entries.push(entry(
                "image_required",
                loc,
                format!("A {qtype} question needs an image in the request"),
                q,
                None,
            )),
            None if ids.len() >= 2 => entries.push(entry("missing", &with(loc, "image"), "Field required", q, None)),
            None => {}
            Some(Value::String(name)) if !ids.contains(&name.as_str()) => entries.push(entry(
                "unknown_image",
                &with(loc, "image"),
                "Input should be the id of an image of the request",
                &Value::String(name.clone()),
                None,
            )),
            Some(Value::String(_)) => {}
            Some(other) => {
                entries.push(entry("string_type", &with(loc, "image"), "Input should be a valid string", other, None))
            }
        }
    }
    if let Some(units) = obj.get("units").filter(|v| !v.is_null()) {
        if !matches!(units.as_str(), Some("pixels" | "normalized")) {
            entries.push(entry(
                "literal_error",
                &with(loc, "units"),
                "Input should be 'pixels' or 'normalized'",
                units,
                Some(json!({"expected": "'pixels' or 'normalized'"})),
            ));
        }
    }
    if obj.get("instructions").is_none_or(Value::is_null) {
        let name = if qtype == "point" { "Point" } else { "Box" };
        deferred.push(bad_request(None, format!("{name} question must have instructions: {id}")));
    }
}

fn check_question(
    id: &str,
    q: &Value,
    entries: &mut Vec<Value>,
    deferred: &mut Vec<Error>,
    profile: Profile,
    limits: &Limits,
    image_ids: Option<&[&str]>,
) {
    let base = [json!("body"), json!("questions"), json!(id)];
    let Some(obj) = q.as_object() else {
        entries.push(entry("model_type", &base, "Input should be a valid dictionary or object", q, None));
        return;
    };
    let Some(qtype) = obj.get("type").filter(|t| !t.is_null()) else {
        entries.push(entry("missing", &with(&base, "type"), "Field required", q, None));
        return;
    };
    let known: &[&str] = match profile {
        Profile::Decide => &["truth", "noul", "choice", "score", "number", "point", "box"],
        Profile::Systemone => &["noul", "choice", "score"],
    };
    let Some(qtype) = qtype.as_str().filter(|t| known.contains(t)) else {
        // An unknown type is the 400 "Invalid request."; on /v1/systemone a
        // number question is unknown from the start, its fields unread.
        deferred.push(usage_error("Invalid request.").with_reason(format!("question '{id}': unknown type {qtype}")));
        return;
    };
    let loc = with(&base, qtype);
    let instructions = obj.get("instructions").filter(|v| !v.is_null());
    if let Some(i) = instructions.filter(|i| !is_text(i)) {
        text_entries(&with(&loc, "instructions"), i, entries);
    }
    let criteria = obj.get("criteria").filter(|v| !v.is_null());
    match qtype {
        "point" | "box" => check_place(id, qtype, obj, q, &loc, entries, deferred, image_ids),
        "number" => {
            if limits.is_zero(&["number_max_grid_values"]) {
                deferred.push(usage_error(format!("Model {} does not answer number questions.", limits.model)));
                return;
            }
            let most = limits.count(&["number_max_grid_values"]).unwrap_or(MAX_GRID_VALUES);
            check_number(id, obj, q, &loc, entries, deferred, most);
        }
        "choice" => {
            let most = limits.count(&["max_choice_options"]).unwrap_or(MAX_CHOICE_OPTIONS);
            match criteria {
                None => entries.push(entry("missing", &with(&loc, "criteria"), "Field required", q, None)),
                Some(Value::Object(options)) => {
                    for (name, desc) in options {
                        if !desc.is_null() && !is_text(desc) {
                            text_entries(&with(&with(&loc, "criteria"), name.as_str()), desc, entries);
                        }
                    }
                    if options.is_empty() {
                        deferred
                            .push(bad_request(None, format!("Choice question must have at least one choice: {id}")));
                    } else if options.len() as u64 > most {
                        deferred
                            .push(bad_request(None, format!("Too many choices. Must have at most {most} choices.")));
                    }
                }
                Some(other) => entries.push(entry(
                    "dict_type",
                    &with(&loc, "criteria"),
                    "Input should be a valid dictionary",
                    other,
                    None,
                )),
            }
        }
        "score" => {
            let most = limits.count(&["max_score_levels"]).unwrap_or(MAX_SCORE_LEVELS);
            let Some(criteria) = criteria else {
                entries.push(entry("missing", &with(&loc, "criteria"), "Field required", q, None));
                return;
            };
            let levels: Vec<&Value> = match criteria {
                Value::Array(a) => a.iter().collect(),
                // The object form keyed "0" to "K-1" is a list on /v1/decide only.
                Value::Object(m)
                    if profile == Profile::Decide && (0..m.len()).all(|i| m.contains_key(&i.to_string())) =>
                {
                    (0..m.len()).map(|i| &m[&i.to_string()]).collect()
                }
                other => {
                    entries.push(entry(
                        "list_type",
                        &with(&loc, "criteria"),
                        "Input should be a valid list",
                        other,
                        None,
                    ));
                    return;
                }
            };
            if levels.is_empty() {
                entries.push(entry(
                    "too_short",
                    &with(&loc, "criteria"),
                    "List should have at least 1 item after validation, not 0",
                    criteria,
                    None,
                ));
                return;
            }
            for (i, level) in levels.iter().enumerate() {
                if !is_text(level) {
                    text_entries(&with(&with(&loc, "criteria"), i), level, entries);
                }
            }
            if levels.len() as u64 > most {
                deferred.push(bad_request(None, format!("Too many score levels. Must have at most {most} levels.")));
            }
        }
        _ => {
            // truth and its alias noul
            let sides = match criteria {
                None => None,
                Some(Value::Object(m)) => Some(m),
                Some(other) => {
                    entries.push(entry(
                        "dict_type",
                        &with(&loc, "criteria"),
                        "Input should be a valid dictionary",
                        other,
                        None,
                    ));
                    return;
                }
            };
            let side = |k: &str| sides.and_then(|m| m.get(k)).filter(|v| !v.is_null());
            for k in ["true", "false"] {
                if let Some(v) = side(k).filter(|v| !is_text(v)) {
                    text_entries(&with(&with(&loc, "criteria"), k), v, entries);
                }
            }
            if instructions.is_none() && side("true").is_none() && side("false").is_none() {
                let name = if qtype == "noul" { "Noul" } else { "Truth" };
                deferred.push(bad_request(None, format!("{name} question must have criteria or instructions: {id}")));
            }
        }
    }
}

// --------------------------------------------------------------------------- number grids

/// An exact decimal: `mantissa / 10^scale`, with no trailing zeros.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Decimal {
    mantissa: i128,
    scale: u32,
}

/// `10^n`, or `None` past what an `i128` holds.
fn pow10(n: u32) -> Option<i128> {
    10i128.checked_pow(n)
}

impl Decimal {
    fn int(n: i128) -> Decimal {
        Decimal { mantissa: n, scale: 0 }
    }

    /// Reads a JSON number exactly from its shortest text (`0.1` is one
    /// tenth, not the float nearest to it). `None` for a value too large or
    /// too precise to hold exactly.
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
            mantissa = mantissa.checked_mul(pow10(scale.unsigned_abs())?)?;
            scale = 0;
        }
        let mut d = Decimal { mantissa: if negative { -mantissa } else { mantissa }, scale: scale as u32 };
        while d.scale > 0 && d.mantissa % 10 == 0 {
            d.mantissa /= 10;
            d.scale -= 1;
        }
        Some(d)
    }

    /// The value times `10^places`, rounded down.
    fn scaled_floor(self, places: u32) -> Option<i128> {
        if places >= self.scale {
            self.mantissa.checked_mul(pow10(places - self.scale)?)
        } else {
            match pow10(self.scale - places) {
                Some(p) => Some(self.mantissa.div_euclid(p)),
                // Smaller than one unit at `places`.
                None => Some(if self.mantissa < 0 { -1 } else { 0 }),
            }
        }
    }

    fn cmp_value(self, other: Decimal) -> Option<std::cmp::Ordering> {
        let places = self.scale.max(other.scale);
        Some(self.scaled_floor(places)?.cmp(&other.scaled_floor(places)?))
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

/// The JSON number of a decimal's text: an integer when whole.
fn json_number(d: Decimal) -> Value {
    serde_json::from_str(&d.to_string()).unwrap_or(Value::Null)
}

fn decimal_entry(loc: &[Value], sent: &Value) -> Value {
    entry(
        "decimal_max_places",
        loc,
        format!("Decimal input should have no more than {MAX_NUMBER_DECIMALS} decimal places"),
        sent,
        Some(json!({"decimal_places": MAX_NUMBER_DECIMALS})),
    )
}

/// A grid of a number question, built in exact decimals from `min`, `max`
/// and `step` (defaults 0, 255 and 1), with the entries of the API's `422`
/// in its order, then its `400` for missing instructions.
fn check_number(
    id: &str,
    obj: &Map<String, Value>,
    q: &Value,
    loc: &[Value],
    entries: &mut Vec<Value>,
    deferred: &mut Vec<Error>,
    most: u64,
) {
    use std::cmp::Ordering::{Greater, Less};
    let mut values = [Decimal::int(0), Decimal::int(255), Decimal::int(1)];
    let mut ok = true;
    for (slot, name) in values.iter_mut().zip(["min", "max", "step"]) {
        let Some(sent) = obj.get(name).filter(|v| !v.is_null()) else { continue };
        let floc = with(loc, name);
        let Value::Number(n) = sent else {
            entries.push(entry("float_type", &floc, "Input should be a valid number", sent, None));
            ok = false;
            continue;
        };
        let parsed = Decimal::parse(n);
        let f = n.as_f64().unwrap_or(0.0);
        let above = parsed.map_or(f > 0.0, |d| d.cmp_value(Decimal::int(TWO_53)) == Some(Greater));
        let below = parsed.map_or(f < 0.0, |d| d.cmp_value(Decimal::int(-TWO_53)) == Some(Less));
        if name != "step" && below {
            entries.push(entry(
                "greater_than_equal",
                &floc,
                format!("Input should be greater than or equal to -{TWO_53}"),
                sent,
                Some(json!({"ge": -TWO_53})),
            ));
            ok = false;
            continue;
        }
        if name != "step" && above {
            entries.push(entry(
                "less_than_equal",
                &floc,
                format!("Input should be less than or equal to {TWO_53}"),
                sent,
                Some(json!({"le": TWO_53})),
            ));
            ok = false;
            continue;
        }
        if name == "step" && parsed.map_or(f <= 0.0, |d| d.mantissa <= 0) {
            entries.push(entry("greater_than", &floc, "Input should be greater than 0", sent, Some(json!({"gt": 0}))));
            ok = false;
            continue;
        }
        let Some(d) = parsed else {
            // Too precise to hold exactly: far over 6 decimals for min and
            // step; a max that small changes no grid.
            if name != "max" {
                entries.push(decimal_entry(&floc, sent));
                ok = false;
            }
            continue;
        };
        if name != "max" && d.scale > MAX_NUMBER_DECIMALS {
            entries.push(decimal_entry(&floc, sent));
            ok = false;
            continue;
        }
        *slot = d;
    }
    if !ok {
        return;
    }
    let [lo, hi, step] = values;
    let places = lo.scale.max(step.scale);
    if places > 0 {
        // Every value must fit 15 significant digits: 10^(15-d) - 10^-d.
        let bound = Decimal { mantissa: pow10(15).unwrap_or(0) - 1, scale: places };
        let negative = Decimal { mantissa: -bound.mantissa, scale: places };
        let text = bound.to_string();
        for (name, val) in [("min", lo), ("max", hi)] {
            let raw = obj.get(name).cloned().unwrap_or(Value::Null);
            if val.cmp_value(bound) == Some(Greater) {
                entries.push(entry(
                    "less_than_equal",
                    &with(loc, name),
                    format!("Input should be less than or equal to {text}"),
                    &raw,
                    Some(json!({"le": json_number(bound)})),
                ));
                return;
            }
            if val.cmp_value(negative) == Some(Less) {
                entries.push(entry(
                    "greater_than_equal",
                    &with(loc, name),
                    format!("Input should be greater than or equal to -{text}"),
                    &raw,
                    Some(json!({"ge": json_number(negative)})),
                ));
                return;
            }
        }
    }
    if lo.cmp_value(hi) == Some(Greater) {
        let max_sent = obj.get("max").filter(|v| !v.is_null()).cloned().unwrap_or(json!(255));
        let min_sent = obj.get("min").filter(|v| !v.is_null()).cloned().unwrap_or(json!(0));
        entries.push(entry(
            "less_than_equal",
            &with(loc, "min"),
            format!("Input should be less than or equal to {max_sent}"),
            &min_sent,
            Some(json!({"le": max_sent})),
        ));
        return;
    }
    let (Some(m), Some(big_m), Some(s)) = (lo.scaled_floor(places), hi.scaled_floor(places), step.scaled_floor(places))
    else {
        return;
    };
    let values = big_m.checked_sub(m).map_or(u128::MAX, |span| (span / s.max(1)) as u128 + 1);
    if values > u128::from(most) {
        let n = u64::try_from(values).unwrap_or(u64::MAX);
        entries.push(entry(
            "too_long",
            loc,
            format!("Grid should have at most {most} values, not {n}"),
            q,
            Some(json!({"field_type": "Grid", "max_length": most, "actual_length": n})),
        ));
        return;
    }
    if obj.get("instructions").is_none_or(|i| i.is_null() || i == "") {
        deferred.push(bad_request(None, format!("Number question must have instructions: {id}")));
    }
}

// --------------------------------------------------------------------------- sessions

/// The fixed rules of `POST /v1/sessions`: the nesting limit, `instructions`
/// and `state` of the three forms, `window` 1 to 64 and `ttl_seconds` 1 to
/// 3,600 (a held `sessions.max_window` or `max_ttl_seconds` replaces the
/// bound), every question rule, and a model that serves no sessions when the
/// client holds that.
#[cfg(feature = "sessions")]
pub(crate) fn check_session_open(body: &Map<String, Value>, depth: usize, limits: &Limits) -> Result<()> {
    check_nesting(depth)?;
    let mut entries = Vec::new();
    for name in ["instructions", "state"] {
        if let Some(v) = body.get(name).filter(|v| !v.is_null() && !is_text(v)) {
            text_entries(&[json!("body"), json!(name)], v, &mut entries);
        }
    }
    let bounds = [
        ("window", limits.count(&["sessions", "max_window"]).map_or(MAX_WINDOW, |n| n as i64)),
        ("ttl_seconds", limits.count(&["sessions", "max_ttl_seconds"]).map_or(MAX_TTL_SECONDS, |n| n as i64)),
    ];
    for (name, hi) in bounds {
        let Some(v) = body.get(name).filter(|v| !v.is_null()) else { continue };
        let loc = [json!("body"), json!(name)];
        match v.as_i64() {
            None => entries.push(entry("int_type", &loc, "Input should be a valid integer", v, None)),
            Some(n) if n < 1 => entries.push(entry(
                "greater_than_equal",
                &loc,
                "Input should be greater than or equal to 1",
                v,
                Some(json!({"ge": 1})),
            )),
            Some(n) if n > hi => entries.push(entry(
                "less_than_equal",
                &loc,
                format!("Input should be less than or equal to {hi}"),
                v,
                Some(json!({"le": hi})),
            )),
            Some(_) => {}
        }
    }
    let mut questions = Map::new();
    if let Some(q) = body.get("questions") {
        questions.insert("questions".into(), q.clone());
    }
    check_request(&questions, 0, Profile::Decide, limits, entries)?;
    if limits.is_false(&["sessions", "enabled"]) {
        return Err(usage_error(format!("Model {} does not serve sessions.", limits.model)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(q: Value) -> Result<()> {
        let body = json!({"questions": {"q": q}});
        let depth = nesting_depth(&body);
        check_request(body.as_object().unwrap(), depth, Profile::Decide, &Limits::default(), Vec::new())
    }

    fn kind(q: Value) -> Option<ErrorKind> {
        check(q).err().map(|e| {
            assert_eq!(e.status(), None);
            e.kind()
        })
    }

    fn entries(q: Value) -> Vec<Value> {
        let e = check(q).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation);
        assert_eq!(e.error_type(), None);
        e.entries().unwrap().to_vec()
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
        assert_eq!(json_number(Decimal { mantissa: 999_999_999_999_999, scale: 6 }), json!(999999999.999999));
    }

    #[test]
    fn grids() {
        let n = |extra: Value| {
            let mut q = json!({"type": "number", "instructions": "How many?"});
            q.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            kind(q)
        };
        assert_eq!(n(json!({})), None);
        assert_eq!(n(json!({"min": 0, "max": 4095})), None);
        assert_eq!(n(json!({"min": 0, "max": 4096})), Some(ErrorKind::Validation));
        assert_eq!(n(json!({"min": 0, "max": 100, "step": 0.1})), None);
        assert_eq!(n(json!({"min": 0, "max": 0.3, "step": 0.1})), None);
        assert_eq!(n(json!({"min": 5, "max": 5})), None);
        assert_eq!(n(json!({"min": -40, "max": 10})), None);
        assert_eq!(n(json!({"min": null, "max": null, "step": null})), None);
        assert_eq!(n(json!({"min": 1e300})), Some(ErrorKind::Validation));
        assert_eq!(n(json!({"step": 1e-300})), Some(ErrorKind::Validation));
    }

    #[test]
    fn grid_entries_are_the_apis() {
        let q = |extra: Value| {
            let mut q = json!({"type": "number", "instructions": "How many?"});
            q.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            entries(q)
        };
        let loc = |f: &str| json!(["body", "questions", "q", "number", f]);
        assert_eq!(
            q(json!({"min": 60, "max": 50})),
            vec![json!({"type": "less_than_equal", "loc": loc("min"),
                        "msg": "Input should be less than or equal to 50", "input": 60, "ctx": {"le": 50}})]
        );
        assert_eq!(
            q(json!({"step": 0})),
            vec![json!({"type": "greater_than", "loc": loc("step"), "msg": "Input should be greater than 0",
                        "input": 0, "ctx": {"gt": 0}})]
        );
        assert_eq!(
            q(json!({"min": "5"})),
            vec![
                json!({"type": "float_type", "loc": loc("min"), "msg": "Input should be a valid number", "input": "5"})
            ]
        );
        assert_eq!(q(json!({"min": 0.0000001}))[0]["type"], "decimal_max_places");
        assert_eq!(q(json!({"max": 9007199254740993u64}))[0]["ctx"], json!({"le": 9007199254740992u64}));
        let bound = q(json!({"min": 0, "max": 1000000000, "step": 0.000001}));
        assert_eq!(bound[0]["msg"], "Input should be less than or equal to 999999999.999999");
        let long = q(json!({"min": 0, "max": 1000, "step": 0.1}));
        assert_eq!(long[0]["type"], "too_long");
        assert_eq!(long[0]["loc"], json!(["body", "questions", "q", "number"]));
        assert_eq!(long[0]["ctx"], json!({"field_type": "Grid", "max_length": 4096, "actual_length": 10001}));
    }

    #[test]
    fn grid_faults_come_before_missing_instructions() {
        let e = check(json!({"type": "number", "min": 60, "max": 50})).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation);
        let e = check(json!({"type": "number", "min": 0, "max": 10})).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::BadRequest);
        assert_eq!(e.error_type(), None);
        assert_eq!(e.message(), "Number question must have instructions: q");
        assert!(check(json!({"type": "number", "instructions": ""})).is_err());
    }

    #[test]
    fn the_422_phase_runs_over_every_question_first() {
        let options: Map<String, Value> = (0..256).map(|i| (format!("o{i}"), Value::Null)).collect();
        let body = json!({"questions": {
            "a": {"type": "choice", "criteria": options},
            "b": {"type": "score", "criteria": []},
        }});
        let e =
            check_request(body.as_object().unwrap(), 3, Profile::Decide, &Limits::default(), Vec::new()).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation);
    }

    #[test]
    fn questions() {
        let options =
            |n: usize| -> Value { (0..n).map(|i| (format!("o{i}"), Value::Null)).collect::<Map<_, _>>().into() };
        let levels = |n: usize| -> Value { (0..n).map(|i| json!(i.to_string())).collect::<Vec<_>>().into() };
        assert_eq!(kind(json!({"type": "choice", "criteria": options(255)})), None);
        assert_eq!(kind(json!({"type": "choice", "criteria": options(256)})), Some(ErrorKind::BadRequest));
        assert_eq!(kind(json!({"type": "choice", "criteria": {}})), Some(ErrorKind::BadRequest));
        assert_eq!(kind(json!({"type": "choice"})), Some(ErrorKind::Validation));
        assert_eq!(kind(json!({"type": "choice", "criteria": ["a"]})), Some(ErrorKind::Validation));
        assert_eq!(kind(json!({"type": "score", "criteria": [1, 2]})), Some(ErrorKind::Validation));
        assert_eq!(kind(json!({"type": "score", "criteria": ["a", "b"]})), None);
        assert_eq!(kind(json!({"type": "score", "criteria": {"0": "a", "1": "b"}})), None);
        assert_eq!(kind(json!({"type": "score", "criteria": levels(11)})), Some(ErrorKind::BadRequest));
        assert_eq!(kind(json!({"type": "score", "criteria": []})), Some(ErrorKind::Validation));
        assert_eq!(kind(json!({"type": "truth"})), Some(ErrorKind::BadRequest));
        assert_eq!(kind(json!({"type": "truth", "criteria": "x"})), Some(ErrorKind::Validation));
        assert_eq!(kind(json!({"type": "truth", "criteria": {"false": "no"}})), None);
        assert_eq!(kind(json!({"type": "truth", "instructions": ""})), None);
        assert_eq!(kind(json!({"type": "truth", "instructions": 42})), Some(ErrorKind::Validation));
        assert_eq!(kind(json!({"instructions": "?"})), Some(ErrorKind::Validation));
        assert_eq!(kind(json!("q")), Some(ErrorKind::Validation));
        let e = check(json!({"type": "ranking"})).unwrap_err();
        assert_eq!(
            (e.kind(), e.error_type(), e.message()),
            (ErrorKind::BadRequest, Some("api_usage_error"), "Invalid request.")
        );
    }

    #[test]
    fn systemone_takes_three_types() {
        let body = json!({"questions": {"n": {"type": "number", "instructions": "?"},
                                        "s": {"type": "score", "criteria": {"0": "a"}}}});
        let e = check_request(body.as_object().unwrap(), 3, Profile::Systemone, &Limits::default(), Vec::new())
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Validation); // the score object, a 422, comes first
        let body = json!({"questions": {"n": {"type": "number", "instructions": "?"}}});
        let e = check_request(body.as_object().unwrap(), 3, Profile::Systemone, &Limits::default(), Vec::new())
            .unwrap_err();
        assert_eq!(e.message(), "Invalid request.");
    }

    #[test]
    fn nesting() {
        let mut v = json!("s");
        for _ in 0..127 {
            v = json!([v]);
        }
        assert_eq!(nesting_depth(&v), 127);
        assert!(check_nesting(128).is_ok());
        assert_eq!(check_nesting(129).unwrap_err().message(), "Invalid request.");
    }

    #[test]
    #[cfg(feature = "sessions")]
    fn held_limits() {
        let mut held = HashMap::new();
        let entry = json!({"number_max_grid_values": 0, "max_choice_options": 2,
                           "sessions": {"max_window": 4, "enabled": false}});
        held.insert("m".to_owned(), entry.as_object().unwrap().clone());
        let limits = Limits::of("m", &held);
        let body = json!({"questions": {"c": {"type": "choice", "criteria": {"a": null, "b": null, "c": null}}}});
        let e = check_request(body.as_object().unwrap(), 4, Profile::Decide, &limits, Vec::new()).unwrap_err();
        assert_eq!(e.message(), "Too many choices. Must have at most 2 choices.");
        let body = json!({"questions": {"n": {"type": "number", "instructions": "?"}}});
        let e = check_request(body.as_object().unwrap(), 3, Profile::Decide, &limits, Vec::new()).unwrap_err();
        assert_eq!(e.message(), "Model m does not answer number questions.");
        let open = json!({"window": 5, "questions": {"t": {"type": "truth", "instructions": "?"}}});
        let e = check_session_open(open.as_object().unwrap(), 3, &limits).unwrap_err();
        assert_eq!(e.entries().unwrap()[0]["ctx"], json!({"le": 4}));
        let open = json!({"questions": {"t": {"type": "truth", "instructions": "?"}}});
        let e = check_session_open(open.as_object().unwrap(), 3, &limits).unwrap_err();
        assert_eq!(e.message(), "Model m does not serve sessions.");
        assert!(check_session_open(open.as_object().unwrap(), 3, &Limits::default()).is_ok());
    }

    #[test]
    fn held_limits_by_alias_or_version() {
        let mut held = HashMap::new();
        let entry = json!({"name": "decisionnode-1.0", "version": "decisionnode-1.0", "alias": "decisionnode-latest",
                           "max_choice_options": 2});
        held.insert("decisionnode-1.0".to_owned(), entry.as_object().unwrap().clone());
        for name in ["decisionnode-1.0", "decisionnode-latest"] {
            assert_eq!(Limits::of(name, &held).count(&["max_choice_options"]), Some(2), "{name}");
        }
        assert_eq!(Limits::of("decisionnode-9", &held).count(&["max_choice_options"]), None);
    }
}
