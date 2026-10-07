# DecisionNode Rust SDK

The official Rust client for the [DecisionNode](https://decisionnode.com) API: typed `truth`, `choice` and `score` decisions over any state, with images, retries and typed errors. Async on [reqwest](https://docs.rs/reqwest) and Tokio, with an optional blocking client.

## Install

```sh
cargo add bynn-decisionnode
```

The crate is imported as `decisionnode`. For the synchronous client, enable the `blocking` feature:

```sh
cargo add bynn-decisionnode --features blocking
```

## Set your key

```sh
export DECISIONNODE_API_KEY="dn_..."
```

## Quickstart

```rust
use decisionnode::{choice, score, truth, DecideRequest, DecisionNode};
use serde_json::json;

#[tokio::main]
async fn main() -> decisionnode::Result<()> {
    let dn = DecisionNode::new(); // reads DECISIONNODE_API_KEY

    let d = dn
        .decide(
            DecideRequest::new()
                .state(json!({"ticket": "I was charged twice and nobody has replied for 3 days."}))
                .question("route", choice("Where should this go?", [("billing", "money"), ("bug", "broken"), ("account", "login")]))
                .question("urgency", score("How urgent is this?", ["routine", "today", "urgent", "critical"]))
                .question("escalate", truth("Escalate to a human now?")),
        )
        .await?;

    d["route"].choice();                           // Some("billing")
    &d["route"].as_choice().unwrap().probabilities; // {"billing": 0.87, "bug": 0.08, "account": 0.05}
    d["urgency"].score();                          // Some(2.31)
    d["escalate"].truth();                         // Some(0.94)
    Ok(())
}
```

The same with the blocking client:

```rust
use decisionnode::blocking::DecisionNode;

let dn = DecisionNode::new();
let answer = dn.check("Customer: please refund my last order.", "Is this a refund request?")?;
println!("{}", answer.truth); // 0.97
```

## The client

| Method | Sends | Returns |
|---|---|---|
| `decide(request)` | One `POST /v1/decide` | `Decision` |
| `decide_many(requests)`, `decide_many_with(requests, concurrency)` | One call per request, at most 8 (or `concurrency`) at a time | `Result<Vec<DecisionResult>>`: one `DecisionResult` (a `Result<Decision>`) per request, in input order. One failure does not stop the others. Every request is checked before any is sent: an invalid one (an empty question map, a value that cannot be serialised), or a `concurrency` below 1, is an argument error (`Err` of the base kind, see [Errors](#errors)) and nothing is sent. |
| `check(state, question)` | One `truth` question, id `check` | `TruthAnswer`. A missing or malformed answer is a `Server` error (below) |
| `classify(state, choice(...))` | One `choice` question, id `classify` | `ChoiceAnswer`, the same |
| `score(state, score(...))` | One `score` question, id `score` | `ScoreAnswer`, the same |
| `gate(action, context)` | Three fixed questions, below | `Decision` |
| `models()` | `GET /v1/models` | `Vec<ModelInfo>` with each model's published `limits` |
| `decide_raw(body)` | The body exactly as given | The response body as `serde_json::Value`, for API fields this version does not know yet |

`check_with`, `classify_with`, `score_with` and `gate_with` take `CallOptions` for images and a model, like `decide`:

```rust
use decisionnode::{CallOptions, Image};

let answer = dn
    .check_with(
        json!({"claim": {"total": "740.00"}}),
        "Does the total on the receipt equal the total in the claim?",
        CallOptions::new().image(Image::path("receipt", "receipt.jpg")?).model("decisionnode-1.0-flash"),
    )
    .await?;
```

`gate` sends the state `{"action": action, "context": context}` and these three questions, worded the same in every DecisionNode SDK (`gate_questions()` returns them):

| Id | Type | Instructions | Criteria |
|---|---|---|---|
| `risk` | score | How much harm could this action cause if it is wrong? | `none`, `low`, `moderate`, `high`, `severe` |
| `touches_production` | truth | Does this action change a production system, production data or anything customers use? | |
| `verdict` | choice | Should this action run as it is, wait for a person to confirm it, or be blocked? | `allow` "safe to run without asking", `confirm` "a person should confirm it first", `block` "should not run" |

A client is cheap to clone and safe to share between tasks and threads; clones share one connection pool.

### Questions

| Builder | Wire form |
|---|---|
| `truth(instructions)`, `.if_true(...)`, `.if_false(...)` | `{"type": "truth", "instructions": ..., "criteria": {"true": ..., "false": ...}}`, criteria left out when neither side is given |
| `choice(instructions, options)` | `{"type": "choice", "instructions": ..., "criteria": {name: description}}` |
| `score(instructions, levels)` | `{"type": "score", "instructions": ..., "criteria": [levels, low to high]}` |

The state, instructions, descriptions and levels take a string or any value that implements `serde::Serialize`. Options are a list of names (`["billing", "bug"]`), name and description pairs, or a map. `Truth::new()`, `Choice::new(options)` and `Score::new(levels)` build a question without instructions. A `serde_json::Value` in the wire form is accepted wherever a builder is:

```rust
let request = DecideRequest::new()
    .state("Is hotdog a sandwich?")
    .question("q", json!({"type": "truth", "criteria": {"true": "bread around a filling", "false": null}}));
```

A question id is any string and comes back as the key of its answer. Without a state the request sends `""`.

### Answers

| Type | Fields |
|---|---|
| `Decision` | `answers` (by question id, also `decision["id"]` and `decision.get("id")`), `model`, `usage.input_tokens`, `usage.output_tokens`, `request_id`, `latency_ms`, `safety`, `raw` |
| `ModelInfo` | `name`, `description`, `release_date`, `limits` (every other field of the entry), `raw` |
| `Answer::Truth(TruthAnswer)` | `truth` |
| `Answer::Choice(ChoiceAnswer)` | `choice`, `confidence`, `probabilities` (by option name, exactly as sent) |
| `Answer::Score(ScoreAnswer)` | `score`, `confidence`, `probabilities` (by level index), `legend` (level index to the level as sent) |
| `Answer::Unknown(UnknownAnswer)` | `answer_type` and `raw`, for a question type this version does not know, or a known type whose fields are missing or of the wrong type. Such an answer never fails the decision. |

`model` is read from the body; `request_id`, `latency_ms` and `safety` from the `x-request-id`, `x-decisionnode-latency-ms` and `x-decisionnode-safety` headers. A value the response does not carry is `None`: the token counts are `Option<u64>` and `latency_ms` is `Option<f64>`, never 0 in place of a missing value.

Numbers are passed through as received: no rounding and no renormalising, so a probability list may sum to 0.99 or 1.01 and an entry may read `0.8200000000000001`. Read answers by id, never by position.

### Images

```rust
use decisionnode::Image;

let request = DecideRequest::new()
    .state(json!({"claim": {"total": "740.00", "currency": "SEK"}}))
    .image(Image::path("receipt", "receipt.jpg")?)                 // read now; type from the first bytes
    .image(Image::bytes("scan", png_bytes)?)                       // type from the first bytes
    .image(Image::bytes_with_type("raw", jpeg_bytes, "image/jpeg")) // type named, sent as given
    .image(Image::url("photo", "https://example.com/photo.webp"))
    .question("total_matches", truth("Does the total on the receipt equal the total in the claim?"));
```

A file is read when the `Image` is made, so a file that cannot be read fails there, before any request. The media type of a file, or of bytes passed to `Image::bytes`, is taken from the first bytes (JPEG, PNG or WebP); any other format is an argument error. Up to 4 images a request, 10 MB each, `https` URLs only; the server enforces the limits.

### Safety

On the hosted API every decision carries the result of the safety check, parsed from the `x-decisionnode-safety` header:

```rust
if let Some(safety) = &d.safety {
    println!("{} {:?} {:?} {:?}", safety.checked, safety.harm, safety.self_harm, safety.flagged);
}
```

`flagged` lists the categories above your key's thresholds. When the check did not run, `checked` is `false` and `reason` says why. A self-hosted server sends no header and `safety` is `None`. To also get the result in the response body, send the request header `x-decisionnode-safety-body: true`. Set it once on the client with `ClientBuilder::header` and every request carries it; the field then arrives in `d.raw`:

```rust
let dn = DecisionNode::builder().header("x-decisionnode-safety-body", "true").build()?;
let d = dn.decide(request).await?;
println!("{}", d.raw["safety"]);
```

## Errors

Every failure is a `decisionnode::Error`, also exported under the name used across the DecisionNode SDKs, `decisionnode::DecisionNodeError` (a type alias, so either name catches every error). `error.kind()` says what happened:

| Status | `ErrorKind` | Extra fields |
|---|---|---|
| 400 | `BadRequest`; `ContextLimit` when `error_type` is `max_tokens_exceeded` | |
| 401 | `Authentication` | |
| 402 | `InsufficientCredit` | |
| 403 with `refusal_error` | `Refusal` | `category()` (`harm` or `self_harm`), `safety()` |
| 404 | `NotFound` | |
| 422 | `Validation` | `entries()` |
| 429 | `RateLimit` | `retry_after()` |
| 529 | `Overloaded` | `retry_after()` |
| 403 otherwise | `PermissionDenied` | |
| other 5xx | `Server` | |
| 2xx whose body is not JSON | `Server` | `status()`, `request_id()`, `reason()` |
| 2xx where `check`, `classify` or `score` gets no answer of the type it promises | `Server` | `status()`, `request_id()`, `reason()` |
| any other status: 3xx, 405, 409, 418, 408 once its retries are spent | `Other`, the base `DecisionNodeError` | `status()` |
| no response | `Connection`, `Timeout` | |
| an argument refused before sending | `Other`, the base `DecisionNodeError`, without a status (`is_argument()` is true) | |

Only 200 to 299 is a success. Redirects are never followed, whatever transport settings you pass (see [Configuration](#configuration)), so the key is never sent to another URL or over plain HTTP: a 3xx is an error carrying its status.

`decide` never fails over one odd answer: an answer with missing or wrongly typed fields comes back as `Answer::Unknown`. `check`, `classify` and `score` promise a typed answer, so when theirs is missing or malformed they return a `Server` error with the response's status, request id and `reason()` instead.

The error's own message, its `Display`, is the server's `message` exactly (`detail.message`, or `detail` when it is a string), or the client's description when there was no response; it is `""` when the server sent none. `message()` returns the same text, `""` and never missing when there is none. Every error also has `kind()`, `status()`, `error_type()`, `request_id()` and `reason()`: the server's explanation in words, from the `x-decisionnode-error` header. `{:?}` prints all of them. A `ContextLimit` error is also a `BadRequest`: `error.is(ErrorKind::BadRequest)` is true for both. Before sending, a call fails with `Authentication` when no key is set. Arguments the client refuses before sending anything are errors of the base kind, `Other`, without a status (`error.is_argument()` is true; there is no separate error kind for them): an empty question map, a value that cannot be serialised, an image file that cannot be read, image bytes that are not JPEG, PNG or WebP, a base URL that is not `https`, a timeout of 0 or below, a negative `max_retries`, an invalid request in `decide_many`, or a `decide_many_with` concurrency below 1. Everything else is the server's to judge.

```rust
use decisionnode::ErrorKind;

match dn.decide(request).await {
    Ok(d) => println!("{:?}", d["escalate"].truth()),
    Err(e) if e.kind() == ErrorKind::RateLimit => println!("retry after {:?}", e.retry_after()),
    Err(e) if e.kind() == ErrorKind::Refusal => println!("refused: {:?}", e.category()),
    Err(e) => eprintln!("{} {:?}: {e} ({:?})", e.kind(), e.status(), e.reason()),
}
```

## Timeouts and retries

Each attempt times out after 10 seconds, counted from sending the request to the last byte of the response body. A call is retried twice after the first attempt on 408, 429, every 5xx (529 included), timeouts and connection errors. The wait is `retry-after-ms` when the server sends it, else `Retry-After` in seconds or as an HTTP date, capped at 60 s; without either, an exponential backoff from 0.5 s with jitter, capped at 8 s. `ClientBuilder::sleeper` replaces the wait, for tests that record it instead of spending it. 400, 401, 402, 403, 404 and 422 are never retried. A decision changes nothing on the server, so a retry is always safe.

## Configuration

```rust
use std::time::Duration;
use decisionnode::{reqwest, DecisionNode, HttpSettings};

let dn = DecisionNode::builder()
    .api_key("dn_...")                              // default: DECISIONNODE_API_KEY
    .base_url("https://api.decisionnode.com")       // default: DECISIONNODE_BASE_URL, else this URL
    .model("decisionnode-latest")                   // default: DECISIONNODE_MODEL, else decisionnode-latest
    .timeout(Duration::from_secs(10))               // per attempt, above 0; or .timeout_secs(10.0)
    .max_retries(2)                                 // 0 or more
    .header("x-decisionnode-safety-body", "true")   // sent on every request
    .http_settings(HttpSettings::new()              // transport settings: proxy, root certificates, pool
        .proxy(reqwest::Proxy::https("http://proxy.example:3128").expect("a valid proxy URL")))
    .build()?;                                      // .build_blocking()? for the blocking client
```

An explicit setting wins over the environment, and the environment over the default. An empty string, for the key, the base URL or the model (also a request's own model), counts as not given. The base URL must be `https`; plain `http` is accepted only for `localhost`, `127.0.0.1` and `::1`, so a key never travels unencrypted, and anything else fails `build()`. The SDK builds its own HTTP client and takes no client or `reqwest::ClientBuilder` of yours, because it could not show that such a client refuses redirects and keeps its TLS checks on. `http_settings` takes an `HttpSettings` instead: a proxy (`https` requests tunnel through it, so TLS stays end to end), extra root certificates or only your own (`add_root_certificate`, `tls_built_in_root_certs`), a client certificate (`identity`), `connect_timeout`, pool and TCP options, `local_address`, `http1_only` and `resolve`. None of them can turn on redirects or turn off certificate or hostname checks, and with an `https` base URL the client sends nothing over plain HTTP. Models are `decisionnode-latest`, `decisionnode-flash-latest` and the pinned ids such as `decisionnode-1.0` and `decisionnode-1.0-flash`; `models()` lists the ones your key may use. A request picks its own with `DecideRequest::model`.

| Feature | Default | |
|---|---|---|
| `rustls-tls` | on | TLS with rustls |
| `native-tls` | off | TLS with the platform's library |
| `blocking` | off | `decisionnode::blocking::DecisionNode` |

The blocking client runs calls on a small runtime of its own: use the async client inside async code.

The SDK sends no telemetry and logs no state or questions. Its only added header is `User-Agent: decisionnode-rust/<version>`. With a [`log`](https://docs.rs/log) logger at debug level for `decisionnode`, it prints the method, path, status, request id and timing of each attempt.

## Development

```sh
cargo test --features blocking
```

The integration tests run the DecisionNode SDK conformance suite (every request, response, error, behaviour and constant case, with both clients) against its mock server, from the suite `DECISIONNODE_CONFORMANCE_DIR` points at; they need `python3` for the mock. Without the variable the conformance test fails rather than skipping. CI checks the suite out at one pinned commit, set in a single place at the top of `.github/workflows/ci.yml`, and pins every action to a commit SHA. This crate implements DecisionNode SDK specification version 1.0, exported as `decisionnode::SPEC_VERSION`.

## License

MIT. See [LICENSE](LICENSE).
