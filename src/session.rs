//! Sessions: a fixed context opened once, then a stream of frames over a
//! WebSocket, each answered against the context and the latest frames.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, Stream, StreamExt};
use futures_util::SinkExt;
use indexmap::IndexMap;
use reqwest::header::{HeaderValue, AUTHORIZATION, USER_AGENT};
use reqwest::Method;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::check::{check_session_open, nesting_depth, object_depth, usage_error, MAX_NESTING};
use crate::client::{build_questions, is_loopback, DecisionNode, Policy, USER_AGENT_VALUE};
use crate::error::{Error, ErrorKind, Result};
use crate::image::{check_images, Image};
use crate::question::Question;
use crate::response::{Answer, Usage};
use crate::safety::Safety;

/// The close codes after which a session is gone and a new one is opened
/// with the same body: `1001`, `1006`, `1011`, `4404`, `4408`, `4409`, `4410`.
pub const REOPEN_CODES: [u16; 7] = [1001, 1006, 1011, 4404, 4408, 4409, 4410];

/// The largest `seq` a frame may carry: 2^53 - 1.
const MAX_SEQ: i64 = (1 << 53) - 1;

/// How long [`DecisionNode::supervise`] waits for the outstanding replies
/// once its feed has ended.
const SUPERVISE_DRAIN: Duration = Duration::from_secs(3);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What [`DecisionNode::open_session`] opens: the fixed part of a session.
///
/// ```
/// use decisionnode::{choice, truth, SessionOptions};
/// use serde_json::json;
///
/// let options = SessionOptions::new()
///     .instructions("You are the mission supervisor of a survey drone.")
///     .state(json!({"mission": "Survey the north field"}))
///     .question("mode", choice("Which flight mode now?", ["continue", "hold", "return", "land"]))
///     .question("abort", truth("Should the mission be aborted now?"))
///     .window(4);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionOptions {
    state: Option<std::result::Result<Value, String>>,
    instructions: Option<std::result::Result<Value, String>>,
    questions: Vec<(String, Question)>,
    model: Option<String>,
    window: Option<i64>,
    ttl_seconds: Option<i64>,
}

fn json_of(value: impl Serialize, what: &str) -> std::result::Result<Value, String> {
    serde_json::to_value(value).map_err(|e| format!("{what} cannot be serialised to JSON: {e}"))
}

impl SessionOptions {
    /// Nothing set. Add at least one question.
    pub fn new() -> SessionOptions {
        SessionOptions::default()
    }

    /// The static context every frame is answered with: a string, or any
    /// value that serialises to JSON.
    pub fn state(mut self, state: impl Serialize) -> SessionOptions {
        self.state = Some(json_of(state, "the state"));
        self
    }

    /// Who the model decides as.
    pub fn instructions(mut self, instructions: impl Serialize) -> SessionOptions {
        self.instructions = Some(json_of(instructions, "the instructions"));
        self
    }

    /// Adds a question under an id. A frame names questions by these ids.
    pub fn question(mut self, id: impl Into<String>, question: impl Into<Question>) -> SessionOptions {
        let id = id.into();
        let question = question.into();
        match self.questions.iter_mut().find(|(k, _)| *k == id) {
            Some(slot) => slot.1 = question,
            None => self.questions.push((id, question)),
        }
        self
    }

    /// Adds several questions.
    pub fn questions<I, K, Q>(mut self, questions: I) -> SessionOptions
    where
        I: IntoIterator<Item = (K, Q)>,
        K: Into<String>,
        Q: Into<Question>,
    {
        for (id, q) in questions {
            self = self.question(id, q);
        }
        self
    }

    /// The model, over the client's. An empty string counts as not given.
    pub fn model(mut self, model: impl Into<String>) -> SessionOptions {
        self.model = Some(model.into()).filter(|m| !m.is_empty());
        self
    }

    /// How many frames each frame is answered with, itself included (1 to
    /// 64, default 8 on the server).
    pub fn window(mut self, window: i64) -> SessionOptions {
        self.window = Some(window);
        self
    }

    /// The session's lifetime in seconds from the open (1 to 3,600, default
    /// 300 on the server).
    pub fn ttl_seconds(mut self, ttl_seconds: i64) -> SessionOptions {
        self.ttl_seconds = Some(ttl_seconds);
        self
    }

