//! Errors returned by the client.
//!
//! Every failure is an [`Error`]. Its [`ErrorKind`] says what happened, and the
//! accessors give the details the API sent: the HTTP status, the `error_type` and
//! `message` of the body, the request id, and the reason in words from the
//! `x-decisionnode-error` header.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::safety::Safety;

/// The result type of every call.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The base error of the SDK. An alias of [`Error`], under the name used across
/// the DecisionNode SDKs.
pub type DecisionNodeError = Error;

/// What kind of failure an [`Error`] is.
///
/// Each kind maps to one status of the API, or to a failure without a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// 400: the request is well formed but not usable.
    BadRequest,
    /// 400 with `error_type` `max_tokens_exceeded`: the request is over the
    /// model's context limit. A special case of [`ErrorKind::BadRequest`].
    ContextLimit,
    /// 401: the key is missing or not valid. Also returned before sending
    /// when no API key is configured.
    Authentication,
    /// 402: the account is out of credit.
    InsufficientCredit,
    /// 403 with `error_type` `refusal_error`: the request was refused under the
    /// usage policy. See [`Error::category`] and [`Error::safety`].
    Refusal,
    /// 403 for any other reason.
    PermissionDenied,
    /// 404: unknown path.
    NotFound,
    /// 422: the request body failed validation. See [`Error::entries`].
    Validation,
    /// 429: rate limited. See [`Error::retry_after`].
    RateLimit,
    /// 529: the service is overloaded. See [`Error::retry_after`].
    Overloaded,
    /// Any other 5xx, or a 2xx response whose body is not JSON or not a valid
    /// answer.
    Server,
    /// The base `DecisionNodeError`. With a status: a status with no kind of
    /// its own, such as a 3xx (redirects are never followed), 405, 409, 418,
    /// or a 408 once its retries are spent. Without a status: an argument
    /// refused before anything was sent, such as an empty question map, a value
    /// that cannot be serialised, an image that cannot be read or recognised,
    /// a base URL that is not `https`, a timeout of zero or a concurrency below
    /// 1. See [`Error::is_argument`].
    Other,
    /// No response: the connection failed or was dropped.
    Connection,
    /// No response within the timeout.
    Timeout,
}

