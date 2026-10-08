//! A synchronous client, enabled with the `blocking` feature.
//!
//! It has the same methods as the asynchronous [`crate::DecisionNode`] and
//! returns their results directly: decisions, uploads, the Batch API through
//! [`DecisionNode::batches`] and, with the `sessions` feature, sessions with a
//! blocking [`Session`]. It runs them on a small runtime of its own, so it
//! must not be used from inside an asynchronous context: use the asynchronous
//! client there.
//!
//! ```no_run
//! use decisionnode::blocking::DecisionNode;
//!
//! let dn = DecisionNode::new(); // reads DECISIONNODE_API_KEY
//! let answer = dn.check("Customer: please refund my last order.", "Is this a refund request?")?;
//! println!("{}", answer.truth);
//! # Ok::<(), decisionnode::Error>(())
//! ```

use std::fmt;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use std::time::Duration;

use crate::batch::{Batch, BatchPage, BatchRequest, BatchResults, BatchRun};
use crate::client::{CallOptions, ClientBuilder, DecideRequest};
use crate::error::{Error, ErrorKind, Result};
use crate::image::Image;
use crate::question::{Choice, Number, Score, Truth};
use crate::response::{ChoiceAnswer, Decision, DecisionResult, ModelInfo, NumberAnswer, ScoreAnswer, TruthAnswer};

/// The synchronous DecisionNode client.
///
/// Cheap to clone and safe to share between threads.
#[derive(Clone)]
pub struct DecisionNode {
    client: crate::DecisionNode,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl fmt::Debug for DecisionNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("blocking::DecisionNode").field(&self.client).finish()
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
    /// When the HTTP client or its runtime cannot be created. Use
    /// [`DecisionNode::builder`] and [`ClientBuilder::build_blocking`] to
    /// handle that as an error.
    pub fn new() -> DecisionNode {
        DecisionNode::builder().build_blocking().expect("the DecisionNode client could not be created")
    }

