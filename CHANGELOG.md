# Changelog

All notable changes to this crate are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the crate follows [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

First release, implementing DecisionNode SDK specification version 1.1 (`SPEC_VERSION`): every public call, checked against conformance suite 1.4 with the async and the blocking client.

### Added

- `DecisionNode`, an async client for `POST /v1/decide` and `GET /v1/models`, configured by arguments, the `DECISIONNODE_API_KEY`, `DECISIONNODE_BASE_URL` and `DECISIONNODE_MODEL` environment variables, and defaults.
- `decide`, `decide_many` (input order, at most 8 at a time by default; every request is checked before any is sent, so an invalid request is an argument error or the API's own error, a concurrency below 1 an argument error, and nothing is sent), `check`, `classify`, `score`, `gate`, `models` and `decide_raw`, with `*_with` variants of the helpers for images and a model.
- `gate` with fixed question wording, the same in every DecisionNode SDK.
- An empty key, base URL or model counts as not given: the environment, then the default, applies.
- Question builders `truth`, `choice` and `score`; plain JSON questions in the wire form are accepted too.
- Typed answers: `TruthAnswer`, `ChoiceAnswer`, `ScoreAnswer`, and `UnknownAnswer` for answer types this version does not know and for known types with missing or wrongly typed fields, so one odd answer never fails a decision; `ModelInfo` for each entry of `models()` and `DecisionResult` for each result of `decide_many`. Numbers are passed through unrounded, and a missing token count or latency is `None`, never 0.
- Images from an `https` URL (the recommended form), a file path, bytes, base64 text or an upload id. A file is read when the `Image` is made; the media type of local bytes is read from the first bytes (JPEG, PNG or WebP), and a type named with `Image::bytes_with_type` must match them. Local images go inline as base64 when a request's local images total under `DEFAULT_INLINE_THRESHOLD` (524,288 bytes, `ClientBuilder::inline_threshold`), and are uploaded at or above it: one `POST /v1/uploads`, the `PUT`s in parallel with exactly the returned headers (never the key or the client's headers, no redirects), then `upload_id`. An in-memory cache by SHA-256 (256 entries, until 5 minutes before expiry) sends bytes uploaded before by their `upload_id` whatever their size, and requests holding the same bytes at once share one upload. A stale upload the client placed is uploaded again and the request resent once. `upload(images)` uploads explicitly.
- Local checks before any request, with the API's own error kind, `error_type` and message and no status (`Error::is_local`): image format, size (10 MB), pixels from the header (40 megapixels; PNG, JPEG and WebP headers read without decoding), count (4), URL form (`https`, 2,048 characters), unique ids; choices of 1 to 255 options, scores of 1 to 10 levels, a truth with instructions or criteria, a number with instructions and a grid of at most 4,096 values with `step` above 0 and `min` at most `max`.
- `number` questions (`number`, `Number`, `NumberAnswer`, the `number` helper) and `noul` answers read into `TruthAnswer`.
- `systemone`: `POST /v1/systemone` with the model always sent and `truth` questions written as `noul`.
- Sessions (default `sessions` feature, on tokio-tungstenite): `open_session`, `Session::connect`, `send`, `next_event`, `replies`, `end`, `close`, typed `Reply`, `FrameError` and `SessionClosed`, `supervise` with reopening on the reopen codes, and `ErrorKind::SessionLimit` for a `429` `session_limit_exceeded`, never retried.
- The Batch API: `batches()` with `create` (JSON Lines), `add` (split at 10 MiB), `finalize` (a `409` resolved with `get`), `get`, `list`, `results` (line by line, errors as values) and `cancel`, and `run_batch` / `run_batch_with`. `create`, `add` and `finalize` retry only on `429` and `529`.
- The safety check result on every decision, parsed from `x-decisionnode-safety`.
- Typed errors by `ErrorKind`, with the server's reason, request id, refusal category, validation entries and `retry_after`. A 403 that is not a refusal is `PermissionDenied`; any status without a kind of its own, 3xx included, is the base `DecisionNodeError` (`ErrorKind::Other`) with its status. An error's `Display` and `message()` are the server's message exactly, `""` when it sent none. Arguments refused before sending are errors of the base kind without a status (`Error::is_argument`), not a kind of their own.
- Only 2xx is a success, and redirects are never followed, so the key is never sent to another URL. A 2xx whose body is not JSON is a `Server` error with the status, request id and the reason from `x-decisionnode-error`.
- `check`, `classify` and `score` return a `Server` error (status, request id, reason) when their own answer is missing or malformed, since they promise a typed answer; `decide` returns `UnknownAnswer` for the same answer.
- `HttpSettings`, passed with `ClientBuilder::http_settings`: proxy, root certificates, client certificate, connect timeout, pool and TCP options. The SDK builds its HTTP client itself and takes no client of the caller's making, so no setting can turn on redirects or turn off certificate and hostname checks.
- The base error is exported as `DecisionNodeError`, an alias of `Error`.
- The base URL must be `https`; plain `http` only for `localhost`, `127.0.0.1` and `::1`.
- A 10 second timeout per attempt, from sending to the last byte of the body (set as a `Duration` or with `timeout_secs`; a timeout of 0 or below and a negative `max_retries` are argument errors), and 2 retries on 408, 429, 5xx, timeouts and connection errors, honouring `retry-after-ms` and `Retry-After` (seconds or an HTTP date), capped at 60 s. `ClientBuilder::sleeper` replaces the wait between attempts, for tests.
- A blocking client behind the `blocking` feature, with every call, a blocking `Session` and the Batch API.
