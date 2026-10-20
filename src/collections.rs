//! Collections: a workspace's collections of enrolled faces, which the
//! `face_recognition` booster searches, and the public ones it may use; and
//! its knowledge bases, collections of type `text` whose documents the
//! `knowledge` booster reads (see [`crate::Document`] and [`crate::knowledge`]).

use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::check::{entry, usage_error};
use crate::client::{DecisionNode, Policy};
use crate::error::{Error, ErrorKind, Result};
use crate::image::{check_image_url, Image};

/// The longest collection name, in characters.
const NAME_MAX: usize = 80;
/// The longest item reference, in characters.
pub(crate) const REFERENCE_MAX: usize = 64;
/// The longest item label, in characters.
pub(crate) const LABEL_MAX: usize = 128;
/// The longest scope key, in characters.
const SCOPE_KEY_MAX: usize = 40;
/// An item reference: 1 to 64 of these characters.
const REFERENCE_PATTERN: &str = "^[A-Za-z0-9_.:-]{1,64}$";
/// The largest page of `list_items`.
const MAX_LIST_LIMIT: i64 = 100;
/// The most keys an item's metadata holds.
const METADATA_KEYS: usize = 20;
/// The longest metadata key, in characters.
const METADATA_KEY_CHARS: usize = 40;
/// The longest metadata text, in characters.
const METADATA_TEXT_CHARS: usize = 500;

/// A change to an enrolled face, for [`Collections::update_item`]: only the
/// fields set are sent.
///
/// ```
/// use decisionnode::ItemUpdate;
/// use serde_json::json;
///
/// let change = ItemUpdate::new().label("Front desk").metadata(json!({"badge": 1042}));
/// let cleared = ItemUpdate::new().clear_label();
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ItemUpdate {
    reference: Option<String>,
    label: Option<Option<String>>,
    metadata: Option<Value>,
    pinned: Option<bool>,
    pin_rank: Option<i64>,
    title: Option<String>,
}

impl ItemUpdate {
    /// A change of nothing yet; set at least one field.
    pub fn new() -> ItemUpdate {
        ItemUpdate::default()
    }

    /// A new reference (1 to 64 of `A-Z`, `a-z`, `0-9`, `_`, `.`, `:` and `-`).
    pub fn reference(mut self, reference: impl Into<String>) -> ItemUpdate {
        self.reference = Some(reference.into());
        self
    }

    /// A new label (1 to 128 characters).
    pub fn label(mut self, label: impl Into<String>) -> ItemUpdate {
        self.label = Some(Some(label.into()));
        self
    }

    /// Clears the label: sent as `"label": null`.
    pub fn clear_label(mut self) -> ItemUpdate {
        self.label = Some(None);
        self
    }

    /// New metadata, which replaces the item's whole object (`{}` clears it):
    /// at most 20 keys of 1 to 40 of `a-z`, `0-9` and `_`, each value a text
    /// of at most 500 characters, a number or a boolean.
    pub fn metadata(mut self, metadata: impl Into<Value>) -> ItemUpdate {
        self.metadata = Some(metadata.into());
        self
    }

    /// A document's pin: `true` always gives it to a decision that names
    /// its knowledge base. Applied at once, never billed.
    pub fn pinned(mut self, pinned: bool) -> ItemUpdate {
        self.pinned = Some(pinned);
        self
    }

    /// A pinned document's order, 0 to 1,000, lowest first.
    pub fn pin_rank(mut self, pin_rank: i64) -> ItemUpdate {
        self.pin_rank = Some(pin_rank);
        self
    }

    /// A document's new title, 1 to 200 characters: a new version of the
    /// document (`202`), embedded again.
    pub fn title(mut self, title: impl Into<String>) -> ItemUpdate {
        self.title = Some(title.into());
        self
    }
}

