//! Runs the DecisionNode SDK conformance suite against this crate.
//!
//! The suite is a directory of language-neutral cases and a mock API server.
//! Point `DECISIONNODE_CONFORMANCE_DIR` at it and run
//!
//! ```sh
//! cargo test --all-features --test conformance
//! cargo test --all-features --test conformance -- --kind behaviour --case retry-408 -v
//! ```
//!
//! Every case runs with the asynchronous client, and again with the blocking
//! client when the `blocking` feature is on. Without the variable the test
//! fails: it never skips. The session cases need the `sessions` feature (on by
//! default); without it they fail.
//!
//! Options: `--kind <requests|responses|errors|behaviour|constants>` (repeatable),
//! `--case <id>` (repeatable), `--driver <async|blocking>`, `--mock-url <url>`
//! (use a mock that is already running), `-v` (print every case).

mod compare;
mod mock;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use decisionnode::{
    Batch, BatchPage, BatchRequest, BatchResult, BatchStatus, CallOptions, Choice, ChoiceAnswer, ClientBuilder,
    DecideRequest, Decision, DecisionNode, DecisionResult, Error, Image, Levels, ModelInfo, Number, NumberAnswer,
    Options, Question, Safety, Score, ScoreAnswer, Truth, TruthAnswer,
};
use serde_json::{json, Map, Value};
use sha2::Digest;

use compare::{matches, same, short, Fail};
use mock::Mock;

#[cfg(feature = "sessions")]
use decisionnode::{Frame, SessionClosed, SessionEvent, SessionOptions};

const KINDS: [&str; 5] = ["requests", "responses", "errors", "behaviour", "constants"];
const ENV_VARS: [&str; 3] = ["DECISIONNODE_API_KEY", "DECISIONNODE_BASE_URL", "DECISIONNODE_MODEL"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Driver {
    Async,
    #[cfg(feature = "blocking")]
    Blocking,
}

impl Driver {
    fn name(self) -> &'static str {
        match self {
            Driver::Async => "async",
            #[cfg(feature = "blocking")]
            Driver::Blocking => "blocking",
        }
    }

    fn all() -> Vec<Driver> {
        #[allow(unused_mut)]
        let mut v = vec![Driver::Async];
        #[cfg(feature = "blocking")]
        v.push(Driver::Blocking);
        v
    }
}

struct Args {
    kinds: Vec<String>,
    cases: Vec<String>,
    filters: Vec<String>,
    drivers: Vec<Driver>,
    mock_url: Option<String>,
    verbose: bool,
}

fn parse_args() -> Args {
    let mut args =
        Args { kinds: vec![], cases: vec![], filters: vec![], drivers: vec![], mock_url: None, verbose: false };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--kind" => args.kinds.extend(it.next()),
            "--case" => args.cases.extend(it.next()),
            "--mock-url" => args.mock_url = it.next(),
            "--driver" => {
                let d = it.next().unwrap_or_default();
                args.drivers.extend(Driver::all().into_iter().filter(|x| x.name() == d));
            }
            "-v" | "--verbose" => args.verbose = true,
            other if other.starts_with('-') => {} // flags cargo passes to test binaries
            other => args.filters.push(other.to_owned()),
        }
    }
    if args.drivers.is_empty() {
        args.drivers = Driver::all();
    }
    args
}

fn main() {
    let args = parse_args();
    let Some(root) = std::env::var_os("DECISIONNODE_CONFORMANCE_DIR").filter(|v| !v.is_empty()).map(PathBuf::from)
    else {
        eprintln!("conformance: FAIL: DECISIONNODE_CONFORMANCE_DIR is not set; point it at the conformance suite");
        std::process::exit(1);
    };

    let cases = load_cases(&root);
    let mock = match &args.mock_url {
        Some(url) => Mock::connect(url),
        None => Mock::start(&root),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let ctx = Ctx { root: root.clone(), mock, runtime };

    let mut failed = Vec::new();
    let mut lines = Vec::new();
    for driver in &args.drivers {
        for kind in KINDS {
            if !args.kinds.is_empty() && !args.kinds.iter().any(|k| k == kind) {
                continue;
            }
            let (mut passed, mut count) = (0, 0);
            for case in cases[kind].as_array().into_iter().flatten() {
                let id = case["id"].as_str().unwrap_or_default();
                if !args.cases.is_empty() && !args.cases.iter().any(|c| c == id) {
                    continue;
                }
                if !args.filters.is_empty() && !args.filters.iter().any(|f| id.contains(f.as_str())) {
                    continue;
                }
                count += 1;
                let t0 = Instant::now();
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_case(&ctx, *driver, kind, case)));
                let label = format!("{}/{kind}/{id}", driver.name());
                match result {
                    Ok(Ok(())) => {
                        passed += 1;
                        if args.verbose {
                            println!("ok   {label} ({} ms)", t0.elapsed().as_millis());
                        }
                    }
                    Ok(Err(Fail(msg))) => {
                        println!("FAIL {label}: {msg}");
                        failed.push(label);
                    }
                    Err(_) => {
                        println!("FAIL {label}: panicked");
                        failed.push(label);
                    }
                }
            }
            if count > 0 {
                lines.push(format!("{:<9} {:<10} {passed}/{count} passed", driver.name(), kind));
            }
        }
    }
    ctx.mock.stop();
    for l in &lines {
        println!("{l}");
    }
    if failed.is_empty() {
        println!("conformance: PASS");
    } else {
        println!("conformance: FAIL ({})", failed.len());
        std::process::exit(1);
    }
}

fn load_cases(root: &Path) -> Map<String, Value> {
    let mut out = Map::new();
    for kind in KINDS {
        let dir = root.join("cases").join(kind);
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        names.sort();
        let items = names
            .iter()
            .map(|p| {
                serde_json::from_str::<Value>(&std::fs::read_to_string(p).unwrap())
                    .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
            })
            .collect();
        out.insert(kind.to_owned(), Value::Array(items));
    }
    out
}

struct Ctx {
    root: PathBuf,
    mock: Mock,
    runtime: tokio::runtime::Runtime,
}

fn run_case(ctx: &Ctx, driver: Driver, kind: &str, case: &Value) -> Result<(), Fail> {
    match kind {
        "requests" => run_request(ctx, driver, case),
        "responses" => run_response(ctx, driver, case),
        "errors" => run_error(ctx, driver, case),
        "constants" => run_constant(case),
        _ => run_behaviour(ctx, driver, case),
    }
}

// --------------------------------------------------------------------------- clients

enum Client {
    Async(DecisionNode),
    #[cfg(feature = "blocking")]
    Blocking(decisionnode::blocking::DecisionNode),
}

fn build(driver: Driver, builder: ClientBuilder) -> Result<Client, Error> {
    match driver {
        Driver::Async => builder.build().map(Client::Async),
        #[cfg(feature = "blocking")]
        Driver::Blocking => builder.build_blocking().map(Client::Blocking),
    }
}

/// A session of either client.
enum SessionHandle {
    #[cfg(feature = "sessions")]
    Async(decisionnode::Session),
    #[cfg(all(feature = "sessions", feature = "blocking"))]
    Blocking(decisionnode::blocking::Session),
}

#[cfg(feature = "sessions")]
impl SessionHandle {
    fn raw(&self) -> &Value {
        match self {
            SessionHandle::Async(s) => s.raw(),
            #[cfg(feature = "blocking")]
            SessionHandle::Blocking(s) => s.raw(),
        }
    }

    fn answers(&self) -> &indexmap::IndexMap<String, decisionnode::Answer> {
        match self {
            SessionHandle::Async(s) => s.answers(),
            #[cfg(feature = "blocking")]
            SessionHandle::Blocking(s) => s.answers(),
        }
    }

    fn connect(&self, ctx: &Ctx) -> Result<(), Error> {
        match self {
            SessionHandle::Async(s) => ctx.runtime.block_on(s.connect()),
            #[cfg(feature = "blocking")]
            SessionHandle::Blocking(s) => s.connect(),
        }
    }

    fn send(&self, ctx: &Ctx, frame: Frame) -> Result<u64, Error> {
        match self {
            SessionHandle::Async(s) => ctx.runtime.block_on(s.send(frame)),
            #[cfg(feature = "blocking")]
            SessionHandle::Blocking(s) => s.send(frame),
        }
    }

    fn next_event(&self, ctx: &Ctx, timeout: Duration) -> Result<Option<SessionEvent>, Error> {
        match self {
            SessionHandle::Async(s) => ctx.runtime.block_on(s.next_event_timeout(timeout)),
            #[cfg(feature = "blocking")]
            SessionHandle::Blocking(s) => s.next_event_timeout(timeout),
        }
    }

    fn end(&self, ctx: &Ctx) -> Result<(), Error> {
        match self {
            SessionHandle::Async(s) => ctx.runtime.block_on(s.end()),
            #[cfg(feature = "blocking")]
            SessionHandle::Blocking(s) => s.end(),
        }
    }
}

// --------------------------------------------------------------------------- calls