    /// A builder to configure the client. Finish it with
    /// [`ClientBuilder::build_blocking`].
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// Wraps an asynchronous client.
    pub fn from_async(client: crate::DecisionNode) -> Result<DecisionNode> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("decisionnode-blocking")
            .enable_all()
            .build()
            .map_err(|e| Error::new(ErrorKind::Connection, "the client runtime could not be started").with_source(e))?;
        Ok(DecisionNode { client, runtime: Arc::new(runtime) })
    }

    /// The asynchronous client this one wraps.
    pub fn as_async(&self) -> &crate::DecisionNode {
        &self.client
    }

    fn run<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// See [`crate::DecisionNode::decide`].
    pub fn decide(&self, request: DecideRequest) -> Result<Decision> {
        self.run(self.client.decide(request))
    }

    /// See [`crate::DecisionNode::decide_many`].
    pub fn decide_many<I: IntoIterator<Item = DecideRequest>>(&self, requests: I) -> Result<Vec<DecisionResult>> {
        self.run(self.client.decide_many(requests))
    }

    /// See [`crate::DecisionNode::decide_many_with`].
    pub fn decide_many_with<I, C>(&self, requests: I, concurrency: C) -> Result<Vec<DecisionResult>>
    where
        I: IntoIterator<Item = DecideRequest>,
        C: TryInto<usize>,
    {
        self.run(self.client.decide_many_with(requests, concurrency))
    }

    /// See [`crate::DecisionNode::check`].
    pub fn check(&self, state: impl Serialize, question: impl Into<Truth>) -> Result<TruthAnswer> {
        self.run(self.client.check(state, question))
    }

    /// See [`crate::DecisionNode::check_with`].
    pub fn check_with(
        &self,
        state: impl Serialize,
        question: impl Into<Truth>,
        options: CallOptions,
    ) -> Result<TruthAnswer> {
        self.run(self.client.check_with(state, question, options))
    }

    /// See [`crate::DecisionNode::classify`].
    pub fn classify(&self, state: impl Serialize, question: Choice) -> Result<ChoiceAnswer> {
        self.run(self.client.classify(state, question))
    }

    /// See [`crate::DecisionNode::classify_with`].
    pub fn classify_with(&self, state: impl Serialize, question: Choice, options: CallOptions) -> Result<ChoiceAnswer> {
        self.run(self.client.classify_with(state, question, options))
    }

    /// See [`crate::DecisionNode::score`].
    pub fn score(&self, state: impl Serialize, question: Score) -> Result<ScoreAnswer> {
        self.run(self.client.score(state, question))
    }

    /// See [`crate::DecisionNode::score_with`].
    pub fn score_with(&self, state: impl Serialize, question: Score, options: CallOptions) -> Result<ScoreAnswer> {
        self.run(self.client.score_with(state, question, options))
    }

    /// See [`crate::DecisionNode::gate`].
    pub fn gate(&self, action: impl Serialize, context: impl Serialize) -> Result<Decision> {
        self.run(self.client.gate(action, context))
    }

    /// See [`crate::DecisionNode::gate_with`].
    pub fn gate_with(&self, action: impl Serialize, context: impl Serialize, options: CallOptions) -> Result<Decision> {
        self.run(self.client.gate_with(action, context, options))
    }

    /// See [`crate::DecisionNode::models`].
    pub fn models(&self) -> Result<Vec<ModelInfo>> {
        self.run(self.client.models())
    }

    /// See [`crate::DecisionNode::decide_raw`].
    pub fn decide_raw(&self, body: impl Serialize) -> Result<Value> {
        self.run(self.client.decide_raw(body))
    }

    /// See [`crate::DecisionNode::systemone`].
    pub fn systemone(&self, request: DecideRequest) -> Result<Decision> {
        self.run(self.client.systemone(request))
    }

    /// See [`crate::DecisionNode::number`].
    pub fn number(&self, state: impl Serialize, question: Number) -> Result<NumberAnswer> {
        self.run(self.client.number(state, question))
    }

    /// See [`crate::DecisionNode::number_with`].
    pub fn number_with(&self, state: impl Serialize, question: Number, options: CallOptions) -> Result<NumberAnswer> {
        self.run(self.client.number_with(state, question, options))
    }

    /// See [`crate::DecisionNode::upload`].
    pub fn upload(&self, images: impl IntoIterator<Item = Image>) -> Result<Vec<String>> {
        self.run(self.client.upload(images))
    }

    /// The Batch API. See [`crate::DecisionNode::batches`].
    pub fn batches(&self) -> Batches<'_> {
        Batches { client: self }
    }

    /// See [`crate::DecisionNode::run_batch`].
    pub fn run_batch(&self, requests: impl IntoIterator<Item = BatchRequest>) -> Result<BatchRun> {
        self.run(self.client.run_batch(requests))
    }

    /// See [`crate::DecisionNode::run_batch_with`].
    pub fn run_batch_with(
        &self,
        requests: impl IntoIterator<Item = BatchRequest>,
        poll_interval: Duration,
        timeout: Duration,
    ) -> Result<BatchRun> {
        self.run(self.client.run_batch_with(requests, poll_interval, timeout))
    }
}

/// The Batch API of a blocking client. See [`crate::Batches`].
#[derive(Clone, Copy, Debug)]
pub struct Batches<'a> {
    client: &'a DecisionNode,
}

impl Batches<'_> {
    /// See [`crate::Batches::create`].
    pub fn create(&self, requests: impl IntoIterator<Item = BatchRequest>) -> Result<Batch> {
        self.client.run(self.client.client.batches().create(requests))
    }

    /// See [`crate::Batches::add`].
    pub fn add(&self, batch_id: &str, requests: impl IntoIterator<Item = BatchRequest>) -> Result<Batch> {
        self.client.run(self.client.client.batches().add(batch_id, requests))
    }

    /// See [`crate::Batches::finalize`].
    pub fn finalize(&self, batch_id: &str) -> Result<Batch> {
        self.client.run(self.client.client.batches().finalize(batch_id))
    }

    /// See [`crate::Batches::get`].
    pub fn get(&self, batch_id: &str) -> Result<Batch> {
        self.client.run(self.client.client.batches().get(batch_id))
    }

    /// See [`crate::Batches::list`].
    pub fn list(&self, offset: Option<u64>, limit: Option<u64>) -> Result<BatchPage> {
        self.client.run(self.client.client.batches().list(offset, limit))
    }

    /// See [`crate::Batches::results`].
    pub fn results(&self, batch_id: &str) -> Result<BatchResults> {
        self.client.run(self.client.client.batches().results(batch_id))
    }

    /// See [`crate::Batches::cancel`].
    pub fn cancel(&self, batch_id: &str) -> Result<Batch> {
        self.client.run(self.client.client.batches().cancel(batch_id))
    }
}

