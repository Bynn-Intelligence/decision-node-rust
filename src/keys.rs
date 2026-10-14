//! Device keys: a "manage device keys" key creates, lists, changes and
//! revokes the workspace's decisions-only keys, for example one per device.

use std::collections::BTreeMap;

use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::check::usage_error;
use crate::client::{new_idempotency_key, DecisionNode, Policy};
use crate::error::{Error, ErrorKind, Result};
use crate::usage::{device_filter, escape, label_filters, status_filter};

/// The largest page of [`Keys::list`].
const PAGE_MAX: i64 = 200;

/// The device keys of a client, from [`DecisionNode::keys`].
///
/// Every call returns the API's object exactly as sent. `create` sends an
/// `Idempotency-Key` (the one set on the [`NewKey`], else a new random UUID,
/// the same on every attempt), so a retried create makes one key and answers
/// its secret again; it retries as every call does and also on a `409`
/// `idempotency_in_progress`. The secret is in the answer only: the client
/// never logs it. The other calls retry as every call does.
#[derive(Clone, Copy, Debug)]
pub struct Keys<'a> {
    client: &'a DecisionNode,
}

impl DecisionNode {
    /// The workspace's device keys, managed with a "manage device keys" key.
    ///
    /// ```no_run
    /// # async fn run(dn: decisionnode::DecisionNode) -> decisionnode::Result<()> {
    /// use decisionnode::{KeyQuery, NewKey};
    ///
    /// let made = dn.keys().create(&NewKey::new("Door 7").device_id("door-7").label("site", "berlin")).await?;
    /// let secret = made["secret"].as_str().unwrap_or_default(); // shown once: hand it to the device
    /// let berlin = dn.keys().all(&KeyQuery::new().label("site", "berlin")).await?;
    /// # let _ = (secret, berlin);
    /// # Ok(())
    /// # }
    /// ```
    pub fn keys(&self) -> Keys<'_> {
        Keys { client: self }
    }
}

/// A decisions-only key to create, for [`Keys::create`]. Each field is sent
/// only when set.
#[derive(Clone, Debug, PartialEq)]
pub struct NewKey {
    name: String,
    device_id: Option<String>,
    labels: Option<BTreeMap<String, String>>,
    requests_per_second: Option<i64>,
    tokens_per_second: Option<i64>,
    daily_spend_cap: Option<f64>,
    expires_at: Option<String>,
    may_search_collections: Option<bool>,
    idempotency_key: Option<String>,
}

impl NewKey {
    /// A key with this name (1 to 48 letters, digits, spaces, dots, dashes
    /// and underscores, unique among the workspace's active keys).
    pub fn new(name: impl Into<String>) -> NewKey {
        NewKey {
            name: name.into(),
            device_id: None,
            labels: None,
            requests_per_second: None,
            tokens_per_second: None,
            daily_spend_cap: None,
            expires_at: None,
            may_search_collections: None,
            idempotency_key: None,
        }
    }

    /// The device it is for, unique among the workspace's active keys; set
    /// once, never changed.
    pub fn device_id(mut self, device_id: impl Into<String>) -> NewKey {
        self.device_id = Some(device_id.into());
        self
    }

    /// A label (up to 16), to find and revoke keys by.
    pub fn label(mut self, name: impl Into<String>, value: impl Into<String>) -> NewKey {
        self.labels.get_or_insert_with(BTreeMap::new).insert(name.into(), value.into());
        self
    }

    /// Its own rate limit, below the workspace's.
    pub fn requests_per_second(mut self, n: i64) -> NewKey {
        self.requests_per_second = Some(n);
        self
    }

    /// Its own token rate limit, below the workspace's.
    pub fn tokens_per_second(mut self, n: i64) -> NewKey {
        self.tokens_per_second = Some(n);
        self
    }

    /// What it may spend a day, in USD (sent as `{"usd": ...}`).
    pub fn daily_spend_cap(mut self, usd: f64) -> NewKey {
        self.daily_spend_cap = Some(usd);
        self
    }

