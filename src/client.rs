//! The asynchronous client.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::{self, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use reqwest::Method;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::{header, retry_after, Error, ErrorKind, Result};
use crate::http::HttpSettings;
use crate::image::Image;
use crate::question::{choice, score, truth, Choice, Question, Score, Truth};
use crate::response::{ChoiceAnswer, Decision, DecisionResult, ModelInfo, ScoreAnswer, TruthAnswer};

/// The base URL of the hosted API.
pub const DEFAULT_BASE_URL: &str = "https://api.decisionnode.com";
/// The model used when none is given.
pub const DEFAULT_MODEL: &str = "decisionnode-latest";
/// The timeout of each attempt.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Retries after the first attempt.
pub const DEFAULT_MAX_RETRIES: u32 = 2;
/// How many requests [`DecisionNode::decide_many`] sends at a time.
pub const DEFAULT_CONCURRENCY: usize = 8;

const USER_AGENT_VALUE: &str = concat!("decisionnode-rust/", env!("CARGO_PKG_VERSION"));

/// A wait between attempts, see [`ClientBuilder::sleeper`].
pub type Sleeper = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// An empty string counts as not given.
fn given(value: String) -> Option<String> {
    Some(value).filter(|v| !v.is_empty())
}

// --------------------------------------------------------------------------- request

/// One decision request: a state, questions by id, and optional images and
/// model.
///
/// ```
/// use decisionnode::{choice, truth, DecideRequest};
/// use serde_json::json;
///
/// let request = DecideRequest::new()
///     .state(json!({"ticket": "I was charged twice."}))
///     .question("route", choice("Where should this go?", ["billing", "bug", "account"]))
///     .question("escalate", truth("Escalate to a human now?"));
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecideRequest {
    state: Option<std::result::Result<Value, String>>,
    questions: Vec<(String, Question)>,
    images: Vec<Image>,
    model: Option<String>,
}

impl DecideRequest {
    /// An empty request. Add at least one question.
    pub fn new() -> DecideRequest {
        DecideRequest::default()
    }

    /// What to decide on: a string, or any value that serialises to JSON. A
    /// string is sent as it is. Without a state the request sends `""`.
    pub fn state(mut self, state: impl Serialize) -> DecideRequest {
        self.state =
            Some(serde_json::to_value(state).map_err(|e| format!("the state cannot be serialised to JSON: {e}")));
        self
    }

    /// Adds a question under an id. The id is any string and comes back as the
    /// key of its answer. A later question with the same id replaces it.
    pub fn question(mut self, id: impl Into<String>, question: impl Into<Question>) -> DecideRequest {
        let id = id.into();
        let question = question.into();
        match self.questions.iter_mut().find(|(k, _)| *k == id) {
            Some(slot) => slot.1 = question,
            None => self.questions.push((id, question)),
        }
        self
    }

    /// Adds several questions.
    pub fn questions<I, K, Q>(mut self, questions: I) -> DecideRequest
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

    /// Adds an image.
    pub fn image(mut self, image: Image) -> DecideRequest {
        self.images.push(image);
        self
    }

    /// Adds several images.
    pub fn images(mut self, images: impl IntoIterator<Item = Image>) -> DecideRequest {
        self.images.extend(images);
        self
    }

    /// The model for this request, over the client's. An empty string counts
    /// as not given: the client's model applies.
    pub fn model(mut self, model: impl Into<String>) -> DecideRequest {
        self.model = given(model.into());
        self
    }

    fn into_body(self, default_model: &str) -> Result<Value> {
        if self.questions.is_empty() {
            return Err(Error::invalid("questions must hold at least one question"));
        }
        let state = match self.state {
            None => Value::String(String::new()),
            Some(Ok(Value::Null)) => Value::String(String::new()),
            Some(Ok(v)) => v,
            Some(Err(e)) => return Err(Error::invalid(e)),
        };
        let mut questions = Map::new();
        for (id, q) in self.questions {
            let wire = q.into_value().map_err(|e| Error::invalid(format!("question {id:?}: {e}")))?;
            questions.insert(id, wire);
        }
        let mut body = Map::new();
        body.insert("model".into(), Value::String(self.model.unwrap_or_else(|| default_model.to_owned())));
        body.insert("state".into(), state);
        body.insert("questions".into(), Value::Object(questions));
        if !self.images.is_empty() {
            body.insert("images".into(), self.images.iter().map(Image::to_wire).collect());
        }
        Ok(Value::Object(body))
    }
}

/// Images and a model for the one-question helpers
/// ([`DecisionNode::check_with`] and the others).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallOptions {
    images: Vec<Image>,
    model: Option<String>,
}

