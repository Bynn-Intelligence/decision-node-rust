# DecisionNode Rust SDK

The official Rust client for the [DecisionNode](https://decisionnode.com) API: typed `truth`, `choice`, `score`, `number`, `point` and `box` decisions over any state, with up to 16 images, videos sent as frames, sessions, batches, retries and typed errors. Async on [reqwest](https://docs.rs/reqwest) and Tokio, with an optional blocking client.

The client checks every request against the API's fixed rules before it sends anything, reuses its connections, and picks the fastest form for each image, so a call costs one round trip wherever one is enough.

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
| `number(state, number(...))` | One `number` question, id `number` | `NumberAnswer`, the same |
| `gate(action, context)` | Three fixed questions, below | `Decision` |
| `models()` | `GET /v1/models` | `Vec<ModelInfo>` with each model's published `limits` (number grid, upload, `sessions` and `batch` limits included) |
| `upload(images)` | `POST /v1/uploads` and one `PUT` per image not uploaded before | One upload id per image |
| `open_session(options)`, `supervise(options, feed, on_reply)` | Sessions, see [Sessions](#sessions) | `Session` |
| `batches()`, `run_batch(requests)` | The Batch API, see [Batches](#batches) | `Batch`, `BatchResult` |
| `check_decide(&request)`, `check_session(&options)`, `check_batch(&requests)`, `check_batch_add(&requests)`, `Session::check_frame(&frame)` | Nothing: every local check the call would run | `Ok(())`, or the error the call would return |
| `decide_raw(body)` | The body exactly as given | The response body as `serde_json::Value`, for API fields this version does not know yet |

`check_with`, `classify_with`, `score_with`, `number_with` and `gate_with` take `CallOptions` for images, videos and a model, like `decide`:

```rust
use decisionnode::{CallOptions, Image};

let answer = dn
    .check_with(
        json!({"claim": {"total": "740.00"}}),
        "Does the total on the receipt equal the total in the claim?",
        CallOptions::new().image(Image::url("receipt", "https://files.example.com/receipt.jpg")).model("decisionnode-1.0-flash"),
    )
    .await?;
```

`gate` sends the state `{"action": action, "context": context}` and these three questions, worded the same in every DecisionNode SDK (`gate_questions()` returns them):

| Id | Type | Instructions | Criteria |
|---|---|---|---|
| `risk` | score | How much harm could this action cause if it is wrong? | `none`, `low`, `moderate`, `high`, `severe` |
| `touches_production` | truth | Does this action change a production system, production data or anything customers use? | |
| `verdict` | choice | Should this action run as it is, wait for a person to confirm it, or be blocked? | `allow` "safe to run without asking", `confirm` "a person should confirm it first", `block` "should not run" |

A client is cheap to clone and safe to share between tasks and threads; clones share one connection pool and one upload cache.

### Questions

| Builder | Wire form |
|---|---|
| `truth(instructions)`, `.if_true(...)`, `.if_false(...)` | `{"type": "truth", "instructions": ..., "criteria": {"true": ..., "false": ...}}`, criteria left out when neither side is given |
| `choice(instructions, options)` | `{"type": "choice", "instructions": ..., "criteria": {name: description}}` |
| `score(instructions, levels)` | `{"type": "score", "instructions": ..., "criteria": [levels, low to high]}` |
| `number(instructions)`, `.min(...)`, `.max(...)`, `.step(...)` | `{"type": "number", "instructions": ..., "min": ..., "max": ..., "step": ...}`, each bound left out unless given, so the API's defaults apply (0, 255, 1) |
| `point(instructions)`, `.image(...)`, `.units(...)` | `{"type": "point", "instructions": ..., "image": ..., "units": ...}`, `image` and `units` left out unless given |
| `bounding_box(instructions)`, `.image(...)`, `.units(...)` | `{"type": "box", ...}`, built like `point` (`box` is a reserved word in Rust) |

The state, instructions, descriptions and levels take a string or any value that implements `serde::Serialize`. Options are a list of names (`["billing", "bug"]`), name and description pairs, or a map. `Truth::new()`, `Choice::new(options)` and `Score::new(levels)` build a question without instructions. A `serde_json::Value` in the wire form is accepted wherever a builder is:

```rust
let request = DecideRequest::new()
    .state("Is hotdog a sandwich?")
    .question("q", json!({"type": "truth", "criteria": {"true": "bread around a filling", "false": null}}));
```

A point or a box is a place in one of the request's images:

```rust
use decisionnode::{bounding_box, point, Image};

let d = dn
    .decide(
        DecideRequest::new()
            .image(Image::url("belt", "https://files.example.com/line-3/cam-2/0917.jpg"))
            .question("pick", point("Point to the yellow package closest to the robot arm."))
            .question("damage", bounding_box("Draw a box around the damaged package.").image("belt"))
            .question("any_damage", truth("Is any package on the belt damaged?")),
    )
    .await?;
let pick = d["pick"].as_point().unwrap(); // x 812, y 455, present 0.97
let damage = d["damage"].as_box().unwrap(); // x1 300, y1 120, x2 520, y2 410
```

`x` and `y` are measured from the top-left corner of the image, `x` to the right and `y` down; a box runs from its top-left corner (`x1`, `y1`) to its bottom-right corner (`x2`, `y2`). `units` is `"pixels"` (the default: pixels of the image as sent) or `"normalized"` (fractions of the width and the height). `present` is the probability that what the instructions describe is in the image at all; `confidence` the probability that the place is right if it is. `image`, `width` and `height` name the image looked in and its size in pixels. The request needs an image, and `.image(id)` names the one to look in by its id when it carries two or more; a reference by number ("Picture 2") goes in the instructions.

A question id is any string and comes back as the key of its answer. Without a state the request sends `""`.

### Answers

| Type | Fields |
|---|---|
| `Decision` | `answers` (by question id, also `decision["id"]` and `decision.get("id")`), `model`, `usage.input_tokens`, `usage.output_tokens`, `usage.video_frames`, `usage.video_seconds`, `warnings`, `request_id`, `latency_ms`, `safety`, `raw` |
| `ModelInfo` | `name`, `description`, `release_date`, `limits` (every other field of the entry), `raw` |
| `Answer::Truth(TruthAnswer)` | `truth` |
| `Answer::Choice(ChoiceAnswer)` | `choice`, `confidence`, `probabilities` (by option name, exactly as sent) |
| `Answer::Score(ScoreAnswer)` | `score`, `confidence`, `probabilities` (by level index), `legend` (level index to the level as sent) |
| `Answer::Number(NumberAnswer)` | `number`, `expected`, `confidence`, `probabilities` (by the value exactly as the API writes it, `"7"`, `"3.5"`) |
| `Answer::Point(PointAnswer)` | `x`, `y`, `present`, `confidence`, `image`, `width`, `height` |
| `Answer::Box(BoxAnswer)` | `x1`, `y1`, `x2`, `y2`, `present`, `confidence`, `image`, `width`, `height` |
| `Answer::Unknown(UnknownAnswer)` | `answer_type` and `raw`, for a question type this version does not know, or a known type whose fields are missing or of the wrong type. Such an answer never fails the decision. |

`model` is read from the body; `request_id`, `latency_ms` and `safety` from the `x-request-id`, `x-decisionnode-latency-ms` and `x-decisionnode-safety` headers. A value the response does not carry is `None`: the token counts are `Option<u64>` and `latency_ms` is `Option<f64>`, never 0 in place of a missing value. For a request with videos, `usage.video_frames` is the number of frames read and `usage.video_seconds` the seconds of the windows read (`None` without videos), and `warnings` holds the server's notes exactly as sent when it read a video at a lower rate or resolution than asked (empty when there are none).

Numbers are passed through as received: no rounding and no renormalising, so a probability list may sum to 0.99 or 1.01 and an entry may read `0.8200000000000001`. Read answers by id, never by position.

### Images

Send an image by URL when you can. It is the fastest form: nothing is uploaded, the request stays small, and the API fetches the image while it reads the request.

```rust
use decisionnode::Image;

let request = DecideRequest::new()
    .state(json!({"claim": {"total": "740.00", "currency": "SEK"}}))
    .image(Image::url("receipt", "https://files.example.com/claims/4411/receipt.jpg")) // recommended
    .image(Image::path("scan", "scan.jpg")?)                                           // read now
    .image(Image::bytes("photo", png_bytes))                                           // type from the bytes
    .question("total_matches", truth("Does the total on the receipt equal the total in the claim?"));
```

| Form | What is sent |
|---|---|
| `Image::url(id, url)` | `{"id", "url"}`, never fetched by the client. Checked first: `https` and at most 2,048 characters |
| `Image::path(id, path)`, `Image::bytes(id, data)`, `Image::bytes_with_type(id, data, media_type)`, `Image::base64(id, text)` | Checked locally, then inline as base64 when all the request's local images total under the inline threshold (`DEFAULT_INLINE_THRESHOLD`, 512 KiB, set with `ClientBuilder::inline_threshold`), and uploaded at or above it: one `POST /v1/uploads` for all of them, the `PUT`s in parallel, then the request names them by `upload_id` |
| An image the client uploaded before | Its cached `upload_id`, whatever its size, until 5 minutes before the upload expires |
| `Image::upload_id(id, upload_id)` | `{"id", "upload_id"}` as given |

The model sees each image as "Picture 1", "Picture 2", ... with its id beside it, numbered from 1 in the order of `images`, and each video as "Video 1", "Video 2", ... in the order of `videos`, numbered separately. Text in the state, the instructions or a criterion can refer to an image by position ("Picture 2", "image 2", "the second image", "the last image") or by its id ("the receipt", "`receipt`"), and to a video the same way ("Video 1" or its id):

```rust
let request = DecideRequest::new()
    .image(Image::url("receipt", "https://files.example.com/claims/4411/receipt.jpg"))
    .image(Image::url("statement", "https://files.example.com/claims/4411/statement.png"))
    .question("charged", truth("Does the total in Picture 1 match the charge on the statement?"));
```

The local checks are the API's own: JPEG, PNG or WebP by the first bytes (a declared type must match them), at most 10 MB, at most 40 megapixels read from the image header, at most 16 images in a request (URLs included; a 17th is the API's `Validation` error with a `too_long` entry), ids unique. `upload` takes up to 16 images a call. A file is read when the `Image` is made, so a file that cannot be read fails there. No image is resized or re-encoded.

The upload cache lives in the client's memory only: by SHA-256 of the bytes and the media type, at most 256 entries, never written to disk and never shared between clients. Requests holding the same bytes at the same time share one upload. If the API answers that an upload the client placed has expired, was consumed or is not there, the client uploads the image again and resends the request once.

The `PUT` goes to the storage URL the API returned, with exactly the headers it returned: never the API key, never the client's own headers, and no redirect followed. A `PUT` answered `403` after its URL's 5 minutes have passed (a slow network, a paused process) is presigned again once. Each `PUT` attempt has its own timeout, 60 seconds by default (`ClientBuilder::upload_timeout`), apart from the API call's 10. `upload(images)` uploads whatever the size and returns the ids, for images you send many times:

```rust
let ids = dn.upload([Image::path("receipt", "receipt.jpg")?]).await?;
let again = DecideRequest::new().image(Image::upload_id("receipt", &ids[0])).question("q", truth("Is it paid?"));
```

### Videos

A video travels as the frames you already hold, each with its time in seconds, and the server reads them as one video, in order and with their times. Only frames are sent: no video file, no video URL and no sound.

```rust
use decisionnode::{number, Video, VideoFrame};

let mut dock = Video::new("dock");
for (i, path) in ["dock-0000.jpg", "dock-0001.jpg", "dock-0002.jpg"].iter().enumerate() {
    dock = dock.frame(VideoFrame::bytes(i as f64 * 0.5, std::fs::read(path)?)); // 0, 0.5 and 1 s
}

let d = dn
    .decide(
        DecideRequest::new()
            .state("Warehouse camera 4, loading dock, shift B.")
            .video(dock)
            .question("fall", truth("Does a person fall in the video?"))
            .question("forklifts", number("How many forklifts pass the door?").min(0).max(50)),
    )
    .await?;
println!("{:?} {:?}", d["fall"].truth(), d.warnings);
```

| Form | What is sent |
|---|---|
| `VideoFrame::bytes(t, data)`, `VideoFrame::bytes_with_type(t, data, media_type)` | A frame `t` seconds into the video, its bytes as base64; without a media type it is read from the first bytes (JPEG, PNG or WebP) |
| `VideoFrame::base64(t, text, media_type)` | A frame given as base64 text, sent as given, with its media type |
| `Video::new(id)`, `.frame(...)`, `.frames(...)`, `.fps(...)`, `.start(...)`, `.end(...)` | A video: its id (unique across the request's images and videos), its frames with strictly increasing times, and optionally the most frames a second to read (above 0 and at most 4; the API's default is 2) and the window `[start, end)` on the frames' own times |

`DecideRequest::video` takes up to 4 videos, each a `Video` or a `serde_json::Value` in the wire form (`Video::from(json!({...}))`, and `VideoFrame::from` for a frame), checked by the same rules. The helpers' `CallOptions`, `decide_many` and batch requests take videos as `decide` does; sessions do not. Frames always go inline: they are never uploaded or cached, and no frame is resized or re-encoded.

Before anything is sent the client checks the API's video rules. More than 4 videos is the API's `Validation` error with a `too_long` entry on `["body", "videos"]`; every other fault is a `BadRequest` error with the API's message: a video without an id or with an id already used by an image or a video, a video without frames, a frame time that is negative or not after the one before it, a frame that is not JPEG, PNG or WebP by its media type, `fps` not above 0 and at most 4, `start` or `end` negative, and `end` not after `start`. How many frames a request may carry, their sizes and their pixels are the server's to judge. To turn a video file into frames, use the `dn` command line tool: `dn --video dock.mp4`.

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

## Sessions

A session holds a fixed context on the server and answers a stream of frames over a WebSocket, each against the context and the latest frames. Sessions are behind the `sessions` feature, on by default.

```rust
use decisionnode::{choice, truth, Frame, SessionEvent, SessionOptions};

let session = dn
    .open_session(
        SessionOptions::new()
            .instructions("You are the mission supervisor of a survey drone.")
            .state(json!({"mission": "Survey the north field"}))
            .question("mode", choice("Which flight mode now?", ["continue", "hold", "return", "land"]))
            .question("abort", truth("Should the mission be aborted now?")),
    )
    .await?;
session.connect().await?;
session.send(Frame::new(json!({"battery": 0.31, "wind_mps": 11.4}))).await?;
match session.next_event().await? {
    SessionEvent::Reply(reply) => println!("{:?}", reply.answers["mode"].choice()),
    SessionEvent::FrameError(e) => println!("frame {:?}: {}", e.seq, e.message),
    SessionEvent::Closed(closed) => println!("closed with {}", closed.code),
    _ => {}
}
session.end().await?;
```

`send` never waits for a reply, and `seq` counts from 1 unless a frame sets it. When frames arrive faster than they are answered, the latest wins: a reply lists the frames it superseded in `dropped`. Frame errors are values, never errors of the call. The key travels only in the handshake's `Authorization` header, and only to the API's own host. The socket opens with the client's `HttpSettings`, so a proxy, extra root certificates and a client certificate apply to it too. A frame's image follows the same inline or upload rule as a request's; a frame takes no URL.

`supervise(options, feed, on_reply)` keeps a session going for a long-lived feed: it sends each reading, passes every reply to `on_reply`, reopens on the close codes that call for it (at once after `4408` and `4410`, otherwise after a backoff of 1 s to 30 s with jitter), connects again to the same session after `1013`, and stops on any other code. A `429` `session_limit_exceeded` is never retried: it is an `ErrorKind::SessionLimit` error, a special case of `RateLimit`.

## Batches

The Batch API answers up to 10,000 decide requests within 24 hours at half the price. `run_batch` does the whole lifecycle: it checks every request, creates a draft, adds the requests in calls of at most 10 MiB, finalizes, polls every 60 seconds (`run_batch_with` sets the interval, at least 10 seconds, and a limit; a longer `Retry-After` from the server wins) and returns the ended batch with its results.

```rust
use decisionnode::{truth, BatchRequest, BatchStatus};

let requests = tickets.iter().map(|t| {
    BatchRequest::new(t.id.clone(), DecideRequest::new().state(&t.text).question("refund", truth("Is a refund asked for?")))
});
let run = dn.run_batch(requests).await?;
for result in run.results {
    let result = result?;
    match (&result.status, &result.decision, &result.error) {
        (BatchStatus::Http(200), Some(d), _) => println!("{}: {:?}", result.custom_id, d["refund"].truth()),
        (_, _, Some(e)) => println!("{}: {} {e}", result.custom_id, e.kind()),
        (status, _, _) => println!("{}: {status:?}", result.custom_id),
    }
}
```

`batches()` gives each call on its own: `create`, `add`, `finalize`, `get`, `list`, `results`, `stream_results` and `cancel`. `results` reads the whole file; `stream_results` reads it from the network line by line, a `Stream` of results (an `Iterator` in the blocking client) that holds one line at a time however large the file. Lines carry their `custom_id`, the model (the request's, else the client's) and the state (`""` when none); their local images follow the inline or upload rule per line. A result line's error is a value with the kind and fields the same status gives on the live API. `get`, `list`, `results` and `cancel` are retried like every call; `create`, `add` and `finalize` only on `429` and `529`, never after a timeout or a lost answer, so a batch is never created twice. A `finalize` answered `409` after an earlier one went through returns the batch.

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
| 429 | `RateLimit`; `SessionLimit` when `error_type` is `session_limit_exceeded` | `retry_after()` |
| 529 | `Overloaded` | `retry_after()` |
| 403 otherwise | `PermissionDenied` | |
| other 5xx | `Server` | |
| 2xx whose body is not JSON | `Server` | `status()`, `request_id()`, `reason()` |
| 2xx where `check`, `classify` or `score` gets no answer of the type it promises | `Server` | `status()`, `request_id()`, `reason()` |
| any other status: 3xx, 405, 409, 418, 408 once its retries are spent | `Other`, the base `DecisionNodeError` | `status()` |
| no response | `Connection`, `Timeout` | |
| a request the API would refuse, refused before sending | the API's own kind, `BadRequest`, or `Validation` for what the API answers with a 422, with its `error_type` and message, without a status (`is_local()` is true) | `entries()` for a `Validation` |
| a storage `PUT` that failed | `Other`, the base `DecisionNodeError`, with the storage's status, `error_type` `upload_failed` and the storage's error code in `reason()` | |
| an argument refused before sending | `Other`, the base `DecisionNodeError`, without a status (`is_argument()` is true) | |

Only 200 to 299 is a success. Redirects are never followed, whatever transport settings you pass (see [Configuration](#configuration)), so the key is never sent to another URL or over plain HTTP: a 3xx is an error carrying its status.

`decide` never fails over one odd answer: an answer with missing or wrongly typed fields comes back as `Answer::Unknown`. `check`, `classify` and `score` promise a typed answer, so when theirs is missing or malformed they return a `Server` error with the response's status, request id and `reason()` instead.

The error's own message, its `Display`, is the server's `message` exactly (`detail.message`, or `detail` when it is a string), or the client's description when there was no response; it is `""` when the server sent none. `message()` returns the same text, `""` and never missing when there is none. Every error also has `kind()`, `status()`, `error_type()`, `request_id()` and `reason()`: the server's explanation in words, from the `x-decisionnode-error` header. `{:?}` prints all of them. A `ContextLimit` error is also a `BadRequest` and a `SessionLimit` error a `RateLimit`: `error.is(ErrorKind::BadRequest)` is true for both. Before sending, a call fails with `Authentication` when no key is set. Arguments the client refuses before sending anything are errors of the base kind, `Other`, without a status (`error.is_argument()` is true; there is no separate error kind for them): an empty question map, a value that cannot be serialised, an image file that cannot be read, a base URL that is not `https`, a timeout of 0 or below, a negative `max_retries` or `inline_threshold`, a `decide_many_with` concurrency below 1, or a `run_batch_with` poll interval below 10 seconds. A request that breaks one of the API's fixed rules fails before sending, with what the server would answer and no status (`is_local()`): the same kind, the same `error_type`, the documented message, and for a 422 a `Validation` error whose `entries()` are the server's list. That covers the 128-level nesting limit; question types, missing or mistyped fields, option and level counts, truth, number, point and box text, number grids, and a point's or box's image and units; a state of the wrong type; images (ids, format, 10 MB, 40 megapixels, an upload reference's media type), videos (ids, frames, frame times and media types, `fps`, `start` and `end`), more than 16 images or 4 videos (a `too_long` entry after the request's other entries, a local image listed by its id and media type without its bytes) and image URLs (`https`, 2,048 characters, no user information, port 443, no literal private address); a session's `window` and `ttl_seconds`; a session frame, which `Session::send` refuses with nothing written and no `seq` used; `batches().list` paging; and batch `custom_id` rules. After a `models()` call the client also checks the limits that list states for the model a request names, found by its name or by the alias or pinned version an entry states. The same checks run alone, sending nothing, with `check_decide`, `check_session`, `check_batch`, `check_batch_add` and `Session::check_frame`. `decide_many` and batch calls check every request first, so one invalid request fails the call and nothing is sent. Limits that depend on the model, such as tokens, are the server's to judge.

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
    .inline_threshold(524_288)                      // local images under this many bytes go inline
    .upload_timeout(Duration::from_secs(60))        // per storage PUT attempt, apart from timeout
    .header("x-decisionnode-safety-body", "true")   // sent on every request
    .http_settings(HttpSettings::new()              // transport settings: proxy, root certificates, pool
        .proxy(reqwest::Proxy::https("http://proxy.example:3128").expect("a valid proxy URL")))
    .build()?;                                      // .build_blocking()? for the blocking client
```

An explicit setting wins over the environment, and the environment over the default. An empty string, for the key, the base URL or the model (also a request's own model), counts as not given. The base URL must be `https`; plain `http` is accepted only for `localhost`, `127.0.0.1` and `::1`, so a key never travels unencrypted, and anything else fails `build()`. The SDK builds its own HTTP client and takes no client or `reqwest::ClientBuilder` of yours, because it could not show that such a client refuses redirects and keeps its TLS checks on. `http_settings` takes an `HttpSettings` instead: a proxy (`https` requests tunnel through it, so TLS stays end to end), extra root certificates or only your own (`add_root_certificate`, `tls_built_in_root_certs`), a client certificate (`identity`), `connect_timeout`, pool and TCP options, `local_address`, `http1_only` and `resolve`. They apply to every connection the client opens: API calls, uploads to storage and session sockets. None of them can turn on redirects or turn off certificate or hostname checks, and with an `https` base URL the client sends nothing over plain HTTP. Models are `decisionnode-latest`, `decisionnode-flash-latest` and the pinned ids such as `decisionnode-1.0` and `decisionnode-1.0-flash`; `models()` lists the ones your key may use. A request picks its own with `DecideRequest::model`.

| Feature | Default | |
|---|---|---|
| `rustls-tls` | on | TLS with rustls |
| `native-tls` | off | TLS with the platform's library |
| `sessions` | on | Sessions over a WebSocket (`open_session`, `supervise`) |
| `blocking` | off | `decisionnode::blocking::DecisionNode`, with every call, a blocking `Session` and the Batch API |

The blocking client runs calls on a small runtime of its own: use the async client inside async code.

The SDK sends no telemetry and logs no state or questions. Its only added header is `User-Agent: decisionnode-rust/<version>`. With a [`log`](https://docs.rs/log) logger at debug level for `decisionnode`, it prints the method, path, status, request id and timing of each attempt.

## Development

```sh
cargo test --all-features
```

The integration tests run the DecisionNode SDK conformance suite (every request, response, error, behaviour and constant case, with both clients) against its mock server, from the suite `DECISIONNODE_CONFORMANCE_DIR` points at; they need `python3` for the mock. Without the variable the conformance test fails rather than skipping. CI checks the suite out at one pinned commit, set in a single place at the top of `.github/workflows/ci.yml`, and pins every action to a commit SHA. This crate implements DecisionNode SDK specification version 1.2, exported as `decisionnode::SPEC_VERSION`, and passes conformance suite 1.10.

## License

MIT. See [LICENSE](LICENSE).