#[allow(clippy::large_enum_variant)]
enum Outcome {
    Decision(Decision),
    Truth(TruthAnswer),
    Choice(ChoiceAnswer),
    Score(ScoreAnswer),
    Number(NumberAnswer),
    Models(Vec<ModelInfo>),
    Raw(Value),
    Many(Vec<DecisionResult>),
    UploadIds(Vec<String>),
    Batch(Batch),
    BatchPage(BatchPage),
    BatchResults(Vec<BatchResult>),
    BatchRun(Batch, Vec<BatchResult>),
    #[cfg_attr(not(feature = "sessions"), allow(dead_code))]
    Session(SessionHandle),
    #[cfg(feature = "sessions")]
    Supervised(Vec<SessionEvent>, Option<SessionClosed>),
    Error(Error),
}

/// Values saved by earlier steps, for `$ref`.
type Refs = HashMap<String, Outcome>;

fn opt<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|v| !v.is_null())
}

fn single(value: &Value) -> Option<(&str, &Value)> {
    let obj = value.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    obj.iter().next().map(|(k, v)| (k.as_str(), v))
}

/// The JSON of a saved outcome, for a `$ref` path into it.
fn ref_json(o: &Outcome) -> Value {
    match o {
        Outcome::Batch(b) => b.raw.clone(),
        Outcome::BatchPage(p) => p.raw.clone(),
        Outcome::UploadIds(ids) => json!(ids),
        Outcome::Raw(v) => v.clone(),
        Outcome::Decision(d) => d.raw.clone(),
        #[cfg(feature = "sessions")]
        Outcome::Session(s) => s.raw().clone(),
        _ => Value::Null,
    }
}

/// Replaces `$ref` and `$text` everywhere; builders are read later.
fn resolve(value: &Value, refs: &Refs) -> Value {
    match single(value) {
        Some(("$ref", Value::String(path))) => {
            let mut parts = path.split('.');
            let name = parts.next().unwrap_or_default();
            let mut cur = refs.get(name).map(ref_json).unwrap_or(Value::Null);
            for p in parts {
                cur = match cur {
                    Value::Object(mut m) => m.remove(p).unwrap_or(Value::Null),
                    Value::Array(mut a) => match p.parse::<usize>() {
                        Ok(i) if i < a.len() => a.swap_remove(i),
                        _ => Value::Null,
                    },
                    _ => Value::Null,
                };
            }
            cur
        }
        Some(("$text", t)) => {
            let times = t["times"].as_u64().unwrap_or(0) as usize;
            Value::String(t["repeat"].as_str().unwrap_or_default().repeat(times))
        }
        _ => match value {
            Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), resolve(v, refs))).collect()),
            Value::Array(a) => Value::Array(a.iter().map(|v| resolve(v, refs)).collect()),
            other => other.clone(),
        },
    }
}

fn build_truth(args: &Value) -> Truth {
    let mut t = match opt(args, "instructions") {
        Some(i) => decisionnode::truth(i),
        None => Truth::new(),
    };
    if let Some(v) = opt(args, "true") {
        t = t.if_true(v);
    }
    if let Some(v) = opt(args, "false") {
        t = t.if_false(v);
    }
    t
}

fn build_choice(args: &Value) -> Choice {
    let c = Choice::new(Options::from(args.get("options").cloned().unwrap_or(Value::Null)));
    match opt(args, "instructions") {
        Some(i) => c.instructions(i),
        None => c,
    }
}

fn build_score(args: &Value) -> Score {
    let s = Score::new(Levels::from(args.get("levels").cloned().unwrap_or(Value::Null)));
    match opt(args, "instructions") {
        Some(i) => s.instructions(i),
        None => s,
    }
}

fn build_number(args: &Value) -> Number {
    let mut n = Number::new();
    if let Some(i) = opt(args, "instructions") {
        n = n.instructions(i);
    }
    if let Some(v) = opt(args, "min") {
        n = n.min(v);
    }
    if let Some(v) = opt(args, "max") {
        n = n.max(v);
    }
    if let Some(v) = opt(args, "step") {
        n = n.step(v);
    }
    n
}

fn question(value: &Value) -> Question {
    match single(value) {
        Some(("$truth", a)) => build_truth(a).into(),
        Some(("$choice", a)) => build_choice(a).into(),
        Some(("$score", a)) => build_score(a).into(),
        Some(("$number", a)) => build_number(a).into(),
        _ => Question::from(value.clone()),
    }
}

fn questions(args: &Value) -> Vec<(String, Question)> {
    args.get("questions")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(id, q)| (id.clone(), question(q)))
        .collect()
}

fn image(root: &Path, value: &Value) -> Result<Image, Error> {
    let a = match single(value) {
        Some(("$image", a)) => a,
        _ => panic!("not an image: {value}"),
    };
    let id = a["id"].as_str().unwrap();
    if let Some(u) = a.get("url").and_then(Value::as_str) {
        return Ok(Image::url(id, u));
    }
    if let Some(u) = a.get("upload_id").and_then(Value::as_str) {
        return Ok(match a.get("media_type").and_then(Value::as_str) {
            Some(t) => Image::upload_id_with_type(id, u, t),
            None => Image::upload_id(id, u),
        });
    }
    let pad = a.get("pad_to").and_then(Value::as_u64);
    if let (Some(p), None) = (a.get("path").and_then(Value::as_str), pad) {
        return Image::path(id, root.join(p));
    }
    let mut data = match a.get("path").and_then(Value::as_str) {
        Some(p) => std::fs::read(root.join(p)).unwrap(),
        None => base64::engine::general_purpose::STANDARD.decode(a["data_base64"].as_str().unwrap()).unwrap(),
    };
    if let Some(n) = pad {
        // The image's own bytes, then zeros: the header (type, pixels) is
        // kept and the size is the one the case needs.
        data.resize((n as usize).max(data.len()), 0);
    }
    Ok(match a.get("media_type").and_then(Value::as_str) {
        Some(t) => Image::bytes_with_type(id, data, t),
        None => Image::bytes(id, data),
    })
}

fn images(root: &Path, args: &Value) -> Result<Vec<Image>, Error> {
    args.get("images").and_then(Value::as_array).into_iter().flatten().map(|i| image(root, i)).collect()
}

fn decide_request(root: &Path, args: &Value) -> Result<DecideRequest, Error> {
    let mut r = DecideRequest::new();
    if let Some(s) = args.get("state") {
        r = r.state(s);
    }
    r = r.questions(questions(args));
    r = r.images(images(root, args)?);
    if let Some(m) = opt(args, "model").and_then(Value::as_str) {
        r = r.model(m);
    }
    Ok(r)
}

fn batch_requests(root: &Path, args: &Value) -> Result<Vec<BatchRequest>, Error> {
    args["requests"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| Ok(BatchRequest::new(r["custom_id"].as_str().unwrap_or_default(), decide_request(root, r)?)))
        .collect()
}

/// The images and model of a helper call.
fn call_options(root: &Path, args: &Value) -> Result<CallOptions, Error> {
    let mut o = CallOptions::new().images(images(root, args)?);
    if let Some(m) = opt(args, "model").and_then(Value::as_str) {
        o = o.model(m);
    }
    Ok(o)
}

#[cfg(feature = "sessions")]
fn session_options(args: &Value) -> SessionOptions {
    let mut o = SessionOptions::new().questions(questions(args));
    if let Some(s) = opt(args, "state") {
        o = o.state(s);
    }
    if let Some(i) = opt(args, "instructions") {
        o = o.instructions(i);
    }
    if let Some(m) = opt(args, "model").and_then(Value::as_str) {
        o = o.model(m);
    }
    if let Some(w) = opt(args, "window").and_then(Value::as_u64) {
        o = o.window(w);
    }
    if let Some(t) = opt(args, "ttl_seconds").and_then(Value::as_u64) {
        o = o.ttl_seconds(t);
    }
    o
}

#[cfg(feature = "sessions")]
fn frame(root: &Path, args: &Value) -> Result<Frame, Error> {
    let mut f = Frame::default();
    if let Some(v) = opt(args, "frame") {
        f = f.frame(v);
    }
    if let Some(i) = opt(args, "image") {
        f = f.image(image(root, i)?);
    }
    if let Some(q) = opt(args, "questions").and_then(Value::as_array) {
        f = f.questions(q.iter().filter_map(Value::as_str));
    }
    if let Some(r) = opt(args, "reset").and_then(Value::as_bool) {
        f = f.reset(r);
    }
    if let Some(s) = opt(args, "seq").and_then(Value::as_u64) {
        f = f.seq(s);
    }
    Ok(f)
}