impl CallOptions {
    /// No images, the client's model.
    pub fn new() -> CallOptions {
        CallOptions::default()
    }

    /// Adds an image.
    pub fn image(mut self, image: Image) -> CallOptions {
        self.images.push(image);
        self
    }

    /// Adds several images.
    pub fn images(mut self, images: impl IntoIterator<Item = Image>) -> CallOptions {
        self.images.extend(images);
        self
    }

    /// The model for this call, over the client's. An empty string counts as
    /// not given: the client's model applies.
    pub fn model(mut self, model: impl Into<String>) -> CallOptions {
        self.model = given(model.into());
        self
    }

    fn apply(self, mut request: DecideRequest) -> DecideRequest {
        request = request.images(self.images);
        if let Some(m) = self.model {
            request = request.model(m);
        }
        request
    }
}

// --------------------------------------------------------------------------- builder

/// Configures a [`DecisionNode`] client.
///
/// Every setting is optional. The key, base URL and model fall back to
/// `DECISIONNODE_API_KEY`, `DECISIONNODE_BASE_URL` and `DECISIONNODE_MODEL`,
/// then to the defaults. An empty string counts as not given, so the
/// environment, then the default, applies.
#[derive(Default)]
#[must_use]
pub struct ClientBuilder {
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    timeout: Option<std::result::Result<Duration, String>>,
    max_retries: Option<std::result::Result<u32, String>>,
    headers: Vec<(String, String)>,
    http: HttpSettings,
    sleeper: Option<Sleeper>,
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

impl ClientBuilder {
    /// The API key. Default: `DECISIONNODE_API_KEY`.
    pub fn api_key(mut self, key: impl Into<String>) -> ClientBuilder {
        self.api_key = given(key.into());
        self
    }

    /// The base URL, without a path. Default: `DECISIONNODE_BASE_URL`, else
    /// `https://api.decisionnode.com`.
    ///
    /// It must be `https`, so the key never travels unencrypted; `http` is
    /// accepted only for `localhost`, `127.0.0.1` and `::1`. Anything else
    /// fails [`ClientBuilder::build`].
    pub fn base_url(mut self, url: impl Into<String>) -> ClientBuilder {
        self.base_url = given(url.into());
        self
    }

    /// The model sent when a request names none. Default:
    /// `DECISIONNODE_MODEL`, else `decisionnode-latest`.
    pub fn model(mut self, model: impl Into<String>) -> ClientBuilder {
        self.model = given(model.into());
        self
    }

    /// The timeout of each attempt, from sending the request to the last byte
    /// of the response body. Default: 10 seconds. Zero fails
    /// [`ClientBuilder::build`].
    pub fn timeout(mut self, timeout: Duration) -> ClientBuilder {
        self.timeout = Some(Ok(timeout));
        self
    }

    /// [`ClientBuilder::timeout`] as a number of seconds, for example `2.5`.
    /// A value of 0 or below, or one that is not finite, fails
    /// [`ClientBuilder::build`].
    pub fn timeout_secs(mut self, seconds: f64) -> ClientBuilder {
        self.timeout = Some(
            Duration::try_from_secs_f64(seconds)
                .ok()
                .filter(|_| seconds.is_finite())
                .ok_or_else(|| format!("timeout must be above 0 seconds, got {seconds}")),
        );
        self
    }

    /// Retries after the first attempt, on 408, 429, 5xx, timeouts and
    /// connection errors. Default: 2. Set 0 to turn retries off. A negative
    /// value (or one that does not fit a `u32`) fails [`ClientBuilder::build`].
    pub fn max_retries<R: TryInto<u32>>(mut self, retries: R) -> ClientBuilder {
        self.max_retries = Some(retries.try_into().map_err(|_| "max_retries must be 0 or more".to_owned()));
        self
    }

    /// A header sent on every request, beside the client's own. For example
    /// `x-decisionnode-safety-body: true` adds the safety result to the
    /// response body.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> ClientBuilder {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Several headers, see [`ClientBuilder::header`].
    pub fn headers<I, K, V>(mut self, headers: I) -> ClientBuilder
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (k, v) in headers {
            self = self.header(k, v);
        }
        self
    }

    /// Transport settings for the HTTP client the SDK builds: a proxy, your
    /// own root certificates, a client certificate, connection pool and socket
    /// options. See [`HttpSettings`].
    ///
    /// The SDK does not take an HTTP client of your making, because it could
    /// not show that such a client refuses redirects and keeps TLS checks on.
    /// Every setting [`HttpSettings`] offers keeps both: the client never
    /// follows a redirect, so the key is never sent to another URL or over
    /// plain HTTP, and certificate and hostname checks stay on.
    ///
    /// ```
    /// use decisionnode::HttpSettings;
    ///
    /// let dn = decisionnode::DecisionNode::builder()
    ///     .http_settings(HttpSettings::new().pool_max_idle_per_host(4))
    ///     .build()?;
    /// # Ok::<(), decisionnode::Error>(())
    /// ```
    pub fn http_settings(mut self, settings: HttpSettings) -> ClientBuilder {
        self.http = settings;
        self
    }

