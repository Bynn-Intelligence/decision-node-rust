//! Runs the DecisionNode SDK conformance suite against this crate.
//!
//! The suite is a directory of language-neutral cases and a mock API server.
//! Point `DECISIONNODE_CONFORMANCE_DIR` at it and run
//!
//! ```sh
//! cargo test --features blocking --test conformance
//! cargo test --features blocking --test conformance -- --kind behaviour --case retry-408 -v
//! ```
//!
//! Every case runs with the asynchronous client, and again with the blocking
//! client when the `blocking` feature is on. Without the variable the test
//! fails: it never skips.
//!
//! Options: `--kind <requests|responses|errors|behaviour|constants>` (repeatable),
//! `--case <id>` (repeatable), `--driver <async|blocking>`, `--mock-url <url>`
//! (use a mock that is already running), `-v` (print every case).

mod compare;
mod mock;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use decisionnode::{
    CallOptions, Choice, ChoiceAnswer, ClientBuilder, DecideRequest, Decision, DecisionNode, DecisionResult, Error,
    Image, Levels, ModelInfo, Options, Question, Safety, Score, ScoreAnswer, Truth, TruthAnswer,
};
use serde_json::{json, Map, Value};

use compare::{matches, same, short, Fail};
use mock::Mock;

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

// --------------------------------------------------------------------------- calls

enum Prepared {
    Decide(DecideRequest),
    Many(Vec<DecideRequest>, Option<i64>),
    Check(Value, Truth, CallOptions),
    Classify(Value, Choice, CallOptions),
    Score(Value, Score, CallOptions),
    Gate(Value, Value, CallOptions),
    Models,
    Raw(Value),
}

#[allow(clippy::large_enum_variant)]
enum Outcome {
    Decision(Decision),
    Truth(TruthAnswer),
    Choice(ChoiceAnswer),
    Score(ScoreAnswer),
    Models(Vec<ModelInfo>),
    Raw(Value),
    Many(Vec<DecisionResult>),
    Error(Error),
}

fn opt<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|v| !v.is_null())
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

fn single(value: &Value) -> Option<(&str, &Value)> {
    let obj = value.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    obj.iter().next().map(|(k, v)| (k.as_str(), v))
}

fn question(value: &Value) -> Question {
    match single(value) {
        Some(("$truth", a)) => build_truth(a).into(),
        Some(("$choice", a)) => build_choice(a).into(),
        Some(("$score", a)) => build_score(a).into(),
        _ => Question::from(value.clone()),
    }
}

fn image(root: &Path, value: &Value) -> Result<Image, Error> {
    let a = match single(value) {
        Some(("$image", a)) => a,
        _ => panic!("not an image: {value}"),
    };
    let id = a["id"].as_str().unwrap();
    if let Some(p) = a.get("path").and_then(Value::as_str) {
        Image::path(id, root.join(p))
    } else if let Some(u) = a.get("url").and_then(Value::as_str) {
        Ok(Image::url(id, u))
    } else {
        let data = base64::engine::general_purpose::STANDARD.decode(a["data_base64"].as_str().unwrap()).unwrap();
        match a.get("media_type").and_then(Value::as_str) {
            Some(t) => Ok(Image::bytes_with_type(id, data, t)),
            None => Image::bytes(id, data),
        }
    }
}

fn images(root: &Path, args: &Value) -> Result<Vec<Image>, Error> {
    args.get("images").and_then(Value::as_array).into_iter().flatten().map(|i| image(root, i)).collect()
}

fn decide_request(root: &Path, args: &Value) -> Result<DecideRequest, Error> {
    let mut r = DecideRequest::new();
    if let Some(s) = args.get("state") {
        r = r.state(s);
    }
    if let Some(qs) = args.get("questions").and_then(Value::as_object) {
        for (id, q) in qs {
            r = r.question(id.clone(), question(q));
        }
    }
    r = r.images(images(root, args)?);
    if let Some(m) = opt(args, "model").and_then(Value::as_str) {
        r = r.model(m);
    }
    Ok(r)
}

/// The images and model of a helper call.
fn call_options(root: &Path, args: &Value) -> Result<CallOptions, Error> {
    let mut o = CallOptions::new().images(images(root, args)?);
    if let Some(m) = opt(args, "model").and_then(Value::as_str) {
        o = o.model(m);
    }
    Ok(o)
}