    /// Builds the open body and checks it: an empty question map is an
    /// argument error; a body the API would refuse is its own error.
    fn body(self, client: &DecisionNode) -> Result<Value> {
        let questions = build_questions(self.questions)?;
        let mut body = Map::new();
        let model = self.model.unwrap_or_else(|| client.inner.model.clone());
        let limits = client.limits(&model);
        body.insert("model".into(), Value::String(model));
        if let Some(i) = self.instructions {
            body.insert("instructions".into(), i.map_err(Error::invalid)?);
        }
        if let Some(s) = self.state {
            body.insert("state".into(), s.map_err(Error::invalid)?);
        }
        body.insert("questions".into(), Value::Object(questions));
        if let Some(w) = self.window {
            body.insert("window".into(), w.into());
        }
        if let Some(t) = self.ttl_seconds {
            body.insert("ttl_seconds".into(), t.into());
        }
        check_session_open(&body, object_depth(&body), &limits)?;
        Ok(Value::Object(body))
    }
}

/// One frame of a session: a reading, an image, or both.
///
/// ```
/// use decisionnode::Frame;
/// use serde_json::json;
///
/// let frame = Frame::new(json!({"battery": 0.31, "wind_mps": 11.4}));
/// let only_abort = Frame::new("gust warning").questions(["abort"]);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    frame: Option<std::result::Result<Value, String>>,
    image: Option<Image>,
    questions: Option<Vec<String>>,
    reset: Option<bool>,
    seq: Option<i64>,
}

impl Frame {
    /// A frame holding this reading: a string, or any value that serialises
    /// to JSON.
    pub fn new(frame: impl Serialize) -> Frame {
        Frame::default().frame(frame)
    }

    /// A frame holding an image only.
    pub fn image_only(image: Image) -> Frame {
        Frame::default().image(image)
    }

    /// The reading, or a note beside an image.
    pub fn frame(mut self, frame: impl Serialize) -> Frame {
        self.frame = Some(json_of(frame, "the frame"));
        self
    }

    /// One image: local bytes (inline under the client's inline threshold,
    /// uploaded at or above it, by its cached upload id when the client
    /// uploaded it before) or an [`Image::upload_id`]. A frame takes no URL.
    pub fn image(mut self, image: Image) -> Frame {
        self.image = Some(image);
        self
    }

    /// Answer only these questions of the session, by id.
    pub fn questions<I, S>(mut self, ids: I) -> Frame
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.questions = Some(ids.into_iter().map(Into::into).collect());
        self
    }

    /// Empty the window before this frame is answered.
    pub fn reset(mut self, reset: bool) -> Frame {
        self.reset = Some(reset);
        self
    }

    /// The frame's own number, carried back in its reply. By default the
    /// session counts from 1.
    pub fn seq(mut self, seq: i64) -> Frame {
        self.seq = Some(seq);
        self
    }
}

/// The answer to one frame.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Reply {
    /// The `seq` of the frame answered.
    pub seq: Option<u64>,
    /// The answers, by question id, typed as a decision's.
    pub answers: IndexMap<String, Answer>,
    /// The frames this one superseded since the previous reply (latest wins),
    /// as received; `None` when there were none.
    pub dropped: Option<Vec<u64>>,
    /// What this frame billed.
    pub usage: Usage,
    /// The server's time for this frame.
    pub latency_ms: Option<f64>,
    /// The safety check on this frame.
    pub safety: Option<Safety>,
    /// The reply as received.
    pub raw: Value,
}

/// A frame the session could not answer. A value, never raised: the session
/// goes on.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct FrameError {
    /// The frame's `seq`; `None` when the message could not be read as a frame.
    pub seq: Option<u64>,
    /// `api_usage_error`, `rate_limit_error`, `overloaded_error`, `api_error`,
    /// `refusal_error`, or an upload error of an image.
    pub error_type: Option<String>,
    /// What went wrong, in words.
    pub message: String,
    /// For `refusal_error`: `harm` or `self_harm`.
    pub category: Option<String>,
    /// For `rate_limit_error` and `overloaded_error`: when to send again.
    pub retry_after_ms: Option<u64>,
    /// For `refusal_error`: what the refused frame billed.
    pub usage: Option<Usage>,
    /// For `refusal_error`: the safety check.
    pub safety: Option<Safety>,
    /// The message as received.
    pub raw: Value,
}