    /// Replaces the wait between attempts. The function gets each wait the
    /// client schedules and returns the future to await; the default is
    /// `tokio::time::sleep`. Meant for tests, to record the waits without
    /// spending them.
    ///
    /// ```
    /// use std::sync::{Arc, Mutex};
    /// use std::time::Duration;
    ///
    /// let waits = Arc::new(Mutex::new(Vec::<Duration>::new()));
    /// let seen = waits.clone();
    /// let dn = decisionnode::DecisionNode::builder()
    ///     .sleeper(move |d| {
    ///         seen.lock().unwrap().push(d);
    ///         Box::pin(async {})
    ///     })
    ///     .build()?;
    /// # Ok::<(), decisionnode::Error>(())
    /// ```
    pub fn sleeper<F>(mut self, sleeper: F) -> ClientBuilder
    where
        F: Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync + 'static,
    {
        self.sleeper = Some(Arc::new(sleeper));
        self
    }

    /// Builds the client. Fails, with an error of the base kind and no
    /// status, on a header name or value that is not valid, a base URL that is
    /// not `https` (plain `http` only for `localhost`, `127.0.0.1` and `::1`),
    /// a timeout of zero or below and a negative `max_retries`. A missing API key is reported by the first call,
    /// before it sends.
    pub fn build(self) -> Result<DecisionNode> {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            let n = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| Error::invalid(format!("header name {name:?} is not valid")).with_source(e))?;
            let v = HeaderValue::from_str(value)
                .map_err(|e| Error::invalid(format!("the value of header {name:?} is not valid")).with_source(e))?;
            headers.append(n, v);
        }
        let base_url = self
            .base_url
            .or_else(|| env("DECISIONNODE_BASE_URL"))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
            .trim_end_matches('/')
            .to_owned();
        check_base_url(&base_url)?;
        let timeout = self.timeout.unwrap_or(Ok(DEFAULT_TIMEOUT)).map_err(Error::invalid)?;
        if timeout.is_zero() {
            return Err(Error::invalid("timeout must be above 0"));
        }
        let max_retries = self.max_retries.unwrap_or(Ok(DEFAULT_MAX_RETRIES)).map_err(Error::invalid)?;
        let http = self.http.build(base_url.starts_with("https:"))?;
        Ok(DecisionNode {
            inner: Arc::new(Inner {
                api_key: self.api_key.or_else(|| env("DECISIONNODE_API_KEY")),
                base_url,
                model: self.model.or_else(|| env("DECISIONNODE_MODEL")).unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
                timeout,
                max_retries,
                headers,
                http,
                sleeper: self.sleeper,
            }),
        })
    }

    /// Builds a [`crate::blocking::DecisionNode`] with these settings.
    #[cfg(feature = "blocking")]
    pub fn build_blocking(self) -> Result<crate::blocking::DecisionNode> {
        crate::blocking::DecisionNode::from_async(self.build()?)
    }
}

// --------------------------------------------------------------------------- client

/// The DecisionNode client.
///
/// Cheap to clone and safe to share between tasks and threads: clones share
/// one connection pool.
///
/// ```no_run
/// use decisionnode::{choice, score, truth, DecideRequest, DecisionNode};
/// use serde_json::json;
///
/// # async fn run() -> decisionnode::Result<()> {
/// let dn = DecisionNode::new(); // reads DECISIONNODE_API_KEY
///
/// let d = dn
///     .decide(
///         DecideRequest::new()
///             .state(json!({"ticket": "I was charged twice and nobody has replied for 3 days."}))
///             .question("route", choice("Where should this go?", [("billing", "money"), ("bug", "broken"), ("account", "login")]))
///             .question("urgency", score("How urgent is this?", ["routine", "today", "urgent", "critical"]))
///             .question("escalate", truth("Escalate to a human now?")),
///     )
///     .await?;
///
/// println!("{:?}", d["route"].choice());
/// println!("{:?}", d["escalate"].truth());
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct DecisionNode {
    inner: Arc<Inner>,
}

struct Inner {
    api_key: Option<String>,
    base_url: String,
    model: String,
    timeout: Duration,
    max_retries: u32,
    headers: HeaderMap,
    http: reqwest::Client,
    sleeper: Option<Sleeper>,
}