impl ErrorKind {
    /// The name of the error class across the DecisionNode SDKs, for example
    /// `"RateLimitError"`.
    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::BadRequest => "BadRequestError",
            ErrorKind::ContextLimit => "ContextLimitError",
            ErrorKind::Authentication => "AuthenticationError",
            ErrorKind::InsufficientCredit => "InsufficientCreditError",
            ErrorKind::Refusal => "RefusalError",
            ErrorKind::PermissionDenied => "PermissionDeniedError",
            ErrorKind::NotFound => "NotFoundError",
            ErrorKind::Validation => "ValidationError",
            ErrorKind::RateLimit => "RateLimitError",
            ErrorKind::Overloaded => "OverloadedError",
            ErrorKind::Server => "ServerError",
            ErrorKind::Other => "DecisionNodeError",
            ErrorKind::Connection => "ConnectionError",
            ErrorKind::Timeout => "TimeoutError",
        }
    }

    /// The more general kind this one is a case of, if any:
    /// [`ErrorKind::ContextLimit`] is a [`ErrorKind::BadRequest`].
    pub fn parent(self) -> Option<ErrorKind> {
        match self {
            ErrorKind::ContextLimit => Some(ErrorKind::BadRequest),
            _ => None,
        }
    }

    /// True when this kind is `other` or a special case of it.
    pub fn is_a(self, other: ErrorKind) -> bool {
        self == other || self.parent() == Some(other)
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An error from the DecisionNode client.
pub struct Error {
    inner: Box<Inner>,
}

struct Inner {
    kind: ErrorKind,
    status: Option<u16>,
    error_type: Option<String>,
    message: Option<String>,
    request_id: Option<String>,
    reason: Option<String>,
    category: Option<String>,
    safety: Option<Safety>,
    entries: Option<Vec<Value>>,
    retry_after: Option<Duration>,
    body: Option<Value>,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error {
            inner: Box::new(Inner {
                kind,
                status: None,
                error_type: None,
                message: Some(message.into()),
                request_id: None,
                reason: None,
                category: None,
                safety: None,
                entries: None,
                retry_after: None,
                body: None,
                source: None,
            }),
        }
    }

    /// An argument refused before anything is sent: the base kind, no status.
    pub(crate) fn invalid(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Other, message)
    }

    pub(crate) fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Error {
        self.inner.source = Some(Box::new(source));
        self
    }

    /// Names the request of a `decide_many` call that an argument error is
    /// about, by its position.
    pub(crate) fn in_request(mut self, index: usize) -> Error {
        let message = self.inner.message.take().unwrap_or_default();
        self.inner.message = Some(format!("request {index}: {message}"));
        self
    }

    /// The status, request id and reason of the 2xx response this error is
    /// about: a body that is not JSON, or not the shape the call promises.
    pub(crate) fn with_response(mut self, status: u16, headers: &HeaderMap) -> Error {
        self.inner.status = Some(status);
        self.inner.request_id = header(headers, "x-request-id");
        self.inner.reason = header(headers, "x-decisionnode-error");
        self
    }

    /// Builds the error for a response outside 200 to 299.
    pub(crate) fn from_response(status: u16, headers: &HeaderMap, body: &[u8]) -> Error {
        let parsed: Option<Value> = serde_json::from_slice(body).ok();
        let detail = parsed.as_ref().and_then(|b| b.get("detail"));
        let mut error_type = None;
        let mut message = None;
        let mut category = None;
        let mut entries = None;
        match detail {
            Some(Value::Object(map)) => {
                error_type = map.get("error_type").and_then(Value::as_str).map(str::to_owned);
                message = map.get("message").and_then(Value::as_str).map(str::to_owned);
                category = map.get("category").and_then(Value::as_str).map(str::to_owned);
            }
            Some(Value::String(text)) => message = Some(text.clone()),
            Some(Value::Array(list)) => entries = Some(list.clone()),
            _ => {}
        }

        let kind = match status {
            400 if error_type.as_deref() == Some("max_tokens_exceeded") => ErrorKind::ContextLimit,
            400 => ErrorKind::BadRequest,
            401 => ErrorKind::Authentication,
            402 => ErrorKind::InsufficientCredit,
            403 if error_type.as_deref() == Some("refusal_error") => ErrorKind::Refusal,
            403 => ErrorKind::PermissionDenied,
            404 => ErrorKind::NotFound,
            422 => ErrorKind::Validation,
            429 => ErrorKind::RateLimit,
            529 => ErrorKind::Overloaded,
            500..=599 => ErrorKind::Server,
            _ => ErrorKind::Other,
        };

        let safety = if kind == ErrorKind::Refusal {
            header(headers, "x-decisionnode-safety").map(|v| Safety::parse(&v))
        } else {
            None
        };
        let retry_after =
            if matches!(kind, ErrorKind::RateLimit | ErrorKind::Overloaded) { retry_after(headers) } else { None };

        Error {
            inner: Box::new(Inner {
                kind,
                status: Some(status),
                error_type,
                message,
                request_id: header(headers, "x-request-id"),
                reason: header(headers, "x-decisionnode-error"),
                category: if kind == ErrorKind::Refusal { category } else { None },
                safety,
                entries: if kind == ErrorKind::Validation { entries } else { None },
                retry_after,
                body: parsed,
                source: None,
            }),
        }
    }

    /// What kind of error this is.
    pub fn kind(&self) -> ErrorKind {
        self.inner.kind
    }

    /// True when this is a `kind` error or a special case of it. A
    /// [`ErrorKind::ContextLimit`] error `is(ErrorKind::BadRequest)`.
    pub fn is(&self, kind: ErrorKind) -> bool {
        self.inner.kind.is_a(kind)
    }

    /// The HTTP status, when the API answered.
    pub fn status(&self) -> Option<u16> {
        self.inner.status
    }

    /// The `error_type` of the error body, for example `rate_limit_error`.
    pub fn error_type(&self) -> Option<&str> {
        self.inner.error_type.as_deref()
    }

    /// The error's message, the same text as its `Display`: the server's
    /// `message` (`detail.message`, or `detail` when it is a string), or the
    /// client's description of a failure without a response. `""` when the
    /// server sent none, never missing; the server's explanation is then in
    /// [`Error::reason`].
    pub fn message(&self) -> &str {
        self.inner.message.as_deref().unwrap_or("")
    }

    /// The request id from the `x-request-id` header. Quote it when you contact
    /// support.
    pub fn request_id(&self) -> Option<&str> {
        self.inner.request_id.as_deref()
    }

    /// The server's explanation in words, from the `x-decisionnode-error`
    /// header, for example "question 'q': a score takes up to 10 levels, got 11".
    pub fn reason(&self) -> Option<&str> {
        self.inner.reason.as_deref()
    }

    /// For a [`ErrorKind::Refusal`]: the refused category, `harm` or `self_harm`.
    pub fn category(&self) -> Option<&str> {
        self.inner.category.as_deref()
    }

    /// For a [`ErrorKind::Refusal`]: what the safety check found.
    pub fn safety(&self) -> Option<&Safety> {
        self.inner.safety.as_ref()
    }

    /// For a [`ErrorKind::Validation`]: the server's list of validation entries.
    pub fn entries(&self) -> Option<&[Value]> {
        self.inner.entries.as_deref()
    }

    /// For a [`ErrorKind::RateLimit`] or [`ErrorKind::Overloaded`]: how long the
    /// server asks you to wait, from `retry-after-ms`, else `Retry-After` in
    /// seconds or as an HTTP date, capped at 60 seconds.
    pub fn retry_after(&self) -> Option<Duration> {
        self.inner.retry_after
    }

    /// The parsed error body, when it was JSON.
    pub fn body(&self) -> Option<&Value> {
        self.inner.body.as_ref()
    }

    /// True for an argument the client refused before sending anything: an
    /// error of the base kind ([`ErrorKind::Other`]) without a status.
    pub fn is_argument(&self) -> bool {
        self.inner.kind == ErrorKind::Other && self.inner.status.is_none()
    }

    /// True for a failure without a response: a timeout or a connection error.
    pub fn is_transport(&self) -> bool {
        matches!(self.inner.kind, ErrorKind::Timeout | ErrorKind::Connection)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Error");
        d.field("kind", &self.inner.kind);
        if let Some(s) = self.inner.status {
            d.field("status", &s);
        }
        if let Some(v) = &self.inner.error_type {
            d.field("error_type", v);
        }
        if let Some(v) = &self.inner.message {
            d.field("message", v);
        }
        if let Some(v) = &self.inner.request_id {
            d.field("request_id", v);
        }
        if let Some(v) = &self.inner.reason {
            d.field("reason", v);
        }
        if let Some(v) = &self.inner.category {
            d.field("category", v);
        }
        if let Some(v) = &self.inner.retry_after {
            d.field("retry_after", v);
        }
        if let Some(v) = &self.inner.source {
            d.field("source", v);
        }
        d.finish()
    }
}