/// A new collection, for [`Collections::create_with`]: a collection of
/// faces, or a knowledge base of documents with an optional scope key.
///
/// ```
/// use decisionnode::NewCollection;
///
/// let staff = NewCollection::face("Staff");
/// let missions = NewCollection::text("Mission briefs").scope_key("mission");
/// ```
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub struct NewCollection {
    name: String,
    collection_type: String,
    scope_key: Option<String>,
}

impl NewCollection {
    /// A collection of faces (type `face`).
    pub fn face(name: impl Into<String>) -> NewCollection {
        NewCollection { name: name.into(), collection_type: "face".into(), scope_key: None }
    }

    /// A knowledge base (type `text`).
    pub fn text(name: impl Into<String>) -> NewCollection {
        NewCollection { name: name.into(), collection_type: "text".into(), scope_key: None }
    }

    /// The collection's type, `face` or `text`.
    pub fn collection_type(mut self, collection_type: impl Into<String>) -> NewCollection {
        self.collection_type = collection_type.into();
        self
    }

    /// A knowledge base's scope key (1 to 40 of `a-z`, `0-9` and `_`): the
    /// metadata key whose value scopes a document to, say, one mission. A
    /// decision names the value in the booster's `scope`. Fixed for the
    /// collection's life.
    pub fn scope_key(mut self, scope_key: impl Into<String>) -> NewCollection {
        self.scope_key = Some(scope_key.into());
        self
    }
}

/// Which page of a collection's faces [`Collections::list_items_with`] reads.
/// Each part is sent only when set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ItemQuery {
    offset: Option<i64>,
    limit: Option<i64>,
    reference: Option<String>,
    reference_prefix: Option<String>,
}

impl ItemQuery {
    /// The first page, with the API's page size.
    pub fn new() -> ItemQuery {
        ItemQuery::default()
    }

    /// How many faces to skip, 0 or more.
    pub fn offset(mut self, offset: i64) -> ItemQuery {
        self.offset = Some(offset);
        self
    }

    /// The page size, 1 to 100.
    pub fn limit(mut self, limit: i64) -> ItemQuery {
        self.limit = Some(limit);
        self
    }

    /// Only the faces enrolled under this reference.
    pub fn reference(mut self, reference: impl Into<String>) -> ItemQuery {
        self.reference = Some(reference.into());
        self
    }

    /// Only the faces whose reference starts with this prefix. Not together
    /// with [`ItemQuery::reference`].
    pub fn reference_prefix(mut self, prefix: impl Into<String>) -> ItemQuery {
        self.reference_prefix = Some(prefix.into());
        self
    }
}

/// The collections of a client, from [`DecisionNode::collections`].
///
/// Every call returns the API's object exactly as sent, and is retried as
/// every call is. `create` and `enroll` send an `Idempotency-Key`, a new
/// random UUID for each call and the same on every attempt of it, so the API
/// creates, enrolls and bills once however often the call is tried; they are
/// also retried on a `409` `idempotency_in_progress`, after the wait it asks
/// for. A `409` `idempotency_key_reused` is returned as the base error.
#[derive(Clone, Copy, Debug)]
pub struct Collections<'a> {
    pub(crate) client: &'a DecisionNode,
}

impl DecisionNode {
    /// The workspace's collections of faces.
    ///
    /// ```no_run
    /// # async fn run(dn: decisionnode::DecisionNode) -> decisionnode::Result<()> {
    /// use decisionnode::Image;
    ///
    /// let staff = dn.collections().create("Staff").await?;
    /// let id = staff["id"].as_str().unwrap_or_default();
    /// let badge = Image::url("badge", "https://files.example.com/badges/emp-1042.jpg");
    /// dn.collections().enroll(id, badge, "emp-1042", Some("Reception"), None).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn collections(&self) -> Collections<'_> {
        Collections { client: self }
    }
}