impl fmt::Debug for DecisionNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionNode")
            .field("base_url", &self.inner.base_url)
            .field("model", &self.inner.model)
            .field("api_key", &self.inner.api_key.as_ref().map(|_| "<set>"))
            .field("timeout", &self.inner.timeout)
            .field("max_retries", &self.inner.max_retries)
            .finish()
    }
}

impl Default for DecisionNode {
    fn default() -> DecisionNode {
        DecisionNode::new()
    }
}

impl DecisionNode {
    /// A client configured from the environment and the defaults.
    ///
    /// # Panics
    ///
    /// When the HTTP client cannot be created (the TLS backend fails to
    /// initialise). Use [`DecisionNode::builder`] to handle that as an error.
    pub fn new() -> DecisionNode {
        DecisionNode::builder().build().expect("the DecisionNode client could not be created")
    }

    /// A builder to configure the client.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// The base URL requests go to.
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// The model sent when a request names none.
    pub fn model(&self) -> &str {
        &self.inner.model
    }

    /// Sends one decision request.
    pub async fn decide(&self, request: DecideRequest) -> Result<Decision> {
        let body = request.into_body(&self.inner.model)?;
        Ok(self.decide_body(body).await?.decision)
    }

    async fn decide_body(&self, body: Value) -> Result<Answered> {
        let reply = self.send(Method::POST, "/v1/decide", Some(&body)).await?;
        let decision = Decision::from_response(reply.body, reply.status, &reply.headers)?;
        Ok(Answered { decision, status: reply.status, headers: reply.headers })
    }

    /// Sends many decision requests, at most 8 at a time. See
    /// [`DecisionNode::decide_many_with`].
    pub async fn decide_many<I>(&self, requests: I) -> Result<Vec<DecisionResult>>
    where
        I: IntoIterator<Item = DecideRequest>,
    {
        self.run_many(requests, DEFAULT_CONCURRENCY).await
    }

    /// Sends many decision requests, at most `concurrency` at a time.
    ///
    /// Every request is checked before any is sent: an invalid one (an empty
    /// question map, a state or question that cannot be serialised) is an
    /// error of the base kind ([`Error::is_argument`]) and nothing is sent. So
    /// is a `concurrency` below 1 (or one that does not fit a `usize`).
    ///
    /// Otherwise returns one result per request, in input order: the
    /// decision, or the error that request ended with. One failure does not
    /// stop the others.
    pub async fn decide_many_with<I, C>(&self, requests: I, concurrency: C) -> Result<Vec<DecisionResult>>
    where
        I: IntoIterator<Item = DecideRequest>,
        C: TryInto<usize>,
    {
        let concurrency = check_concurrency(concurrency)?;
        self.run_many(requests, concurrency).await
    }

    async fn run_many<I>(&self, requests: I, concurrency: usize) -> Result<Vec<DecisionResult>>
    where
        I: IntoIterator<Item = DecideRequest>,
    {
        let bodies = requests
            .into_iter()
            .enumerate()
            .map(|(i, r)| r.into_body(&self.inner.model).map_err(|e| e.in_request(i)))
            .collect::<Result<Vec<_>>>()?;
        Ok(stream::iter(bodies)
            .map(|b| async move { self.decide_body(b).await.map(|a| a.decision) })
            .buffered(concurrency)
            .collect()
            .await)
    }

    /// Sends a one-question helper request and returns its decision with the
    /// response's status and headers, for `take_answer`.
    async fn ask(&self, request: DecideRequest) -> Result<Answered> {
        let body = request.into_body(&self.inner.model)?;
        self.decide_body(body).await
    }

    /// Asks one `truth` question, under the id `check`.
    ///
    /// `question` is the instructions as a string, or a [`truth`] builder with
    /// criteria.
    pub async fn check(&self, state: impl Serialize, question: impl Into<Truth>) -> Result<TruthAnswer> {
        self.check_with(state, question, CallOptions::new()).await
    }

    /// [`DecisionNode::check`] with images and a model, like
    /// [`DecisionNode::decide`].
    pub async fn check_with(
        &self,
        state: impl Serialize,
        question: impl Into<Truth>,
        options: CallOptions,
    ) -> Result<TruthAnswer> {
        let request = options.apply(DecideRequest::new().state(state).question("check", question.into()));
        take_answer(self.ask(request).await?, "check", crate::Answer::as_truth)
    }

    /// Asks one `choice` question, under the id `classify`.
    pub async fn classify(&self, state: impl Serialize, question: Choice) -> Result<ChoiceAnswer> {
        self.classify_with(state, question, CallOptions::new()).await
    }

