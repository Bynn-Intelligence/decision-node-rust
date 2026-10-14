//! Per-key usage: what each API key of the workspace used, by period, model
//! or channel.

use std::collections::BTreeMap;

use reqwest::Method;
use serde_json::Value;

use crate::check::usage_error;
use crate::client::{DecisionNode, Policy};
use crate::error::{Error, ErrorKind, Result};

/// The most keys a page of [`UsageReports::list`] holds.
const PAGE_MAX: i64 = 1000;
/// The most `key_id` filters one query takes.
const MAX_KEY_IDS: usize = 100;

/// The per-key usage of a client, from [`DecisionNode::usage`].
///
/// Every call returns the API's object exactly as sent and is retried as
/// every call is. The rules the API checks while it reads the query
/// (`granularity`, `group_by`, the label filters, `device_id`, the key ids,
/// `scope`, `status` and `limit`) are checked before anything is sent, with
/// the API's own error; the period's rules and the cursor are the API's.
#[derive(Clone, Copy, Debug)]
pub struct UsageReports<'a> {
    client: &'a DecisionNode,
}

impl DecisionNode {
    /// The workspace's usage, key by key.
    ///
    /// ```no_run
    /// # async fn run(dn: decisionnode::DecisionNode) -> decisionnode::Result<()> {
    /// use decisionnode::{UsagePeriod, UsageQuery};
    ///
    /// let month = dn.usage().key("key_00000000000000000000000000000d01", &UsagePeriod::new().granularity("day")).await?;
    /// let keys = dn
    ///     .usage()
    ///     .all(&UsageQuery::new("2026-09-01T00:00:00Z", "2026-10-01T00:00:00Z").label("customer", "acme"))
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn usage(&self) -> UsageReports<'_> {
        UsageReports { client: self }
    }
}

/// The period and the breakdown of one key's usage, for
/// [`UsageReports::key`]. Each part is sent only when set; without a period
/// the API reads the UTC month so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsagePeriod {
    start: Option<String>,
    end: Option<String>,
    granularity: Option<String>,
    group_by: Option<String>,
}

impl UsagePeriod {
    /// The UTC month so far, in one bucket.
    pub fn new() -> UsagePeriod {
        UsagePeriod::default()
    }

    /// The start of the period, an RFC 3339 time.
    pub fn start(mut self, start: impl Into<String>) -> UsagePeriod {
        self.start = Some(start.into());
        self
    }

    /// The end of the period, an RFC 3339 time.
    pub fn end(mut self, end: impl Into<String>) -> UsagePeriod {
        self.end = Some(end.into());
        self
    }

    /// The buckets: `none`, `hour`, `day` or `month`.
    pub fn granularity(mut self, granularity: impl Into<String>) -> UsagePeriod {
        self.granularity = Some(granularity.into());
        self
    }

    /// A breakdown by `model` or by `channel`.
    pub fn group_by(mut self, group_by: impl Into<String>) -> UsagePeriod {
        self.group_by = Some(group_by.into());
        self
    }
}

/// The keys and the period of [`UsageReports::list`] and
/// [`UsageReports::all`]. Each filter is sent only when set.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageQuery {
    period: UsagePeriod,
    labels: BTreeMap<String, String>,
    device_id: Option<String>,
    key_ids: Vec<String>,
    scope: Option<String>,
    status: Option<String>,
    include_empty: Option<bool>,
    limit: Option<i64>,
    cursor: Option<String>,
}

impl UsageQuery {
    /// The usage of every key over this period (RFC 3339 times).
    pub fn new(start: impl Into<String>, end: impl Into<String>) -> UsageQuery {
        UsageQuery {
            period: UsagePeriod::new().start(start).end(end),
            labels: BTreeMap::new(),
            device_id: None,
            key_ids: Vec::new(),
            scope: None,
            status: None,
            include_empty: None,
            limit: None,
            cursor: None,
        }
    }

    /// The buckets: `none`, `hour`, `day` or `month`.
    pub fn granularity(mut self, granularity: impl Into<String>) -> UsageQuery {
        self.period = self.period.granularity(granularity);
        self
    }

    /// A breakdown by `model` or by `channel`.
    pub fn group_by(mut self, group_by: impl Into<String>) -> UsageQuery {
        self.period = self.period.group_by(group_by);
        self
    }

    /// Only the keys carrying this label (sent as `label=name:value`).
    pub fn label(mut self, name: impl Into<String>, value: impl Into<String>) -> UsageQuery {
        self.labels.insert(name.into(), value.into());
        self
    }

    /// Only the keys of this device.
    pub fn device_id(mut self, device_id: impl Into<String>) -> UsageQuery {
        self.device_id = Some(device_id.into());
        self
    }

    /// Only this key; add up to 100.
    pub fn key_id(mut self, key_id: impl Into<String>) -> UsageQuery {
        self.key_ids.push(key_id.into());
        self
    }

    /// Only these keys, up to 100.
    pub fn key_ids<I: IntoIterator<Item = S>, S: Into<String>>(mut self, key_ids: I) -> UsageQuery {
        self.key_ids.extend(key_ids.into_iter().map(Into::into));
        self
    }

