# Changelog

All notable changes to this crate are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the crate follows [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

First release, implementing DecisionNode SDK specification version 1.0 (`SPEC_VERSION`).

### Added

- `DecisionNode`, an async client for `POST /v1/decide` and `GET /v1/models`, configured by arguments, the `DECISIONNODE_API_KEY`, `DECISIONNODE_BASE_URL` and `DECISIONNODE_MODEL` environment variables, and defaults.
- `decide`, `decide_many` (input order, at most 8 at a time by default; every request is checked before any is sent, so an invalid request or a concurrency below 1 is an argument error and nothing is sent), `check`, `classify`, `score`, `gate`, `models` and `decide_raw`, with `*_with` variants of the helpers for images and a model.
- `gate` with fixed question wording, the same in every DecisionNode SDK.
- An empty key, base URL or model counts as not given: the environment, then the default, applies.
- Question builders `truth`, `choice` and `score`; plain JSON questions in the wire form are accepted too.
- Typed answers: `TruthAnswer`, `ChoiceAnswer`, `ScoreAnswer`, and `UnknownAnswer` for answer types this version does not know and for known types with missing or wrongly typed fields, so one odd answer never fails a decision; `ModelInfo` for each entry of `models()` and `DecisionResult` for each result of `decide_many`. Numbers are passed through unrounded, and a missing token count or latency is `None`, never 0.
- Images from a file path, bytes or an `https` URL. A file is read when the `Image` is made; the media type of a file, or of bytes given to `Image::bytes`, is detected from the first bytes (JPEG, PNG or WebP), and `Image::bytes_with_type` names it.
- The safety check result on every decision, parsed from `x-decisionnode-safety`.
- Typed errors by `ErrorKind`, with the server's reason, request id, refusal category, validation entries and `retry_after`. A 403 that is not a refusal is `PermissionDenied`; any status without a kind of its own, 3xx included, is the base `DecisionNodeError` (`ErrorKind::Other`) with its status. An error's `Display` and `message()` are the server's message exactly, `""` when it sent none. Arguments refused before sending are errors of the base kind without a status (`Error::is_argument`), not a kind of their own.
- Only 2xx is a success, and redirects are never followed, so the key is never sent to another URL. A 2xx whose body is not JSON is a `Server` error with the status, request id and the reason from `x-decisionnode-error`.
- `check`, `classify` and `score` return a `Server` error (status, request id, reason) when their own answer is missing or malformed, since they promise a typed answer; `decide` returns `UnknownAnswer` for the same answer.
- `HttpSettings`, passed with `ClientBuilder::http_settings`: proxy, root certificates, client certificate, connect timeout, pool and TCP options. The SDK builds its HTTP client itself and takes no client of the caller's making, so no setting can turn on redirects or turn off certificate and hostname checks.
- The base error is exported as `DecisionNodeError`, an alias of `Error`.
- The base URL must be `https`; plain `http` only for `localhost`, `127.0.0.1` and `::1`.
- A 10 second timeout per attempt, from sending to the last byte of the body (set as a `Duration` or with `timeout_secs`; a timeout of 0 or below and a negative `max_retries` are argument errors), and 2 retries on 408, 429, 5xx, timeouts and connection errors, honouring `retry-after-ms` and `Retry-After` (seconds or an HTTP date), capped at 60 s. `ClientBuilder::sleeper` replaces the wait between attempts, for tests.
- A blocking client behind the `blocking` feature.