    /// [`DecisionNode::classify`] with images and a model, like
    /// [`DecisionNode::decide`].
    pub async fn classify_with(
        &self,
        state: impl Serialize,
        question: Choice,
        options: CallOptions,
    ) -> Result<ChoiceAnswer> {
        let request = options.apply(DecideRequest::new().state(state).question("classify", question));
        take_answer(self.ask(request).await?, "classify", crate::Answer::as_choice)
    }

    /// Asks one `score` question, under the id `score`.
    pub async fn score(&self, state: impl Serialize, question: Score) -> Result<ScoreAnswer> {
        self.score_with(state, question, CallOptions::new()).await
    }

    /// [`DecisionNode::score`] with images and a model, like
    /// [`DecisionNode::decide`].
    pub async fn score_with(
        &self,
        state: impl Serialize,
        question: Score,
        options: CallOptions,
    ) -> Result<ScoreAnswer> {
        let request = options.apply(DecideRequest::new().state(state).question("score", question));
        take_answer(self.ask(request).await?, "score", crate::Answer::as_score)
    }

    /// Judges an action before it runs, with three fixed questions, worded
    /// the same in every DecisionNode SDK (see [`gate_questions`]): `risk` (a
    /// score from none to severe), `touches_production` (a truth) and
    /// `verdict` (a choice of `allow`, `confirm` or `block`).
    ///
    /// The state is `{"action": action, "context": context}`; a `null`
    /// context is left out.
    pub async fn gate(&self, action: impl Serialize, context: impl Serialize) -> Result<Decision> {
        self.gate_with(action, context, CallOptions::new()).await
    }

    /// [`DecisionNode::gate`] with images and a model, like
    /// [`DecisionNode::decide`].
    pub async fn gate_with(
        &self,
        action: impl Serialize,
        context: impl Serialize,
        options: CallOptions,
    ) -> Result<Decision> {
        let mut state = Map::new();
        state.insert("action".into(), to_value(action, "the action")?);
        let context = to_value(context, "the context")?;
        if !context.is_null() {
            state.insert("context".into(), context);
        }
        let request = options.apply(DecideRequest::new().state(Value::Object(state)).questions(gate_questions()));
        self.decide(request).await
    }

    /// Lists the models the key may use, with their published limits.
    pub async fn models(&self) -> Result<Vec<ModelInfo>> {
        let reply = self.send(Method::GET, "/v1/models", None).await?;
        ModelInfo::list_from(reply.body, reply.status, &reply.headers)
    }

    /// Sends a body to `POST /v1/decide` exactly as given and returns the
    /// response body as parsed JSON. The way to use an API field that this
    /// version of the SDK does not know yet.
    pub async fn decide_raw(&self, body: impl Serialize) -> Result<Value> {
        let body = to_value(body, "the body")?;
        Ok(self.send(Method::POST, "/v1/decide", Some(&body)).await?.body)
    }

    // ----------------------------------------------------------------------- transport

    async fn send(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Reply> {
        let inner = &*self.inner;
        let key = inner.api_key.as_deref().filter(|k| !k.is_empty()).ok_or_else(|| {
            Error::new(ErrorKind::Authentication, "no API key: pass one to the client or set DECISIONNODE_API_KEY")
        })?;
        let auth = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|e| {
            Error::invalid("the API key contains characters that cannot be sent in a header").with_source(e)
        })?;
        let url = format!("{}{}", inner.base_url, path);
        let payload = match body {
            Some(b) => Some(
                serde_json::to_vec(b).map_err(|e| Error::invalid("the body cannot be serialised").with_source(e))?,
            ),
            None => None,
        };

        let mut attempt: u32 = 0;
        loop {
            let started = Instant::now();
            let mut rb = inner
                .http
                .request(method.clone(), &url)
                .timeout(inner.timeout)
                .header(AUTHORIZATION, auth.clone())
                .header(USER_AGENT, USER_AGENT_VALUE)
                .header(ACCEPT, "application/json");
            if let Some(p) = &payload {
                rb = rb.header(CONTENT_TYPE, "application/json").body(p.clone());
            }
            if !inner.headers.is_empty() {
                rb = rb.headers(inner.headers.clone());
            }

            // The timeout covers the whole attempt, from sending to the last
            // byte of the body: reqwest's own covers the same span, and this
            // one holds whatever the HTTP client was configured with.
            let attempt_future = async {
                let resp = rb.send().await.map_err(|e| transport_error(e, inner.timeout))?;
                let status = resp.status().as_u16();
                let headers = resp.headers().clone();
                let bytes = resp.bytes().await.map_err(|e| transport_error(e, inner.timeout))?;
                Ok((status, headers, bytes))
            };
            let result = match tokio::time::timeout(inner.timeout, attempt_future).await {
                Ok(r) => r,
                Err(_) => Err(Error::new(
                    ErrorKind::Timeout,
                    format!("no response within {:.1} s", inner.timeout.as_secs_f64()),
                )),
            };

            match result {
                Err(err) => {
                    log::debug!(
                        "{method} {path} failed after {} ms: {} (attempt {})",
                        started.elapsed().as_millis(),
                        err.kind(),
                        attempt + 1
                    );
                    if !err.is_transport() || attempt >= inner.max_retries {
                        return Err(err);
                    }
                    self.wait(backoff(attempt)).await;
                }
                Ok((status, headers, bytes)) => {
                    log::debug!(
                        "{method} {path} -> {status} request_id={} in {} ms (attempt {})",
                        header(&headers, "x-request-id").unwrap_or_default(),
                        started.elapsed().as_millis(),
                        attempt + 1
                    );
                    // Only 2xx is a success. Redirects are never followed, so a 3xx
                    // arrives here and is an error like any other status.
                    if (200..=299).contains(&status) {
                        return match serde_json::from_slice::<Value>(&bytes) {
                            Ok(body) => Ok(Reply { body, status, headers }),
                            Err(e) => Err(Error::new(ErrorKind::Server, "the response is not JSON")
                                .with_response(status, &headers)
                                .with_source(e)),
                        };
                    }
                    if is_retryable(status) && attempt < inner.max_retries {
                        self.wait(retry_after(&headers).unwrap_or_else(|| backoff(attempt))).await;
                    } else {
                        return Err(Error::from_response(status, &headers, &bytes));
                    }
                }
            }
            attempt += 1;
        }
    }
}