/// The end of a session's socket, with its close code.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionClosed {
    /// The close code, `1006` when the connection was lost without one.
    pub code: u16,
    /// The close reason, short text for logs.
    pub reason: String,
    /// True when the session is gone and a new one should be opened with the
    /// same body ([`REOPEN_CODES`]). False for `1013` (the session is kept:
    /// connect to it again) and for the codes that need a fix first.
    pub reopen: bool,
}

impl SessionClosed {
    fn new(code: u16, reason: impl Into<String>) -> SessionClosed {
        SessionClosed { code, reason: reason.into(), reopen: REOPEN_CODES.contains(&code) }
    }
}

impl fmt::Display for SessionClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session closed with {}", self.code)?;
        if !self.reason.is_empty() {
            write!(f, ": {}", self.reason)?;
        }
        Ok(())
    }
}

impl std::error::Error for SessionClosed {}

/// What a session's socket delivers.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SessionEvent {
    /// The answer to a frame.
    Reply(Reply),
    /// A frame that could not be answered.
    FrameError(FrameError),
    /// The socket closed. Always the last event.
    Closed(SessionClosed),
}

impl SessionEvent {
    fn parse(text: &str) -> SessionEvent {
        let raw: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let seq = raw.get("seq").and_then(Value::as_u64);
        if let Some(err) = raw.get("error") {
            let field = |k: &str| err.get(k).and_then(Value::as_str).map(str::to_owned);
            return SessionEvent::FrameError(FrameError {
                seq,
                error_type: field("error_type"),
                message: field("message").unwrap_or_default(),
                category: field("category"),
                retry_after_ms: raw.get("retry_after_ms").and_then(Value::as_u64),
                usage: raw.get("usage").map(|u| Usage::from_body(Some(u))),
                safety: raw.get("safety").and_then(Safety::from_body),
                raw,
            });
        }
        SessionEvent::Reply(Reply {
            seq,
            answers: answers_of(&raw),
            dropped: raw.get("dropped").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect()),
            usage: Usage::from_body(raw.get("usage")),
            latency_ms: raw.get("latency_ms").and_then(Value::as_f64),
            safety: raw.get("safety").and_then(Safety::from_body),
            raw,
        })
    }
}

fn answers_of(raw: &Value) -> IndexMap<String, Answer> {
    raw.get("answers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(id, a)| (id.clone(), Answer::parse(a.clone())))
        .collect()
}

/// An open session, from [`DecisionNode::open_session`].
///
/// Call [`Session::connect`], then [`Session::send`] frames and read
/// [`Session::next_event`] (or the [`Session::replies`] stream) from as many
/// tasks as you like: sending never waits for a reply. Pings are answered
/// on their own. [`Session::end`] ends the session.
pub struct Session {
    client: DecisionNode,
    raw: Value,
    id: String,
    stream_url: String,
    answers: IndexMap<String, Answer>,
    seq: AtomicU64,
    sink: tokio::sync::Mutex<Option<SplitSink<Socket, Message>>>,
    events: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<SessionEvent>>>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    closed: Mutex<Option<SessionClosed>>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session").field("id", &self.id).field("stream_url", &self.stream_url).finish_non_exhaustive()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
            reader.abort();
        }
    }
}

