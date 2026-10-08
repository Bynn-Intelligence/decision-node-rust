//! The Batch API: up to 10,000 decide requests answered within 24 hours at
//! half the price, with results kept for 7 days.

use std::fmt;
use std::time::{Duration, Instant};

use bytes::Bytes;
use indexmap::IndexMap;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Method;
use serde_json::{Map, Value};

use crate::client::{DecideRequest, DecisionNode, Payload, Policy, Prepared};
use crate::error::{retry_after, Error, ErrorKind, Result};
use crate::response::{Decision, Usage};

/// The largest body of one `add` call; a longer list is split into several
/// calls, in order.
pub(crate) const MAX_ADD_BYTES: usize = 10 * 1024 * 1024;
/// The shortest poll interval of [`DecisionNode::run_batch_with`].
pub const MIN_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// The poll interval of [`DecisionNode::run_batch`].
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// How long [`DecisionNode::run_batch`] waits for a batch to end: its 24 hours
/// to run plus the time to queue it.
pub const DEFAULT_BATCH_TIMEOUT: Duration = Duration::from_secs(26 * 3600);

/// One request of a batch: a decide request and the `custom_id` its result
/// comes back under.
///
/// ```
/// use decisionnode::{truth, BatchRequest, DecideRequest};
///
/// let r = BatchRequest::new(
///     "ticket-1",
///     DecideRequest::new().state("I was charged twice.").question("refund", truth("Is a refund asked for?")),
/// );
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct BatchRequest {
    custom_id: String,
    request: DecideRequest,
}

impl BatchRequest {
    /// A request under `custom_id`: 1 to 512 characters, unique in the batch.
    pub fn new(custom_id: impl Into<String>, request: DecideRequest) -> BatchRequest {
        BatchRequest { custom_id: custom_id.into(), request }
    }

    /// The `custom_id`.
    pub fn custom_id(&self) -> &str {
        &self.custom_id
    }
}

/// A line checked and ready: its `custom_id` and its decide body.
struct Line {
    custom_id: String,
    prepared: Prepared,
}

/// The counters of a batch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BatchCounters {
    /// Every line accepted into the batch, refused-at-upload lines included.
    pub requests: u64,
    /// Finalized, not dispatched.
    pub queued: u64,
    /// Dispatched and not yet recorded.
    pub in_progress: u64,
    /// Answered with status 200; billed.
    pub completed: u64,
    /// Ended with an error status.
    pub failed: u64,
    /// Unfinished when the batch expired; not billed.
    pub expired: u64,
    /// Not started when the batch was cancelled; not billed.
    pub cancelled: u64,
}

/// A batch: every field of the batch object, as of the call that returned it.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Batch {
    /// `batch_` and 10 characters.
    pub id: String,
    /// `draft`, `queued`, `running`, `completed`, `failed`, `cancelled` or
    /// `expired`.
    pub state: String,
    /// The counters.
    pub counters: BatchCounters,
    /// When the batch was created, RFC 3339 UTC.
    pub created_at: Option<String>,
    /// When it was finalized; `None` while a draft.
    pub finalized_at: Option<String>,
    /// When its unfinished lines expire: 24 hours after finalize.
    pub expires_at: Option<String>,
    /// When it ended; `None` while it runs.
    pub ended_at: Option<String>,
    /// Where its results are, once it has ended.
    pub results_url: Option<String>,
    /// When its results are deleted: 7 days after it ended.
    pub results_expire_at: Option<String>,
    /// The price factor, `0.5`.
    pub price_factor: Option<f64>,
    /// Input tokens of the lines answered so far.
    pub usage: Usage,
    /// When `failed`: `error_type` and `message`.
    pub error: Option<Value>,
    /// The batch object as received.
    pub raw: Value,
}