    /// Only the keys of this kind: `full`, `decide` or `keys`.
    pub fn scope(mut self, scope: impl Into<String>) -> UsageQuery {
        self.scope = Some(scope.into());
        self
    }

    /// Only the keys in this state: `active`, `expired`, `revoked` or `all`.
    pub fn status(mut self, status: impl Into<String>) -> UsageQuery {
        self.status = Some(status.into());
        self
    }

    /// Whether keys that used nothing over the period are listed too.
    pub fn include_empty(mut self, include_empty: bool) -> UsageQuery {
        self.include_empty = Some(include_empty);
        self
    }

    /// Keys per page, 1 to 1000. A page can hold fewer.
    pub fn limit(mut self, limit: i64) -> UsageQuery {
        self.limit = Some(limit);
        self
    }

    /// The page after the one whose `next_cursor` this is.
    pub fn cursor(mut self, cursor: impl Into<String>) -> UsageQuery {
        self.cursor = Some(cursor.into());
        self
    }
}

/// The period's query parameters, `granularity` and `group_by` checked as
/// the API checks them.
fn period_query(p: &UsagePeriod) -> Result<Vec<(&'static str, String)>> {
    if p.granularity.as_deref().is_some_and(|g| !matches!(g, "none" | "hour" | "day" | "month")) {
        return Err(usage_error("granularity is none, hour, day or month."));
    }
    if p.group_by.as_deref().is_some_and(|g| !matches!(g, "model" | "channel")) {
        return Err(usage_error("group_by is model or channel."));
    }
    Ok([("start", &p.start), ("end", &p.end), ("granularity", &p.granularity), ("group_by", &p.group_by)]
        .into_iter()
        .filter_map(|(name, value)| value.clone().map(|v| (name, v)))
        .collect())
}

/// A label name: 1 to 32 of `a-z`, `0-9`, `_`, `.` and `-`.
fn label_name_ok(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
}