/// The call of a case. An argument the SDK refuses while it is being built
/// (an image that cannot be read or recognised) is the call's error.
fn prepare(root: &Path, call: &Value) -> Result<Prepared, Error> {
    let args = &call["args"];
    let state = || args.get("state").cloned().unwrap_or_else(|| json!(""));
    let options = || call_options(root, args);
    Ok(match call["method"].as_str().unwrap() {
        "decide" => Prepared::Decide(decide_request(root, args)?),
        "decide_many" => Prepared::Many(
            args["requests"].as_array().unwrap().iter().map(|r| decide_request(root, r)).collect::<Result<_, _>>()?,
            opt(args, "concurrency").and_then(Value::as_i64),
        ),
        "check" => Prepared::Check(state(), build_truth(args), options()?),
        "classify" => Prepared::Classify(state(), build_choice(args), options()?),
        "score" => Prepared::Score(state(), build_score(args), options()?),
        "gate" => {
            Prepared::Gate(args["action"].clone(), args.get("context").cloned().unwrap_or(Value::Null), options()?)
        }
        "models" => Prepared::Models,
        "decide_raw" => Prepared::Raw(args["body"].clone()),
        other => panic!("unknown method {other}"),
    })
}

fn wrap<T>(r: Result<T, Error>, f: impl FnOnce(T) -> Outcome) -> Outcome {
    match r {
        Ok(v) => f(v),
        Err(e) => Outcome::Error(e),
    }
}

fn invoke(ctx: &Ctx, driver: Driver, builder: ClientBuilder, call: &Value) -> Outcome {
    let prepared = match prepare(&ctx.root, call) {
        Ok(p) => p,
        Err(e) => return Outcome::Error(e),
    };
    match driver {
        Driver::Async => {
            let client = match builder.build() {
                Ok(c) => c,
                Err(e) => return Outcome::Error(e),
            };
            ctx.runtime.block_on(invoke_async(&client, prepared))
        }
        #[cfg(feature = "blocking")]
        Driver::Blocking => {
            let client = match builder.build_blocking() {
                Ok(c) => c,
                Err(e) => return Outcome::Error(e),
            };
            invoke_blocking(&client, prepared)
        }
    }
}

async fn invoke_async(c: &DecisionNode, p: Prepared) -> Outcome {
    match p {
        Prepared::Decide(r) => wrap(c.decide(r).await, Outcome::Decision),
        Prepared::Many(rs, None) => wrap(c.decide_many(rs).await, Outcome::Many),
        Prepared::Many(rs, Some(n)) => wrap(c.decide_many_with(rs, n).await, Outcome::Many),
        Prepared::Check(s, q, o) => wrap(c.check_with(s, q, o).await, Outcome::Truth),
        Prepared::Classify(s, q, o) => wrap(c.classify_with(s, q, o).await, Outcome::Choice),
        Prepared::Score(s, q, o) => wrap(c.score_with(s, q, o).await, Outcome::Score),
        Prepared::Gate(a, ctx, o) => wrap(c.gate_with(a, ctx, o).await, Outcome::Decision),
        Prepared::Models => wrap(c.models().await, Outcome::Models),
        Prepared::Raw(b) => wrap(c.decide_raw(b).await, Outcome::Raw),
    }
}

