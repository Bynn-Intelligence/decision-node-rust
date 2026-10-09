//! Comparisons used by the conformance runner.

use serde_json::Value;

/// A failed expectation, with what differed.
pub struct Fail(pub String);

/// Keys whose values compare whole instead of key by key.
const EXACT: [&str; 9] =
    ["probabilities", "legend", "raw", "entries", "safety", "body", "limits", "evidence", "boosters"];

/// Keys compared whole whose top-level key order must also be the order
/// expected: a decision's `evidence` (the request's order) and an error's
/// `boosters` (the header's).
const ORDERED: [&str; 2] = ["evidence", "boosters"];

/// True when two objects list the same keys in the same order (anything else
/// has no order to compare).
fn same_order(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => x.keys().eq(y.keys()),
        _ => true,
    }
}

/// JSON equality with numbers compared exactly as IEEE doubles and objects by
/// key, regardless of order.
pub fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => x == y,
        },
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(v, w)| same(v, w)),
        _ => a == b,
    }
}

/// Every key of `expected` must be in `actual` with the same value. Keys in
/// [`EXACT`] compare whole; other objects compare by the keys expected.
pub fn matches(expected: &Value, actual: &Value, path: &str) -> Result<(), Fail> {
    if let (Value::Object(e), Value::Object(a)) = (expected, actual) {
        for (k, v) in e {
            let Some(got) = a.get(k) else {
                return Err(Fail(format!("{path}.{k}: missing (expected {})", short(v))));
            };
            if EXACT.contains(&k.as_str()) {
                if !same(v, got) || (ORDERED.contains(&k.as_str()) && !same_order(v, got)) {
                    return Err(Fail(format!("{path}.{k}: expected {}, got {}", short(v), short(got))));
                }
            } else {
                matches(v, got, &format!("{path}.{k}"))?;
            }
        }
        return Ok(());
    }
    if same(expected, actual) {
        Ok(())
    } else {
        Err(Fail(format!("{path}: expected {}, got {}", short(expected), short(actual))))
    }
}

/// A value shortened for a failure message.
pub fn short(v: &Value) -> String {
    let text = v.to_string();
    if text.chars().count() < 160 {
        text
    } else {
        format!("{}...", text.chars().take(157).collect::<String>())
    }
}
