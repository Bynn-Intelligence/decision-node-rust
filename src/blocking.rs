//! A synchronous client, enabled with the `blocking` feature.
//!
//! It has the same methods as the asynchronous [`crate::DecisionNode`] and
//! returns their results directly. It runs them on a small runtime of its own,
//! so it must not be used from inside an asynchronous context: use the
//! asynchronous client there.
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

use crate::client::{CallOptions, ClientBuilder, DecideRequest};
use crate::error::{Error, ErrorKind, Result};
use crate::question::{Choice, Score, Truth};
use crate::response::{ChoiceAnswer, Decision, DecisionResult, ModelInfo, ScoreAnswer, TruthAnswer};

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
}