impl fmt::Display for Error {
    /// The error's own message: the server's `message`, unchanged, or the
    /// client's description when there was no response. Empty when the server
    /// sent none. The kind, status, reason and request id are separate
    /// accessors, and all of them are in the `Debug` form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.inner.message.as_deref().unwrap_or(""))
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source.as_ref().map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

pub(crate) fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
}

/// The longest wait the client honours, whatever the server asks for.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// The wait the server asks for: `retry-after-ms` when sent, else
/// `Retry-After` in seconds or as an HTTP date. Capped at 60 seconds.
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    retry_after_at(headers, SystemTime::now())
}

fn retry_after_at(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let seconds = |name: &str, scale: f64| {
        header(headers, name)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .map(|v| (v / scale).clamp(0.0, MAX_RETRY_AFTER.as_secs_f64()))
            .map(Duration::from_secs_f64)
    };
    let date = || {
        let at = parse_http_date(header(headers, "retry-after")?.trim())?;
        Some(at.duration_since(now).unwrap_or(Duration::ZERO).min(MAX_RETRY_AFTER))
    };
    seconds("retry-after-ms", 1000.0).or_else(|| seconds("retry-after", 1.0)).or_else(date)
}

/// Parses an HTTP date: the IMF-fixdate form (`Sun, 06 Nov 1994 08:49:37 GMT`)
/// and the two obsolete forms HTTP still accepts (`Sunday, 06-Nov-94 08:49:37
/// GMT` and `Sun Nov  6 08:49:37 1994`).
pub(crate) fn parse_http_date(text: &str) -> Option<SystemTime> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let month = |m: &str| MONTHS.iter().position(|x| x.eq_ignore_ascii_case(m)).map(|i| i as u32 + 1);
    let parts: Vec<&str> = text.split_whitespace().collect();
    let (day, mon, year, time) = match parts.as_slice() {
        // IMF-fixdate: Sun, 06 Nov 1994 08:49:37 GMT
        [_, d, m, y, t, "GMT"] => (d.parse::<u32>().ok()?, month(m)?, y.parse::<i64>().ok()?, *t),
        // RFC 850: Sunday, 06-Nov-94 08:49:37 GMT
        [_, dmy, t, "GMT"] => {
            let mut it = dmy.split('-');
            let (d, m, y) = (it.next()?, it.next()?, it.next()?);
            let y = y.parse::<i64>().ok()?;
            (
                d.parse().ok()?,
                month(m)?,
                if y < 70 {
                    2000 + y
                } else if y < 100 {
                    1900 + y
                } else {
                    y
                },
                *t,
            )
        }
        // asctime: Sun Nov  6 08:49:37 1994
        [_, m, d, t, y] => (d.parse().ok()?, month(m)?, y.parse().ok()?, *t),
        _ => return None,
    };
    let mut hms = time.split(':').map(|x| x.parse::<u64>().ok());
    let (h, mi, se) = (hms.next()??, hms.next()??, hms.next()??);
    if hms.next().is_some()
        || !(1..=31).contains(&day)
        || h > 23
        || mi > 59
        || se > 60
        || !(1970..=9999).contains(&year)
    {
        return None;
    }
    // Days since 1970-01-01 in the proleptic Gregorian calendar.
    let (y, m) = if mon <= 2 { (year - 1, mon + 9) } else { (year, mon - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m as i64 + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(days).ok()? * 86_400 + h * 3600 + mi * 60 + se;
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        map
    }

    #[test]
    fn maps_statuses_to_kinds() {
        let h = HeaderMap::new();
        let cases = [
            (400, r#"{"detail":"Too many choices."}"#, ErrorKind::BadRequest),
            (400, r#"{"detail":{"error_type":"max_tokens_exceeded"}}"#, ErrorKind::ContextLimit),
            (401, r#"{"detail":{"error_type":"authentication_error","message":"x"}}"#, ErrorKind::Authentication),
            (402, r#"{"detail":{"error_type":"insufficient_credit_error"}}"#, ErrorKind::InsufficientCredit),
            (403, r#"{"detail":{"error_type":"refusal_error","category":"harm"}}"#, ErrorKind::Refusal),
            (403, r#"{"detail":{"error_type":"permission_error"}}"#, ErrorKind::PermissionDenied),
            (404, r#"{"detail":{"error_type":"not_found_error"}}"#, ErrorKind::NotFound),
            (405, r#"{"detail":{"error_type":"method_not_allowed"}}"#, ErrorKind::Other),
            (302, r#"{"detail":{"error_type":"redirect"}}"#, ErrorKind::Other),
            (409, r#"{"detail":{"message":"Conflict."}}"#, ErrorKind::Other),
            (408, r#"{}"#, ErrorKind::Other),
            (422, r#"{"detail":[]}"#, ErrorKind::Validation),
            (429, r#"{}"#, ErrorKind::RateLimit),
            (500, r#"{}"#, ErrorKind::Server),
            (502, "<html></html>", ErrorKind::Server),
            (529, r#"{}"#, ErrorKind::Overloaded),
        ];
        for (status, body, kind) in cases {
            assert_eq!(Error::from_response(status, &h, body.as_bytes()).kind(), kind, "status {status}");
        }
    }

    #[test]
    fn reads_the_three_body_shapes() {
        let h = headers(&[("x-request-id", "req_1"), ("x-decisionnode-error", "why")]);
        let e = Error::from_response(
            400,
            &h,
            br#"{"detail":{"error_type":"api_usage_error","message":"Invalid request."}}"#,
        );
        assert_eq!(e.error_type(), Some("api_usage_error"));
        assert_eq!(e.message(), "Invalid request.");
        assert_eq!(e.request_id(), Some("req_1"));
        assert_eq!(e.reason(), Some("why"));

        let e = Error::from_response(400, &h, br#"{"detail":"Too many score levels."}"#);
        assert_eq!(e.error_type(), None);
        assert_eq!(e.message(), "Too many score levels.");

        let e = Error::from_response(422, &h, br#"{"detail":[{"type":"missing"}]}"#);
        assert_eq!(e.entries().unwrap().len(), 1);
        assert_eq!(e.message(), "");
    }

    #[test]
    fn context_limit_is_a_bad_request() {
        let e = Error::from_response(400, &HeaderMap::new(), br#"{"detail":{"error_type":"max_tokens_exceeded"}}"#);
        assert!(e.is(ErrorKind::ContextLimit));
        assert!(e.is(ErrorKind::BadRequest));
        assert!(!e.is(ErrorKind::Server));
    }

    #[test]
    fn retry_after_prefers_milliseconds() {
        assert_eq!(
            retry_after(&headers(&[("retry-after", "2"), ("retry-after-ms", "1500")])),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(retry_after(&headers(&[("retry-after", "2")])), Some(Duration::from_secs(2)));
        assert_eq!(retry_after(&headers(&[("retry-after", "soon")])), None);
        assert_eq!(retry_after(&headers(&[("retry-after", "1e300")])), Some(MAX_RETRY_AFTER));
        assert_eq!(retry_after(&headers(&[("retry-after", "120")])), Some(MAX_RETRY_AFTER));
        assert_eq!(retry_after(&headers(&[("retry-after-ms", "90000")])), Some(MAX_RETRY_AFTER));
        assert_eq!(retry_after(&headers(&[("retry-after-ms", "-5")])), Some(Duration::ZERO));
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn retry_after_reads_http_dates() {
        let at = |t: &str| parse_http_date(t).unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(at("Sun, 06 Nov 1994 08:49:37 GMT"), 784_111_777);
        assert_eq!(at("Sunday, 06-Nov-94 08:49:37 GMT"), 784_111_777);
        assert_eq!(at("Sun Nov  6 08:49:37 1994"), 784_111_777);
        assert_eq!(at("Thu, 29 Feb 2024 00:00:00 GMT"), 1_709_164_800);
        assert!(parse_http_date("06 Nov 1994").is_none());
        assert!(parse_http_date("Sun, 06 Nov 1994 25:00:00 GMT").is_none());

        let now = UNIX_EPOCH + Duration::from_secs(784_111_777);
        let h = |v: &str| headers(&[("retry-after", v)]);
        assert_eq!(retry_after_at(&h("Sun, 06 Nov 1994 08:49:40 GMT"), now), Some(Duration::from_secs(3)));
        assert_eq!(retry_after_at(&h("Sun, 06 Nov 1994 09:49:37 GMT"), now), Some(MAX_RETRY_AFTER));
        assert_eq!(retry_after_at(&h("Sun, 06 Nov 1994 08:00:00 GMT"), now), Some(Duration::ZERO));
    }

    #[test]
    fn refusal_reads_category_and_safety() {
        let h = headers(&[("x-decisionnode-safety", "harm=0.964;blocked, self_harm=0.002")]);
        let e = Error::from_response(
            403,
            &h,
            br#"{"detail":{"error_type":"refusal_error","message":"no","category":"harm"}}"#,
        );
        assert_eq!(e.category(), Some("harm"));
        let s = e.safety().unwrap();
        assert_eq!(s.flagged, vec!["harm".to_string()]);
        assert_eq!(s.harm, Some(0.964));
    }

    #[test]
    fn display_is_the_server_message() {
        let h = headers(&[("x-request-id", "req_9")]);
        let e = Error::from_response(
            429,
            &h,
            br#"{"detail":{"error_type":"rate_limit_error","message":"Rate limit exceeded."}}"#,
        );
        assert_eq!(e.to_string(), "Rate limit exceeded.");
        assert_eq!(e.message(), "Rate limit exceeded.");
        let e = Error::from_response(400, &h, br#"{"detail":{"error_type":"max_tokens_exceeded"}}"#);
        assert_eq!(e.to_string(), "");
        assert_eq!(e.message(), "");
        assert!(!e.is_argument());
        assert!(Error::invalid("x").is_argument());
    }
}