enum Prepared {
    Decide(DecideRequest),
    Systemone(DecideRequest),
    Many(Vec<DecideRequest>, Option<i64>),
    Check(Value, Truth, CallOptions),
    Classify(Value, Choice, CallOptions),
    Score(Value, Score, CallOptions),
    Number(Value, Number, CallOptions),
    Gate(Value, Value, CallOptions),
    Models,
    Raw(Value),
    Upload(Vec<Image>),
    #[cfg(feature = "sessions")]
    OpenSession(SessionOptions),
    #[cfg(feature = "sessions")]
    Supervise(SessionOptions, Vec<Frame>, Duration),
    BatchCreate(Vec<BatchRequest>),
    BatchAdd(String, Vec<BatchRequest>),
    BatchFinalize(String),
    BatchGet(String),
    BatchList(Option<u64>, Option<u64>),
    BatchResults(String),
    BatchCancel(String),
    RunBatch(Vec<BatchRequest>, Duration),
}

/// The call of a case. An argument the SDK refuses while it is being built
/// (an image file that cannot be read) is the call's error.
fn prepare(root: &Path, call: &Value, refs: &Refs) -> Result<Prepared, Error> {
    let method = call["method"].as_str().unwrap();
    if method == "decide_raw" {
        return Ok(Prepared::Raw(call["args"]["body"].clone()));
    }
    let args = &resolve(&call["args"], refs);
    let state = || args.get("state").cloned().unwrap_or_else(|| json!(""));
    let options = || call_options(root, args);
    let batch_id = || args["batch_id"].as_str().unwrap_or_default().to_owned();
    Ok(match method {
        "decide" => Prepared::Decide(decide_request(root, args)?),
        "systemone" => Prepared::Systemone(decide_request(root, args)?),
        "decide_many" => Prepared::Many(
            args["requests"].as_array().unwrap().iter().map(|r| decide_request(root, r)).collect::<Result<_, _>>()?,
            opt(args, "concurrency").and_then(Value::as_i64),
        ),
        "check" => Prepared::Check(state(), build_truth(args), options()?),
        "classify" => Prepared::Classify(state(), build_choice(args), options()?),
        "score" => Prepared::Score(state(), build_score(args), options()?),
        "number" => Prepared::Number(state(), build_number(args), options()?),
        "gate" => {
            Prepared::Gate(args["action"].clone(), args.get("context").cloned().unwrap_or(Value::Null), options()?)
        }
        "models" => Prepared::Models,
        "upload" => Prepared::Upload(images(root, args)?),
        #[cfg(feature = "sessions")]
        "open_session" => Prepared::OpenSession(session_options(args)),
        #[cfg(feature = "sessions")]
        "supervise" => Prepared::Supervise(
            session_options(&args["body"]),
            args["feed"].as_array().into_iter().flatten().map(|f| frame(root, f)).collect::<Result<_, _>>()?,
            Duration::from_millis(args.get("feed_interval_ms").and_then(Value::as_u64).unwrap_or(0)),
        ),
        "batches.create" => Prepared::BatchCreate(batch_requests(root, args)?),
        "batches.add" => Prepared::BatchAdd(batch_id(), batch_requests(root, args)?),
        "batches.finalize" => Prepared::BatchFinalize(batch_id()),
        "batches.get" => Prepared::BatchGet(batch_id()),
        "batches.list" => {
            Prepared::BatchList(opt(args, "offset").and_then(Value::as_u64), opt(args, "limit").and_then(Value::as_u64))
        }
        "batches.results" => Prepared::BatchResults(batch_id()),
        "batches.cancel" => Prepared::BatchCancel(batch_id()),
        "run_batch" => Prepared::RunBatch(
            batch_requests(root, args)?,
            Duration::try_from_secs_f64(args.get("poll_interval").and_then(Value::as_f64).unwrap_or(60.0))
                .unwrap_or(Duration::ZERO),
        ),
        other => panic!("unknown method {other} (is the sessions feature on?)"),
    })
}

fn wrap<T>(r: Result<T, Error>, f: impl FnOnce(T) -> Outcome) -> Outcome {
    match r {
        Ok(v) => f(v),
        Err(e) => Outcome::Error(e),
    }
}

fn collect_results(r: Result<decisionnode::BatchResults, Error>) -> Result<Vec<BatchResult>, Error> {
    r?.collect()
}

fn batch_run(r: Result<decisionnode::BatchRun, Error>) -> Outcome {
    match r {
        Ok(run) => match run.results.collect::<Result<Vec<_>, _>>() {
            Ok(results) => Outcome::BatchRun(run.batch, results),
            Err(e) => Outcome::Error(e),
        },
        Err(e) => Outcome::Error(e),
    }
}

fn invoke(ctx: &Ctx, client: &Client, call: &Value, refs: &Refs) -> Outcome {
    let prepared = match prepare(&ctx.root, call, refs) {
        Ok(p) => p,
        Err(e) => return Outcome::Error(e),
    };
    match client {
        Client::Async(c) => ctx.runtime.block_on(invoke_async(c, prepared)),
        #[cfg(feature = "blocking")]
        Client::Blocking(c) => invoke_blocking(c, prepared),
    }
}

async fn invoke_async(c: &DecisionNode, p: Prepared) -> Outcome {
    match p {
        Prepared::Decide(r) => wrap(c.decide(r).await, Outcome::Decision),
        Prepared::Systemone(r) => wrap(c.systemone(r).await, Outcome::Decision),
        Prepared::Many(rs, None) => wrap(c.decide_many(rs).await, Outcome::Many),
        Prepared::Many(rs, Some(n)) => wrap(c.decide_many_with(rs, n).await, Outcome::Many),
        Prepared::Check(s, q, o) => wrap(c.check_with(s, q, o).await, Outcome::Truth),
        Prepared::Classify(s, q, o) => wrap(c.classify_with(s, q, o).await, Outcome::Choice),
        Prepared::Score(s, q, o) => wrap(c.score_with(s, q, o).await, Outcome::Score),
        Prepared::Number(s, q, o) => wrap(c.number_with(s, q, o).await, Outcome::Number),
        Prepared::Gate(a, ctx, o) => wrap(c.gate_with(a, ctx, o).await, Outcome::Decision),
        Prepared::Models => wrap(c.models().await, Outcome::Models),
        Prepared::Raw(b) => wrap(c.decide_raw(b).await, Outcome::Raw),
        Prepared::Upload(images) => wrap(c.upload(images).await, Outcome::UploadIds),
        #[cfg(feature = "sessions")]
        Prepared::OpenSession(o) => wrap(c.open_session(o).await, |s| Outcome::Session(SessionHandle::Async(s))),
        #[cfg(feature = "sessions")]
        Prepared::Supervise(o, frames, interval) => {
            use futures_util::StreamExt;
            let feed = futures_util::stream::iter(frames.into_iter().enumerate()).then(move |(i, f)| async move {
                if i > 0 {
                    tokio::time::sleep(interval).await;
                }
                f
            });
            let mut events = Vec::new();
            let closed = c.supervise(o, feed, |e| events.push(e)).await;
            wrap(closed, |closed| Outcome::Supervised(events, closed))
        }
        Prepared::BatchCreate(rs) => wrap(c.batches().create(rs).await, Outcome::Batch),
        Prepared::BatchAdd(id, rs) => wrap(c.batches().add(&id, rs).await, Outcome::Batch),
        Prepared::BatchFinalize(id) => wrap(c.batches().finalize(&id).await, Outcome::Batch),
        Prepared::BatchGet(id) => wrap(c.batches().get(&id).await, Outcome::Batch),
        Prepared::BatchList(o, l) => wrap(c.batches().list(o, l).await, Outcome::BatchPage),
        Prepared::BatchResults(id) => wrap(collect_results(c.batches().results(&id).await), Outcome::BatchResults),
        Prepared::BatchCancel(id) => wrap(c.batches().cancel(&id).await, Outcome::Batch),
        Prepared::RunBatch(rs, poll) => {
            batch_run(c.run_batch_with(rs, poll, decisionnode::DEFAULT_BATCH_TIMEOUT).await)
        }
    }
}