/// A 2xx response whose body is JSON.
struct Reply {
    body: Value,
    status: u16,
    headers: HeaderMap,
}

/// A decision with the status and headers of its response.
struct Answered {
    decision: Decision,
    status: u16,
    headers: HeaderMap,
}

impl DecisionNode {
    async fn wait(&self, duration: Duration) {
        match &self.inner.sleeper {
            Some(sleeper) => sleeper(duration).await,
            None => tokio::time::sleep(duration).await,
        }
    }
}

/// `https`, or `http` to the local machine only, so a key never travels
/// unencrypted.
fn check_base_url(base_url: &str) -> Result<()> {
    let url = reqwest::Url::parse(base_url)
        .map_err(|e| Error::invalid(format!("base_url {base_url:?} is not a valid URL: {e}")).with_source(e))?;
    match url.scheme() {
        "https" => Ok(()),
        "http" if matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) => Ok(()),
        "http" => Err(Error::invalid(format!(
            "base_url {base_url:?} uses plain http; use https (http is accepted only for localhost, 127.0.0.1 and ::1)"
        ))),
        other => Err(Error::invalid(format!("base_url {base_url:?}: the scheme must be https, not {other}"))),
    }
}

pub(crate) fn check_concurrency<C: TryInto<usize>>(concurrency: C) -> Result<usize> {
    match concurrency.try_into() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(Error::invalid("concurrency must be at least 1")),
    }
}

fn to_value(value: impl Serialize, what: &str) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| Error::invalid(format!("{what} cannot be serialised to JSON: {e}")))
}

/// The typed answer a helper promises. A missing or malformed answer is a
/// `ServerError` with the response's status, request id and reason: a helper
/// cannot return the type it promises, so it does not fall back to
/// [`crate::UnknownAnswer`] as [`DecisionNode::decide`] does.
fn take_answer<T: Clone>(a: Answered, id: &str, pick: fn(&crate::Answer) -> Option<&T>) -> Result<T> {
    a.decision.answers.get(id).and_then(pick).cloned().ok_or_else(|| {
        Error::new(ErrorKind::Server, format!("the response has no answer of the expected type for {id:?}"))
            .with_response(a.status, &a.headers)
    })
}

fn transport_error(e: reqwest::Error, timeout: Duration) -> Error {
    if e.is_timeout() {
        Error::new(ErrorKind::Timeout, format!("no response within {:.1} s", timeout.as_secs_f64())).with_source(e)
    } else if e.is_builder() {
        Error::invalid(format!("the request could not be built: {e}")).with_source(e)
    } else {
        Error::new(ErrorKind::Connection, format!("connection error: {e}")).with_source(e)
    }
}

/// 408, 429 and every 5xx are retried.
pub(crate) fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 429 | 500..=599)
}

/// Exponential backoff from 0.5 s, capped at 8 s, with up to 25% jitter below.
pub(crate) fn backoff(attempt: u32) -> Duration {
    let base = (0.5 * 2f64.powi(attempt.min(16) as i32)).min(8.0);
    Duration::from_secs_f64(base * (0.75 + fastrand::f64() * 0.25))
}