impl Session {
    fn from_open(
        client: DecisionNode,
        raw: Value,
        status: u16,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<Session> {
        let text = |k: &str| raw.get(k).and_then(Value::as_str).map(str::to_owned);
        let (Some(id), Some(stream_url)) = (text("session_id"), text("stream_url")) else {
            return Err(
                Error::new(ErrorKind::Server, "the response is not a valid session").with_response(status, headers)
            );
        };
        Ok(Session {
            answers: answers_of(&raw),
            client,
            id,
            stream_url,
            raw,
            seq: AtomicU64::new(0),
            sink: tokio::sync::Mutex::new(None),
            events: tokio::sync::Mutex::new(None),
            reader: Mutex::new(None),
            closed: Mutex::new(None),
        })
    }

    /// The session id, `ses_` and 24 hex digits.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The URL of the session's socket.
    pub fn stream_url(&self) -> &str {
        &self.stream_url
    }

    /// The pinned model version that answers every frame.
    pub fn model(&self) -> Option<&str> {
        self.raw.get("model").and_then(Value::as_str)
    }

    /// The window in force.
    pub fn window(&self) -> Option<u64> {
        self.raw.get("window").and_then(Value::as_u64)
    }

    /// The lifetime in force, in seconds.
    pub fn ttl_seconds(&self) -> Option<u64> {
        self.raw.get("ttl_seconds").and_then(Value::as_u64)
    }

    /// When the session ends at the latest, RFC 3339 UTC.
    pub fn expires_at(&self) -> Option<&str> {
        self.raw.get("expires_at").and_then(Value::as_str)
    }

    /// The session's limits: `max_frame_tokens`, `max_frame_images`,
    /// `window_tokens`, `idle_timeout_seconds`.
    pub fn limits(&self) -> Option<&Map<String, Value>> {
        self.raw.get("limits").and_then(Value::as_object)
    }

    /// The question ids, in request order.
    pub fn questions(&self) -> Vec<&str> {
        self.raw.get("questions").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect()
    }

    /// What the open billed.
    pub fn usage(&self) -> Usage {
        Usage::from_body(self.raw.get("usage"))
    }

    /// Every question answered on the fixed part, with no frame.
    pub fn answers(&self) -> &IndexMap<String, Answer> {
        &self.answers
    }

    /// The safety check on the fixed part.
    pub fn safety(&self) -> Option<Safety> {
        self.raw.get("safety").and_then(Safety::from_body)
    }

    /// The open response as received.
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    /// Opens the session's socket, with the key in the handshake's
    /// `Authorization` header (never in the URL) and the client's own
    /// headers. A session closed with `1013` can be connected again.
    ///
    /// A handshake the server completes and then closes (an unknown key, an
    /// ended session, a socket already attached) is reported as the first
    /// event, [`SessionEvent::Closed`] with its code.
    ///
    /// The key goes only to the API's own host: a stream URL on another host
    /// than the client's base URL, or not `wss` (`ws` only for the local
    /// machine), is an argument error and nothing is sent. The socket connects
    /// directly, with the platform's root certificates; the proxy and the
    /// certificates of [`crate::HttpSettings`] apply to HTTP calls only.
    pub async fn connect(&self) -> Result<()> {
        let inner = &*self.client.inner;
        let url = reqwest::Url::parse(&self.stream_url)
            .map_err(|e| Error::invalid(format!("the stream URL is not valid: {e}")).with_source(e))?;
        match url.scheme() {
            "wss" => {}
            "ws" if is_loopback(&url) => {}
            other => {
                return Err(Error::invalid(format!(
                    "the stream URL must use wss (ws only for localhost, 127.0.0.1 and ::1), not {other}"
                )))
            }
        }
        let base_host = reqwest::Url::parse(&inner.base_url).ok().and_then(|b| b.host_str().map(str::to_owned));
        if url.host_str() != base_host.as_deref() {
            return Err(Error::invalid(format!(
                "the stream URL's host {:?} is not the API's ({:?}); the key is not sent there",
                url.host_str().unwrap_or(""),
                base_host.unwrap_or_default()
            )));
        }
        let mut request = self
            .stream_url
            .as_str()
            .into_client_request()
            .map_err(|e| Error::invalid(format!("the stream URL is not valid: {e}")).with_source(e))?;
        let headers = request.headers_mut();
        headers.insert(AUTHORIZATION, self.client.authorization()?);
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        for (name, value) in &inner.headers {
            headers.append(name.clone(), value.clone());
        }

        let handshake = tokio_tungstenite::connect_async_tls_with_config(request, None, true, None);
        let (socket, _) = match tokio::time::timeout(inner.timeout, handshake).await {
            Err(_) => {
                return Err(Error::new(
                    ErrorKind::Timeout,
                    format!("no handshake within {:.1} s", inner.timeout.as_secs_f64()),
                ))
            }
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
                let status = response.status().as_u16();
                let body = response.body().clone().unwrap_or_default();
                return Err(Error::from_response(status, response.headers(), &body));
            }
            Ok(Err(e)) => {
                return Err(
                    Error::new(ErrorKind::Connection, format!("the session socket could not open: {e}")).with_source(e)
                )
            }
            Ok(Ok(pair)) => pair,
        };

        let (sink, mut stream) = socket.split();
        let (tx, rx) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            // Pings are answered by the socket while it is read.
            let closed = loop {
                match stream.next().await {
                    Some(Ok(Message::Text(text))) => {
                        if tx.send(SessionEvent::parse(text.as_str())).is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        break match frame {
                            Some(f) => SessionClosed::new(u16::from(f.code), f.reason.as_str()),
                            None => SessionClosed::new(1005, ""),
                        };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break SessionClosed::new(1006, ""),
                }
            };
            let _ = tx.send(SessionEvent::Closed(closed));
        });
        *self.sink.lock().await = Some(sink);
        *self.events.lock().await = Some(rx);
        *self.closed.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(old) = self.reader.lock().unwrap_or_else(|e| e.into_inner()).replace(reader) {
            old.abort();
        }
        Ok(())
    }

    /// Sends one frame and returns its `seq`, without waiting for the reply.
    ///
    /// The `seq` counts from 1 unless the frame sets one. A local image is
    /// checked as in a request and sent inline or uploaded by the client's
    /// inline threshold; a URL is refused locally, since frames take none.
    pub async fn send(&self, frame: Frame) -> Result<u64> {
        let value = frame.frame.transpose().map_err(Error::invalid)?;
        self.check_frame(frame.seq, value.as_ref(), frame.image.as_ref(), frame.questions.as_deref())?;
        let seq = match frame.seq {
            Some(s) => s as u64,
            None => self.seq.fetch_add(1, Ordering::Relaxed) + 1,
        };
        let mut message = Map::new();
        message.insert("seq".into(), seq.into());
        if let Some(f) = value {
            message.insert("frame".into(), f);
        }
        if let Some(image) = frame.image {
            let placed = self.client.place_images(std::slice::from_ref(&image), false, false).await?;
            message.insert("image".into(), placed.wires.into_iter().next().unwrap_or(Value::Null));
        }
        if let Some(q) = frame.questions {
            message.insert("questions".into(), q.into());
        }
        if let Some(r) = frame.reset {
            message.insert("reset".into(), r.into());
        }
        let text = Value::Object(message).to_string();
        let mut sink = self.sink.lock().await;
        let sink = sink.as_mut().ok_or_else(|| Error::invalid("the session is not connected; call connect first"))?;
        sink.send(Message::text(text)).await.map_err(|e| {
            Error::new(ErrorKind::Connection, format!("the frame could not be sent: {e}")).with_source(e)
        })?;
        Ok(seq)
    }

    /// A frame's fixed rules, checked before anything is sent: the frame
    /// error the server would send, as a [`ErrorKind::BadRequest`] error
    /// (`api_usage_error`). Nothing goes on the socket and no `seq` is used.
    fn check_frame(
        &self,
        seq: Option<i64>,
        frame: Option<&Value>,
        image: Option<&Image>,
        questions: Option<&[String]>,
    ) -> Result<()> {
        if seq.is_some_and(|s| !(0..=MAX_SEQ).contains(&s)) {
            return Err(usage_error(format!("seq: must be an integer from 0 to {MAX_SEQ}.")));
        }
        if frame.is_none() && image.is_none() {
            return Err(usage_error("frame: a frame needs a frame value or an image."));
        }
        if frame.is_some_and(|f| !(f.is_string() || f.is_object() || f.is_array()) || f == "") {
            return Err(usage_error("frame: must be a string, an object or an array, not empty."));
        }
        if let Some(ids) = questions {
            if ids.is_empty() {
                return Err(usage_error("questions: must be a non-empty list of the session's question ids."));
            }
            let known = self.questions();
            if self.raw.get("questions").is_some_and(Value::is_array) {
                if let Some(unknown) = ids.iter().find(|id| !known.contains(&id.as_str())) {
                    return Err(usage_error(format!("questions: unknown question id '{unknown}'")));
                }
            }
        }
        if frame.is_some_and(|f| nesting_depth(f) + 1 > MAX_NESTING) {
            return Err(usage_error(format!(
                "The frame nests more than {MAX_NESTING} objects and arrays inside each other."
            )));
        }
        if let Some(image) = image {
            if image.is_url() {
                return Err(usage_error("image: a frame takes bytes or an upload, not a url."));
            }
            let model = self.model().map(str::to_owned).unwrap_or_else(|| self.client.inner.model.clone());
            check_images(std::slice::from_ref(image), &self.client.limits(&model))?;
        }
        Ok(())
    }

    /// The next event: a [`Reply`], a [`FrameError`], or
    /// [`SessionEvent::Closed`], which is the last and is returned again on
    /// every later call.
    pub async fn next_event(&self) -> Result<SessionEvent> {
        if let Some(closed) = self.closed() {
            return Ok(SessionEvent::Closed(closed));
        }
        let mut events = self.events.lock().await;
        let events =
            events.as_mut().ok_or_else(|| Error::invalid("the session is not connected; call connect first"))?;
        let event = events.recv().await.unwrap_or_else(|| SessionEvent::Closed(SessionClosed::new(1006, "")));
        Ok(self.note(event))
    }

    /// [`Session::next_event`] with a limit: `None` when no event came in
    /// time.
    pub async fn next_event_timeout(&self, timeout: Duration) -> Result<Option<SessionEvent>> {
        match tokio::time::timeout(timeout, self.next_event()).await {
            Ok(event) => event.map(Some),
            Err(_) => Ok(None),
        }
    }

    /// An event that has already arrived, without waiting.
    pub fn try_next_event(&self) -> Option<SessionEvent> {
        if let Some(closed) = self.closed() {
            return Some(SessionEvent::Closed(closed));
        }
        let mut events = self.events.try_lock().ok()?;
        let event = events.as_mut()?.try_recv().ok()?;
        Some(self.note(event))
    }

    fn closed(&self) -> Option<SessionClosed> {
        self.closed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn note(&self, event: SessionEvent) -> SessionEvent {
        if let SessionEvent::Closed(c) = &event {
            *self.closed.lock().unwrap_or_else(|e| e.into_inner()) = Some(c.clone());
        }
        event
    }

    /// The events as a stream, ending after [`SessionEvent::Closed`].
    pub fn replies(&self) -> impl Stream<Item = SessionEvent> + '_ {
        futures_util::stream::unfold(false, move |done| async move {
            if done {
                return None;
            }
            let event = self.next_event().await.ok()?;
            let last = matches!(event, SessionEvent::Closed(_));
            Some((event, last))
        })
    }

    /// Ends the session with `DELETE /v1/sessions/{id}`; the server then
    /// closes the socket with `1000`. A session already gone (`404`) counts
    /// as ended.
    pub async fn end(&self) -> Result<()> {
        let path = format!("/v1/sessions/{}", self.id);
        match self.client.request(Method::DELETE, &path, None, Policy::Default).await {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(404) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Closes the socket from this side with `1000`. The session ends on the
    /// server too.
    pub async fn close(&self) {
        if let Some(sink) = self.sink.lock().await.as_mut() {
            let frame = CloseFrame { code: CloseCode::Normal, reason: "".into() };
            let _ = sink.send(Message::Close(Some(frame))).await;
        }
    }
}

impl DecisionNode {
    /// Opens a session with `POST /v1/sessions`: the fixed context and its
    /// questions, answered once with no frame. The model is always sent (the
    /// options', else the client's). Questions are checked as in a request.
    ///
    /// A `429` `session_limit_exceeded` (the key holds as many open sessions
    /// as it may) is never retried: it is an [`ErrorKind::SessionLimit`]
    /// error, a special case of [`ErrorKind::RateLimit`].
    ///
    /// ```no_run
    /// # async fn run(dn: decisionnode::DecisionNode) -> decisionnode::Result<()> {
    /// use decisionnode::{truth, Frame, SessionEvent, SessionOptions};
    /// use serde_json::json;
    ///
    /// let session = dn
    ///     .open_session(SessionOptions::new().state("Survey drone").question("abort", truth("Abort now?")))
    ///     .await?;
    /// session.connect().await?;
    /// session.send(Frame::new(json!({"battery": 0.31}))).await?;
    /// if let SessionEvent::Reply(reply) = session.next_event().await? {
    ///     println!("{:?}", reply.answers["abort"].truth());
    /// }
    /// session.end().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open_session(&self, options: SessionOptions) -> Result<Session> {
        let body = options.body(self)?;
        let reply = self.request_json(Method::POST, "/v1/sessions", Some(&body), Policy::SessionOpen).await?;
        Session::from_open(self.clone(), reply.body, reply.status, &reply.headers)
    }

    /// Keeps a session going for a long-lived feed: opens a session with
    /// `options`, connects, sends each reading of `feed` as it comes (latest
    /// wins on the server) and passes every reply and frame error to
    /// `on_reply`.
    ///
    /// When the socket closes with a reopen code ([`REOPEN_CODES`]) a new
    /// session is opened with the same options before the next reading: at
    /// once after `4408` and `4410`, otherwise after a backoff of 1 s to 30 s
    /// with jitter, reset after a session that answered frames. After `1013`
    /// the same session is connected again after the backoff. Any other code
    /// stops the supervisor, which returns that [`SessionClosed`].
    ///
    /// When the feed ends it waits up to 3 seconds for the outstanding
    /// replies, ends the session and returns the last [`SessionClosed`] it
    /// saw, or `None`.
    pub async fn supervise<S, F>(&self, options: SessionOptions, feed: S, on_reply: F) -> Result<Option<SessionClosed>>
    where
        S: Stream<Item = Frame>,
        F: FnMut(SessionEvent),
    {
        let mut sup = Supervisor {
            client: self,
            options,
            on_reply,
            session: None,
            reconnect: None,
            last: None,
            failures: 0,
            answered: 0,
            pending: 0,
        };
        let mut feed = std::pin::pin!(feed);
        while let Some(reading) = feed.next().await {
            sup.drain(Duration::ZERO).await;
            if let Some(stop) = sup.ensure_session().await? {
                return Ok(Some(stop));
            }
            if let Some(session) = &sup.session {
                session.send(reading).await?;
                sup.pending += 1;
            }
        }
        sup.drain(SUPERVISE_DRAIN).await;
        if let Some(session) = sup.session.take() {
            session.end().await?;
            session.close().await;
        }
        Ok(sup.last)
    }
}