#[cfg(feature = "blocking")]
fn invoke_blocking(c: &decisionnode::blocking::DecisionNode, p: Prepared) -> Outcome {
    match p {
        Prepared::Decide(r) => wrap(c.decide(r), Outcome::Decision),
        Prepared::Systemone(r) => wrap(c.systemone(r), Outcome::Decision),
        Prepared::Many(rs, None) => wrap(c.decide_many(rs), Outcome::Many),
        Prepared::Many(rs, Some(n)) => wrap(c.decide_many_with(rs, n), Outcome::Many),
        Prepared::Check(s, q, o) => wrap(c.check_with(s, q, o), Outcome::Truth),
        Prepared::Classify(s, q, o) => wrap(c.classify_with(s, q, o), Outcome::Choice),
        Prepared::Score(s, q, o) => wrap(c.score_with(s, q, o), Outcome::Score),
        Prepared::Number(s, q, o) => wrap(c.number_with(s, q, o), Outcome::Number),
        Prepared::Gate(a, ctx, o) => wrap(c.gate_with(a, ctx, o), Outcome::Decision),
        Prepared::Models => wrap(c.models(), Outcome::Models),
        Prepared::Raw(b) => wrap(c.decide_raw(b), Outcome::Raw),
        Prepared::Upload(images) => wrap(c.upload(images), Outcome::UploadIds),
        #[cfg(feature = "sessions")]
        Prepared::OpenSession(o) => wrap(c.open_session(o), |s| Outcome::Session(SessionHandle::Blocking(s))),
        #[cfg(feature = "sessions")]
        Prepared::Supervise(o, frames, interval) => {
            let feed = frames.into_iter().enumerate().map(move |(i, f)| {
                if i > 0 {
                    std::thread::sleep(interval);
                }
                f
            });
            let mut events = Vec::new();
            let closed = c.supervise(o, feed, |e| events.push(e));
            wrap(closed, |closed| Outcome::Supervised(events, closed))
        }
        Prepared::BatchCreate(rs) => wrap(c.batches().create(rs), Outcome::Batch),
        Prepared::BatchAdd(id, rs) => wrap(c.batches().add(&id, rs), Outcome::Batch),
        Prepared::BatchFinalize(id) => wrap(c.batches().finalize(&id), Outcome::Batch),
        Prepared::BatchGet(id) => wrap(c.batches().get(&id), Outcome::Batch),
        Prepared::BatchList(o, l) => wrap(c.batches().list(o, l), Outcome::BatchPage),
        Prepared::BatchResults(id) => wrap(collect_results(c.batches().results(&id)), Outcome::BatchResults),
        Prepared::BatchCancel(id) => wrap(c.batches().cancel(&id), Outcome::Batch),
        Prepared::RunBatch(rs, poll) => batch_run(c.run_batch_with(rs, poll, decisionnode::DEFAULT_BATCH_TIMEOUT)),
    }
}

/// Builds the client of a case: the environment first, then the arguments.
fn make_client(case: &Value, mock_url: &str, extra: &[(&str, String)]) -> ClientBuilder {
    for var in ENV_VARS {
        std::env::remove_var(var);
    }
    if let Some(env) = case.get("env").and_then(Value::as_object) {
        for (k, v) in env {
            std::env::set_var(k, fill(v.as_str().unwrap(), mock_url));
        }
    }
    let spec = case.get("client").cloned().unwrap_or_else(|| json!({}));
    let mut b = DecisionNode::builder();
    if let Some(k) = opt(&spec, "api_key").and_then(Value::as_str) {
        b = b.api_key(k);
    }
    if let Some(m) = opt(&spec, "model").and_then(Value::as_str) {
        b = b.model(m);
    }
    // Seconds as given, 0 and negatives included: the SDK must refuse those.
    if let Some(t) = opt(&spec, "timeout").and_then(Value::as_f64) {
        b = b.timeout_secs(t);
    }
    if let Some(r) = opt(&spec, "max_retries").and_then(Value::as_i64) {
        b = b.max_retries(r);
    }
    if let Some(t) = opt(&spec, "inline_threshold").and_then(Value::as_i64) {
        b = b.inline_threshold(t);
    }
    match spec.get("base_url") {
        None => b = b.base_url(mock_url),
        Some(Value::String(u)) => b = b.base_url(fill(u, mock_url)),
        Some(_) => {}
    }
    if opt(&spec, "http_client").and_then(Value::as_str) == Some("caller_following_redirects") {
        // The SDK takes no HTTP client of the caller's making, only transport
        // settings, none of which can turn redirects on. The closest a caller
        // gets is a client built from their own settings.
        b = b.http_settings(decisionnode::HttpSettings::new().pool_max_idle_per_host(2).tcp_nodelay(true));
    }
    if let Some(h) = opt(&spec, "headers").and_then(Value::as_object) {
        for (k, v) in h {
            b = b.header(k.as_str(), v.as_str().unwrap());
        }
    }
    for (k, v) in extra {
        b = b.header(*k, v.as_str());
    }
    b
}

/// Replaces `{mock_url}` and `{mock_port}` with the mock's URL and port.
fn fill(text: &str, mock_url: &str) -> String {
    let port = mock_url.rsplit(':').next().unwrap_or_default().trim_end_matches('/');
    text.replace("{mock_url}", mock_url).replace("{mock_port}", port)
}

fn session() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("rust-{}-{}-{}", std::process::id(), nanos, N.fetch_add(1, Ordering::Relaxed))
}

// --------------------------------------------------------------------------- normalising

fn num(v: f64) -> Value {
    serde_json::Number::from_f64(v).map(Value::Number).unwrap_or(Value::Null)
}

fn norm_safety(s: &Option<Safety>) -> Value {
    let Some(s) = s else { return Value::Null };
    if !s.checked {
        return json!({"checked": false, "reason": s.reason});
    }
    let mut out = Map::new();
    out.insert("checked".into(), json!(true));
    out.insert("harm".into(), s.harm.map(num).unwrap_or(Value::Null));
    out.insert("self_harm".into(), s.self_harm.map(num).unwrap_or(Value::Null));
    for (k, v) in &s.categories {
        if k != "harm" && k != "self_harm" {
            out.insert(k.clone(), num(*v));
        }
    }
    out.insert("flagged".into(), json!(s.flagged));
    Value::Object(out)
}

fn norm_truth(a: &TruthAnswer) -> Value {
    json!({"kind": "truth", "truth": num(a.truth)})
}

fn norm_choice(a: &ChoiceAnswer) -> Value {
    let probs: Map<String, Value> = a.probabilities.iter().map(|(k, v)| (k.clone(), num(*v))).collect();
    json!({"kind": "choice", "choice": a.choice, "confidence": num(a.confidence), "probabilities": probs})
}

fn norm_score(a: &ScoreAnswer) -> Value {
    let probs: Map<String, Value> = a.probabilities.iter().map(|(k, v)| (k.to_string(), num(*v))).collect();
    let legend: Map<String, Value> = a.legend.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    json!({"kind": "score", "score": num(a.score), "confidence": num(a.confidence), "probabilities": probs, "legend": legend})
}

fn norm_number(a: &NumberAnswer) -> Value {
    let probs: Map<String, Value> = a.probabilities.iter().map(|(k, v)| (k.clone(), num(*v))).collect();
    json!({"kind": "number", "number": num(a.number), "expected": num(a.expected),
           "confidence": num(a.confidence), "probabilities": probs})
}

fn norm_answer(a: &decisionnode::Answer) -> Value {
    if let Some(t) = a.as_truth() {
        norm_truth(t)
    } else if let Some(c) = a.as_choice() {
        norm_choice(c)
    } else if let Some(s) = a.as_score() {
        norm_score(s)
    } else if let Some(n) = a.as_number() {
        norm_number(n)
    } else {
        json!({"kind": "unknown", "type": a.answer_type(), "raw": a.raw()})
    }
}

fn norm_answers(answers: &indexmap::IndexMap<String, decisionnode::Answer>) -> Value {
    Value::Object(answers.iter().map(|(k, v)| (k.clone(), norm_answer(v))).collect())
}

fn norm_decision(d: &Decision) -> Value {
    json!({
        "model": d.model,
        "request_id": d.request_id,
        "latency_ms": d.latency_ms.map(num),
        "usage": {"input_tokens": d.usage.input_tokens, "output_tokens": d.usage.output_tokens},
        "safety": norm_safety(&d.safety),
        "answers": norm_answers(&d.answers),
        "raw": d.raw,
    })
}

fn norm_error(e: &Error) -> Value {
    json!({
        "status": e.status(),
        "error_type": e.error_type(),
        // The error's own message, "" when the server sent none.
        "message": e.message(),
        "request_id": e.request_id(),
        "reason": e.reason(),
        "category": e.category(),
        "safety": norm_safety(&e.safety().cloned()),
        "entries": e.entries(),
        "retry_after": e.retry_after().map(|d| num(d.as_secs_f64())),
    })
}

#[cfg(feature = "sessions")]
fn norm_session(s: &SessionHandle) -> Value {
    let mut out = s.raw().clone();
    out["answers"] = norm_answers(s.answers());
    out
}

#[cfg(feature = "sessions")]
fn norm_event(e: &SessionEvent) -> Value {
    match e {
        SessionEvent::Reply(r) => {
            let mut out = json!({
                "seq": r.seq,
                "answers": norm_answers(&r.answers),
                "usage": r.raw.get("usage").cloned().unwrap_or(Value::Null),
                "latency_ms": r.latency_ms.map(num),
                "safety": r.raw.get("safety").cloned().unwrap_or(Value::Null),
            });
            if let Some(d) = &r.dropped {
                out["dropped"] = json!(d);
            }
            json!({"reply": out})
        }
        SessionEvent::FrameError(f) => {
            let mut out = json!({"seq": f.seq, "error_type": f.error_type, "message": f.message});
            if let Some(c) = &f.category {
                out["category"] = json!(c);
            }
            if let Some(r) = f.retry_after_ms {
                out["retry_after_ms"] = json!(r);
            }
            for k in ["usage", "safety"] {
                if let Some(v) = f.raw.get(k).filter(|v| !v.is_null()) {
                    out[k] = v.clone();
                }
            }
            json!({"frame_error": out})
        }
        SessionEvent::Closed(c) => json!({"closed": {"code": c.code, "reopen": c.reopen}}),
        _ => json!({"unknown_event": true}),
    }
}