#[cfg(feature = "sessions")]
mod sessions {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::error::Result;
    use crate::session::{Frame, SessionClosed, SessionEvent, SessionOptions};

    use super::DecisionNode;

    /// A session of a blocking client. Its data (id, answers, limits) is read
    /// through [`crate::Session`], which it dereferences to; its calls block.
    pub struct Session {
        session: crate::Session,
        runtime: Arc<tokio::runtime::Runtime>,
    }

    impl std::fmt::Debug for Session {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_tuple("blocking::Session").field(&self.session).finish()
        }
    }

    impl std::ops::Deref for Session {
        type Target = crate::Session;

        fn deref(&self) -> &crate::Session {
            &self.session
        }
    }

    impl Session {
        /// See [`crate::Session::connect`].
        pub fn connect(&self) -> Result<()> {
            self.runtime.block_on(self.session.connect())
        }

        /// See [`crate::Session::send`].
        pub fn send(&self, frame: Frame) -> Result<u64> {
            self.runtime.block_on(self.session.send(frame))
        }

        /// See [`crate::Session::next_event`].
        pub fn next_event(&self) -> Result<SessionEvent> {
            self.runtime.block_on(self.session.next_event())
        }

        /// See [`crate::Session::next_event_timeout`].
        pub fn next_event_timeout(&self, timeout: Duration) -> Result<Option<SessionEvent>> {
            self.runtime.block_on(self.session.next_event_timeout(timeout))
        }

        /// The events as an iterator, ending after [`SessionEvent::Closed`].
        pub fn replies(&self) -> impl Iterator<Item = SessionEvent> + '_ {
            let mut done = false;
            std::iter::from_fn(move || {
                if done {
                    return None;
                }
                let event = self.next_event().ok()?;
                done = matches!(event, SessionEvent::Closed(_));
                Some(event)
            })
        }

        /// See [`crate::Session::end`].
        pub fn end(&self) -> Result<()> {
            self.runtime.block_on(self.session.end())
        }

        /// See [`crate::Session::close`].
        pub fn close(&self) {
            self.runtime.block_on(self.session.close())
        }

        /// The asynchronous session this one wraps.
        pub fn as_async(&self) -> &crate::Session {
            &self.session
        }
    }

    impl DecisionNode {
        /// See [`crate::DecisionNode::open_session`].
        pub fn open_session(&self, options: SessionOptions) -> Result<Session> {
            let session = self.run(self.client.open_session(options))?;
            Ok(Session { session, runtime: self.runtime.clone() })
        }

        /// See [`crate::DecisionNode::supervise`]. The feed is read on a
        /// thread of its own, so it may block between readings.
        pub fn supervise<I, F>(&self, options: SessionOptions, feed: I, on_reply: F) -> Result<Option<SessionClosed>>
        where
            I: IntoIterator<Item = Frame>,
            I::IntoIter: Send,
            F: FnMut(SessionEvent),
        {
            let feed = feed.into_iter();
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(1);
            std::thread::scope(|scope| {
                scope.spawn(move || {
                    for frame in feed {
                        if tx.blocking_send(frame).is_err() {
                            break;
                        }
                    }
                });
                let stream = futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx));
                self.run(self.client.supervise(options, stream, on_reply))
            })
        }
    }
}

#[cfg(feature = "sessions")]
pub use sessions::Session;