struct Supervisor<'a, F> {
    client: &'a DecisionNode,
    options: SessionOptions,
    on_reply: F,
    session: Option<Session>,
    /// A session closed with `1013`, connected again instead of reopened.
    reconnect: Option<Session>,
    last: Option<SessionClosed>,
    failures: u32,
    answered: usize,
    pending: usize,
}

impl<F: FnMut(SessionEvent)> Supervisor<'_, F> {
    /// Reads the events that have arrived, or waits up to `block` for the
    /// outstanding replies.
    async fn drain(&mut self, block: Duration) {
        let deadline = Instant::now() + block;
        while let Some(session) = &self.session {
            let event = if block.is_zero() {
                match session.try_next_event() {
                    Some(e) => e,
                    None => return,
                }
            } else {
                let left = deadline.saturating_duration_since(Instant::now());
                match session.next_event_timeout(left).await {
                    Ok(Some(e)) => e,
                    _ => return,
                }
            };
            match event {
                SessionEvent::Closed(closed) => {
                    let session = self.session.take();
                    if closed.code == 1013 {
                        self.reconnect = session;
                    }
                    self.last = Some(closed);
                    return;
                }
                other => {
                    let dropped = match &other {
                        SessionEvent::Reply(r) => {
                            self.answered += 1;
                            r.dropped.as_ref().map_or(0, Vec::len)
                        }
                        _ => 0,
                    };
                    self.pending = self.pending.saturating_sub(1 + dropped);
                    (self.on_reply)(other);
                    if !block.is_zero() && self.pending == 0 {
                        return;
                    }
                }
            }
        }
    }

    /// Opens or reconnects until a session is up; returns the close that
    /// stops the supervisor.
    async fn ensure_session(&mut self) -> Result<Option<SessionClosed>> {
        while self.session.is_none() {
            if let Some(last) = &self.last {
                if !last.reopen && last.code != 1013 {
                    return Ok(Some(last.clone()));
                }
                if !matches!(last.code, 4408 | 4410) {
                    if self.answered > 0 {
                        self.failures = 0;
                    }
                    let base = 2f64.powi(self.failures.min(5) as i32).min(30.0);
                    self.client.wait(Duration::from_secs_f64(base * (0.5 + fastrand::f64() * 0.5))).await;
                    self.failures += 1;
                }
            }
            let session = match self.reconnect.take() {
                Some(s) => s,
                None => self.client.open_session(self.options.clone()).await?,
            };
            self.answered = 0;
            self.pending = 0;
            match session.connect().await {
                Ok(()) => {
                    self.session = Some(session);
                    self.last = None;
                }
                Err(e) if e.is_transport() => self.last = Some(SessionClosed::new(1006, e.message())),
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reopen_codes() {
        for code in REOPEN_CODES {
            assert!(SessionClosed::new(code, "").reopen, "{code}");
        }
        for code in [1000, 1008, 1009, 1013, 4401, 4402, 4403] {
            assert!(!SessionClosed::new(code, "").reopen, "{code}");
        }
        assert_eq!(SessionClosed::new(4410, "expired").to_string(), "session closed with 4410: expired");
    }

    #[test]
    fn events_parse() {
        let SessionEvent::Reply(r) = SessionEvent::parse(
            r#"{"seq":3,"answers":{"abort":{"type":"truth","truth":0.07}},"dropped":[2],"usage":{"input_tokens":9,"output_tokens":0},"latency_ms":1.5,"safety":{"checked":true,"harm":0.0021,"self_harm":0.0008,"flagged":[]}}"#,
        ) else {
            panic!("not a reply")
        };
        assert_eq!(r.seq, Some(3));
        assert_eq!(r.dropped, Some(vec![2]));
        assert_eq!(r.answers["abort"].truth(), Some(0.07));
        assert_eq!(r.usage.input_tokens, Some(9));
        assert_eq!(r.safety.unwrap().harm, Some(0.0021));

        let SessionEvent::FrameError(e) = SessionEvent::parse(
            r#"{"seq":null,"error":{"error_type":"api_usage_error","message":"Frames are text messages holding one JSON object."}}"#,
        ) else {
            panic!("not a frame error")
        };
        assert_eq!(e.seq, None);
        assert_eq!(e.error_type.as_deref(), Some("api_usage_error"));
        assert!(e.usage.is_none());
    }

    #[tokio::test]
    async fn the_key_goes_only_to_the_apis_host() {
        let dn = DecisionNode::builder().api_key("k").base_url("http://127.0.0.1:9").build().unwrap();
        let headers = reqwest::header::HeaderMap::new();
        for url in
            ["ws://localhost:9/v1/sessions/s/stream", "wss://elsewhere.example/v1/sessions/s/stream", "ws://10.0.0.1/x"]
        {
            let raw = json!({"session_id": "ses_1", "stream_url": url});
            let session = Session::from_open(dn.clone(), raw, 200, &headers).unwrap();
            assert!(session.connect().await.unwrap_err().is_argument(), "{url}");
        }
        let session = Session::from_open(dn, json!({"session_id": "ses_1"}), 200, &headers).unwrap_err();
        assert_eq!(session.kind(), ErrorKind::Server);
    }

    #[test]
    fn open_bodies_send_only_what_is_given() {
        let dn = DecisionNode::builder().model("decisionnode-latest").build().unwrap();
        let body = SessionOptions::new().question("abort", crate::truth("Abort?")).body(&dn).unwrap();
        assert_eq!(
            body,
            json!({"model": "decisionnode-latest", "questions": {"abort": {"type": "truth", "instructions": "Abort?"}}})
        );
        assert!(SessionOptions::new().body(&dn).unwrap_err().is_argument());
        let local = SessionOptions::new().question("s", crate::score("?", Vec::<String>::new())).body(&dn).unwrap_err();
        assert_eq!(local.kind(), ErrorKind::Validation);
    }
}