#[cfg(feature = "blocking")]
fn invoke_blocking(c: &decisionnode::blocking::DecisionNode, p: Prepared) -> Outcome {
    match p {
        Prepared::Decide(r) => wrap(c.decide(r), Outcome::Decision),
        Prepared::Many(rs, None) => wrap(c.decide_many(rs), Outcome::Many),
        Prepared::Many(rs, Some(n)) => wrap(c.decide_many_with(rs, n), Outcome::Many),
        Prepared::Check(s, q, o) => wrap(c.check_with(s, q, o), Outcome::Truth),
        Prepared::Classify(s, q, o) => wrap(c.classify_with(s, q, o), Outcome::Choice),
        Prepared::Score(s, q, o) => wrap(c.score_with(s, q, o), Outcome::Score),
        Prepared::Gate(a, ctx, o) => wrap(c.gate_with(a, ctx, o), Outcome::Decision),
        Prepared::Models => wrap(c.models(), Outcome::Models),
        Prepared::Raw(b) => wrap(c.decide_raw(b), Outcome::Raw),
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

fn norm_answer(a: &decisionnode::Answer) -> Value {
    if let Some(t) = a.as_truth() {
        norm_truth(t)
    } else if let Some(c) = a.as_choice() {
        norm_choice(c)
    } else if let Some(s) = a.as_score() {
        norm_score(s)
    } else {
        json!({"kind": "unknown", "type": a.answer_type(), "raw": a.raw()})
    }
}

fn norm_decision(d: &Decision) -> Value {
    let answers: Map<String, Value> = d.answers.iter().map(|(k, v)| (k.clone(), norm_answer(v))).collect();
    json!({
        "model": d.model,
        "request_id": d.request_id,
        "latency_ms": d.latency_ms.map(num),
        "usage": {"input_tokens": d.usage.input_tokens, "output_tokens": d.usage.output_tokens},
        "safety": norm_safety(&d.safety),
        "answers": answers,
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
        Outcome::Error(e) => format!("error {e}"),
        _ => "a result".into(),
    }
}

// --------------------------------------------------------------------------- the four kinds

fn run_request(ctx: &Ctx, driver: Driver, case: &Value) -> Result<(), Fail> {
    let s = session();
    let b = make_client(case, &ctx.mock.url, &[("x-mock-session", s.clone())]);
    let outcome = invoke(ctx, driver, b, &case["call"]);
    if let Outcome::Error(e) = &outcome {
        return Err(Fail(format!("the call failed: {e}")));
    }
    let log = ctx.mock.requests(&s);
    let log = log["requests"].as_array().unwrap();
    if log.len() != 1 {
        return Err(Fail(format!("expected 1 request, the mock received {}", log.len())));
    }
    let got = &log[0];
    let exp = &case["expect"]["request"];
    for key in ["method", "path"] {
        if got[key] != exp[key] {
            return Err(Fail(format!("{key}: expected {}, got {}", exp[key], got[key])));
        }
    }
    let headers = &got["headers"];
    if let Some(h) = exp.get("headers").and_then(Value::as_object) {
        for (name, value) in h {
            let actual = &headers[name.to_lowercase()];
            if actual != value {
                return Err(Fail(format!("header {name}: expected {value}, got {actual}")));
            }
        }
    }
    if let Some(h) = exp.get("header_patterns").and_then(Value::as_object) {
        for (name, pattern) in h {
            let re = regex::Regex::new(pattern.as_str().unwrap()).unwrap();
            match headers[name.to_lowercase()].as_str() {
                Some(v) if re.is_match(v) => {}
                other => return Err(Fail(format!("header {name}: {other:?} does not match {pattern}"))),
            }
        }
    }
    if let Some(list) = exp.get("absent_headers").and_then(Value::as_array) {
        for name in list {
            let name = name.as_str().unwrap().to_lowercase();
            if headers.get(&name).is_some() {
                return Err(Fail(format!("header {name} must not be sent")));
            }
        }
    }
    let text = got["body_text"].as_str().unwrap_or("");
    let body: Value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(text).map_err(|e| Fail(format!("body is not JSON: {e}")))?
    };
    if let Some(eb) = exp.get("body") {
        if !same(eb, &body) {
            return Err(Fail(format!("body: expected {}, got {}", short(eb), short(&body))));
        }
    } else if exp.get("body_checks").is_none() && !text.is_empty() {
        return Err(Fail(format!("expected no body, got {}", short(&body))));
    }
    if let Some(checks) = exp.get("body_checks").and_then(Value::as_array) {
        for c in checks {
            let ptr = c["pointer"].as_str().unwrap();
            let val = body.pointer(ptr).ok_or_else(|| Fail(format!("body{ptr}: missing")))?;
            if let Some(eq) = c.get("equals") {
                if !same(eq, val) {
                    return Err(Fail(format!("body{ptr}: expected {}, got {}", short(eq), short(val))));
                }
            }
            if let Some(t) = c.get("type").and_then(Value::as_str) {
                let ok = match t {
                    "string" => val.is_string(),
                    "number" => val.is_number(),
                    "integer" => val.is_i64() || val.is_u64(),
                    "boolean" => val.is_boolean(),
                    "null" => val.is_null(),
                    "array" => val.is_array(),
                    "object" => val.is_object(),
                    _ => false,
                };
                if !ok {
                    return Err(Fail(format!("body{ptr}: expected a {t}")));
                }
            }
            if let Some(keys) = c.get("keys") {
                let got_keys: Vec<Value> =
                    val.as_object().map(|o| o.keys().map(|k| json!(k)).collect()).unwrap_or_default();
                if &Value::Array(got_keys.clone()) != keys {
                    return Err(Fail(format!("body{ptr}: keys expected {keys}, got {}", Value::Array(got_keys))));
                }
            }
        }
    }
    if let Some(ptrs) = exp.get("ordered_keys").and_then(Value::as_array) {
        for p in ptrs {
            let p = p.as_str().unwrap();
            let want: Vec<&String> =
                exp["body"].pointer(p).and_then(Value::as_object).map(|o| o.keys().collect()).unwrap_or_default();
            let have: Vec<&String> =
                body.pointer(p).and_then(Value::as_object).map(|o| o.keys().collect()).unwrap_or_default();
            if want != have {
                return Err(Fail(format!("body{p}: key order expected {want:?}, got {have:?}")));
            }
        }
    }
    Ok(())
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
    let outcome = invoke(ctx, driver, b, &case["call"]);
    let exp = &case["expect"];
    if let Some(d) = exp.get("decision") {
        match &outcome {
            Outcome::Decision(got) => matches(d, &norm_decision(got), "decision"),
            o => Err(Fail(format!("expected a decision, got {}", describe(o)))),
        }
    } else if let Some(a) = exp.get("answer") {
        let got = match &outcome {
            Outcome::Truth(t) => norm_truth(t),
            Outcome::Choice(c) => norm_choice(c),
            Outcome::Score(s) => norm_score(s),
            o => return Err(Fail(format!("expected an answer, got {}", describe(o)))),
        };
        matches(a, &got, "answer")
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
    } else {
        Err(Fail("the case has no expectation this runner knows".into()))
    }
}

fn run_error(ctx: &Ctx, driver: Driver, case: &Value) -> Result<(), Fail> {
    let s = session();
    let scenario = format!("error/{}", case["id"].as_str().unwrap());
    let b = make_client(case, &ctx.mock.url, &[("x-mock-session", s.clone()), ("x-mock-scenario", scenario)]);
    match invoke(ctx, driver, b, &case["call"]) {
        Outcome::Error(e) => check_error(&case["expect"]["error"], &e, "error")?,
        _ => return Err(Fail("no error was returned".into())),
    }
    let n = ctx.mock.requests(&s)["requests"].as_array().map(Vec::len).unwrap_or(0);
    if n != 1 {
        return Err(Fail(format!("expected 1 attempt with max_retries 0, got {n}")));
    }
    Ok(())
}

fn check_outcome(spec: &Value, result: Result<&Decision, &Error>, path: &str) -> Result<(), Fail> {
    if spec.get("argument_error").and_then(Value::as_bool) == Some(true) {
        return match result {
            Err(e) if e.is_argument() => Ok(()),
            Err(e) => Err(Fail(format!("{path}: expected an argument error, got {} ({e})", e.kind()))),
            Ok(_) => Err(Fail(format!("{path}: expected an argument error, got a result"))),
        };
    }
    if spec.get("result").is_some() {
        let d = result.map_err(|e| Fail(format!("{path}: expected a decision, got {e}")))?;
        if let Some(exp) = spec.get("decision") {
            matches(exp, &norm_decision(d), &format!("{path}.decision"))?;
        }
        Ok(())
    } else {
        match result {
            Err(e) => check_error(spec, e, path),
            Ok(_) => Err(Fail(format!("{path}: expected {}, got a result", spec["error"]))),
        }
    }
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
    let started = Instant::now();
    let outcome = invoke(ctx, driver, b, &case["call"]);
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    std::thread::sleep(Duration::from_millis(50));
    let info = ctx.mock.requests(&s);
    let log = info["requests"].as_array().cloned().unwrap_or_default();
    let check = |alt: &Value| -> Result<(), Fail> {
        let attempts = alt["attempts"].as_u64().unwrap() as usize;
        if log.len() != attempts {
            return Err(Fail(format!("attempts: expected {attempts}, the mock received {}", log.len())));
        }
        if let Some(spec) = alt.get("outcome") {
            match &outcome {
                Outcome::Decision(d) => check_outcome(spec, Ok(d), "outcome")?,
                Outcome::Error(e) => check_outcome(spec, Err(e), "outcome")?,
                o => return Err(Fail(format!("outcome: unexpected {}", describe(o)))),
            }
        }
        Ok(())
    };
    if let Some(alts) = exp.get("one_of").and_then(Value::as_array) {
        // The rule allows more than one way to comply; one must hold whole.
        let mut reasons = Vec::new();
        if !alts.iter().enumerate().any(|(i, alt)| match check(alt) {
            Ok(()) => true,
            Err(Fail(m)) => {
                reasons.push(format!("one_of[{i}]: {m}"));
                false
            }
        }) {
            return Err(Fail(reasons.join("; ")));
        }
    } else {
        check(exp)?;
    }
    if let Some(results) = exp.get("results").and_then(Value::as_array) {
        let Outcome::Many(got) = &outcome else {
            return Err(Fail(format!("results: expected a list, got {}", describe(&outcome))));
        };
        if got.len() != results.len() {
            return Err(Fail(format!("results: expected {}, got {}", results.len(), got.len())));
        }
        for (i, (spec, r)) in results.iter().zip(got).enumerate() {
            check_outcome(spec, r.as_ref(), &format!("results[{i}]"))?;
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
        other => return Err(Fail(format!("no constant {other}"))),
    };
    if same(&case["equals"], &got) {
        Ok(())
    } else {
        Err(Fail(format!("{name}: expected {}, got {got}", case["equals"])))
    }
}