    /// When it expires, an RFC 3339 time.
    pub fn expires_at(mut self, at: impl Into<String>) -> NewKey {
        self.expires_at = Some(at.into());
        self
    }

    /// Whether it may use the `face_recognition` booster.
    pub fn may_search_collections(mut self, may: bool) -> NewKey {
        self.may_search_collections = Some(may);
        self
    }

    /// The `Idempotency-Key` to send (1 to 255 printable ASCII characters),
    /// instead of a new random one: a create retried with it later answers
    /// the same key and secret.
    pub fn idempotency_key(mut self, key: impl Into<String>) -> NewKey {
        self.idempotency_key = Some(key.into());
        self
    }

    fn body(&self) -> Value {
        let mut b = Map::new();
        b.insert("name".into(), json!(self.name));
        if let Some(d) = &self.device_id {
            b.insert("device_id".into(), json!(d));
        }
        if let Some(l) = &self.labels {
            b.insert("labels".into(), json!(l));
        }
        if let Some(limits) = limits(self.requests_per_second, self.tokens_per_second) {
            b.insert("limits".into(), limits);
        }
        if let Some(usd) = self.daily_spend_cap {
            b.insert("daily_spend_cap".into(), json!({"usd": usd}));
        }
        if let Some(at) = &self.expires_at {
            b.insert("expires_at".into(), json!(at));
        }
        if let Some(may) = self.may_search_collections {
            b.insert("may_search_collections".into(), json!(may));
        }
        Value::Object(b)
    }
}

/// A key's own limits, when either is set.
fn limits(requests_per_second: Option<i64>, tokens_per_second: Option<i64>) -> Option<Value> {
    let mut l = Map::new();
    if let Some(n) = requests_per_second {
        l.insert("requests_per_second".into(), json!(n));
    }
    if let Some(n) = tokens_per_second {
        l.insert("tokens_per_second".into(), json!(n));
    }
    (!l.is_empty()).then_some(Value::Object(l))
}

/// A change to a key, for [`Keys::update`]: only the fields set are sent;
/// the `clear_` forms send `null`, which removes the setting.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyUpdate {
    name: Option<String>,
    labels: Option<Option<BTreeMap<String, String>>>,
    limits: Option<Option<(Option<i64>, Option<i64>)>>,
    daily_spend_cap: Option<Option<f64>>,
    expires_at: Option<Option<String>>,
    may_search_collections: Option<bool>,
}

impl KeyUpdate {
    /// A change of nothing yet; set at least one field.
    pub fn new() -> KeyUpdate {
        KeyUpdate::default()
    }

    /// A new name.
    pub fn name(mut self, name: impl Into<String>) -> KeyUpdate {
        self.name = Some(name.into());
        self
    }