fn status_json(s: &BatchStatus) -> Value {
    match s {
        BatchStatus::Http(n) => json!(n),
        BatchStatus::Expired => json!("expired"),
        BatchStatus::Cancelled => json!("cancelled"),
        BatchStatus::Other(v) => v.clone(),
        _ => Value::Null,
    }
}

/// Catches an error through the exported name `DecisionNodeError`, as a
/// caller would write it.
fn catch_as_base(err: &Error) -> Option<&decisionnode::DecisionNodeError> {
    let result: Result<(), &decisionnode::DecisionNodeError> = Err(err);
    result.err()
}

fn check_error(spec: &Value, err: &Error, path: &str) -> Result<(), Fail> {
    let cls = spec.get("error").or_else(|| spec.get("class")).and_then(Value::as_str).unwrap();
    let err = match spec.get("catch_as").and_then(Value::as_str) {
        None => err,
        Some("DecisionNodeError") => {
            catch_as_base(err).ok_or_else(|| Fail(format!("{path}: not caught as DecisionNodeError")))?
        }
        Some(other) => return Err(Fail(format!("{path}: no exported error named {other}"))),
    };
    let kind = err.kind();
    let ok = if spec.get("subclass_ok").and_then(Value::as_bool).unwrap_or(false) {
        cls == "DecisionNodeError" || kind.name() == cls || kind.parent().is_some_and(|p| p.name() == cls)
    } else {
        kind.name() == cls
    };
    if !ok {
        return Err(Fail(format!("{path}: expected {cls}, got {} ({err})", kind.name())));
    }
    matches(spec.get("fields").unwrap_or(&json!({})), &norm_error(err), &format!("{path}.fields"))
}

fn describe(o: &Outcome) -> String {
    match o {
        Outcome::Error(e) => format!("error {} ({e})", e.kind()),
        _ => "a result".into(),
    }
}

fn check_batch_results(spec: &Value, results: &[BatchResult], path: &str) -> Result<(), Fail> {
    let expected = spec.as_array().cloned().unwrap_or_default();
    if expected.len() != results.len() {
        return Err(Fail(format!("{path}: expected {} results, got {}", expected.len(), results.len())));
    }
    for (i, (spec, r)) in expected.iter().zip(results).enumerate() {
        let at = format!("{path}[{i}]");
        if let Some(c) = spec.get("custom_id") {
            if c != r.custom_id.as_str() {
                return Err(Fail(format!("{at}.custom_id: expected {c}, got {}", r.custom_id)));
            }
        }
        if let Some(s) = spec.get("status") {
            if !same(s, &status_json(&r.status)) {
                return Err(Fail(format!("{at}.status: expected {s}, got {:?}", r.status)));
            }
        }
        if let Some(h) = spec.get("headers") {
            let got: Map<String, Value> = r.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            matches(h, &Value::Object(got), &format!("{at}.headers"))?;
        }
        if let Some(d) = spec.get("decision").filter(|d| !d.is_null()) {
            let got = r.decision.as_ref().ok_or_else(|| Fail(format!("{at}: expected a decision, got none")))?;
            matches(d, &norm_decision(got), &format!("{at}.decision"))?;
        }
        if let Some(e) = spec.get("error") {
            let got = r.error.as_ref().ok_or_else(|| Fail(format!("{at}: expected an error value, got none")))?;
            check_error(e, got, &format!("{at}.error"))?;
        }
        if spec.get("no_decision").and_then(Value::as_bool) == Some(true) && r.decision.is_some() {
            return Err(Fail(format!("{at}: expected no decision")));
        }
    }
    Ok(())
}

fn in_range(r: &Value, value: f64, what: &str) -> Result<(), Fail> {
    if let Some(min) = r.get("min").and_then(Value::as_f64) {
        if value < min {
            return Err(Fail(format!("{what} {value:.0} is below {min}")));
        }
    }
    if let Some(max) = r.get("max").and_then(Value::as_f64) {
        if value > max {
            return Err(Fail(format!("{what} {value:.0} is above {max}")));
        }
    }
    Ok(())
}

/// An outcome by reference, so one result of `decide_many` is checked like a
/// call's.
enum OutcomeRef<'a> {
    Decision(&'a Decision),
    Error(&'a Error),
    Other(&'a Outcome),
}

fn check_decision_result(spec: &Value, result: Result<&Decision, &Error>, path: &str) -> Result<(), Fail> {
    match result {
        Ok(d) => check_outcome(spec, &OutcomeRef::Decision(d), path),
        Err(e) => check_outcome(spec, &OutcomeRef::Error(e), path),
    }
}

fn check_outcome(spec: &Value, result: &OutcomeRef<'_>, path: &str) -> Result<(), Fail> {
    let error = match result {
        OutcomeRef::Error(e) => Some(*e),
        OutcomeRef::Other(Outcome::Error(e)) => Some(e),
        _ => None,
    };
    if spec.get("argument_error").and_then(Value::as_bool) == Some(true) {
        return match error {
            Some(e) if e.is_argument() => Ok(()),
            Some(e) => Err(Fail(format!("{path}: expected an argument error, got {} ({e})", e.kind()))),
            None => Err(Fail(format!("{path}: expected an argument error, got a result"))),
        };
    }
    let Some(kind) = spec.get("result").and_then(Value::as_str) else {
        return match error {
            Some(e) if e.is_argument() && spec.get("subclass_ok").and_then(Value::as_bool) != Some(true) => {
                Err(Fail(format!("{path}: unexpected argument error: {e}")))
            }
            Some(e) => check_error(spec, e, path),
            None => Err(Fail(format!("{path}: expected {}, got a result", spec["error"]))),
        };
    };
    if let Some(e) = error {
        return Err(Fail(format!("{path}: expected {kind}, got {} ({e})", e.kind())));
    }
    let outcome = match result {
        OutcomeRef::Decision(d) => {
            if kind != "decision" {
                return Err(Fail(format!("{path}: expected {kind}, got a decision")));
            }
            return match spec.get("decision") {
                Some(exp) => matches(exp, &norm_decision(d), &format!("{path}.decision")),
                None => Ok(()),
            };
        }
        OutcomeRef::Other(o) => *o,
        OutcomeRef::Error(_) => unreachable!("an error is handled above"),
    };
    let empty = json!({});
    match (kind, outcome) {
        ("decision", Outcome::Decision(d)) => match spec.get("decision") {
            Some(exp) => matches(exp, &norm_decision(d), &format!("{path}.decision")),
            None => Ok(()),
        },
        ("answer", o) => {
            let got = match o {
                Outcome::Truth(t) => norm_truth(t),
                Outcome::Choice(c) => norm_choice(c),
                Outcome::Score(s) => norm_score(s),
                Outcome::Number(n) => norm_number(n),
                o => return Err(Fail(format!("{path}: expected an answer, got {}", describe(o)))),
            };
            matches(spec.get("answer").unwrap_or(&empty), &got, &format!("{path}.answer"))
        }
        ("batch", Outcome::Batch(b)) => matches(spec.get("batch").unwrap_or(&empty), &b.raw, &format!("{path}.batch")),
        ("batch_page", Outcome::BatchPage(p)) => {
            matches(spec.get("batch_page").unwrap_or(&empty), &p.raw, &format!("{path}.batch_page"))
        }
        ("batch_results", Outcome::BatchResults(r)) => {
            check_batch_results(&spec["batch_results"], r, &format!("{path}.batch_results"))
        }
        ("batch_run", Outcome::BatchRun(b, r)) => {
            matches(spec.get("batch").unwrap_or(&empty), &b.raw, &format!("{path}.batch"))?;
            match spec.get("batch_results") {
                Some(exp) => check_batch_results(exp, r, &format!("{path}.batch_results")),
                None => Ok(()),
            }
        }
        #[cfg(feature = "sessions")]
        ("session", Outcome::Session(s)) => {
            matches(spec.get("session").unwrap_or(&empty), &norm_session(s), &format!("{path}.session"))
        }
        ("upload_ids", Outcome::UploadIds(ids)) => {
            if let Some(n) = spec.get("count").and_then(Value::as_u64) {
                if ids.len() as u64 != n {
                    return Err(Fail(format!("{path}: expected {n} upload ids, got {ids:?}")));
                }
            }
            let re = regex::Regex::new("^upl_[0-9a-f]{32}$").unwrap();
            if let Some(bad) = ids.iter().find(|id| !re.is_match(id)) {
                return Err(Fail(format!("{path}: {bad:?} is not an upload id")));
            }
            if let Some(distinct) = spec.get("distinct").and_then(Value::as_bool) {
                let unique: std::collections::HashSet<_> = ids.iter().collect();
                if (unique.len() == ids.len()) != distinct {
                    return Err(Fail(format!("{path}: upload ids {ids:?}")));
                }
            }
            Ok(())
        }
        #[cfg(feature = "sessions")]
        ("supervised", Outcome::Supervised(events, closed)) => {
            let replies = events.iter().filter(|e| matches!(e, SessionEvent::Reply(_))).count();
            in_range(spec.get("replies").unwrap_or(&empty), replies as f64, "replies")?;
            if let Some(exp) = spec.get("closed") {
                let got = closed.as_ref().map(|c| json!({"code": c.code, "reopen": c.reopen})).unwrap_or(Value::Null);
                if !same(exp, &got) {
                    return Err(Fail(format!("{path}.closed: expected {exp}, got {got}")));
                }
            }
            Ok(())
        }
        (kind, o) => Err(Fail(format!("{path}: expected {kind}, got {}", describe(o)))),
    }
}

// --------------------------------------------------------------------------- requests

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 3 <= bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(text: &str) -> Map<String, Value> {
    let mut out = Map::new();
    for pair in text.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.entry(percent_decode(k)).or_insert(Value::String(percent_decode(v)));
    }
    out
}

fn parse_jsonl(text: &str) -> Result<Vec<Value>, Fail> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(Vec::new());
    }
    if t.starts_with('[') {
        return serde_json::from_str::<Vec<Value>>(t).map_err(|e| Fail(format!("body is not a JSON array: {e}")));
    }
    text.split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| Fail(format!("a body line is not JSON: {e}"))))
        .collect()
}