/// The three questions of [`DecisionNode::gate`], with the fixed wording every
/// DecisionNode SDK sends.
pub fn gate_questions() -> Vec<(&'static str, Question)> {
    vec![
        (
            "risk",
            score(
                "How much harm could this action cause if it is wrong?",
                ["none", "low", "moderate", "high", "severe"],
            )
            .into(),
        ),
        (
            "touches_production",
            truth("Does this action change a production system, production data or anything customers use?").into(),
        ),
        (
            "verdict",
            choice(
                "Should this action run as it is, wait for a person to confirm it, or be blocked?",
                [
                    ("allow", "safe to run without asking"),
                    ("confirm", "a person should confirm it first"),
                    ("block", "should not run"),
                ],
            )
            .into(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retryable_statuses() {
        for s in [408, 429, 500, 502, 503, 504, 520, 529, 599] {
            assert!(is_retryable(s), "{s}");
        }
        for s in [400, 401, 402, 403, 404, 405, 422] {
            assert!(!is_retryable(s), "{s}");
        }
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        for _ in 0..100 {
            let first = backoff(0).as_secs_f64();
            assert!((0.375..=0.5).contains(&first), "{first}");
            let second = backoff(1).as_secs_f64();
            assert!((0.75..=1.0).contains(&second), "{second}");
            let late = backoff(30).as_secs_f64();
            assert!((6.0..=8.0).contains(&late), "{late}");
        }
    }

    #[test]
    fn body_defaults() {
        let body = DecideRequest::new().question("q", truth("?")).into_body("decisionnode-latest").unwrap();
        assert_eq!(
            body,
            json!({"model": "decisionnode-latest", "state": "", "questions": {"q": {"type": "truth", "instructions": "?"}}})
        );
    }

    #[test]
    fn body_with_model_state_and_replaced_question() {
        let body = DecideRequest::new()
            .state(json!(["a", 1]))
            .question("q", truth("first"))
            .question("q", truth("second"))
            .model("decisionnode-1.0-flash")
            .into_body("decisionnode-latest")
            .unwrap();
        assert_eq!(body["model"], "decisionnode-1.0-flash");
        assert_eq!(body["state"], json!(["a", 1]));
        assert_eq!(body["questions"]["q"]["instructions"], "second");
        assert!(body.get("images").is_none());
    }

    #[test]
    fn empty_questions_are_refused_before_sending() {
        let err = DecideRequest::new().state("s").into_body("m").unwrap_err();
        assert!(err.is_argument());
        assert_eq!(err.kind(), ErrorKind::Other);
    }

    #[tokio::test]
    async fn missing_key_fails_before_sending() {
        let dn = DecisionNode::builder().base_url("http://127.0.0.1:9").build().unwrap();
        let dn = DecisionNode { inner: Arc::new(Inner { api_key: None, ..clone_inner(&dn) }) };
        let err = dn.decide(DecideRequest::new().question("q", truth("?"))).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Authentication);
        assert_eq!(err.status(), None);
    }

    fn clone_inner(dn: &DecisionNode) -> Inner {
        Inner {
            api_key: dn.inner.api_key.clone(),
            base_url: dn.inner.base_url.clone(),
            model: dn.inner.model.clone(),
            timeout: dn.inner.timeout,
            max_retries: dn.inner.max_retries,
            headers: dn.inner.headers.clone(),
            http: dn.inner.http.clone(),
            sleeper: None,
        }
    }

    #[test]
    fn invalid_headers_are_refused() {
        let err = DecisionNode::builder().header("bad header", "x").build().unwrap_err();
        assert!(err.is_argument());
    }

    #[test]
    fn plain_http_only_to_the_local_machine() {
        for ok in ["https://api.decisionnode.com", "http://localhost:8080", "http://127.0.0.1:9", "http://[::1]:9"] {
            assert!(DecisionNode::builder().base_url(ok).build().is_ok(), "{ok}");
        }
        for bad in ["http://api.decisionnode.com", "http://10.0.0.1", "http://localhost.example.com", "ftp://x", "nope"]
        {
            assert!(DecisionNode::builder().base_url(bad).build().unwrap_err().is_argument(), "{bad}");
        }
    }

    #[test]
    fn timeout_must_be_above_zero() {
        assert!(DecisionNode::builder().timeout(Duration::ZERO).build().unwrap_err().is_argument());
        assert!(DecisionNode::builder().timeout(Duration::from_millis(1)).build().is_ok());
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(DecisionNode::builder().timeout_secs(bad).build().unwrap_err().is_argument(), "{bad}");
        }
        assert!(DecisionNode::builder().timeout_secs(0.5).build().is_ok());
    }

    #[test]
    fn max_retries_must_not_be_negative() {
        assert!(DecisionNode::builder().max_retries(-1).build().unwrap_err().is_argument());
        assert!(DecisionNode::builder().max_retries(0).build().is_ok());
        assert!(DecisionNode::builder().max_retries(4u64).build().is_ok());
    }

    #[tokio::test]
    async fn a_client_with_settings_never_follows_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{addr}/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        let dn = DecisionNode::builder()
            .api_key("k")
            .base_url(format!("http://{addr}"))
            .max_retries(0)
            .http_settings(HttpSettings::new().pool_max_idle_per_host(1))
            .build()
            .unwrap();
        let err = dn.decide(DecideRequest::new().question("q", truth("?"))).await.unwrap_err();
        assert_eq!(err.status(), Some(302));
        assert_eq!(err.kind(), ErrorKind::Other);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_2xx_that_is_not_json_carries_the_reason() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = "<html>busy</html>";
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nx-request-id: req_7\r\nx-decisionnode-error: upstream returned html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        let dn =
            DecisionNode::builder().api_key("k").base_url(format!("http://{addr}")).max_retries(0).build().unwrap();
        let err = dn.decide(DecideRequest::new().question("q", truth("?"))).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Server);
        assert_eq!(err.status(), Some(200));
        assert_eq!(err.request_id(), Some("req_7"));
        assert_eq!(err.reason(), Some("upstream returned html"));
    }

    #[tokio::test]
    async fn decide_many_checks_every_request_before_sending() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = hits.clone();
        tokio::spawn(async move {
            while listener.accept().await.is_ok() {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let dn =
            DecisionNode::builder().api_key("k").base_url(format!("http://{addr}")).max_retries(0).build().unwrap();
        let requests = vec![
            DecideRequest::new().question("q", truth("a?")),
            DecideRequest::new().question("q", truth("b?")),
            DecideRequest::new().state("c"),
        ];
        let err = dn.decide_many_with(requests.clone(), 1).await.unwrap_err();
        assert!(err.is_argument());
        assert!(err.message().starts_with("request 2: "), "{}", err.message());
        assert!(dn.decide_many(requests).await.unwrap_err().is_argument());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn helper_answer_errors_carry_the_reason() {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("req_1"));
        headers.insert("x-decisionnode-error", HeaderValue::from_static("question 'check': incomplete"));
        let decision =
            Decision::from_response(json!({"answers": {"check": {"type": "truth", "truth": "0.9"}}}), 200, &headers)
                .unwrap();
        let err =
            take_answer(Answered { decision, status: 200, headers }, "check", crate::Answer::as_truth).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Server);
        assert_eq!(err.status(), Some(200));
        assert_eq!(err.request_id(), Some("req_1"));
        assert_eq!(err.reason(), Some("question 'check': incomplete"));
    }

    #[test]
    fn empty_settings_count_as_not_given() {
        let dn = DecisionNode::builder().api_key("").base_url("").model("").build().unwrap();
        assert_ne!(dn.base_url(), "");
        assert_ne!(dn.model(), "");
        assert_eq!(DecideRequest::new().model(""), DecideRequest::new());
        assert_eq!(CallOptions::new().model(""), CallOptions::new());
    }

    #[test]
    fn concurrency_below_one_is_refused() {
        assert!(check_concurrency(0usize).unwrap_err().is_argument());
        assert!(check_concurrency(-1i64).unwrap_err().is_argument());
        assert_eq!(check_concurrency(3).unwrap(), 3);
    }

    #[test]
    fn gate_wording_is_fixed() {
        let qs: Map<String, Value> =
            gate_questions().into_iter().map(|(id, q)| (id.to_owned(), q.into_value().unwrap())).collect();
        assert_eq!(
            Value::Object(qs),
            json!({
                "risk": {"type": "score", "instructions": "How much harm could this action cause if it is wrong?",
                         "criteria": ["none", "low", "moderate", "high", "severe"]},
                "touches_production": {"type": "truth",
                         "instructions": "Does this action change a production system, production data or anything customers use?"},
                "verdict": {"type": "choice",
                         "instructions": "Should this action run as it is, wait for a person to confirm it, or be blocked?",
                         "criteria": {"allow": "safe to run without asking", "confirm": "a person should confirm it first",
                                      "block": "should not run"}}
            })
        );
    }

    #[test]
    fn gate_questions_have_the_fixed_ids() {
        let ids: Vec<_> = gate_questions().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["risk", "touches_production", "verdict"]);
    }

    #[test]
    fn user_agent_names_the_language() {
        assert!(USER_AGENT_VALUE.starts_with("decisionnode-rust/"));
    }
}