/// A path segment: every byte but the unreserved characters written as `%XX`.
pub(crate) fn segment(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// How much of a string a collections `422` entry echoes, in characters.
const ECHO_CHARS: usize = 64;

/// An input as a collections `422` entry echoes it, as the API does: a string
/// cut to its first 64 characters, with `...` added when it was longer.
pub(crate) fn echoed(value: &str) -> Value {
    if value.chars().count() > ECHO_CHARS {
        Value::String(format!("{}...", value.chars().take(ECHO_CHARS).collect::<String>()))
    } else {
        Value::String(value.to_owned())
    }
}

/// A string field's `422` entry, as the API answers it: empty, too long or,
/// with a pattern, not matching it. Lengths are counted in characters.
pub(crate) fn string_entries(field: &str, value: &str, most: usize, pattern: bool) -> Vec<Value> {
    let loc = [json!("body"), json!(field)];
    if value.is_empty() {
        let msg = "String should have at least 1 character";
        return vec![entry("string_too_short", &loc, msg, &json!(""), Some(json!({"min_length": 1})))];
    }
    if value.chars().count() > most {
        let msg = format!("String should have at most {most} characters");
        return vec![entry("string_too_long", &loc, msg, &echoed(value), Some(json!({"max_length": most})))];
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-');
    if pattern && !value.chars().all(allowed) {
        let msg = format!("String should match pattern '{REFERENCE_PATTERN}'");
        let input = Value::String(value.to_owned());
        return vec![entry("string_pattern_mismatch", &loc, msg, &input, Some(json!({"pattern": REFERENCE_PATTERN})))];
    }
    Vec::new()
}

/// A caller's value as an API message shows it: its first `most`
/// characters, any but printable ASCII written as `\u{hex}`, then `...` when
/// it was longer.
fn printable(value: &str, most: usize) -> String {
    let mut out: String = value
        .chars()
        .take(most)
        .map(|c| if (' '..='~').contains(&c) { c.to_string() } else { format!("\\u{{{:x}}}", u32::from(c)) })
        .collect();
    if value.chars().count() > most {
        out.push_str("...");
    }
    out
}

/// An input as a collections `422` entry echoes a value of any type: a
/// string in part (see [`echoed`]), an object or an array never.
fn echoed_value(value: &Value) -> Value {
    match value {
        Value::String(s) => echoed(s),
        Value::Object(_) | Value::Array(_) => Value::Null,
        other => other.clone(),
    }
}

/// An item's metadata's `422` entries, in key order: an object of at most 20
/// keys (that alone, before any key is read), each key 1 to 40 of `a-z`,
/// `0-9` and `_`, each value a text of at most 500 characters, a number or a
/// boolean. A text's control characters, a number's range and the total size
/// are the API's to judge.
pub(crate) fn metadata_entries(metadata: &Value) -> Vec<Value> {
    let loc = [json!("body"), json!("metadata")];
    let Some(map) = metadata.as_object() else {
        return vec![entry(
            "value_error",
            &loc,
            "Metadata is an object of keys and values.",
            &echoed_value(metadata),
            None,
        )];
    };
    if map.len() > METADATA_KEYS {
        return vec![entry(
            "value_error",
            &loc,
            format!("Metadata holds at most {METADATA_KEYS} keys."),
            &Value::Null,
            None,
        )];
    }
    let mut out = Vec::new();
    for (key, value) in map {
        let length = key.chars().count();
        let key_ok = (1..=METADATA_KEY_CHARS).contains(&length)
            && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !key_ok {
            let mut shown: String =
                key.chars().take(METADATA_KEY_CHARS).map(|c| if c.is_control() { '?' } else { c }).collect();
            if length > METADATA_KEY_CHARS {
                shown.push_str("...");
            }
            let msg = format!("A metadata key is 1 to {METADATA_KEY_CHARS} characters of a-z, 0-9 and _: \"{shown}\".");
            out.push(entry("value_error", &loc, msg, &Value::Null, None));
            continue;
        }
        let at = [json!("body"), json!("metadata"), json!(key)];
        match value {
            Value::String(text) if text.chars().count() > METADATA_TEXT_CHARS => out.push(entry(
                "value_error",
                &at,
                format!("A metadata text is at most {METADATA_TEXT_CHARS} characters."),
                &echoed(text),
                None,
            )),
            Value::String(_) | Value::Number(_) | Value::Bool(_) => {}
            other => out.push(entry(
                "value_error",
                &at,
                "A metadata value is a text, a number or true or false.",
                &echoed_value(other),
                None,
            )),
        }
    }
    out
}

/// The `offset` and `limit` of a page, checked as the API checks them.
pub(crate) fn page_query(offset: Option<i64>, limit: Option<i64>) -> Result<Vec<String>> {
    if let Some(o) = offset.filter(|o| *o < 0) {
        return Err(usage_error(format!("Query parameter offset must be an integer of 0 or more, got {o}.")));
    }
    if let Some(l) = limit.filter(|l| !(1..=MAX_LIST_LIMIT).contains(l)) {
        return Err(usage_error(format!(
            "Query parameter limit must be an integer from 1 to {MAX_LIST_LIMIT}, got {l}."
        )));
    }
    let mut query = Vec::new();
    if let Some(o) = offset {
        query.push(format!("offset={o}"));
    }
    if let Some(l) = limit {
        query.push(format!("limit={l}"));
    }
    Ok(query)
}

/// A `reference` or `reference_prefix` of the items list, as the API checks
/// it.
fn check_reference_query(name: &str, value: &str) -> Result<()> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-');
    let length = value.chars().count();
    if (1..=REFERENCE_MAX).contains(&length) && value.chars().all(allowed) {
        return Ok(());
    }
    Err(usage_error(format!(
        "Query parameter {name} must be 1 to 64 characters of A-Z, a-z, 0-9, _, ., : and -, got {}.",
        printable(value, REFERENCE_MAX)
    )))
}

impl Collections<'_> {
    async fn call(&self, method: Method, path: &str, body: Option<&Value>, policy: Policy) -> Result<Value> {
        Ok(self.client.request_json(method, path, body, policy).await?.body)
    }

    /// `POST /v1/collections`: creates a collection of faces with this name
    /// (1 to 80 characters) and returns it.
    pub async fn create(&self, name: impl Into<String>) -> Result<Value> {
        self.create_with(&NewCollection::face(name)).await
    }

    /// [`Collections::create`] with the collection's type, `face` or `text`.
    pub async fn create_with_type(&self, name: impl Into<String>, collection_type: impl Into<String>) -> Result<Value> {
        self.create_with(&NewCollection::face(name).collection_type(collection_type)).await
    }

    /// `POST /v1/collections`: creates a collection of faces or a knowledge
    /// base and returns it as sent. A type other than `face` or `text`, a
    /// name not 1 to 80 characters and a scope key not 1 to 40 of `a-z`,
    /// `0-9` and `_` are refused before anything is sent, as the API would.
    pub async fn create_with(&self, collection: &NewCollection) -> Result<Value> {
        let NewCollection { name, collection_type, scope_key } = collection;
        let mut entries = Vec::new();
        if !matches!(collection_type.as_str(), "face" | "text") {
            entries.push(entry(
                "literal_error",
                &[json!("body"), json!("type")],
                "Input should be 'face' or 'text'",
                &echoed(collection_type),
                Some(json!({"expected": "'face' or 'text'"})),
            ));
        }
        entries.extend(string_entries("name", name, NAME_MAX, false));
        if let Some(key) = scope_key {
            let length = key.chars().count();
            let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_';
            if !(1..=SCOPE_KEY_MAX).contains(&length) || !key.chars().all(allowed) {
                entries.push(entry(
                    "string_pattern_mismatch",
                    &[json!("body"), json!("scope_key")],
                    "String should match pattern '^[a-z0-9_]{1,40}$'",
                    &echoed(key),
                    None,
                ));
            }
        }
        if !entries.is_empty() {
            return Err(Error::local_entries(entries));
        }
        let mut body = Map::new();
        body.insert("type".into(), Value::String(collection_type.clone()));
        body.insert("name".into(), Value::String(name.clone()));
        if let Some(key) = scope_key {
            body.insert("scope_key".into(), Value::String(key.clone()));
        }
        Ok(self.client.request_json_idempotent(Method::POST, "/v1/collections", &Value::Object(body)).await?.body)
    }

    /// `GET /v1/collections`: the workspace's collections, then the public
    /// ones it may use, in the order sent.
    pub async fn list(&self) -> Result<Vec<Value>> {
        let reply = self.client.request_json(Method::GET, "/v1/collections", None, Policy::Default).await?;
        match reply.body.get("collections").and_then(Value::as_array) {
            Some(list) => Ok(list.clone()),
            None => Err(Error::new(ErrorKind::Server, "the response is not a valid collection list")
                .with_response(reply.status, &reply.headers)),
        }
    }

    /// `GET /v1/collections/{id}`: one collection, by its id (a public
    /// collection by its name).
    pub async fn get(&self, collection_id: &str) -> Result<Value> {
        self.call(Method::GET, &format!("/v1/collections/{}", segment(collection_id)), None, Policy::Default).await
    }

    /// `DELETE /v1/collections/{id}`: deletes a collection and all it holds,
    /// and returns the answer as sent: `{"id", "deleted": true}`, or for a
    /// knowledge base not purged within 10 seconds `202` `{"id", "status":
    /// "purging"}`, the purge finishing on the server.
    pub async fn delete(&self, collection_id: &str) -> Result<Value> {
        self.call(Method::DELETE, &format!("/v1/collections/{}", segment(collection_id)), None, Policy::Default).await
    }

    /// `POST /v1/collections/{id}/items`: enrolls the face in an image under
    /// a reference (1 to 64 of `A-Z`, `a-z`, `0-9`, `_`, `.`, `:` and `-`),
    /// an optional label (1 to 128 characters) and optional metadata (see
    /// [`ItemUpdate::metadata`]). The image is sent as
    /// a decision's is: a URL as given (after the URL rules a decision's
    /// image follows), local bytes inline or uploaded by the inline
    /// threshold. Whether the picture can be read is the API's to say.
    pub async fn enroll(
        &self,
        collection_id: &str,
        image: Image,
        reference: impl Into<String>,
        label: Option<&str>,
        metadata: Option<Value>,
    ) -> Result<Value> {
        let reference = reference.into();
        // Its length first (1 to 64), then its pattern.
        let mut entries = string_entries("reference", &reference, REFERENCE_MAX, true);
        if let Some(l) = label {
            entries.extend(string_entries("label", l, LABEL_MAX, false));
        }
        if let Some(m) = &metadata {
            entries.extend(metadata_entries(m));
        }
        if !entries.is_empty() {
            return Err(Error::local_entries(entries));
        }
        // Only a URL's rules are checked here; every other fault of the
        // picture is the server's `422` `image_unreadable`.
        check_image_url(&image)?;
        let images = [image];
        let placed = self.client.place_images(&images, true, false).await?;
        let mut body = Map::new();
        body.insert("image".into(), placed.wires.into_iter().next().unwrap_or(Value::Null));
        body.insert("reference".into(), Value::String(reference));
        if let Some(l) = label {
            body.insert("label".into(), Value::String(l.to_owned()));
        }
        if let Some(m) = metadata {
            body.insert("metadata".into(), m);
        }
        let path = format!("/v1/collections/{}/items", segment(collection_id));
        Ok(self.client.request_json_idempotent(Method::POST, &path, &Value::Object(body)).await?.body)
    }

    /// `GET /v1/collections/{id}/items`: one page of a collection's faces.
    /// `offset` and `limit` are sent only when given; an `offset` below 0 or
    /// a `limit` outside 1 to 100 is refused before anything is sent, with
    /// the API's own error.
    pub async fn list_items(&self, collection_id: &str, offset: Option<i64>, limit: Option<i64>) -> Result<Value> {
        let mut query = ItemQuery::new();
        query.offset = offset;
        query.limit = limit;
        self.list_items_with(collection_id, &query).await
    }

    /// [`Collections::list_items`] with every part of the query: the page
    /// and, as the API answers it, the faces of one reference or of a
    /// reference prefix (not both). The page comes back as sent.
    pub async fn list_items_with(&self, collection_id: &str, q: &ItemQuery) -> Result<Value> {
        let mut query = page_query(q.offset, q.limit)?;
        if q.reference.is_some() && q.reference_prefix.is_some() {
            return Err(usage_error("Send query parameter reference or reference_prefix, not both."));
        }
        for (name, value) in [("reference", &q.reference), ("reference_prefix", &q.reference_prefix)] {
            if let Some(v) = value {
                check_reference_query(name, v)?;
                query.push(format!("{name}={}", segment(v)));
            }
        }
        let mut path = format!("/v1/collections/{}/items", segment(collection_id));
        if !query.is_empty() {
            path = format!("{path}?{}", query.join("&"));
        }
        self.call(Method::GET, &path, None, Policy::Default).await
    }

    /// `PATCH /v1/collections/{id}/items/{item_id}`: changes an enrolled
    /// face or a document, sending only the fields the [`ItemUpdate`] sets,
    /// and returns the item as sent. Its reference and label are checked as
    /// [`Collections::enroll`] checks them, a title and `pin_rank` as
    /// [`Collections::add_document`] does; a change of nothing is refused
    /// before anything is sent. Like `enroll`, it sends an `Idempotency-Key`
    /// and retries as `enroll` does. A document's `reference`, `label`,
    /// `metadata`, `pinned` and `pin_rank` apply at once and are never
    /// billed (`200`); a new title is a new version (`202`).
    pub async fn update_item(&self, collection_id: &str, item_id: &str, update: &ItemUpdate) -> Result<Value> {
        let mut entries = Vec::new();
        let mut body = Map::new();
        if let Some(r) = &update.reference {
            entries.extend(string_entries("reference", r, REFERENCE_MAX, true));
            body.insert("reference".into(), Value::String(r.clone()));
        }
        if let Some(label) = &update.label {
            if let Some(l) = label {
                entries.extend(string_entries("label", l, LABEL_MAX, false));
            }
            body.insert("label".into(), label.clone().map_or(Value::Null, Value::String));
        }
        if let Some(m) = &update.metadata {
            entries.extend(metadata_entries(m));
            body.insert("metadata".into(), m.clone());
        }
        if let Some(p) = update.pinned {
            body.insert("pinned".into(), Value::Bool(p));
        }
        if let Some(rank) = update.pin_rank {
            entries.extend(crate::knowledge::pin_rank_entries(rank));
            body.insert("pin_rank".into(), json!(rank));
        }
        if let Some(t) = &update.title {
            entries.extend(crate::knowledge::title_entries(t));
            body.insert("title".into(), Value::String(t.clone()));
        }
        if body.is_empty() {
            entries.push(entry(
                "missing",
                &[json!("body")],
                "Send reference, label, metadata or any of them",
                &Value::Null,
                None,
            ));
        }
        if !entries.is_empty() {
            return Err(Error::local_entries(entries));
        }
        let path = format!("/v1/collections/{}/items/{}", segment(collection_id), segment(item_id));
        Ok(self.client.request_json_idempotent(Method::PATCH, &path, &Value::Object(body)).await?.body)
    }

    /// `DELETE /v1/collections/{id}/items/{item_id}`: deletes one enrolled
    /// face.
    pub async fn delete_item(&self, collection_id: &str, item_id: &str) -> Result<Value> {
        let path = format!("/v1/collections/{}/items/{}", segment(collection_id), segment(item_id));
        self.call(Method::DELETE, &path, None, Policy::Default).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(e: &Error) -> Vec<(String, Value)> {
        assert_eq!((e.kind(), e.status(), e.request_id()), (ErrorKind::Validation, None, None));
        e.entries().unwrap().iter().map(|x| (x["type"].as_str().unwrap().to_owned(), x["loc"].clone())).collect()
    }

    #[test]
    fn path_segments_are_escaped() {
        assert_eq!(segment("col_3f6c-1.a~"), "col_3f6c-1.a~");
        assert_eq!(segment("wanted persons/é"), "wanted%20persons%2F%C3%A9");
    }

    #[test]
    fn string_rules() {
        assert!(string_entries("name", "Staff", NAME_MAX, false).is_empty());
        let short = string_entries("name", "", NAME_MAX, false);
        assert_eq!(short[0]["msg"], "String should have at least 1 character");
        assert_eq!(short[0]["ctx"], json!({"min_length": 1}));
        // Counted in characters: 80 non-ASCII characters are taken.
        assert!(string_entries("name", &"é".repeat(80), NAME_MAX, false).is_empty());
        let long = string_entries("name", &"é".repeat(81), NAME_MAX, false);
        // A long input is echoed as its first 64 characters and "...".
        assert_eq!(long[0]["input"], json!(format!("{}...", "é".repeat(64))));
        assert_eq!(echoed(&"a".repeat(64)), json!("a".repeat(64)));
        assert_eq!(
            (long[0]["type"].clone(), long[0]["ctx"].clone()),
            (json!("string_too_long"), json!({"max_length": 80}))
        );
        assert!(string_entries("reference", "wanted:2024-12345_a.b", REFERENCE_MAX, true).is_empty());
        let bad = string_entries("reference", "emp 1042", REFERENCE_MAX, true);
        assert_eq!(bad[0]["msg"], "String should match pattern '^[A-Za-z0-9_.:-]{1,64}$'");
        assert_eq!(bad[0]["ctx"], json!({"pattern": "^[A-Za-z0-9_.:-]{1,64}$"}));
        let empty = string_entries("reference", "", REFERENCE_MAX, true);
        assert_eq!((empty[0]["type"].clone(), empty[0]["input"].clone()), (json!("string_too_short"), json!("")));
        assert_eq!(string_entries("label", "", LABEL_MAX, false)[0]["type"], "string_too_short");
    }

    #[test]
    fn metadata_rules() {
        assert!(metadata_entries(&json!({"badge": 1042, "desk": "front", "vip": true, "rate": 0.5})).is_empty());
        let not_object = metadata_entries(&json!("x"));
        assert_eq!(
            (not_object[0]["msg"].clone(), not_object[0]["input"].clone()),
            (json!("Metadata is an object of keys and values."), json!("x"))
        );
        assert_eq!(metadata_entries(&json!([1]))[0]["input"], Value::Null);
        let many: Map<String, Value> = (0..21).map(|i| (format!("k{i}"), json!(i))).collect();
        let many = metadata_entries(&Value::Object(many));
        assert_eq!(many.len(), 1);
        assert_eq!(
            (many[0]["msg"].clone(), many[0]["input"].clone()),
            (json!("Metadata holds at most 20 keys."), Value::Null)
        );
        let long_key = format!("A\u{1}{}", "b".repeat(45));
        let got = metadata_entries(
            &json!({"ok": 1, long_key.clone(): 1, "text": "t".repeat(501), "list": [1], "none": null}),
        );
        assert_eq!(got.len(), 4);
        assert_eq!(got[0]["loc"], json!(["body", "metadata"]));
        assert_eq!(
            got[0]["msg"],
            json!(format!("A metadata key is 1 to 40 characters of a-z, 0-9 and _: \"A?{}...\".", "b".repeat(38)))
        );
        assert_eq!(
            (got[1]["loc"].clone(), got[1]["input"].clone()),
            (json!(["body", "metadata", "text"]), json!(format!("{}...", "t".repeat(64))))
        );
        assert_eq!(
            (got[2]["msg"].clone(), got[2]["input"].clone()),
            (json!("A metadata value is a text, a number or true or false."), Value::Null)
        );
        assert_eq!(got[3]["loc"], json!(["body", "metadata", "none"]));
        assert_eq!(
            metadata_entries(&json!({"": 1}))[0]["msg"],
            "A metadata key is 1 to 40 characters of a-z, 0-9 and _: \"\"."
        );
    }

    #[test]
    fn reference_queries() {
        assert!(check_reference_query("reference", "emp-1042").is_ok());
        let e = check_reference_query("reference_prefix", "emp 1é").unwrap_err();
        assert_eq!(
            e.message(),
            "Query parameter reference_prefix must be 1 to 64 characters of A-Z, a-z, 0-9, _, ., : and -, got emp 1\\u{e9}."
        );
        assert_eq!(printable(&"a".repeat(65), 64), format!("{}...", "a".repeat(64)));
        assert!(check_reference_query("reference", "").is_err());
    }

    #[tokio::test]
    async fn local_checks_send_nothing() {
        let dn = DecisionNode::builder().api_key("k").base_url("http://127.0.0.1:9").build().unwrap();
        let c = dn.collections();
        let e = c.create_with_type("", "person").await.unwrap_err();
        assert_eq!(
            kinds(&e),
            [
                ("literal_error".to_owned(), json!(["body", "type"])),
                ("string_too_short".to_owned(), json!(["body", "name"]))
            ]
        );
        let e = c.create("x".repeat(81)).await.unwrap_err();
        assert_eq!(kinds(&e), [("string_too_long".to_owned(), json!(["body", "name"]))]);
        let badge = || Image::url("badge", "https://files.example.com/badge.jpg");
        // An empty reference and an empty label are too short.
        let e = c.enroll("col_1", badge(), "", Some(&"l".repeat(129)), None).await.unwrap_err();
        assert_eq!(
            kinds(&e),
            [
                ("string_too_short".to_owned(), json!(["body", "reference"])),
                ("string_too_long".to_owned(), json!(["body", "label"]))
            ]
        );
        let e = c.enroll("col_1", badge(), " ".repeat(65), None, None).await.unwrap_err();
        assert_eq!(kinds(&e), [("string_too_long".to_owned(), json!(["body", "reference"]))]);
        let e = c.enroll("col_1", badge(), "r".repeat(65), None, None).await.unwrap_err();
        assert_eq!(kinds(&e), [("string_too_long".to_owned(), json!(["body", "reference"]))]);
        // Only the URL rules of the picture are checked here: bytes that are
        // no image are the server's to refuse.
        let e = c.enroll("col_1", Image::bytes("badge", &b"not an image"[..]), "emp-1", None, None).await.unwrap_err();
        assert!(e.is_transport(), "{e:?}");
        let e = c
            .enroll("col_1", Image::url("badge", "http://files.example.com/b.jpg"), "emp-1", None, None)
            .await
            .unwrap_err();
        assert_eq!((e.kind(), e.status()), (ErrorKind::BadRequest, None));
        let e = c.update_item("col_1", "itm_1", &ItemUpdate::new()).await.unwrap_err();
        assert_eq!(
            e.entries().unwrap(),
            [
                json!({"type": "missing", "loc": ["body"], "msg": "Send reference, label, metadata or any of them", "input": null})
            ]
        );
        let e = c.update_item("col_1", "itm_1", &ItemUpdate::new().reference("a b").label("")).await.unwrap_err();
        assert_eq!(
            kinds(&e),
            [
                ("string_pattern_mismatch".to_owned(), json!(["body", "reference"])),
                ("string_too_short".to_owned(), json!(["body", "label"]))
            ]
        );
        let both = ItemQuery::new().reference("a").reference_prefix("b");
        let e = c.list_items_with("col_1", &both).await.unwrap_err();
        assert_eq!(e.message(), "Send query parameter reference or reference_prefix, not both.");
        let e = c.list_items("col_1", Some(-1), None).await.unwrap_err();
        assert_eq!(e.message(), "Query parameter offset must be an integer of 0 or more, got -1.");
        let e = c.list_items("col_1", None, Some(101)).await.unwrap_err();
        assert_eq!(e.message(), "Query parameter limit must be an integer from 1 to 100, got 101.");
    }
}