    /// New labels, which replace the key's whole set.
    pub fn labels<I, K, V>(mut self, labels: I) -> KeyUpdate
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.labels = Some(Some(labels.into_iter().map(|(k, v)| (k.into(), v.into())).collect()));
        self
    }

    /// Removes every label.
    pub fn clear_labels(mut self) -> KeyUpdate {
        self.labels = Some(None);
        self
    }

    /// New own limits; a `None` part is left out of the object.
    pub fn limits(mut self, requests_per_second: Option<i64>, tokens_per_second: Option<i64>) -> KeyUpdate {
        self.limits = Some(Some((requests_per_second, tokens_per_second)));
        self
    }

    /// Removes its own limits: the workspace's apply.
    pub fn clear_limits(mut self) -> KeyUpdate {
        self.limits = Some(None);
        self
    }

    /// A new daily spend cap, in USD.
    pub fn daily_spend_cap(mut self, usd: f64) -> KeyUpdate {
        self.daily_spend_cap = Some(Some(usd));
        self
    }

    /// Removes the daily spend cap.
    pub fn clear_daily_spend_cap(mut self) -> KeyUpdate {
        self.daily_spend_cap = Some(None);
        self
    }

    /// A new expiry, an RFC 3339 time.
    pub fn expires_at(mut self, at: impl Into<String>) -> KeyUpdate {
        self.expires_at = Some(Some(at.into()));
        self
    }

    /// Removes the expiry: the key no longer expires.
    pub fn clear_expires_at(mut self) -> KeyUpdate {
        self.expires_at = Some(None);
        self
    }

    /// Whether it may use the `face_recognition` booster.
    pub fn may_search_collections(mut self, may: bool) -> KeyUpdate {
        self.may_search_collections = Some(may);
        self
    }

    fn body(&self) -> Map<String, Value> {
        let mut b = Map::new();
        if let Some(n) = &self.name {
            b.insert("name".into(), json!(n));
        }
        if let Some(l) = &self.labels {
            b.insert("labels".into(), l.as_ref().map_or(Value::Null, |l| json!(l)));
        }
        if let Some(l) = &self.limits {
            let object = l.map(|(rps, tps)| limits(rps, tps).unwrap_or_else(|| json!({})));
            b.insert("limits".into(), object.unwrap_or(Value::Null));
        }
        if let Some(c) = &self.daily_spend_cap {
            b.insert("daily_spend_cap".into(), c.map_or(Value::Null, |usd| json!({"usd": usd})));
        }
        if let Some(at) = &self.expires_at {
            b.insert("expires_at".into(), at.as_ref().map_or(Value::Null, |a| json!(a)));
        }
        if let Some(may) = self.may_search_collections {
            b.insert("may_search_collections".into(), json!(may));
        }
        b
    }
}

/// Which keys [`Keys::list`] and [`Keys::all`] read. Each filter is sent
/// only when set; without a status the API lists the active keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyQuery {
    labels: BTreeMap<String, String>,
    device_id: Option<String>,
    status: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

impl KeyQuery {
    /// The active keys, the API's page size.
    pub fn new() -> KeyQuery {
        KeyQuery::default()
    }

    /// Only the keys carrying this label (sent as `label=name:value`).
    pub fn label(mut self, name: impl Into<String>, value: impl Into<String>) -> KeyQuery {
        self.labels.insert(name.into(), value.into());
        self
    }

    /// Only the key of this device.
    pub fn device_id(mut self, device_id: impl Into<String>) -> KeyQuery {
        self.device_id = Some(device_id.into());
        self
    }

    /// Only the keys in this state: `active`, `expired`, `revoked` or `all`.
    pub fn status(mut self, status: impl Into<String>) -> KeyQuery {
        self.status = Some(status.into());
        self
    }

    /// Keys per page, 1 to 200.
    pub fn limit(mut self, limit: i64) -> KeyQuery {
        self.limit = Some(limit);
        self
    }

    /// The page after the one whose `next_cursor` this is.
    pub fn cursor(mut self, cursor: impl Into<String>) -> KeyQuery {
        self.cursor = Some(cursor.into());
        self
    }

    fn query(&self) -> Result<Vec<(&'static str, String)>> {
        let mut out = Vec::new();
        label_filters(&self.labels, &mut out)?;
        if let Some(d) = &self.device_id {
            device_filter(d)?;
            out.push(("device_id", d.clone()));
        }
        if let Some(s) = &self.status {
            status_filter(s)?;
            out.push(("status", s.clone()));
        }
        if let Some(l) = self.limit {
            if !(1..=PAGE_MAX).contains(&l) {
                return Err(usage_error("limit is a whole number from 1 to 200."));
            }
            out.push(("limit", l.to_string()));
        }
        if let Some(c) = &self.cursor {
            out.push(("cursor", c.clone()));
        }
        Ok(out)
    }
}

/// The keys [`Keys::revoke_by_labels`] revokes: the active keys every label
/// matches, and one of the device ids when any are set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyRevocation {
    labels: BTreeMap<String, String>,
    device_ids: Option<Vec<String>>,
    dry_run: Option<bool>,
}

impl KeyRevocation {
    /// Revokes nothing yet: add at least one label.
    pub fn new() -> KeyRevocation {
        KeyRevocation::default()
    }