impl Batch {
    pub(crate) fn from_value(raw: Value) -> Option<Batch> {
        let obj = raw.as_object()?;
        let text = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_owned);
        let c = obj.get("counters");
        let n = |k: &str| c.and_then(|c| c.get(k)).and_then(Value::as_u64).unwrap_or(0);
        Some(Batch {
            id: text("id")?,
            state: text("state").unwrap_or_default(),
            counters: BatchCounters {
                requests: n("requests"),
                queued: n("queued"),
                in_progress: n("in_progress"),
                completed: n("completed"),
                failed: n("failed"),
                expired: n("expired"),
                cancelled: n("cancelled"),
            },
            created_at: text("created_at"),
            finalized_at: text("finalized_at"),
            expires_at: text("expires_at"),
            ended_at: text("ended_at"),
            results_url: text("results_url"),
            results_expire_at: text("results_expire_at"),
            price_factor: obj.get("price_factor").and_then(Value::as_f64),
            usage: Usage::from_body(obj.get("usage")),
            error: obj.get("error").filter(|v| !v.is_null()).cloned(),
            raw,
        })
    }

    /// True once the batch has ended: `completed`, `failed`, `cancelled` or
    /// `expired`. Its results can be read then.
    pub fn is_ended(&self) -> bool {
        self.ended_at.is_some() || matches!(self.state.as_str(), "completed" | "failed" | "cancelled" | "expired")
    }
}

/// One page of [`Batches::list`], newest batch first.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct BatchPage {
    /// The batches of this page.
    pub batches: Vec<Batch>,
    /// The offset of the first batch.
    pub offset: u64,
    /// The page size asked for.
    pub limit: u64,
    /// Every batch of the key still kept.
    pub total: u64,
    /// The offset of the next page; `None` on the last.
    pub next_offset: Option<u64>,
    /// The page as received.
    pub raw: Value,
}

/// The status of one result line: the HTTP status the request got, or a
/// line that never ran.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BatchStatus {
    /// The status the request got, as on the live API.
    Http(u16),
    /// Unfinished when the batch expired.
    Expired,
    /// Not started when the batch was cancelled.
    Cancelled,
    /// A status this version does not know.
    Other(Value),
}

/// One result line: the request's `custom_id`, its status, and its decision
/// or its error as a value (never raised).
#[non_exhaustive]
pub struct BatchResult {
    /// The request's `custom_id`.
    pub custom_id: String,
    /// The status.
    pub status: BatchStatus,
    /// The decision of a `200` line, parsed as [`DecisionNode::decide`]
    /// parses one, numbers as received.
    pub decision: Option<Decision>,
    /// The error of a line that got an error status: the error kind and
    /// fields the same status and body give on the live API.
    pub error: Option<Error>,
    /// The line's headers, names in lower case.
    pub headers: IndexMap<String, String>,
    /// The line as received.
    pub raw: Value,
}

impl fmt::Debug for BatchResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BatchResult")
            .field("custom_id", &self.custom_id)
            .field("status", &self.status)
            .field("decision", &self.decision.is_some())
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl BatchResult {
    fn from_line(raw: Value) -> Option<BatchResult> {
        let obj = raw.as_object()?;
        let custom_id = obj.get("custom_id")?.as_str()?.to_owned();
        let headers: IndexMap<String, String> = obj
            .get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.to_ascii_lowercase(), v.as_str()?.to_owned())))
            .collect();
        let status = match obj.get("status") {
            Some(Value::Number(n)) => match n.as_u64().and_then(|n| u16::try_from(n).ok()) {
                Some(code) => BatchStatus::Http(code),
                None => BatchStatus::Other(Value::Number(n.clone())),
            },
            Some(Value::String(s)) if s == "expired" => BatchStatus::Expired,
            Some(Value::String(s)) if s == "cancelled" => BatchStatus::Cancelled,
            other => BatchStatus::Other(other.cloned().unwrap_or(Value::Null)),
        };
        let mut decision = None;
        let mut error = None;
        if let BatchStatus::Http(code) = status {
            let header_map = to_header_map(&headers);
            let body = obj.get("body").cloned().unwrap_or(Value::Null);
            if (200..=299).contains(&code) {
                match Decision::from_response(body, code, &header_map) {
                    Ok(d) => decision = Some(d),
                    Err(e) => error = Some(e),
                }
            } else {
                let bytes = serde_json::to_vec(&body).unwrap_or_default();
                error = Some(Error::from_response(code, &header_map, &bytes));
            }
        }
        Some(BatchResult { custom_id, status, decision, error, headers, raw })
    }
}

