//! The official Rust client for the [DecisionNode](https://decisionnode.com) API.
//!
//! DecisionNode answers typed questions about a state: `truth` (is this
//! statement true, as a probability), `choice` (one option of a set), `score`
//! (a place on an ordered scale) and `number` (a value on a grid). This crate
//! checks the request locally, sends it over pooled connections with retries,
//! and returns typed answers and typed errors. It adds no behaviour of its own
//! to a decision: the values you read are the values the API sent.
//!
//! Beside [`DecisionNode::decide`] it covers every public call: the strict
//! [`DecisionNode::systemone`] path, [`DecisionNode::models`], image uploads
//! ([`DecisionNode::upload`], also chosen for you by size), and the Batch API ([`DecisionNode::batches`],
//! [`DecisionNode::run_batch`]).
//!
//! ```no_run
//! use decisionnode::{choice, score, truth, DecideRequest, DecisionNode};
//! use serde_json::json;
//!
//! # async fn run() -> decisionnode::Result<()> {
//! let dn = DecisionNode::new(); // reads DECISIONNODE_API_KEY
//!
//! let d = dn
//!     .decide(
//!         DecideRequest::new()
//!             .state(json!({"ticket": "I was charged twice and nobody has replied for 3 days."}))
//!             .question("route", choice("Where should this go?", [("billing", "money"), ("bug", "broken"), ("account", "login")]))
//!             .question("urgency", score("How urgent is this?", ["routine", "today", "urgent", "critical"]))
//!             .question("escalate", truth("Escalate to a human now?")),
//!     )
//!     .await?;
//!
//! let route = d["route"].as_choice().unwrap();
//! println!("{} {:?}", route.choice, route.probabilities);
//! println!("{:?}", d["urgency"].score());
//! println!("{:?}", d["escalate"].truth());
//! # Ok(())
//! # }
//! ```
//!
//! A synchronous client is in [`blocking`], behind the `blocking` feature.
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]

mod batch;
#[cfg(feature = "blocking")]
pub mod blocking;
mod check;
mod client;
pub mod error;
mod http;
mod image;
pub mod question;
mod response;
mod safety;
mod upload;

pub use batch::{
    Batch, BatchCounters, BatchPage, BatchRequest, BatchResult, BatchResults, BatchRun, BatchStatus, Batches,
    DEFAULT_BATCH_TIMEOUT, DEFAULT_POLL_INTERVAL, MIN_POLL_INTERVAL,
};
pub use client::{
    gate_questions, CallOptions, ClientBuilder, DecideRequest, DecisionNode, Sleeper, DEFAULT_BASE_URL,
    DEFAULT_CONCURRENCY, DEFAULT_INLINE_THRESHOLD, DEFAULT_MAX_RETRIES, DEFAULT_MODEL, DEFAULT_TIMEOUT,
};
pub use error::{DecisionNodeError, Error, ErrorKind, Result};
pub use http::HttpSettings;
pub use image::Image;
pub use question::{choice, number, score, truth, Choice, Levels, Number, Options, Question, Score, Truth};
pub use response::{
    Answer, ChoiceAnswer, Decision, DecisionResult, ModelInfo, NumberAnswer, ScoreAnswer, TruthAnswer, UnknownAnswer,
    Usage,
};
pub use safety::Safety;

pub use indexmap;
pub use reqwest;
pub use serde_json;

/// The version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The version of the DecisionNode SDK specification this crate implements.
pub const SPEC_VERSION: &str = "1.1";