    /// A label every revoked key carries.
    pub fn label(mut self, name: impl Into<String>, value: impl Into<String>) -> KeyRevocation {
        self.labels.insert(name.into(), value.into());
        self
    }

    /// Only the keys of these devices (up to 1,000).
    pub fn device_ids<I: IntoIterator<Item = S>, S: Into<String>>(mut self, device_ids: I) -> KeyRevocation {
        self.device_ids.get_or_insert_with(Vec::new).extend(device_ids.into_iter().map(Into::into));
        self
    }

    /// Counts the keys it would revoke (`would_revoke`) without revoking any.
    pub fn dry_run(mut self, dry_run: bool) -> KeyRevocation {
        self.dry_run = Some(dry_run);
        self
    }
}

impl Keys<'_> {
    async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        Ok(self.client.request_json(method, path, body, Policy::Default).await?.body)
    }

    /// `POST /v1/keys`: creates a decisions-only key and returns `{key,
    /// secret}` as sent. The secret is shown this once.
    pub async fn create(&self, key: &NewKey) -> Result<Value> {
        let idempotency = key.idempotency_key.clone().unwrap_or_else(new_idempotency_key);
        Ok(self.client.request_json_with_key(Method::POST, "/v1/keys", &key.body(), &idempotency).await?.body)
    }

    /// `GET /v1/keys`: one page of keys, as sent: `items` and `next_cursor`.
    /// The label, `device_id`, `status` and `limit` filters are checked
    /// before anything is sent, with the API's own error.
    pub async fn list(&self, query: &KeyQuery) -> Result<Value> {
        let q = query.query()?;
        let mut path = "/v1/keys".to_owned();
        if !q.is_empty() {
            let pairs: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", escape(v))).collect();
            path = format!("{path}?{}", pairs.join("&"));
        }
        self.call(Method::GET, &path, None).await
    }

    /// Every key the query picks: the pages of [`Keys::list`], `next_cursor`
    /// followed until it is `null`, their `items` in order. A cursor set on
    /// the query is not used: this starts at the first page.
    pub async fn all(&self, query: &KeyQuery) -> Result<Vec<Value>> {
        let mut query = query.clone();
        query.cursor = None;
        let mut items = Vec::new();
        loop {
            let page = self.list(&query).await?;
            match page.get("items") {
                Some(Value::Array(list)) => items.extend(list.iter().cloned()),
                None | Some(Value::Null) => {}
                Some(_) => return Err(Error::new(ErrorKind::Server, "the response is not a valid key page")),
            }
            match page.get("next_cursor").and_then(Value::as_str).filter(|c| !c.is_empty()) {
                Some(next) => query.cursor = Some(next.to_owned()),
                None => return Ok(items),
            }
        }
    }

    /// `PATCH /v1/keys/{key_id}`: changes a key and returns it as sent.
    /// A change of nothing is refused before anything is sent, with the
    /// API's own error.
    pub async fn update(&self, key_id: &str, update: &KeyUpdate) -> Result<Value> {
        let body = update.body();
        if body.is_empty() {
            return Err(usage_error("Nothing to change."));
        }
        self.call(Method::PATCH, &format!("/v1/keys/{}", escape(key_id)), Some(&Value::Object(body))).await
    }

    /// `DELETE /v1/keys/{key_id}`: revokes a key; returns `{id, revoked}` as
    /// sent. A key revoked before is the API's `409` `already_revoked`.
    pub async fn revoke(&self, key_id: &str) -> Result<Value> {
        self.call(Method::DELETE, &format!("/v1/keys/{}", escape(key_id)), None).await
    }

    /// `POST /v1/keys/revoke`: revokes up to 10,000 active keys the labels
    /// (and device ids) pick; returns `{revoked, more}` as sent, or
    /// `{would_revoke, more}` for a dry run. Call again while `more` is true.
    pub async fn revoke_by_labels(&self, revocation: &KeyRevocation) -> Result<Value> {
        let mut b = Map::new();
        b.insert("labels".into(), json!(revocation.labels));
        if let Some(ids) = &revocation.device_ids {
            b.insert("device_ids".into(), json!(ids));
        }
        if let Some(d) = revocation.dry_run {
            b.insert("dry_run".into(), json!(d));
        }
        self.call(Method::POST, "/v1/keys/revoke", Some(&Value::Object(b))).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One request the stub received: method, path with query, the
    /// `Idempotency-Key` and the body.
    #[derive(Clone, Debug)]
    struct Seen {
        line: String,
        key: Option<String>,
        body: Value,
    }

    /// A stub API answering each call with a fixed body; the first create
    /// is answered `409` `idempotency_in_progress`, so it is retried.
    async fn stub() -> (DecisionNode, Arc<Mutex<Vec<Seen>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let mut creates = 0;
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut raw = Vec::new();
                let mut buf = vec![0u8; 65536];
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).to_string();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
                    let length = head
                        .lines()
                        .find_map(|l| l.to_lowercase().strip_prefix("content-length: ").map(str::to_owned))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if n == 0 || body.len() >= length {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&raw).to_string();
                let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                let line = head.lines().next().unwrap_or("").rsplit_once(' ').map(|(l, _)| l.to_owned()).unwrap();
                let key =
                    head.lines().find_map(|l| l.to_lowercase().strip_prefix("idempotency-key: ").map(str::to_owned));
                log.lock().unwrap().push(Seen {
                    line: line.clone(),
                    key,
                    body: serde_json::from_str(body).unwrap_or(Value::Null),
                });
                let (status, answer) = if line.starts_with("POST /v1/keys/revoke") {
                    ("200 OK", r#"{"revoked":2,"more":false}"#.to_owned())
                } else if line.starts_with("POST /v1/keys") {
                    creates += 1;
                    if creates == 1 {
                        (
                            "409 Conflict",
                            r#"{"detail":{"error_type":"idempotency_in_progress","message":"m"}}"#.to_owned(),
                        )
                    } else {
                        ("201 Created", r#"{"key":{"id":"key_1","name":"Door 7"},"secret":"dn_live_x"}"#.to_owned())
                    }
                } else if line.starts_with("GET /v1/keys") {
                    if line.contains("cursor=") {
                        ("200 OK", r#"{"items":[{"id":"key_2"}],"next_cursor":null}"#.to_owned())
                    } else {
                        ("200 OK", r#"{"items":[{"id":"key_1"}],"next_cursor":"c1"}"#.to_owned())
                    }
                } else if line.starts_with("PATCH") {
                    ("200 OK", r#"{"id":"key_1","name":"Door 8"}"#.to_owned())
                } else {
                    ("200 OK", r#"{"id":"key_1","revoked":true}"#.to_owned())
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nretry-after-ms: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        let dn = DecisionNode::builder()
            .api_key("k")
            .base_url(format!("http://{addr}"))
            .sleeper(|_| Box::pin(async {}))
            .build()
            .unwrap();
        (dn, seen)
    }

    #[tokio::test]
    async fn every_call() {
        let (dn, seen) = stub().await;
        let k = dn.keys();
        let new = NewKey::new("Door 7")
            .device_id("door-7")
            .label("site", "berlin")
            .requests_per_second(5)
            .daily_spend_cap(2.5)
            .may_search_collections(false);
        let made = k.create(&new).await.unwrap();
        assert_eq!(made["secret"], "dn_live_x");
        let creates: Vec<Seen> = seen.lock().unwrap().drain(..).collect();
        // Retried after the 409, with one key on both attempts.
        assert_eq!(creates.len(), 2);
        let key = creates[0].key.clone().unwrap();
        assert_eq!(creates[1].key.as_deref(), Some(key.as_str()));
        assert_eq!(key.len(), 36);
        assert_eq!(
            creates[0].body,
            json!({"name": "Door 7", "device_id": "door-7", "labels": {"site": "berlin"},
                   "limits": {"requests_per_second": 5}, "daily_spend_cap": {"usd": 2.5}, "may_search_collections": false})
        );
        // The caller's own key is sent as given.
        k.create(&NewKey::new("Door 9").idempotency_key("door-9-2026")).await.unwrap();
        assert_eq!(seen.lock().unwrap().drain(..).next_back().unwrap().key.as_deref(), Some("door-9-2026"));

        let page = k.list(&KeyQuery::new().label("site", "berlin").status("all").limit(2)).await.unwrap();
        assert_eq!(page["next_cursor"], "c1");
        let all = k.all(&KeyQuery::new().device_id("door 7")).await.unwrap();
        assert_eq!(all, [json!({"id": "key_1"}), json!({"id": "key_2"})]);
        k.update("key_1", &KeyUpdate::new().name("Door 8").clear_labels().clear_daily_spend_cap()).await.unwrap();
        k.update("key_1", &KeyUpdate::new().limits(None, Some(900)).expires_at("2027-01-01T00:00:00Z")).await.unwrap();
        k.revoke("key_1").await.unwrap();
        let bulk = KeyRevocation::new().label("site", "berlin").device_ids(["door-7"]).dry_run(true);
        assert_eq!(k.revoke_by_labels(&bulk).await.unwrap()["revoked"], 2);
        let rest: Vec<Seen> = seen.lock().unwrap().drain(..).collect();
        let lines: Vec<&str> = rest.iter().map(|s| s.line.as_str()).collect();
        assert_eq!(
            lines,
            [
                "GET /v1/keys?label=site%3Aberlin&status=all&limit=2",
                "GET /v1/keys?device_id=door%207",
                "GET /v1/keys?device_id=door%207&cursor=c1",
                "PATCH /v1/keys/key_1",
                "PATCH /v1/keys/key_1",
                "DELETE /v1/keys/key_1",
                "POST /v1/keys/revoke",
            ]
        );
        assert!(rest.iter().all(|s| s.key.is_none()), "only create sends a key");
        assert_eq!(rest[3].body, json!({"name": "Door 8", "labels": null, "daily_spend_cap": null}));
        assert_eq!(rest[4].body, json!({"limits": {"tokens_per_second": 900}, "expires_at": "2027-01-01T00:00:00Z"}));
        assert_eq!(rest[6].body, json!({"labels": {"site": "berlin"}, "device_ids": ["door-7"], "dry_run": true}));
    }

    #[tokio::test]
    async fn local_checks_send_nothing() {
        let dn = DecisionNode::builder().api_key("k").base_url("http://127.0.0.1:9").build().unwrap();
        let k = dn.keys();
        let message = |e: Error| {
            assert_eq!((e.kind(), e.status(), e.error_type()), (ErrorKind::BadRequest, None, Some("api_usage_error")));
            e.message().to_owned()
        };
        let bad_key = "Idempotency-Key must be sent once, as 1 to 255 printable ASCII characters.";
        assert_eq!(message(k.create(&NewKey::new("a").idempotency_key("")).await.unwrap_err()), bad_key);
        assert_eq!(message(k.create(&NewKey::new("a").idempotency_key("é")).await.unwrap_err()), bad_key);
        assert_eq!(message(k.create(&NewKey::new("a").idempotency_key("x".repeat(256))).await.unwrap_err()), bad_key);
        assert_eq!(
            message(k.list(&KeyQuery::new().limit(201)).await.unwrap_err()),
            "limit is a whole number from 1 to 200."
        );
        assert_eq!(
            message(k.list(&KeyQuery::new().limit(0)).await.unwrap_err()),
            "limit is a whole number from 1 to 200."
        );
        assert_eq!(
            message(k.list(&KeyQuery::new().label("Site", "x")).await.unwrap_err()),
            "A label filter is name:value, as labels are written."
        );
        assert_eq!(
            message(k.list(&KeyQuery::new().device_id(" d")).await.unwrap_err()),
            "device_id is 1 to 128 printable ASCII characters, not starting with a space."
        );
        assert_eq!(
            message(k.list(&KeyQuery::new().status("deleted")).await.unwrap_err()),
            "status is active, expired, revoked or all."
        );
        assert_eq!(message(k.update("key_1", &KeyUpdate::new()).await.unwrap_err()), "Nothing to change.");
    }
}