/// A key's id: `key_` and 32 lowercase hex digits.
fn key_id_ok(id: &str) -> bool {
    id.strip_prefix("key_")
        .is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// The list's query parameters, checked as the API checks them, in its order.
/// The label filters, by name, each checked as the API checks it.
pub(crate) fn label_filters(labels: &BTreeMap<String, String>, out: &mut Vec<(&'static str, String)>) -> Result<()> {
    for (name, value) in labels {
        let length = value.chars().count();
        if !label_name_ok(name) || !(1..=64).contains(&length) || value.chars().any(char::is_control) {
            return Err(usage_error("A label filter is name:value, as labels are written."));
        }
        out.push(("label", format!("{name}:{value}")));
    }
    Ok(())
}

/// A `device_id` filter, checked as the API checks it.
pub(crate) fn device_filter(device_id: &str) -> Result<()> {
    let d = device_id;
    if !(1..=128).contains(&d.len()) || !d.bytes().all(|b| (0x20..=0x7e).contains(&b)) || d.starts_with(' ') {
        return Err(usage_error("device_id is 1 to 128 printable ASCII characters, not starting with a space."));
    }
    Ok(())
}

/// A `status` filter, checked as the API checks it.
pub(crate) fn status_filter(status: &str) -> Result<()> {
    if !matches!(status, "active" | "expired" | "revoked" | "all") {
        return Err(usage_error("status is active, expired, revoked or all."));
    }
    Ok(())
}

fn list_query(q: &UsageQuery) -> Result<Vec<(&'static str, String)>> {
    let mut out = period_query(&q.period)?;
    label_filters(&q.labels, &mut out)?;
    if let Some(d) = &q.device_id {
        device_filter(d)?;
        out.push(("device_id", d.clone()));
    }
    for (i, id) in q.key_ids.iter().enumerate() {
        if !key_id_ok(id) {
            return Err(usage_error("key_id is a key's id, key_ and 32 hex digits."));
        }
        if i == MAX_KEY_IDS {
            return Err(usage_error("At most 100 key_id filters."));
        }
        out.push(("key_id", id.clone()));
    }
    if let Some(s) = &q.scope {
        if !matches!(s.as_str(), "full" | "decide" | "keys") {
            return Err(usage_error("scope is full, decide or keys."));
        }
        out.push(("scope", s.clone()));
    }
    if let Some(s) = &q.status {
        status_filter(s)?;
        out.push(("status", s.clone()));
    }
    if let Some(e) = q.include_empty {
        out.push(("include_empty", e.to_string()));
    }
    if let Some(l) = q.limit {
        if !(1..=PAGE_MAX).contains(&l) {
            return Err(usage_error("limit is a whole number from 1 to 1000."));
        }
        out.push(("limit", l.to_string()));
    }
    if let Some(c) = &q.cursor {
        out.push(("cursor", c.clone()));
    }
    Ok(out)
}

/// Every byte but the unreserved characters written as `%XX`.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl UsageReports<'_> {
    async fn get(&self, path: &str, query: &[(&'static str, String)]) -> Result<Value> {
        let mut path = path.to_owned();
        if !query.is_empty() {
            let pairs: Vec<String> = query.iter().map(|(k, v)| format!("{k}={}", escape(v))).collect();
            path = format!("{path}?{}", pairs.join("&"));
        }
        Ok(self.client.request_json(Method::GET, &path, None, Policy::Default).await?.body)
    }

    /// `GET /v1/keys/{key_id}/usage`: one key's usage over the period, as
    /// sent. A key outside the caller's set is the API's `404`.
    pub async fn key(&self, key_id: &str, period: &UsagePeriod) -> Result<Value> {
        let query = period_query(period)?;
        self.get(&format!("/v1/keys/{}/usage", escape(key_id)), &query).await
    }

    /// `GET /v1/usage`: one page of the keys' usage, as sent: `period`,
    /// `as_of`, `items` and `next_cursor`.
    pub async fn list(&self, query: &UsageQuery) -> Result<Value> {
        let q = list_query(query)?;
        self.get("/v1/usage", &q).await
    }

    /// Every key's usage over the period: the pages of
    /// [`UsageReports::list`], `next_cursor` followed until it is `null`,
    /// their `items` in order. A page can hold fewer keys than its limit. A
    /// cursor set on the query is not used: this starts at the first page.
    pub async fn all(&self, query: &UsageQuery) -> Result<Vec<Value>> {
        let mut query = query.clone();
        query.cursor = None;
        let mut items = Vec::new();
        loop {
            let page = self.list(&query).await?;
            match page.get("items") {
                Some(Value::Array(list)) => items.extend(list.iter().cloned()),
                None | Some(Value::Null) => {}
                Some(_) => return Err(Error::new(ErrorKind::Server, "the response is not a valid usage page")),
            }
            match page.get("next_cursor").and_then(Value::as_str).filter(|c| !c.is_empty()) {
                Some(next) => query.cursor = Some(next.to_owned()),
                None => return Ok(items),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(r: Result<Vec<(&'static str, String)>>) -> String {
        let e = r.unwrap_err();
        assert_eq!((e.kind(), e.status(), e.error_type()), (ErrorKind::BadRequest, None, Some("api_usage_error")));
        e.message().to_owned()
    }

    #[test]
    fn the_period() {
        assert!(period_query(&UsagePeriod::new()).unwrap().is_empty());
        let q = period_query(&UsagePeriod::new().start("a").granularity("day").group_by("channel")).unwrap();
        assert_eq!(q, [("start", "a".into()), ("granularity", "day".into()), ("group_by", "channel".into())]);
        assert_eq!(
            message(period_query(&UsagePeriod::new().granularity("week"))),
            "granularity is none, hour, day or month."
        );
        assert_eq!(message(period_query(&UsagePeriod::new().group_by("device"))), "group_by is model or channel.");
    }

    #[test]
    fn the_list_query() {
        let base = || UsageQuery::new("s", "e");
        let id = |n: usize| format!("key_{n:032x}");
        let q = list_query(
            &base()
                .label("plan", "pro")
                .label("customer", "acme")
                .key_ids([id(1), id(2)])
                .include_empty(false)
                .limit(5),
        )
        .unwrap();
        let names: Vec<&str> = q.iter().map(|(k, _)| *k).collect();
        assert_eq!(names, ["start", "end", "label", "label", "key_id", "key_id", "include_empty", "limit"]);
        // Labels by name.
        assert_eq!((q[2].1.as_str(), q[3].1.as_str()), ("customer:acme", "plan:pro"));
        let label = "A label filter is name:value, as labels are written.";
        assert_eq!(message(list_query(&base().label("Customer", "acme"))), label);
        assert_eq!(message(list_query(&base().label("customer", ""))), label);
        assert_eq!(message(list_query(&base().label("customer", "a\u{7}"))), label);
        assert_eq!(
            message(list_query(&base().device_id(" drone-1"))),
            "device_id is 1 to 128 printable ASCII characters, not starting with a space."
        );
        assert_eq!(message(list_query(&base().key_id("key_ABC"))), "key_id is a key's id, key_ and 32 hex digits.");
        assert_eq!(message(list_query(&base().key_ids((0..101).map(id)))), "At most 100 key_id filters.");
        assert!(list_query(&base().key_ids((0..100).map(id))).is_ok());
        assert_eq!(message(list_query(&base().scope("publishable"))), "scope is full, decide or keys.");
        assert_eq!(message(list_query(&base().status("deleted"))), "status is active, expired, revoked or all.");
        assert_eq!(message(list_query(&base().limit(0))), "limit is a whole number from 1 to 1000.");
        assert_eq!(message(list_query(&base().limit(1001))), "limit is a whole number from 1 to 1000.");
    }

    #[test]
    fn values_are_escaped() {
        assert_eq!(escape("customer:acme"), "customer%3Aacme");
        assert_eq!(escape("key_a/b"), "key_a%2Fb");
        assert_eq!(escape("2026-09-03T11:30:00+02:00"), "2026-09-03T11%3A30%3A00%2B02%3A00");
    }
}