fn json_type(t: &str, v: &Value) -> bool {
    match t {
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => false,
    }
}

fn check_value(check: &Value, body: &Value, at: &str) -> Result<(), Fail> {
    let ptr = check["pointer"].as_str().unwrap();
    let found = body.pointer(ptr);
    if check.get("absent").and_then(Value::as_bool) == Some(true) {
        return match found {
            Some(v) => Err(Fail(format!("{at} body{ptr}: must be absent, got {}", short(v)))),
            None => Ok(()),
        };
    }
    let val = found.ok_or_else(|| Fail(format!("{at} body{ptr}: missing")))?;
    if let Some(eq) = check.get("equals") {
        if !same(eq, val) {
            return Err(Fail(format!("{at} body{ptr}: expected {}, got {}", short(eq), short(val))));
        }
    }
    if let Some(t) = check.get("type").and_then(Value::as_str) {
        if !json_type(t, val) {
            return Err(Fail(format!("{at} body{ptr}: expected a {t}")));
        }
    }
    if let Some(p) = check.get("pattern").and_then(Value::as_str) {
        let re = regex::Regex::new(p).unwrap();
        if !val.as_str().is_some_and(|s| re.is_match(s)) {
            return Err(Fail(format!("{at} body{ptr}: {} does not match {p}", short(val))));
        }
    }
    if let Some(keys) = check.get("keys") {
        let got: Vec<Value> = val.as_object().map(|o| o.keys().map(|k| json!(k)).collect()).unwrap_or_default();
        if &Value::Array(got.clone()) != keys {
            return Err(Fail(format!("{at} body{ptr}: keys expected {keys}, got {}", Value::Array(got))));
        }
    }
    Ok(())
}

/// Checks one recorded request against an expected entry. `any_body` lets
/// an entry of a request sequence leave the body unchecked.
fn check_request(exp: &Value, got: &Value, at: &str, any_body: bool) -> Result<(), Fail> {
    if got["method"] != exp["method"] {
        return Err(Fail(format!("{at} method: expected {}, got {} {}", exp["method"], got["method"], got["path"])));
    }
    let path = got["path"].as_str().unwrap_or_default();
    if let Some(p) = exp.get("path") {
        if p != path {
            return Err(Fail(format!("{at} path: expected {p}, got {path}")));
        }
    }
    if let Some(p) = exp.get("path_pattern").and_then(Value::as_str) {
        if !regex::Regex::new(p).unwrap().is_match(path) {
            return Err(Fail(format!("{at} path: {path} does not match {p}")));
        }
    }
    if let Some(q) = exp.get("query") {
        let parsed = Value::Object(parse_query(got["query"].as_str().unwrap_or_default()));
        if !same(q, &parsed) {
            return Err(Fail(format!("{at} query: expected {q}, got {parsed}")));
        }
    }
    if let Some(s) = exp.get("status") {
        if &got["status"] != s {
            return Err(Fail(format!("{at} status: the mock answered {}, expected {s}", got["status"])));
        }
    }
    let headers = &got["headers"];
    if let Some(h) = exp.get("headers").and_then(Value::as_object) {
        for (name, value) in h {
            let actual = &headers[name.to_lowercase()];
            if actual != value {
                return Err(Fail(format!("{at} header {name}: expected {value}, got {actual}")));
            }
        }
    }
    if let Some(h) = exp.get("header_patterns").and_then(Value::as_object) {
        for (name, pattern) in h {
            let re = regex::Regex::new(pattern.as_str().unwrap()).unwrap();
            match headers[name.to_lowercase()].as_str() {
                Some(v) if re.is_match(v) => {}
                other => return Err(Fail(format!("{at} header {name}: {other:?} does not match {pattern}"))),
            }
        }
    }
    if let Some(list) = exp.get("absent_headers").and_then(Value::as_array) {
        for name in list {
            let name = name.as_str().unwrap().to_lowercase();
            if headers.get(&name).is_some() {
                return Err(Fail(format!("{at} header {name} must not be sent")));
            }
        }
    }
    if let Some(b) = exp.get("body_base64") {
        if &got["body_base64"] != b {
            return Err(Fail(format!("{at} body: the bytes differ from the expected ones")));
        }
        return Ok(());
    }
    if let Some(h) = exp.get("body_sha256").and_then(Value::as_str) {
        let raw = base64::engine::general_purpose::STANDARD.decode(got["body_base64"].as_str().unwrap_or("")).unwrap();
        let digest: String = sha2::Sha256::digest(raw).iter().map(|b| format!("{b:02x}")).collect();
        if digest != h {
            return Err(Fail(format!("{at} body: sha256 {digest}, expected {h}")));
        }
        return Ok(());
    }
    let text = got["body_text"].as_str().unwrap_or("");
    if let Some(lines) = exp.get("jsonl") {
        let got_lines = Value::Array(parse_jsonl(text)?);
        if !same(lines, &got_lines) {
            return Err(Fail(format!("{at} body lines: expected {}, got {}", short(lines), short(&got_lines))));
        }
        return Ok(());
    }
    if exp.get("jsonl_count").is_some() || exp.get("jsonl_checks").is_some() {
        let lines = parse_jsonl(text)?;
        if let Some(n) = exp.get("jsonl_count").and_then(Value::as_u64) {
            if lines.len() as u64 != n {
                return Err(Fail(format!("{at} body: expected {n} lines, got {}", lines.len())));
            }
        }
        for c in exp.get("jsonl_checks").and_then(Value::as_array).into_iter().flatten() {
            let i = c["line"].as_u64().unwrap() as usize;
            let line = lines.get(i).ok_or_else(|| Fail(format!("{at} body: no line {i}")))?;
            check_value(c, line, &format!("{at} line {i}"))?;
        }
        return Ok(());
    }
    if any_body && exp.get("body").is_none() && exp.get("body_checks").is_none() {
        return Ok(());
    }
    let body: Value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(text).map_err(|e| Fail(format!("{at} body is not JSON: {e}")))?
    };
    if let Some(eb) = exp.get("body") {
        if !same(eb, &body) {
            return Err(Fail(format!("{at} body: expected {}, got {}", short(eb), short(&body))));
        }
    } else if exp.get("body_checks").is_none() && !text.is_empty() {
        return Err(Fail(format!("{at} expected no body, got {}", short(&body))));
    }
    for c in exp.get("body_checks").and_then(Value::as_array).into_iter().flatten() {
        check_value(c, &body, at)?;
    }
    if let Some(ptrs) = exp.get("ordered_keys").and_then(Value::as_array) {
        for p in ptrs {
            let p = p.as_str().unwrap();
            let want: Vec<&String> =
                exp["body"].pointer(p).and_then(Value::as_object).map(|o| o.keys().collect()).unwrap_or_default();
            let have: Vec<&String> =
                body.pointer(p).and_then(Value::as_object).map(|o| o.keys().collect()).unwrap_or_default();
            if want != have {
                return Err(Fail(format!("{at} body{p}: key order expected {want:?}, got {have:?}")));
            }
        }
    }
    Ok(())
}

/// Every permutation of `0..n`, for the parallel `PUT`s of one upload.
fn permutations(n: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return vec![vec![]];
    }
    let mut out = Vec::new();
    for p in permutations(n - 1) {
        for i in 0..=p.len() {
            let mut q = p.clone();
            q.insert(i, n - 1);
            out.push(q);
        }
    }
    out
}

fn describe_log(log: &[Value]) -> Vec<String> {
    log.iter().map(|r| format!("{} {}", r["method"].as_str().unwrap_or(""), r["path"].as_str().unwrap_or(""))).collect()
}

