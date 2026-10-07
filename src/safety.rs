//! The safety check result carried by every decision on the hosted API.

use indexmap::IndexMap;

/// What the hosted API's safety check found for a request, parsed from the
/// `x-decisionnode-safety` response header.
///
/// A self-hosted server sends no such header, and [`crate::Decision::safety`]
/// is then `None`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Safety {
    /// Whether the check ran. When it did not, [`Safety::reason`] says why.
    pub checked: bool,
    /// The probability that the request asks to choose, rank or target people
    /// to be harmed.
    pub harm: Option<f64>,
    /// The probability that the request asks for help to harm oneself.
    pub self_harm: Option<f64>,
    /// The categories above the key's thresholds, in header order. A category
    /// that caused a refusal is listed too.
    pub flagged: Vec<String>,
    /// Why the check did not run, for example `no-model-question`.
    pub reason: Option<String>,
    /// Every category in the header with its probability, `harm` and
    /// `self_harm` included, so a category added later is not lost.
    pub categories: IndexMap<String, f64>,
}

impl Safety {
    /// Parses the value of an `x-decisionnode-safety` header: a list of
    /// `name=value` members with optional `;parameters`, or
    /// `unchecked;reason="<code>"`.
    pub fn parse(value: &str) -> Safety {
        let value = value.trim();
        if value.starts_with("unchecked") {
            let reason = value
                .split(';')
                .skip(1)
                .filter_map(|p| p.trim().split_once('='))
                .find(|(k, _)| k.trim() == "reason")
                .map(|(_, v)| v.trim().trim_matches('"').to_owned());
            return Safety {
                checked: false,
                harm: None,
                self_harm: None,
                flagged: Vec::new(),
                reason,
                categories: IndexMap::new(),
            };
        }

        let mut safety = Safety {
            checked: true,
            harm: None,
            self_harm: None,
            flagged: Vec::new(),
            reason: None,
            categories: IndexMap::new(),
        };
        for member in value.split(',') {
            let member = member.trim();
            if member.is_empty() {
                continue;
            }
            let mut parts = member.split(';');
            let head = parts.next().unwrap_or_default();
            let (name, number) = match head.split_once('=') {
                Some((n, v)) => (n.trim(), v.trim().parse::<f64>().ok()),
                None => (head.trim(), None),
            };
            if let Some(n) = number {
                safety.categories.insert(name.to_owned(), n);
            }
            match name {
                "harm" => safety.harm = number,
                "self_harm" => safety.self_harm = number,
                _ => {}
            }
            if parts.any(|p| matches!(p.trim(), "flagged" | "blocked")) {
                safety.flagged.push(name.to_owned());
            }
        }
        safety
    }

    /// True when a category was flagged.
    pub fn is_flagged(&self) -> bool {
        !self.flagged.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scores() {
        let s = Safety::parse("harm=0.031, self_harm=0.004");
        assert!(s.checked);
        assert_eq!(s.harm, Some(0.031));
        assert_eq!(s.self_harm, Some(0.004));
        assert!(s.flagged.is_empty());
        assert_eq!(s.reason, None);
    }

    #[test]
    fn flagged_and_blocked_are_listed_in_order() {
        let s = Safety::parse("harm=0.912;flagged, self_harm=0.951;blocked");
        assert_eq!(s.flagged, vec!["harm", "self_harm"]);
        assert!(s.is_flagged());
    }

    #[test]
    fn unchecked_with_reason() {
        let s = Safety::parse("unchecked;reason=\"no-model-question\"");
        assert!(!s.checked);
        assert_eq!(s.reason.as_deref(), Some("no-model-question"));
        assert_eq!(s.harm, None);
    }

    #[test]
    fn keeps_unknown_categories() {
        let s = Safety::parse("harm=0.1, self_harm=0.2, fraud=0.3;flagged");
        assert_eq!(s.categories.get("fraud"), Some(&0.3));
        assert_eq!(s.flagged, vec!["fraud"]);
    }
}