fn to_header_map(headers: &IndexMap<String, String>) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            map.append(n, v);
        }
    }
    map
}

/// The results of a batch, one [`BatchResult`] per request in upload order,
/// parsed line by line as the iterator is read.
pub struct BatchResults {
    body: Bytes,
    at: usize,
}

impl fmt::Debug for BatchResults {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BatchResults").field("bytes", &self.body.len()).field("at", &self.at).finish()
    }
}

impl Iterator for BatchResults {
    type Item = Result<BatchResult>;

    fn next(&mut self) -> Option<Result<BatchResult>> {
        loop {
            if self.at >= self.body.len() {
                return None;
            }
            let rest = &self.body[self.at..];
            let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
            let line = &rest[..end];
            self.at += end + 1;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let parsed = serde_json::from_slice::<Value>(line).ok().and_then(BatchResult::from_line);
            return Some(parsed.ok_or_else(|| Error::new(ErrorKind::Server, "a result line is not a valid result")));
        }
    }
}

/// An ended batch and its results, from [`DecisionNode::run_batch`].
#[derive(Debug)]
#[non_exhaustive]
pub struct BatchRun {
    /// The batch as it ended.
    pub batch: Batch,
    /// Its results, in upload order.
    pub results: BatchResults,
}

/// The Batch API of a client, from [`DecisionNode::batches`].
///
/// `get`, `list`, `results` and `cancel` are retried as every call is.
/// `create`, `add` and `finalize` are retried only on `429` and `529`, which
/// the API never applies, and never after a timeout or a lost answer, since a
/// repeat could create a second draft.
#[derive(Clone, Copy, Debug)]
pub struct Batches<'a> {
    client: &'a DecisionNode,
}