fn check_request_list(expected: &[Value], log: &[Value]) -> Result<(), Fail> {
    if expected.len() != log.len() {
        return Err(Fail(format!(
            "expected {} requests, the mock received {}: {:?}",
            expected.len(),
            log.len(),
            describe_log(log)
        )));
    }
    let parallel = |e: &Value| e.get("parallel").and_then(Value::as_bool) == Some(true);
    let mut i = 0;
    while i < expected.len() {
        if !parallel(&expected[i]) {
            check_request(&expected[i], &log[i], &format!("requests[{i}]"), false)?;
            i += 1;
            continue;
        }
        let mut j = i;
        while j < expected.len() && parallel(&expected[j]) {
            j += 1;
        }
        // Requests sent in parallel may arrive in any order: one assignment
        // must hold.
        let mut first_reason = None;
        let ok = permutations(j - i).into_iter().any(|perm| {
            let r = perm.iter().enumerate().try_for_each(|(k, &g)| {
                check_request(&expected[i + k], &log[i + g], &format!("requests[{}]", i + k), false)
            });
            if let Err(Fail(m)) = &r {
                first_reason.get_or_insert(m.clone());
            }
            r.is_ok()
        });
        if !ok {
            return Err(Fail(format!(
                "no order of the parallel requests matches: {}",
                first_reason.unwrap_or_default()
            )));
        }
        i = j;
    }
    Ok(())
}

// --------------------------------------------------------------------------- the five kinds

fn run_request(ctx: &Ctx, driver: Driver, case: &Value) -> Result<(), Fail> {
    let s = session();
    let mut extra = vec![("x-mock-session", s.clone())];
    if case.get("mock").is_some_and(|m| !m.is_null()) {
        extra.push(("x-mock-scenario", format!("request/{}", case["id"].as_str().unwrap())));
    }
    let b = make_client(case, &ctx.mock.url, &extra);
    let outcome = match build(driver, b) {
        Ok(client) => invoke(ctx, &client, &case["call"], &Refs::new()),
        Err(e) => Outcome::Error(e),
    };
    if let Outcome::Error(e) = &outcome {
        return Err(Fail(format!("the call failed: {} ({e})", e.kind())));
    }
    std::thread::sleep(Duration::from_millis(20));
    let log = ctx.mock.requests(&s);
    let log = log["requests"].as_array().cloned().unwrap_or_default();
    let exp = &case["expect"];
    match exp.get("request") {
        Some(one) => {
            if log.len() != 1 {
                return Err(Fail(format!("expected 1 request, the mock received {}", log.len())));
            }
            check_request(one, &log[0], "request", false)
        }
        None => check_request_list(exp["requests"].as_array().unwrap(), &log),
    }
}

fn run_response(ctx: &Ctx, driver: Driver, case: &Value) -> Result<(), Fail> {
    let s = session();
    let id = case["id"].as_str().unwrap();
    let scenario = format!("response/{id}");
    let b = make_client(
        &json!({"client": {"api_key": "dn_test_conformance"}}),
        &ctx.mock.url,
        &[("x-mock-session", s), ("x-mock-scenario", scenario)],
    );
    let outcome = match build(driver, b) {
        Ok(client) => invoke(ctx, &client, &case["call"], &Refs::new()),
        Err(e) => Outcome::Error(e),
    };
    let exp = &case["expect"];
    let check = |kind: &str| {
        let spec = json!({"result": kind, kind: exp[kind].clone()});
        check_outcome(&spec, &OutcomeRef::Other(&outcome), "")
    };
    if exp.get("decision").is_some() {
        check("decision")
    } else if exp.get("answer").is_some() {
        check("answer")
    } else if let Some(m) = exp.get("models") {
        match &outcome {
            Outcome::Models(list) => {
                let got: Vec<Value> = list
                    .iter()
                    .map(|m| json!({"name": m.name, "description": m.description, "release_date": m.release_date, "limits": m.limits}))
                    .collect();
                let got = Value::Array(got);
                if same(m, &got) {
                    Ok(())
                } else {
                    Err(Fail(format!("models: expected {}, got {}", short(m), short(&got))))
                }
            }
            o => Err(Fail(format!("expected models, got {}", describe(o)))),
        }
    } else if let Some(r) = exp.get("raw") {
        match &outcome {
            Outcome::Raw(got) if same(r, got) => Ok(()),
            Outcome::Raw(got) => Err(Fail(format!("raw: expected {}, got {}", short(r), short(got)))),
            o => Err(Fail(format!("expected a raw body, got {}", describe(o)))),
        }
    } else if let Some(kind) =
        ["batch", "batch_page", "batch_results", "session"].into_iter().find(|k| exp.get(*k).is_some())
    {
        check(kind)
    } else if let Some(ids) = exp.get("upload_ids") {
        match &outcome {
            Outcome::UploadIds(got) if same(ids, &json!(got)) => Ok(()),
            o => Err(Fail(format!("upload_ids: expected {ids}, got {}", describe(o)))),
        }
    } else {
        Err(Fail("the case has no expectation this runner knows".into()))
    }
}

fn run_error(ctx: &Ctx, driver: Driver, case: &Value) -> Result<(), Fail> {
    let s = session();
    let scenario = format!("error/{}", case["id"].as_str().unwrap());
    let b = make_client(case, &ctx.mock.url, &[("x-mock-session", s.clone()), ("x-mock-scenario", scenario)]);
    let outcome = match build(driver, b) {
        Ok(client) => invoke(ctx, &client, &case["call"], &Refs::new()),
        Err(e) => Outcome::Error(e),
    };
    match outcome {
        Outcome::Error(e) => check_error(&case["expect"]["error"], &e, "error")?,
        _ => return Err(Fail("no error was returned".into())),
    }
    let n = ctx.mock.requests(&s)["requests"].as_array().map(Vec::len).unwrap_or(0);
    if n != 1 {
        return Err(Fail(format!("expected 1 attempt with max_retries 0, got {n}")));
    }
    Ok(())
}

#[cfg(feature = "sessions")]
fn session_of<'a>(refs: &'a Refs, name: &str, at: &str) -> Result<&'a SessionHandle, Fail> {
    match refs.get(name) {
        Some(Outcome::Session(s)) => Ok(s),
        _ => Err(Fail(format!("{at}: no session saved as {name}"))),
    }
}

/// One session step: connect, send, read or end.
#[cfg(feature = "sessions")]
fn session_step(ctx: &Ctx, step: &Value, refs: &Refs, events: &mut Vec<Value>, at: &str) -> Result<(), Fail> {
    if let Some(name) = step.get("connect").and_then(Value::as_str) {
        // A refused handshake comes back as the first event.
        session_of(refs, name, at)?.connect(ctx).map_err(|e| Fail(format!("{at}: connect failed: {e}")))
    } else if let Some(send) = step.get("send") {
        let send = resolve(send, refs);
        let s = session_of(refs, send["session"].as_str().unwrap(), at)?;
        let f = frame(&ctx.root, &send).map_err(|e| Fail(format!("{at}: frame: {e}")))?;
        s.send(ctx, f).map(|_| ()).map_err(|e| Fail(format!("{at}: send failed: {} ({e})", e.kind())))
    } else if let Some(read) = step.get("read") {
        let s = session_of(refs, read["session"].as_str().unwrap(), at)?;
        let mut count = read.get("count").and_then(Value::as_u64);
        let timeout = Duration::from_millis(read.get("timeout_ms").and_then(Value::as_u64).unwrap_or(10_000));
        loop {
            let event = s.next_event(ctx, timeout).map_err(|e| Fail(format!("{at}: reading failed: {e}")))?;
            let event = event.ok_or_else(|| {
                Fail(format!("{at}: no session event arrived in time ({} read so far)", events.len()))
            })?;
            events.push(norm_event(&event));
            if matches!(event, SessionEvent::Closed(_)) {
                return Ok(());
            }
            if let Some(n) = count.as_mut() {
                *n -= 1;
                if *n == 0 {
                    return Ok(());
                }
            }
        }
    } else if let Some(name) = step.get("end").and_then(Value::as_str) {
        match session_of(refs, name, at)?.end(ctx) {
            Ok(()) => events.push(json!({"ended": true})),
            Err(e) => events.push(json!({"end_error": {"class": e.kind().name(), "status": e.status()}})),
        }
        Ok(())
    } else {
        Err(Fail(format!("{at}: unknown step {step}")))
    }
}

#[cfg(not(feature = "sessions"))]
fn session_step(_: &Ctx, step: &Value, _: &Refs, _: &mut Vec<Value>, at: &str) -> Result<(), Fail> {
    Err(Fail(format!("{at}: {step} needs the sessions feature")))
}

