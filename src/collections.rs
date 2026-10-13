//! Collections: a workspace's collections of enrolled faces, which the
//! `face_recognition` booster searches, and the public ones it may use.

use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::check::{entry, usage_error};
use crate::client::{DecisionNode, Policy};
use crate::error::{Error, ErrorKind, Result};
use crate::image::{check_image_url, Image};

/// The longest collection name, in characters.
const NAME_MAX: usize = 80;
/// The longest item reference, in characters.
const REFERENCE_MAX: usize = 64;
/// The longest item label, in characters.
const LABEL_MAX: usize = 128;
/// An item reference: 1 to 64 of these characters.
const REFERENCE_PATTERN: &str = "^[A-Za-z0-9_.:-]{1,64}$";
/// The largest page of `list_items`.
const MAX_LIST_LIMIT: i64 = 100;

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
    client: &'a DecisionNode,
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
    /// dn.collections().enroll(id, badge, "emp-1042", Some("Reception")).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn collections(&self) -> Collections<'_> {
        Collections { client: self }
    }
}

/// A path segment: every byte but the unreserved characters written as `%XX`.
fn segment(id: &str) -> String {
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
fn echoed(value: &str) -> Value {
    if value.chars().count() > ECHO_CHARS {
        Value::String(format!("{}...", value.chars().take(ECHO_CHARS).collect::<String>()))
    } else {
        Value::String(value.to_owned())
    }
}

/// A string field's `422` entry, as the API answers it: empty, too long or,
/// with a pattern, not matching it. Lengths are counted in characters.
fn string_entries(field: &str, value: &str, most: usize, pattern: bool) -> Vec<Value> {
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

impl Collections<'_> {
    async fn call(&self, method: Method, path: &str, body: Option<&Value>, policy: Policy) -> Result<Value> {
        Ok(self.client.request_json(method, path, body, policy).await?.body)
    }

    /// `POST /v1/collections`: creates a collection of faces with this name
    /// (1 to 80 characters) and returns it.
    pub async fn create(&self, name: impl Into<String>) -> Result<Value> {
        self.create_with_type(name, "face").await
    }

    /// [`Collections::create`] with the collection's type. `face` is the only
    /// type; another is refused before anything is sent, as the API would.
    pub async fn create_with_type(&self, name: impl Into<String>, collection_type: impl Into<String>) -> Result<Value> {
        let (name, collection_type) = (name.into(), collection_type.into());
        let mut entries = Vec::new();
        if collection_type != "face" {
            entries.push(entry(
                "literal_error",
                &[json!("body"), json!("type")],
                "Input should be 'face'",
                &echoed(&collection_type),
                Some(json!({"expected": "'face'"})),
            ));
        }
        entries.extend(string_entries("name", &name, NAME_MAX, false));
        if !entries.is_empty() {
            return Err(Error::local_entries(entries));
        }
        let body = json!({"type": collection_type, "name": name});
        Ok(self.client.request_json_idempotent(Method::POST, "/v1/collections", &body).await?.body)
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

    /// `DELETE /v1/collections/{id}`: deletes a collection and its faces.
    pub async fn delete(&self, collection_id: &str) -> Result<Value> {
        self.call(Method::DELETE, &format!("/v1/collections/{}", segment(collection_id)), None, Policy::Default).await
    }

    /// `POST /v1/collections/{id}/items`: enrolls the face in an image under
    /// a reference (1 to 64 of `A-Z`, `a-z`, `0-9`, `_`, `.`, `:` and `-`)
    /// and an optional label (at most 128 characters). The image is sent as
    /// a decision's is: a URL as given (after the URL rules a decision's
    /// image follows), local bytes inline or uploaded by the inline
    /// threshold. Whether the picture can be read is the API's to say.
    pub async fn enroll(
        &self,
        collection_id: &str,
        image: Image,
        reference: impl Into<String>,
        label: Option<&str>,
    ) -> Result<Value> {
        let reference = reference.into();
        // Its length first (1 to 64), then its pattern.
        let mut entries = string_entries("reference", &reference, REFERENCE_MAX, true);
        if let Some(l) = label {
            entries.extend(string_entries("label", l, LABEL_MAX, false));
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
        let path = format!("/v1/collections/{}/items", segment(collection_id));
        Ok(self.client.request_json_idempotent(Method::POST, &path, &Value::Object(body)).await?.body)
    }

    /// `GET /v1/collections/{id}/items`: one page of a collection's faces.
    /// `offset` and `limit` are sent only when given; an `offset` below 0 or
    /// a `limit` outside 1 to 100 is refused before anything is sent, with
    /// the API's own error.
    pub async fn list_items(&self, collection_id: &str, offset: Option<i64>, limit: Option<i64>) -> Result<Value> {
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
        let mut path = format!("/v1/collections/{}/items", segment(collection_id));
        if !query.is_empty() {
            path = format!("{path}?{}", query.join("&"));
        }
        self.call(Method::GET, &path, None, Policy::Default).await
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
        let e = c.enroll("col_1", badge(), "", Some(&"l".repeat(129))).await.unwrap_err();
        assert_eq!(
            kinds(&e),
            [
                ("string_too_short".to_owned(), json!(["body", "reference"])),
                ("string_too_long".to_owned(), json!(["body", "label"]))
            ]
        );
        let e = c.enroll("col_1", badge(), " ".repeat(65), None).await.unwrap_err();
        assert_eq!(kinds(&e), [("string_too_long".to_owned(), json!(["body", "reference"]))]);
        let e = c.enroll("col_1", badge(), "r".repeat(65), None).await.unwrap_err();
        assert_eq!(kinds(&e), [("string_too_long".to_owned(), json!(["body", "reference"]))]);
        // Only the URL rules of the picture are checked here: bytes that are
        // no image are the server's to refuse.
        let e = c.enroll("col_1", Image::bytes("badge", &b"not an image"[..]), "emp-1", None).await.unwrap_err();
        assert!(e.is_transport(), "{e:?}");
        let e =
            c.enroll("col_1", Image::url("badge", "http://files.example.com/b.jpg"), "emp-1", None).await.unwrap_err();
        assert_eq!((e.kind(), e.status()), (ErrorKind::BadRequest, None));
        let e = c.list_items("col_1", Some(-1), None).await.unwrap_err();
        assert_eq!(e.message(), "Query parameter offset must be an integer of 0 or more, got -1.");
        let e = c.list_items("col_1", None, Some(101)).await.unwrap_err();
        assert_eq!(e.message(), "Query parameter limit must be an integer from 1 to 100, got 101.");
    }
}