impl DecisionNode {
    /// The Batch API.
    ///
    /// ```no_run
    /// # async fn run(dn: decisionnode::DecisionNode) -> decisionnode::Result<()> {
    /// use decisionnode::{truth, BatchRequest, DecideRequest};
    ///
    /// let requests = (0..3).map(|i| {
    ///     BatchRequest::new(format!("t{i}"), DecideRequest::new().state(format!("ticket {i}")).question("refund", truth("Refund?")))
    /// });
    /// let draft = dn.batches().create(requests).await?;
    /// let batch = dn.batches().finalize(&draft.id).await?;
    /// println!("{} is {}", batch.id, batch.state);
    /// # Ok(())
    /// # }
    /// ```
    pub fn batches(&self) -> Batches<'_> {
        Batches { client: self }
    }

    /// Runs a batch from start to end with a poll every 60 seconds and a
    /// 26-hour limit. See [`DecisionNode::run_batch_with`].
    pub async fn run_batch(&self, requests: impl IntoIterator<Item = BatchRequest>) -> Result<BatchRun> {
        self.run_batch_with(requests, DEFAULT_POLL_INTERVAL, DEFAULT_BATCH_TIMEOUT).await
    }

    /// Runs a batch from start to end: checks every request, creates an empty
    /// draft, adds the requests in calls of at most 10 MiB, finalizes, reads
    /// the batch every `poll_interval` (or after the server's `Retry-After`
    /// when that is longer) until it has ended, then returns it with its
    /// results.
    ///
    /// A `poll_interval` below 10 seconds is an argument error, before any
    /// request. If an add fails, the draft is cancelled and the add's error
    /// returned. A batch that has not ended within `timeout` is a
    /// [`ErrorKind::Timeout`] error; the batch keeps running and can be read
    /// with [`Batches::get`].
    pub async fn run_batch_with(
        &self,
        requests: impl IntoIterator<Item = BatchRequest>,
        poll_interval: Duration,
        timeout: Duration,
    ) -> Result<BatchRun> {
        if poll_interval < MIN_POLL_INTERVAL {
            return Err(Error::invalid(format!(
                "poll_interval must be at least {} seconds, got {:.3}",
                MIN_POLL_INTERVAL.as_secs(),
                poll_interval.as_secs_f64()
            )));
        }
        let lines = self.prepare_lines(requests)?;
        let started = Instant::now();
        let batches = self.batches();
        let draft = batches.create_lines(Vec::new()).await?;
        if !lines.is_empty() {
            if let Err(e) = batches.add_lines(&draft.id, lines).await {
                if let Err(cancel) = batches.cancel(&draft.id).await {
                    log::debug!("cancelling draft {} after a failed add: {}", draft.id, cancel.kind());
                }
                return Err(e);
            }
        }
        let mut batch = batches.finalize(&draft.id).await?;
        let mut headers = HeaderMap::new();
        let mut waited = Duration::ZERO;
        while !batch.is_ended() {
            if waited.max(started.elapsed()) >= timeout {
                return Err(Error::new(
                    ErrorKind::Timeout,
                    format!("the batch {} did not end within {:.0} s", batch.id, timeout.as_secs_f64()),
                ));
            }
            let wait = retry_after(&headers).map_or(poll_interval, |server| server.max(poll_interval));
            self.wait(wait).await;
            waited += wait;
            (batch, headers) = batches.get_with_headers(&batch.id).await?;
        }
        let results = batches.results(&batch.id).await?;
        Ok(BatchRun { batch, results })
    }

    /// Checks every request before anything is sent.
    fn prepare_lines(&self, requests: impl IntoIterator<Item = BatchRequest>) -> Result<Vec<Line>> {
        requests
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                let prepared =
                    r.request
                        .prepare(&self.inner.model)
                        .map_err(|e| if e.is_argument() { e.in_request(i) } else { e })?;
                Ok(Line { custom_id: r.custom_id, prepared })
            })
            .collect()
    }

    /// One JSON line: `custom_id`, then the decide body, its local images
    /// inline or uploaded by the threshold counted over the line.
    async fn line_bytes(&self, line: Line) -> Result<Vec<u8>> {
        let Line { custom_id, prepared } = line;
        let mut out = Map::new();
        out.insert("custom_id".into(), Value::String(custom_id));
        out.extend(prepared.body);
        if !prepared.images.is_empty() {
            let placed = self.place_images(&prepared.images, true, false).await?;
            out.insert("images".into(), Value::Array(placed.wires));
        }
        let mut bytes = serde_json::to_vec(&Value::Object(out))
            .map_err(|e| Error::invalid("a batch line cannot be serialised").with_source(e))?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// A path segment with every byte outside the unreserved set escaped.
fn segment(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn ndjson(bytes: Vec<u8>) -> Payload {
    Payload { bytes: bytes.into(), content_type: "application/x-ndjson" }
}

impl Batches<'_> {
    async fn call(&self, method: Method, path: &str, payload: Option<Payload>, policy: Policy) -> Result<Batch> {
        let (batch, _) = self.call_with_headers(method, path, payload, policy).await?;
        Ok(batch)
    }

    async fn call_with_headers(
        &self,
        method: Method,
        path: &str,
        payload: Option<Payload>,
        policy: Policy,
    ) -> Result<(Batch, HeaderMap)> {
        let raw = self.client.request(method, path, payload, policy).await?;
        let invalid = || {
            Error::new(ErrorKind::Server, "the response is not a valid batch").with_response(raw.status, &raw.headers)
        };
        let body: Value = serde_json::from_slice(&raw.body).map_err(|_| invalid())?;
        let batch = Batch::from_value(body).ok_or_else(invalid)?;
        Ok((batch, raw.headers))
    }

    /// `POST /v1/batches`: creates a batch with these requests as JSON Lines,
    /// or an empty draft with none. Every line carries its `custom_id`, the
    /// model (the request's, else the client's) and the state (`""` when the
    /// request has none). Every request is checked before anything is sent.
    pub async fn create(&self, requests: impl IntoIterator<Item = BatchRequest>) -> Result<Batch> {
        let lines = self.client.prepare_lines(requests)?;
        self.create_lines(lines).await
    }

    async fn create_lines(&self, lines: Vec<Line>) -> Result<Batch> {
        let mut body = Vec::new();
        for line in lines {
            body.extend(self.client.line_bytes(line).await?);
        }
        self.call(Method::POST, "/v1/batches", Some(ndjson(body)), Policy::Mutation).await
    }

    /// `POST /v1/batches/{id}/requests`: adds requests to a draft, in calls
    /// of at most 10 MiB each, in order. Returns the batch after the last
    /// call.
    pub async fn add(&self, batch_id: &str, requests: impl IntoIterator<Item = BatchRequest>) -> Result<Batch> {
        let lines = self.client.prepare_lines(requests)?;
        if lines.is_empty() {
            return Err(Error::invalid("add needs at least one request"));
        }
        self.add_lines(batch_id, lines).await
    }

    async fn add_lines(&self, batch_id: &str, lines: Vec<Line>) -> Result<Batch> {
        let path = format!("/v1/batches/{}/requests", segment(batch_id));
        let mut chunk: Vec<u8> = Vec::new();
        let mut last = None;
        for line in lines {
            let bytes = self.client.line_bytes(line).await?;
            if !chunk.is_empty() && chunk.len() + bytes.len() > MAX_ADD_BYTES {
                let body = std::mem::take(&mut chunk);
                last = Some(self.call(Method::POST, &path, Some(ndjson(body)), Policy::Mutation).await?);
            }
            chunk.extend(bytes);
        }
        if !chunk.is_empty() {
            last = Some(self.call(Method::POST, &path, Some(ndjson(chunk)), Policy::Mutation).await?);
        }
        last.ok_or_else(|| Error::invalid("add needs at least one request"))
    }

    /// `POST /v1/batches/{id}/finalize`: queues a draft. A `409`
    /// `invalid_batch_state` is read with [`Batches::get`]: when the batch is
    /// no longer a draft (an earlier finalize went through), the batch is
    /// returned; otherwise the `409` is.
    pub async fn finalize(&self, batch_id: &str) -> Result<Batch> {
        let path = format!("/v1/batches/{}/finalize", segment(batch_id));
        match self.call(Method::POST, &path, None, Policy::Mutation).await {
            Err(e) if e.status() == Some(409) && e.error_type() == Some("invalid_batch_state") => {
                let batch = self.get(batch_id).await?;
                if batch.state != "draft" {
                    Ok(batch)
                } else {
                    Err(e)
                }
            }
            other => other,
        }
    }

    /// `GET /v1/batches/{id}`.
    pub async fn get(&self, batch_id: &str) -> Result<Batch> {
        Ok(self.get_with_headers(batch_id).await?.0)
    }

    async fn get_with_headers(&self, batch_id: &str) -> Result<(Batch, HeaderMap)> {
        let path = format!("/v1/batches/{}", segment(batch_id));
        self.call_with_headers(Method::GET, &path, None, Policy::Default).await
    }

    /// `GET /v1/batches`: one page of the key's batches, newest first.
    /// `offset` defaults to 0 and `limit` to 20 (at most 100) on the server.
    pub async fn list(&self, offset: Option<u64>, limit: Option<u64>) -> Result<BatchPage> {
        let mut query = Vec::new();
        if let Some(o) = offset {
            query.push(format!("offset={o}"));
        }
        if let Some(l) = limit {
            query.push(format!("limit={l}"));
        }
        let path = if query.is_empty() { "/v1/batches".to_owned() } else { format!("/v1/batches?{}", query.join("&")) };
        let reply = self.client.request_json(Method::GET, &path, None, Policy::Default).await?;
        let invalid = || {
            Error::new(ErrorKind::Server, "the response is not a valid batch page")
                .with_response(reply.status, &reply.headers)
        };
        let obj = reply.body.as_object().ok_or_else(invalid)?;
        let batches = obj
            .get("batches")
            .and_then(Value::as_array)
            .ok_or_else(invalid)?
            .iter()
            .map(|b| Batch::from_value(b.clone()).ok_or_else(invalid))
            .collect::<Result<Vec<_>>>()?;
        let n = |k: &str| obj.get(k).and_then(Value::as_u64);
        Ok(BatchPage {
            batches,
            offset: n("offset").unwrap_or(0),
            limit: n("limit").unwrap_or(0),
            total: n("total").unwrap_or(0),
            next_offset: n("next_offset"),
            raw: reply.body.clone(),
        })
    }

    /// `GET /v1/batches/{id}/results`: the results as JSON Lines, one
    /// [`BatchResult`] per request in upload order. A batch that has not
    /// ended is a `409` `results_not_ready`; results deleted 7 days after the
    /// end are a `410` `results_expired` (the base kind with its status).
    pub async fn results(&self, batch_id: &str) -> Result<BatchResults> {
        let path = format!("/v1/batches/{}/results", segment(batch_id));
        let raw = self.client.request(Method::GET, &path, None, Policy::Default).await?;
        Ok(BatchResults { body: raw.body, at: 0 })
    }

    /// `POST /v1/batches/{id}/cancel`: cancels a draft, a queued or a running
    /// batch. Lines already running finish; the batch ends `cancelled` once
    /// they have. Safe to repeat.
    pub async fn cancel(&self, batch_id: &str) -> Result<Batch> {
        let path = format!("/v1/batches/{}/cancel", segment(batch_id));
        self.call(Method::POST, &path, None, Policy::Default).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn batch_objects() {
        let b = Batch::from_value(json!({
            "id": "batch_01J9ZB8Q3T", "state": "completed",
            "counters": {"requests": 3, "queued": 0, "in_progress": 0, "completed": 2, "failed": 1, "expired": 0, "cancelled": 0},
            "created_at": "2026-10-08T12:00:00Z", "finalized_at": "2026-10-08T12:00:05Z",
            "expires_at": "2026-10-09T12:00:05Z", "ended_at": "2026-10-08T12:03:00Z",
            "results_url": "https://api.decisionnode.com/v1/batches/batch_01J9ZB8Q3T/results",
            "results_expire_at": "2026-10-15T12:03:00Z", "price_factor": 0.5,
            "usage": {"input_tokens": 94, "output_tokens": 0}, "error": null
        }))
        .unwrap();
        assert!(b.is_ended());
        assert_eq!(b.counters.completed, 2);
        assert_eq!(b.usage.input_tokens, Some(94));
        assert_eq!(b.error, None);
        let draft = Batch::from_value(json!({"id": "batch_x", "state": "draft", "ended_at": null})).unwrap();
        assert!(!draft.is_ended());
        assert!(Batch::from_value(json!({"state": "draft"})).is_none());
    }

    #[test]
    fn result_lines() {
        let text = concat!(
            r#"{"custom_id":"a","status":200,"body":{"model":"decisionnode-1.0","answers":{"t":{"type":"truth","truth":0.88}},"usage":{"input_tokens":5,"output_tokens":0}},"headers":{"X-Request-Id":"req_1"}}"#,
            "\n\n",
            r#"{"custom_id":"b","status":403,"body":{"detail":{"error_type":"refusal_error","message":"no","category":"harm"}},"headers":{"x-decisionnode-safety":"harm=0.9;blocked"}}"#,
            "\r\n",
            r#"{"custom_id":"c","status":"expired","body":null,"headers":{}}"#,
            "\n",
        );
        let results: Vec<BatchResult> =
            BatchResults { body: Bytes::from(text), at: 0 }.collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].status, BatchStatus::Http(200));
        assert_eq!(results[0].headers["x-request-id"], "req_1");
        let d = results[0].decision.as_ref().unwrap();
        assert_eq!(d["t"].truth(), Some(0.88));
        assert_eq!(d.request_id.as_deref(), Some("req_1"));
        let e = results[1].error.as_ref().unwrap();
        assert_eq!(e.kind(), ErrorKind::Refusal);
        assert_eq!(e.category(), Some("harm"));
        assert_eq!(e.safety().unwrap().flagged, vec!["harm".to_string()]);
        assert_eq!(results[2].status, BatchStatus::Expired);
        assert!(results[2].decision.is_none() && results[2].error.is_none());

        let mut bad = BatchResults { body: Bytes::from_static(b"{\"nope\":1}\n"), at: 0 };
        assert_eq!(bad.next().unwrap().unwrap_err().kind(), ErrorKind::Server);
        assert!(bad.next().is_none());
    }

    #[test]
    fn segments_are_escaped() {
        assert_eq!(segment("batch_01J9ZB8Q3T"), "batch_01J9ZB8Q3T");
        assert_eq!(segment("a/b c"), "a%2Fb%20c");
    }
}