/// Runs the steps of a behaviour case on one client and records the session
/// events.
fn run_steps(ctx: &Ctx, client: &Client, case: &Value, session_id: &str, events: &mut Vec<Value>) -> Result<(), Fail> {
    let mut refs = Refs::new();
    for (i, step) in case["steps"].as_array().unwrap().iter().enumerate() {
        let at = format!("steps[{i}]");
        if let Some(call) = step.get("call") {
            let outcome = invoke(ctx, client, call, &refs);
            if let Some(spec) = step.get("outcome") {
                check_outcome(spec, &OutcomeRef::Other(&outcome), &format!("{at}.outcome"))?;
            } else if let Outcome::Error(e) = &outcome {
                return Err(Fail(format!("{at}: the call failed: {} ({e})", e.kind())));
            }
            if let Some(name) = step.get("save_as").and_then(Value::as_str) {
                refs.insert(name.to_owned(), outcome);
            }
        } else if let Some(body) = step.get("mock_clock") {
            ctx.mock.clock(session_id, body);
        } else if let Some(ms) = step.get("sleep_ms").and_then(Value::as_u64) {
            std::thread::sleep(Duration::from_millis(ms));
        } else {
            session_step(ctx, step, &refs, events, &at)?;
        }
    }
    Ok(())
}

fn check_sequence(expected: &[Value], log: &[Value]) -> Result<(), Fail> {
    let got = describe_log(log);
    if expected.len() != log.len() {
        return Err(Fail(format!(
            "request sequence: expected {} requests, the mock received {}: {got:?}",
            expected.len(),
            log.len()
        )));
    }
    for (i, (spec, rec)) in expected.iter().zip(log).enumerate() {
        check_request(spec, rec, &format!("request sequence[{i}] ({})", got[i]), true)?;
    }
    Ok(())
}

fn check_counts(expected: &[Value], log: &[Value]) -> Result<(), Fail> {
    for spec in expected {
        let pattern = spec.get("path_pattern").and_then(Value::as_str).map(|p| regex::Regex::new(p).unwrap());
        let n = log
            .iter()
            .filter(|r| {
                r["method"] == spec["method"]
                    && spec.get("path").is_none_or(|p| &r["path"] == p)
                    && pattern.as_ref().is_none_or(|re| re.is_match(r["path"].as_str().unwrap_or("")))
            })
            .count();
        let what =
            format!("{} {}", spec["method"], spec.get("path").or(spec.get("path_pattern")).unwrap_or(&Value::Null));
        if let Some(c) = spec.get("count").and_then(Value::as_u64) {
            if n as u64 != c {
                return Err(Fail(format!("{what}: expected {c} requests, got {n}")));
            }
        }
        in_range(spec, n as f64, &what)?;
    }
    Ok(())
}

fn check_events(expected: &[Value], events: &[Value]) -> Result<(), Fail> {
    if expected.len() != events.len() {
        return Err(Fail(format!(
            "session events: expected {}, got {}: {}",
            expected.len(),
            events.len(),
            short(&Value::Array(events.to_vec()))
        )));
    }
    for (i, (spec, ev)) in expected.iter().zip(events).enumerate() {
        let (key, want) = spec.as_object().unwrap().iter().next().unwrap();
        let got = ev.get(key).ok_or_else(|| Fail(format!("session events[{i}]: expected {key}, got {}", short(ev))))?;
        if want.is_object() {
            matches(want, got, &format!("session events[{i}].{key}"))?;
        } else if !same(want, got) {
            return Err(Fail(format!("session events[{i}]: expected {}, got {}", short(spec), short(ev))));
        }
    }
    Ok(())
}

fn run_behaviour(ctx: &Ctx, driver: Driver, case: &Value) -> Result<(), Fail> {
    let s = session();
    let mut extra = vec![("x-mock-session", s.clone())];
    if case.get("mock").is_some_and(|m| !m.is_null()) {
        extra.push(("x-mock-scenario", format!("behaviour/{}", case["id"].as_str().unwrap())));
    }
    let mut b = make_client(case, &ctx.mock.url, &extra);
    let exp = &case["expect"];
    // Waits the client schedules are recorded and skipped, so a 60 s cap is
    // checked without spending 60 s.
    let waits = Arc::new(Mutex::new(Vec::<Duration>::new()));
    if exp.get("scheduled_waits_ms").is_some() {
        let seen = waits.clone();
        b = b.sleeper(move |d| {
            seen.lock().unwrap().push(d);
            Box::pin(async {})
        });
    }
    let mut events = Vec::new();
    let started = Instant::now();
    let outcome = match build(driver, b) {
        Ok(client) => {
            if case.get("steps").is_some() {
                run_steps(ctx, &client, case, &s, &mut events)?;
                None
            } else {
                Some(invoke(ctx, &client, &case["call"], &Refs::new()))
            }
        }
        Err(e) => Some(Outcome::Error(e)),
    };
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    std::thread::sleep(Duration::from_millis(50));
    let info = ctx.mock.requests(&s);
    let log = info["requests"].as_array().cloned().unwrap_or_default();
    let check_one = |alt: &Value| -> Result<(), Fail> {
        if let Some(n) = alt.get("attempts").and_then(Value::as_u64) {
            if log.len() as u64 != n {
                return Err(Fail(format!(
                    "attempts: expected {n}, the mock received {}: {:?}",
                    log.len(),
                    describe_log(&log)
                )));
            }
        }
        if let Some(spec) = alt.get("outcome") {
            match &outcome {
                Some(o) => check_outcome(spec, &OutcomeRef::Other(o), "outcome")?,
                None => return Err(Fail("outcome: the case has steps".into())),
            }
        }
        Ok(())
    };
    if let Some(alts) = exp.get("one_of").and_then(Value::as_array) {
        // The rule allows more than one way to comply; one must hold whole.
        let mut reasons = Vec::new();
        if !alts.iter().enumerate().any(|(i, alt)| match check_one(alt) {
            Ok(()) => true,
            Err(Fail(m)) => {
                reasons.push(format!("one_of[{i}]: {m}"));
                false
            }
        }) {
            return Err(Fail(reasons.join("; ")));
        }
    } else {
        check_one(exp)?;
    }
    if let Some(seq) = exp.get("request_sequence").and_then(Value::as_array) {
        check_sequence(seq, &log)?;
    }
    if let Some(counts) = exp.get("request_counts").and_then(Value::as_array) {
        check_counts(counts, &log)?;
    }
    if let Some(exp_events) = exp.get("session_events").and_then(Value::as_array) {
        check_events(exp_events, &events)?;
    }
    if let Some(r) = exp.get("pongs") {
        in_range(r, info["pongs"].as_f64().unwrap_or(0.0), "pongs")?;
    }
    if let Some(results) = exp.get("results").and_then(Value::as_array) {
        let Some(Outcome::Many(got)) = &outcome else {
            return Err(Fail(format!(
                "results: expected a list, got {}",
                outcome.as_ref().map(describe).unwrap_or_default()
            )));
        };
        if got.len() != results.len() {
            return Err(Fail(format!("results: expected {}, got {}", results.len(), got.len())));
        }
        for (i, (spec, r)) in results.iter().zip(got).enumerate() {
            check_decision_result(spec, r.as_ref(), &format!("results[{i}]"))?;
        }
    }
    if let Some(gaps) = exp.get("gaps_ms").and_then(Value::as_array) {
        let times: Vec<f64> = log.iter().map(|r| r["time"].as_f64().unwrap()).collect();
        for (i, r) in gaps.iter().enumerate() {
            let gap = (times[i + 1] - times[i]) * 1000.0;
            in_range(r, gap, &format!("gap {} (ms)", i + 1))?;
        }
    }
    if let Some(r) = exp.get("elapsed_ms") {
        in_range(r, elapsed, "elapsed (ms)")?;
    }
    if let Some(r) = exp.get("max_in_flight") {
        in_range(r, info["max_in_flight"].as_f64().unwrap_or(0.0), "max in flight")?;
    }
    if let Some(ranges) = exp.get("scheduled_waits_ms").and_then(Value::as_array) {
        let got = waits.lock().unwrap().clone();
        if got.len() != ranges.len() {
            return Err(Fail(format!("scheduled waits: expected {}, got {got:?}", ranges.len())));
        }
        for (i, (r, d)) in ranges.iter().zip(&got).enumerate() {
            in_range(r, d.as_secs_f64() * 1000.0, &format!("scheduled wait {} (ms)", i + 1))?;
        }
    }
    if let Some(paths) = exp.get("unrequested_paths").and_then(Value::as_array) {
        for p in paths {
            let p = p.as_str().unwrap();
            let n = ctx.mock.requests_at(p)["requests"].as_array().map(Vec::len).unwrap_or(0);
            if n != 0 {
                return Err(Fail(format!("{p} was requested {n} time(s)")));
            }
        }
    }
    Ok(())
}

fn run_constant(case: &Value) -> Result<(), Fail> {
    let name = case["constant"].as_str().unwrap();
    let got = match name {
        "SPEC_VERSION" => json!(decisionnode::SPEC_VERSION),
        "DEFAULT_INLINE_THRESHOLD" => json!(decisionnode::DEFAULT_INLINE_THRESHOLD),
        other => return Err(Fail(format!("no constant {other}"))),
    };
    if same(&case["equals"], &got) {
        Ok(())
    } else {
        Err(Fail(format!("{name}: expected {}, got {got}", case["equals"])))
    }
}
