//! Knowledge: a workspace's own documents in collections of type `text` (a
//! knowledge base), the `knowledge` booster that gives a decision the
//! passages that apply to it, and search without a decision.
//!
//! ```no_run
//! # async fn run(dn: decisionnode::DecisionNode) -> decisionnode::Result<()> {
//! use decisionnode::{knowledge, truth, DecideRequest, Document, DocumentOptions, NewCollection};
//! use serde_json::json;
//!
//! let policy = dn.collections().create_with(&NewCollection::text("Claims policy")).await?;
//! let id = policy["id"].as_str().unwrap_or_default();
//! dn.collections()
//!     .add_document(id, Document::path("claims-policy-2026.pdf"), &DocumentOptions::new())
//!     .await?;
//! dn.collections().wait_until_ready(id, None).await?;
//!
//! let d = dn
//!     .decide(
//!         DecideRequest::new()
//!             .state(json!({"claim": {"amount": "740.00", "receipt": false}}))
//!             .booster(knowledge(id).require(true))
//!             .question("approve", truth("Should the claim be approved under our policy?")),
//!     )
//!     .await?;
//! let evidence = d.evidence.as_ref().and_then(|e| e.get("knowledge"));
//! if let Some(reading) = evidence.and_then(|e| e.results.first()).and_then(|r| r.as_knowledge()) {
//!     for p in &reading.passages {
//!         println!("{} {:?} {:?} {}..{}", p.n, p.title, p.heading, p.byte_start, p.byte_end);
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::Method;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::Digest;

use crate::booster::Booster;
use crate::check::{entry, usage_error};
use crate::client::{DecisionNode, Policy, Reply, STALE_UPLOAD};
use crate::collections::{echoed, metadata_entries, segment, string_entries, Collections, LABEL_MAX, REFERENCE_MAX};
use crate::error::{Error, ErrorKind, Result};
use crate::upload::{UploadFile, MAX_UPLOAD_FILES};

/// The largest file a document may be, in bytes (50 MB).
pub const MAX_DOCUMENT_BYTES: usize = 52_428_800;
/// How long [`Collections::wait_until_ready`] waits at most.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(300);
/// The first wait between the reads of [`Collections::wait_until_ready`];
/// each later wait is 1.5 times the one before, up to 10 seconds.
pub const DEFAULT_READY_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// The shortest first wait [`Collections::wait_until_ready_with`] takes.
pub const MIN_READY_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// The longest wait between two reads of [`Collections::wait_until_ready`].
const MAX_READY_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// The media types a document's file may have, in the API's order.
pub const DOCUMENT_TYPES: [&str; 11] = [
    "text/plain",
    "text/markdown",
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "text/html",
    "image/png",
    "image/jpeg",
    "image/tiff",
    "image/webp",
];

/// The longest title, in characters.
const TITLE_MAX: usize = 200;
/// The highest `pin_rank`.
const PIN_RANK_MAX: i64 = 1000;

// --------------------------------------------------------------------------- the reading

/// A reading of the `knowledge` booster, and the answer of
/// [`Collections::search`]: the passages of the workspace's documents that
/// apply, in the order they were given to the decision (pinned first), and
/// how they were found. Every value is as sent.
///
/// A reading without `collection` or `passages`, a reading or passage with
/// a field of the wrong type, is a [`crate::Reading::Unknown`] holding its
/// raw fields.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct KnowledgeReading {
    /// The `col_` id of the knowledge base read.
    pub collection: String,
    /// Its name; `None` for a publishable key's reading.
    pub collection_name: Option<String>,
    /// The corpus read.
    pub corpus: Option<KnowledgeCorpus>,
    /// The query the passages were found by.
    pub query: Option<KnowledgeQuery>,
    /// The token budget of the retrieved passages.
    pub budget: Option<KnowledgeBudget>,
    /// The collection's scope key and the value named; `None` for a
    /// collection without one.
    pub scope: Option<KnowledgeScope>,
    /// `scope_not_named`, `over_budget`, `budget_floor`, `pinned_filtered`,
    /// or any other value as sent; empty when there are none.
    pub warnings: Vec<String>,
    /// The passages, pinned first.
    pub passages: Vec<KnowledgePassage>,
    /// The passages left out, by cause (`below_min_score`, `per_item_cap`,
    /// `over_budget`, `pinned_filtered`, `short_entry`, `context_limit`, or
    /// any other), as sent.
    pub omitted: Option<Map<String, Value>>,
    /// How many items were not ready yet.
    pub pending_items: Option<u64>,
    /// How the search ran.
    pub search: Option<KnowledgeSearch>,
    /// `true` when the context limit dropped a passage; `None` on a search's
    /// answer, which has none.
    pub truncated: Option<bool>,
    /// The reading as received.
    pub raw: Value,
}

/// The corpus a [`KnowledgeReading`] read.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct KnowledgeCorpus {
    /// The corpus version, raised by every change a search can see.
    pub version: Option<u64>,
}

/// The query of a [`KnowledgeReading`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct KnowledgeQuery {
    /// `auto` (built from the request), `request` (the `query` option),
    /// `none` (`top_k` 0), or any other value as sent.
    pub source: Option<String>,
    /// The query's tokens.
    pub tokens: Option<u64>,
    /// The query's text: only with `include_text`, and on a search.
    pub text: Option<String>,
}

/// The token budget of a [`KnowledgeReading`]'s retrieved passages.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct KnowledgeBudget {
    /// `auto` or `fixed`, or any other value as sent.
    pub mode: Option<String>,
    /// The retrieved passages' tokens.
    pub tokens: Option<u64>,
    /// `short`, `long`, or `None`.
    pub entry: Option<String>,
    /// `None` in a short entry, else `state`, `images`, `other_boosters`,
    /// `pinned`, `fixed`, `require_floor`, or any other value as sent.
    pub cause: Option<String>,
}

/// The scope of a [`KnowledgeReading`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct KnowledgeScope {
    /// The collection's scope key.
    pub key: Option<String>,
    /// The value named, `None` when none was.
    pub value: Option<String>,
}

/// How the search of a [`KnowledgeReading`] ran.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct KnowledgeSearch {
    /// The plan, for example `exact`.
    pub plan: Option<String>,
    /// How many candidates were ranked.
    pub candidates: Option<u64>,
    /// `true` when an exact rerun made up a short candidate list.
    pub fallback: Option<bool>,
}

/// One passage of a [`KnowledgeReading`]: a chunk of a document, with where
/// it is in the document's stored text.
///
/// `byte_start` and `byte_end` are UTF-8 byte offsets into the item's stored
/// text ([`Collections::get_item`] with `include_text`), the end exclusive,
/// so `&text[byte_start..byte_end]` is the passage.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct KnowledgePassage {
    /// The passage's number in the block given to the decision, from 1.
    pub n: u64,
    /// `true` for a pinned item's passage, always given.
    pub pinned: bool,
    /// The `itm_` id of the document.
    pub item: String,
    /// The chunk's number in the document.
    pub chunk: u64,
    /// Where the passage starts in the stored text, in UTF-8 bytes.
    pub byte_start: u64,
    /// Where it ends, in UTF-8 bytes, exclusive.
    pub byte_end: u64,
    /// The document's reference; `None` for a publishable key's reading.
    pub reference: Option<String>,
    /// The document's title; `None` for a publishable key's reading.
    pub title: Option<String>,
    /// The heading path of the chunk; `None` when it has none, and for a
    /// publishable key's reading.
    pub heading: Option<String>,
    /// The passage's tokens.
    pub tokens: Option<u64>,
    /// The similarity, as sent (4 decimals); `None` for a pin.
    pub score: Option<f64>,
    /// `true` when it was found by its words.
    pub lexical: Option<bool>,
    /// `false` when the decision's context left it out; `None` on a search's
    /// passages.
    pub rendered: Option<bool>,
    /// The passage's text: only with `include_text`, and always on a search.
    pub text: Option<String>,
    /// The passage as received.
    pub raw: Value,
}

/// A field of the reading or of a passage that may be absent or `null`
/// (`Some(None)`); present, it must be of its type, else the reading is
/// malformed (`None`).
fn checked<'a, T>(
    obj: &'a Map<String, Value>,
    key: &str,
    read: impl FnOnce(&'a Value) -> Option<T>,
) -> Option<Option<T>> {
    match obj.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(v) => read(v).map(Some),
    }
}

/// A field inside one of the reading's objects, read as sent: of another
/// type, `None`.
fn inner<'a, T>(obj: &'a Map<String, Value>, key: &str, read: impl FnOnce(&'a Value) -> Option<T>) -> Option<T> {
    obj.get(key).and_then(read)
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

impl KnowledgePassage {
    fn parse(raw: &Value) -> Option<KnowledgePassage> {
        let p = raw.as_object()?;
        Some(KnowledgePassage {
            n: p.get("n")?.as_u64()?,
            pinned: p.get("pinned")?.as_bool()?,
            item: p.get("item")?.as_str()?.to_owned(),
            chunk: p.get("chunk")?.as_u64()?,
            byte_start: p.get("byte_start")?.as_u64()?,
            byte_end: p.get("byte_end")?.as_u64()?,
            reference: checked(p, "reference", text)?,
            title: checked(p, "title", text)?,
            heading: checked(p, "heading", text)?,
            tokens: checked(p, "tokens", Value::as_u64)?,
            score: checked(p, "score", Value::as_f64)?,
            lexical: checked(p, "lexical", Value::as_bool)?,
            rendered: checked(p, "rendered", Value::as_bool)?,
            text: checked(p, "text", text)?,
            raw: raw.clone(),
        })
    }
}

impl KnowledgeReading {
    /// Reads a knowledge result; `None` when a field is of the wrong type or
    /// `collection` or `passages` is missing.
    pub(crate) fn parse(obj: &Map<String, Value>, raw: &Value) -> Option<KnowledgeReading> {
        let passages = obj.get("passages")?.as_array()?.iter().map(KnowledgePassage::parse).collect::<Option<_>>()?;
        let object = |k: &str| checked(obj, k, Value::as_object);
        let warnings = checked(obj, "warnings", |w| w.as_array()?.iter().map(text).collect::<Option<Vec<_>>>())?;
        Some(KnowledgeReading {
            collection: obj.get("collection")?.as_str()?.to_owned(),
            collection_name: checked(obj, "collection_name", text)?,
            corpus: object("corpus")?.map(|c| KnowledgeCorpus { version: inner(c, "version", Value::as_u64) }),
            query: object("query")?.map(|q| KnowledgeQuery {
                source: inner(q, "source", text),
                tokens: inner(q, "tokens", Value::as_u64),
                text: inner(q, "text", text),
            }),
            budget: object("budget")?.map(|b| KnowledgeBudget {
                mode: inner(b, "mode", text),
                tokens: inner(b, "tokens", Value::as_u64),
                entry: inner(b, "entry", text),
                cause: inner(b, "cause", text),
            }),
            scope: object("scope")?
                .map(|s| KnowledgeScope { key: inner(s, "key", text), value: inner(s, "value", text) }),
            warnings: warnings.unwrap_or_default(),
            passages,
            omitted: object("omitted")?.cloned(),
            pending_items: inner(obj, "pending_items", Value::as_u64),
            search: object("search")?.map(|s| KnowledgeSearch {
                plan: inner(s, "plan", text),
                candidates: inner(s, "candidates", Value::as_u64),
                fallback: inner(s, "fallback", Value::as_bool),
            }),
            truncated: checked(obj, "truncated", Value::as_bool)?,
            raw: raw.clone(),
        })
    }
}

// --------------------------------------------------------------------------- the booster

/// The `knowledge` booster: the passages of a knowledge base that apply to
/// the decision, given to it as facts and rules to apply. Made by
/// [`knowledge`]; every option is sent only when set, as given, and checked
/// by the API, not here.
///
/// ```
/// use decisionnode::{knowledge, truth, DecideRequest};
/// use serde_json::json;
///
/// let request = DecideRequest::new()
///     .state(json!({"claim": {"amount": "740.00", "receipt": false}, "mission": "M-42"}))
///     .booster(knowledge("col_3f6c1a9e0b2d4c7f8a1e5d9b2c4f6a80").scope("M-42").top_k(4).require(true))
///     .question("approve", truth("Should the claim be approved under our policy?"));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub struct Knowledge {
    options: Map<String, Value>,
    require: bool,
    /// An option that could not be serialised, an argument error when sent.
    invalid: Option<String>,
}

/// The `knowledge` booster over a knowledge base, by its `col_` id. See
/// [`Knowledge`].
pub fn knowledge(collection: impl Into<String>) -> Knowledge {
    let mut options = Map::new();
    options.insert("collection".into(), Value::String(collection.into()));
    Knowledge { options, require: false, invalid: None }
}

impl Knowledge {
    fn set(mut self, key: &str, value: Value) -> Knowledge {
        self.options.insert(key.into(), value);
        self
    }

    fn set_json(mut self, key: &str, value: impl Serialize) -> Knowledge {
        match serde_json::to_value(value) {
            Ok(v) => self.set(key, v),
            Err(e) => {
                self.invalid = Some(format!("the knowledge option {key} cannot be serialised to JSON: {e}"));
                self
            }
        }
    }

    /// The query, in place of the one built from the request's questions and
    /// state (1 to 2,000 characters).
    pub fn query(self, query: impl Into<String>) -> Knowledge {
        self.set("query", Value::String(query.into()))
    }

    /// The value of the collection's scope key: the passages are then the
    /// global items' and this value's. Without it, only the global items'.
    pub fn scope(self, value: impl Into<String>) -> Knowledge {
        self.set("scope", Value::String(value.into()))
    }

    /// Metadata equality, sent as `match` (a reserved word in Rust):
    /// `{"region": "SE", "product": ["car", "home"]}`, a scalar is equal, an
    /// array is any of.
    pub fn matching(self, matching: impl Serialize) -> Knowledge {
        self.set_json("match", matching)
    }

    /// A typed filter tree over metadata and items, as given.
    pub fn filter(self, filter: impl Serialize) -> Knowledge {
        self.set_json("filter", filter)
    }

    /// The most retrieved passages, pins not counted (0 to 20; 0 is pinned
    /// only). Default 4.
    pub fn top_k(self, top_k: i64) -> Knowledge {
        self.set("top_k", json!(top_k))
    }

    /// The retrieved passages' token budget: `"auto"` (the default) or a
    /// number of tokens, 128 to 8,192.
    pub fn max_tokens(self, max_tokens: impl Into<Value>) -> Knowledge {
        self.set("max_tokens", max_tokens.into())
    }

    /// `false` leaves the pinned items out. Default `true`.
    pub fn pinned(self, pinned: bool) -> Knowledge {
        self.set("pinned", Value::Bool(pinned))
    }

    /// A similarity floor, 0 to 1, for passages found by meaning alone.
    pub fn min_score(self, min_score: f64) -> Knowledge {
        self.set("min_score", json!(min_score))
    }

    /// `true` adds each passage's text and the query's text to the reading;
    /// needs a full-scope key.
    pub fn include_text(self, include_text: bool) -> Knowledge {
        self.set("include_text", Value::Bool(include_text))
    }

    /// Any other option, sent as given.
    pub fn option(self, key: impl Into<String>, value: impl Serialize) -> Knowledge {
        self.set_json(&key.into(), value)
    }

    /// `true`: the decision fails unless the retrieval ran over a ready
    /// knowledge base (a `529` `booster_unavailable`, or a `409`
    /// `knowledge_collection_empty` when it holds no document that is ready
    /// or on its way). Sent only when `true`.
    pub fn require(mut self, require: bool) -> Knowledge {
        self.require = require;
        self
    }
}

/// The booster in its object form, never the bare name, which the API
/// refuses for `knowledge`. An option that could not be serialised is an
/// argument error of the call that sends it.
impl From<Knowledge> for Booster {
    fn from(k: Knowledge) -> Booster {
        let options = match k.invalid {
            Some(e) => Err(e),
            None => Ok(Value::Object(k.options)),
        };
        Booster::with_parts("knowledge", k.require.then_some(true), options)
    }
}

// --------------------------------------------------------------------------- documents

/// The content of one document of a knowledge base: text in memory, a file
/// by its path, its bytes or a reader, or a knowledge upload made before.
///
/// Text and Markdown files under the client's inline threshold that are
/// UTF-8 without NUL are sent as text in the item call itself. Every other
/// file is uploaded first (`POST /v1/uploads` with the knowledge purpose,
/// then the storage `PUT`) and named by its upload id; the API converts it
/// to text. A file's media type is the one given, else the file name's
/// extension's, else read from its first bytes (PDF, PNG, JPEG, TIFF,
/// WebP); DOCX, PPTX, XLSX and text need a type or a file name.
///
/// ```
/// use decisionnode::Document;
///
/// let rules = Document::text("# Receipts\n\nReject any claim over 500 SEK without a receipt.");
/// let notes = Document::text("Plain notes.").format("text");
/// let policy = Document::path("claims-policy-2026.pdf");
/// let sheet = Document::bytes(vec![0x50, 0x4b, 0x03, 0x04])
///     .content_type("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet");
/// ```
#[must_use]
pub struct Document {
    source: Source,
    format: Option<String>,
    content_type: Option<String>,
    filename: Option<String>,
}

enum Source {
    Text(String),
    Path(PathBuf),
    Bytes(Bytes),
    Reader(Box<dyn std::io::Read + Send>),
    AsyncReader(Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    UploadId(String),
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = match &self.source {
            Source::Text(t) => format!("text ({} bytes)", t.len()),
            Source::Path(p) => format!("path {}", p.display()),
            Source::Bytes(b) => format!("bytes ({})", b.len()),
            Source::Reader(_) => "reader".to_owned(),
            Source::AsyncReader(_) => "async reader".to_owned(),
            Source::UploadId(id) => format!("upload {id}"),
        };
        f.debug_struct("Document")
            .field("source", &source)
            .field("format", &self.format)
            .field("content_type", &self.content_type)
            .field("filename", &self.filename)
            .finish()
    }
}

impl Document {
    fn of(source: Source) -> Document {
        Document { source, format: None, content_type: None, filename: None }
    }

    /// Text held in memory, Markdown unless [`Document::format`] says
    /// `text`. Sent inline, whatever its size.
    pub fn text(text: impl Into<String>) -> Document {
        Document::of(Source::Text(text.into()))
    }

    /// A local file, read when the call is made. Its file name is the path's
    /// last part unless [`Document::filename`] gives one.
    pub fn path(path: impl AsRef<Path>) -> Document {
        Document::of(Source::Path(path.as_ref().to_owned()))
    }

    /// The bytes of a file.
    pub fn bytes(data: impl Into<Bytes>) -> Document {
        Document::of(Source::Bytes(data.into()))
    }

    /// A file from a reader, read to its end once, into memory, when the call
    /// is made (the upload needs its size and checksums first). Reading stops
    /// past 50 MB.
    pub fn reader(reader: impl std::io::Read + Send + 'static) -> Document {
        Document::of(Source::Reader(Box::new(reader)))
    }

    /// A file from an async reader, read as [`Document::reader`] reads.
    pub fn async_reader(reader: impl tokio::io::AsyncRead + Send + Unpin + 'static) -> Document {
        Document::of(Source::AsyncReader(Box::new(reader)))
    }

    /// A knowledge upload made before (see [`DecisionNode::upload_documents`]),
    /// sent as given.
    pub fn upload_id(upload_id: impl Into<String>) -> Document {
        Document::of(Source::UploadId(upload_id.into()))
    }

    /// The format of a [`Document::text`]: `markdown` (the API's default) or
    /// `text`. Sent only when set. A file's format follows its media type:
    /// set on a file, it is an argument error of the call.
    pub fn format(mut self, format: impl Into<String>) -> Document {
        self.format = Some(format.into());
        self
    }

    /// The media type of a file, one of [`DOCUMENT_TYPES`].
    pub fn content_type(mut self, content_type: impl Into<String>) -> Document {
        self.content_type = Some(content_type.into());
        self
    }

    /// The file's name, which gives its media type when none is set and its
    /// default title (the name without its last extension).
    pub fn filename(mut self, filename: impl Into<String>) -> Document {
        self.filename = Some(filename.into());
        self
    }
}

/// A document read and ready to send.
enum Loaded {
    Text { text: String, format: Option<String> },
    File(File),
    UploadId(String),
}

/// A document's file, read whole.
struct File {
    data: Bytes,
    content_type: String,
    filename: Option<String>,
    sha256: [u8; 32],
}

impl File {
    fn upload(&self) -> UploadFile<'_> {
        UploadFile {
            data: &self.data,
            content_type: &self.content_type,
            sha256: self.sha256,
            purpose: Some("knowledge"),
            filename: self.filename.as_deref(),
        }
    }

    /// The file name without its last extension.
    fn stem(&self) -> Option<String> {
        let name = self.filename.as_deref()?;
        Some(match name.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => stem.to_owned(),
            _ => name.to_owned(),
        })
    }

    /// Text sent inline: a text or Markdown file under the threshold that is
    /// UTF-8 without NUL. `None` for every other file.
    fn inline(&self, threshold: usize) -> Option<(String, &'static str)> {
        let format = match self.content_type.as_str() {
            "text/markdown" => "markdown",
            "text/plain" => "text",
            _ => return None,
        };
        if self.data.len() >= threshold || self.data.contains(&0) {
            return None;
        }
        let text = std::str::from_utf8(&self.data).ok()?;
        Some((text.to_owned(), format))
    }
}

/// A file's media type by its name's extension, case-insensitive.
fn type_by_name(name: &str) -> Option<&'static str> {
    let (_, ext) = name.rsplit_once('.')?;
    Some(match ext.to_ascii_lowercase().as_str() {
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "pdf" => "application/pdf",
        "docx" => DOCUMENT_TYPES[3],
        "pptx" => DOCUMENT_TYPES[4],
        "xlsx" => DOCUMENT_TYPES[5],
        "html" | "htm" => "text/html",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "tif" | "tiff" => "image/tiff",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// A file's media type by its first bytes: PDF, PNG, JPEG, TIFF or WebP.
fn type_by_bytes(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        Some("image/tiff")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The presign's `400` for a file, before anything is sent.
fn file_error(index: usize, message: String) -> Error {
    usage_error(format!("files[{index}].{message}"))
}

fn empty_file(index: usize) -> Error {
    file_error(index, format!("byte_size: must be an integer from 1 to {MAX_DOCUMENT_BYTES}."))
}

fn too_large(index: usize, size: u64) -> Error {
    file_error(index, format!("byte_size: {size} is over the limit of {MAX_DOCUMENT_BYTES} bytes."))
}

/// Reads at most one byte past the limit, so a larger stream is refused
/// without being read whole.
fn read_limited(reader: impl std::io::Read) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut data = Vec::new();
    reader
        .take(MAX_DOCUMENT_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| Error::invalid(format!("the document could not be read: {e}")).with_source(e))?;
    Ok(data)
}

/// [`read_limited`] off the async runtime's threads.
async fn read_blocking(reader: impl std::io::Read + Send + 'static) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || read_limited(reader))
        .await
        .map_err(|e| Error::invalid(format!("the document could not be read: {e}")))?
}

impl Document {
    /// Reads the document and settles its media type; the presign's checks
    /// (the type, an empty file, the size, in the API's order) are made
    /// here, numbered `index`. A file is read to one byte past the limit at
    /// most.
    async fn load(self, index: usize) -> Result<Loaded> {
        let Document { source, format, content_type, filename } = self;
        if format.is_some() && !matches!(source, Source::Text(_)) {
            return Err(Error::invalid("format goes with a text document; a file's format follows its type"));
        }
        // The bytes, and the size when it is over the limit.
        let (data, size, filename) = match source {
            Source::Text(text) => return Ok(Loaded::Text { text, format }),
            Source::UploadId(id) => return Ok(Loaded::UploadId(id)),
            Source::Bytes(b) => {
                let size = b.len() as u64;
                (b, size, filename)
            }
            Source::Path(path) => {
                let name = filename.or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()));
                let unreadable = |e: std::io::Error| {
                    Error::invalid(format!("the document {} could not be read: {e}", path.display())).with_source(e)
                };
                let file = std::fs::File::open(&path).map_err(unreadable)?;
                let size = file.metadata().map_err(unreadable)?.len();
                let data = read_blocking(file).await?;
                (Bytes::from(data), size, name)
            }
            Source::Reader(reader) => {
                let data = read_blocking(reader).await?;
                let size = data.len() as u64;
                (Bytes::from(data), size, filename)
            }
            Source::AsyncReader(reader) => {
                use tokio::io::AsyncReadExt;
                let mut data = Vec::new();
                reader
                    .take(MAX_DOCUMENT_BYTES as u64 + 1)
                    .read_to_end(&mut data)
                    .await
                    .map_err(|e| Error::invalid(format!("the document could not be read: {e}")).with_source(e))?;
                let size = data.len() as u64;
                (Bytes::from(data), size, filename)
            }
        };
        let content_type = match content_type {
            Some(t) => t,
            None => match filename.as_deref().and_then(type_by_name).or_else(|| type_by_bytes(&data)) {
                Some(t) => t.to_owned(),
                None => {
                    return Err(Error::invalid(
                        "the document's type cannot be told from its file name or its first bytes: \
                         give its content_type or filename",
                    ))
                }
            },
        };
        if !DOCUMENT_TYPES.contains(&content_type.as_str()) {
            let types = DOCUMENT_TYPES.join(", ");
            return Err(file_error(index, format!("content_type: must be one of {types}, got {content_type}.")));
        }
        if data.is_empty() {
            return Err(empty_file(index));
        }
        if size > MAX_DOCUMENT_BYTES as u64 || data.len() > MAX_DOCUMENT_BYTES {
            return Err(too_large(index, size.max(data.len() as u64)));
        }
        let sha256 = sha2::Sha256::digest(&data).into();
        Ok(Loaded::File(File { data, content_type, filename, sha256 }))
    }

    /// The document as a file to upload, a text document by its format
    /// (`text/markdown`, or `text/plain` for `text`).
    async fn load_file(self, index: usize) -> Result<Option<File>> {
        match self.load(index).await? {
            Loaded::File(f) => Ok(Some(f)),
            Loaded::UploadId(_) => Ok(None),
            Loaded::Text { text, format } => {
                let content_type = if format.as_deref() == Some("text") { "text/plain" } else { "text/markdown" };
                if text.is_empty() {
                    return Err(empty_file(index));
                }
                if text.len() > MAX_DOCUMENT_BYTES {
                    return Err(too_large(index, text.len() as u64));
                }
                let data = Bytes::from(text);
                let sha256 = sha2::Sha256::digest(&data).into();
                Ok(Some(File { data, content_type: content_type.to_owned(), filename: None, sha256 }))
            }
        }
    }
}

/// The fields of a document beside its content, for
/// [`Collections::add_document`] and [`Collections::replace_document`].
/// Each is sent only when set.
///
/// ```
/// use decisionnode::DocumentOptions;
/// use serde_json::json;
///
/// let options = DocumentOptions::new()
///     .title("Mission M-42 brief")
///     .reference("m42-brief")
///     .metadata(json!({"mission": "M-42"}))
///     .wait(true);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
#[must_use]
pub struct DocumentOptions {
    title: Option<String>,
    reference: Option<String>,
    pinned: Option<bool>,
    pin_rank: Option<i64>,
    label: Option<String>,
    metadata: Option<Value>,
    wait: bool,
}

impl DocumentOptions {
    /// No field set.
    pub fn new() -> DocumentOptions {
        DocumentOptions::default()
    }

    /// The title, 1 to 200 characters. A text document needs one; a file's
    /// default is its file name without the extension.
    pub fn title(mut self, title: impl Into<String>) -> DocumentOptions {
        self.title = Some(title.into());
        self
    }

    /// A reference, 1 to 64 of `A-Z`, `a-z`, `0-9`, `_`, `.`, `:` and `-`,
    /// unique in the knowledge base. Without one, the item's id is its
    /// reference.
    pub fn reference(mut self, reference: impl Into<String>) -> DocumentOptions {
        self.reference = Some(reference.into());
        self
    }

    /// `true`: always given to a decision that names the knowledge base, for
    /// a rule that must always apply.
    pub fn pinned(mut self, pinned: bool) -> DocumentOptions {
        self.pinned = Some(pinned);
        self
    }

    /// The order of pinned items, 0 to 1,000, lowest first.
    pub fn pin_rank(mut self, pin_rank: i64) -> DocumentOptions {
        self.pin_rank = Some(pin_rank);
        self
    }

    /// A label, 1 to 128 characters.
    pub fn label(mut self, label: impl Into<String>) -> DocumentOptions {
        self.label = Some(label.into());
        self
    }

    /// Metadata, an object as [`crate::ItemUpdate::metadata`] takes. On a
    /// knowledge base with a scope key, the key's value scopes the document;
    /// without it the document is global.
    pub fn metadata(mut self, metadata: impl Into<Value>) -> DocumentOptions {
        self.metadata = Some(metadata.into());
        self
    }

    /// `true`: the server holds the call up to 10 seconds and answers once
    /// the document is ready, else as it stands. Such a call waits
    /// [`crate::DEFAULT_WAIT_TIMEOUT`] (20 seconds) per attempt unless the
    /// client's timeout was set.
    pub fn wait(mut self, wait: bool) -> DocumentOptions {
        self.wait = wait;
        self
    }
}

/// A title's `422` entry: 1 to 200 characters once trimmed.
pub(crate) fn title_entries(title: &str) -> Vec<Value> {
    let length = title.trim().chars().count();
    if (1..=TITLE_MAX).contains(&length) {
        return Vec::new();
    }
    vec![entry(
        "string_too_long",
        &[json!("body"), json!("title")],
        "A title is 1 to 200 characters",
        &echoed(title),
        None,
    )]
}

/// A `pin_rank`'s `422` entry: 0 to 1,000.
pub(crate) fn pin_rank_entries(pin_rank: i64) -> Vec<Value> {
    if (0..=PIN_RANK_MAX).contains(&pin_rank) {
        return Vec::new();
    }
    let msg = "Input should be an integer from 0 to 1000";
    vec![entry("int_range", &[json!("body"), json!("pin_rank")], msg, &json!(pin_rank), None)]
}

/// A body's fields beside its content, and their `422` entries.
fn fields(options: &DocumentOptions, body: &mut Map<String, Value>, entries: &mut Vec<Value>) {
    if let Some(r) = &options.reference {
        entries.extend(string_entries("reference", r, REFERENCE_MAX, true));
        body.insert("reference".into(), Value::String(r.clone()));
    }
    if let Some(p) = options.pinned {
        body.insert("pinned".into(), Value::Bool(p));
    }
    if let Some(rank) = options.pin_rank {
        entries.extend(pin_rank_entries(rank));
        body.insert("pin_rank".into(), json!(rank));
    }
    if let Some(l) = &options.label {
        entries.extend(string_entries("label", l, LABEL_MAX, false));
        body.insert("label".into(), Value::String(l.clone()));
    }
    if let Some(m) = &options.metadata {
        entries.extend(metadata_entries(m));
        body.insert("metadata".into(), m.clone());
    }
}

/// How a document goes in the item call.
enum Content {
    /// The body is ready: text inline, or a caller's upload id.
    Body(Map<String, Value>),
    /// A file to upload first; the body takes its upload id.
    Upload(File, Map<String, Value>),
}

/// The item body of a document, checked as the API checks it.
fn item_body(loaded: Loaded, options: &DocumentOptions, threshold: usize) -> Result<Content> {
    let mut entries = Vec::new();
    let mut body = Map::new();
    let (content, title) = match loaded {
        Loaded::Text { text, format } => {
            if text.is_empty() {
                let at = [json!("body"), json!("text")];
                entries.push(entry("string_too_short", &at, "The text is empty", &json!(""), None));
            }
            if let Some(f) = format.as_deref().filter(|f| !matches!(*f, "markdown" | "text")) {
                let msg = "Input should be 'markdown' or 'text'";
                entries.push(entry("literal_error", &[json!("body"), json!("format")], msg, &echoed(f), None));
            }
            (Some((text, format)), options.title.clone())
        }
        Loaded::File(file) => match file.inline(threshold) {
            Some((text, format)) => {
                let title = options.title.clone().or_else(|| file.stem());
                (Some((text, Some(format.to_owned()))), title)
            }
            None => {
                let mut body = Map::new();
                let mut entries = Vec::new();
                if let Some(t) = &options.title {
                    entries.extend(title_entries(t));
                    body.insert("title".into(), Value::String(t.clone()));
                }
                fields(options, &mut body, &mut entries);
                if !entries.is_empty() {
                    return Err(Error::local_entries(entries));
                }
                return Ok(Content::Upload(file, body));
            }
        },
        Loaded::UploadId(id) => {
            body.insert("upload_id".into(), Value::String(id));
            if let Some(t) = &options.title {
                entries.extend(title_entries(t));
                body.insert("title".into(), Value::String(t.clone()));
            }
            fields(options, &mut body, &mut entries);
            if !entries.is_empty() {
                return Err(Error::local_entries(entries));
            }
            return Ok(Content::Body(body));
        }
    };
    match &title {
        Some(t) => entries.splice(0..0, title_entries(t)).for_each(drop),
        None => {
            entries.insert(0, entry("missing", &[json!("body"), json!("title")], "Field required", &Value::Null, None))
        }
    }
    if let Some(t) = title {
        body.insert("title".into(), Value::String(t));
    }
    if let Some((text, format)) = content {
        body.insert("text".into(), Value::String(text));
        if let Some(f) = format {
            body.insert("format".into(), Value::String(f));
        }
    }
    fields(options, &mut body, &mut entries);
    if !entries.is_empty() {
        return Err(Error::local_entries(entries));
    }
    Ok(Content::Body(body))
}

// --------------------------------------------------------------------------- search

/// A search of a knowledge base without a decision, for
/// [`Collections::search`]. Every option is sent only when set, as given,
/// and checked by the API.
///
/// ```
/// use decisionnode::Search;
///
/// let search = Search::new("receipts for claims over 500 SEK").scope("M-42").top_k(6);
/// ```
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub struct Search {
    body: Map<String, Value>,
    /// An option that could not be serialised, an argument error when sent.
    invalid: Option<String>,
}

impl Search {
    /// A search for this query.
    pub fn new(query: impl Into<String>) -> Search {
        let mut body = Map::new();
        body.insert("query".into(), Value::String(query.into()));
        Search { body, invalid: None }
    }

    fn set(mut self, key: &str, value: Value) -> Search {
        self.body.insert(key.into(), value);
        self
    }

    fn set_json(mut self, key: &str, value: impl Serialize) -> Search {
        match serde_json::to_value(value) {
            Ok(v) => self.set(key, v),
            Err(e) => {
                self.invalid = Some(format!("the search option {key} cannot be serialised to JSON: {e}"));
                self
            }
        }
    }

    /// The value of the collection's scope key.
    pub fn scope(self, value: impl Into<String>) -> Search {
        self.set("scope", Value::String(value.into()))
    }

    /// Metadata equality, sent as `match`; see [`Knowledge::matching`].
    pub fn matching(self, matching: impl Serialize) -> Search {
        self.set_json("match", matching)
    }

    /// A typed filter tree, as given; see [`Knowledge::filter`].
    pub fn filter(self, filter: impl Serialize) -> Search {
        self.set_json("filter", filter)
    }

    /// The most retrieved passages, pins not counted.
    pub fn top_k(self, top_k: i64) -> Search {
        self.set("top_k", json!(top_k))
    }

    /// `false` leaves the pinned items out.
    pub fn pinned(self, pinned: bool) -> Search {
        self.set("pinned", Value::Bool(pinned))
    }

    /// The passages' token budget: `"auto"` or a number of tokens.
    pub fn max_tokens(self, max_tokens: impl Into<Value>) -> Search {
        self.set("max_tokens", max_tokens.into())
    }
}

// --------------------------------------------------------------------------- calls

/// The `400`s of a reference to an upload that can no longer be used.
fn is_stale(e: &Error) -> bool {
    e.kind() == ErrorKind::BadRequest && e.error_type().is_some_and(|t| STALE_UPLOAD.contains(&t))
}

impl Collections<'_> {
    /// Sends an item call with a document, uploading its file first when it
    /// has one. An upload this call placed that the API answers as stale is
    /// uploaded again and the call sent once more with a new
    /// `Idempotency-Key`; a second such answer is returned.
    async fn document_call(
        &self,
        method: Method,
        path: &str,
        document: Document,
        options: &DocumentOptions,
    ) -> Result<Value> {
        let client = self.client;
        let loaded = document.load(0).await?;
        let content = item_body(loaded, options, client.inner.inline_threshold)?;
        let path = if options.wait { format!("{path}?wait=true") } else { path.to_owned() };
        let policy = if options.wait { Policy::IdempotentWait } else { Policy::Idempotent };
        let send = |body: Value| {
            let (method, path) = (method.clone(), path.clone());
            async move {
                let key = crate::client::new_idempotency_key();
                client.request_json_keyed(method, &path, &body, &key, policy).await.map(|r: Reply| r.body)
            }
        };
        match content {
            Content::Body(body) => send(Value::Object(body)).await,
            Content::Upload(file, fields) => {
                let timeout = client.document_upload_timeout();
                let mut resent = false;
                loop {
                    let id = client.upload_files(&[file.upload()], timeout).await?.remove(0);
                    let mut body = Map::new();
                    body.insert("upload_id".into(), Value::String(id));
                    body.extend(fields.clone());
                    match send(Value::Object(body)).await {
                        Err(e) if !resent && is_stale(&e) => {
                            log::debug!(
                                "{path}: the document's upload is stale ({:?}); uploading again",
                                e.error_type()
                            );
                            resent = true;
                        }
                        other => return other,
                    }
                }
            }
        }
    }

    /// `POST /v1/collections/{id}/items` on a knowledge base: adds a
    /// document and returns the item as sent, `queued` (`202`), or with
    /// [`DocumentOptions::wait`] `ready` (`201`) or as it stands after the
    /// server's 10 seconds.
    ///
    /// Text goes inline. A text or Markdown file under the inline threshold
    /// that is UTF-8 without NUL goes inline too, titled by its file name
    /// without the extension unless a title is set. Every other file is
    /// uploaded first and named by its upload id. The call sends an
    /// `Idempotency-Key`, the same on every attempt, and retries as every
    /// call does. A title, reference, label, `pin_rank`, metadata, format or
    /// text the API would refuse, a file over 50 MB, an empty file and a
    /// file of a type the API does not take are refused before anything is
    /// sent, with the API's own error.
    pub async fn add_document(
        &self,
        collection_id: &str,
        document: Document,
        options: &DocumentOptions,
    ) -> Result<Value> {
        let path = format!("/v1/collections/{}/items", segment(collection_id));
        self.document_call(Method::POST, &path, document, options).await
    }

    /// `PUT /v1/collections/{id}/items/{item_id}`: replaces a document whole
    /// and returns the item as sent: `202` a new version, `200` when nothing
    /// changed or, with [`DocumentOptions::wait`], once it is ready. Sent and
    /// checked as [`Collections::add_document`].
    pub async fn replace_document(
        &self,
        collection_id: &str,
        item_id: &str,
        document: Document,
        options: &DocumentOptions,
    ) -> Result<Value> {
        let path = format!("/v1/collections/{}/items/{}", segment(collection_id), segment(item_id));
        self.document_call(Method::PUT, &path, document, options).await
    }

    /// `GET /v1/collections/{id}/items/{item_id}`: one item as sent. With
    /// `include_text`, a document's stored text too (`null` while nothing
    /// serves), which passages' byte offsets index.
    pub async fn get_item(&self, collection_id: &str, item_id: &str, include_text: bool) -> Result<Value> {
        let mut path = format!("/v1/collections/{}/items/{}", segment(collection_id), segment(item_id));
        if include_text {
            path.push_str("?include=text");
        }
        Ok(self.client.request_json(Method::GET, &path, None, Policy::Default).await?.body)
    }

    /// `GET /v1/collections/{id}/items/{item_id}/chunks`: one page of a
    /// document's live chunks, as sent. `offset` and `limit` are sent only
    /// when given and checked as [`Collections::list_items`] checks them.
    pub async fn list_chunks(
        &self,
        collection_id: &str,
        item_id: &str,
        offset: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Value> {
        let query = crate::collections::page_query(offset, limit)?;
        let mut path = format!("/v1/collections/{}/items/{}/chunks", segment(collection_id), segment(item_id));
        if !query.is_empty() {
            path = format!("{path}?{}", query.join("&"));
        }
        Ok(self.client.request_json(Method::GET, &path, None, Policy::Default).await?.body)
    }

    /// `DELETE /v1/collections/{id}/items?status=failed`: deletes every
    /// failed document of a knowledge base and returns the answer as sent,
    /// `{"deleted": n}`. It is idempotent, so it is retried as any call.
    pub async fn delete_failed(&self, collection_id: &str) -> Result<Value> {
        let path = format!("/v1/collections/{}/items?status=failed", segment(collection_id));
        Ok(self.client.request_json(Method::DELETE, &path, None, Policy::Default).await?.body)
    }

    /// `POST /v1/collections/{id}/search`: the passages a decision would be
    /// given for this query, each with its text. Billed as one knowledge
    /// call, and needs a full-scope key. Nothing in the search is checked
    /// here: the API answers a bad option with its `400`.
    pub async fn search(&self, collection_id: &str, search: &Search) -> Result<KnowledgeReading> {
        if let Some(e) = &search.invalid {
            return Err(Error::invalid(e.clone()));
        }
        let path = format!("/v1/collections/{}/search", segment(collection_id));
        let body = Value::Object(search.body.clone());
        let reply = self.client.request_json(Method::POST, &path, Some(&body), Policy::Default).await?;
        match reply.body.as_object().and_then(|o| KnowledgeReading::parse(o, &reply.body)) {
            Some(reading) => Ok(reading),
            None => Err(Error::new(ErrorKind::Server, "the response is not a valid knowledge reading")
                .with_response(reply.status, &reply.headers)),
        }
    }

    /// Waits until a document is `ready` or `failed` and returns the item,
    /// or with no `item_id` until no document of the knowledge base is
    /// queued or processing and returns the collection. See
    /// [`Collections::wait_until_ready_with`]; this waits at most
    /// [`DEFAULT_READY_TIMEOUT`] (5 minutes), first
    /// [`DEFAULT_READY_POLL_INTERVAL`] (1 second) between reads.
    pub async fn wait_until_ready(&self, collection_id: &str, item_id: Option<&str>) -> Result<Value> {
        self.wait_until_ready_with(collection_id, item_id, DEFAULT_READY_TIMEOUT, DEFAULT_READY_POLL_INTERVAL).await
    }

    /// [`Collections::wait_until_ready`] with its timeout and first wait.
    ///
    /// The first read is made at once. Each wait after it is 1.5 times the
    /// one before, up to 10 seconds, and never past `timeout`; each read is
    /// retried as any call, after the `Retry-After` of a `429` or `529`. A
    /// failed document is returned, not raised: its `error` says why. A
    /// status this version does not know is waited through. Past `timeout`,
    /// a deadline on the wall clock from the call,
    /// the call fails with [`ErrorKind::Timeout`]. A `poll_interval` below
    /// 100 ms or a zero `timeout` is an argument error, before anything is
    /// sent.
    pub async fn wait_until_ready_with(
        &self,
        collection_id: &str,
        item_id: Option<&str>,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Value> {
        if poll_interval < MIN_READY_POLL_INTERVAL {
            return Err(Error::invalid(format!(
                "poll_interval must be at least 0.1 seconds, got {:.3}",
                poll_interval.as_secs_f64()
            )));
        }
        if timeout.is_zero() {
            return Err(Error::invalid("timeout must be above 0"));
        }
        let path = match item_id {
            Some(item) => format!("/v1/collections/{}/items/{}", segment(collection_id), segment(item)),
            None => format!("/v1/collections/{}", segment(collection_id)),
        };
        let deadline = Instant::now() + timeout;
        let mut interval = poll_interval;
        loop {
            let reply = self.client.request_json(Method::GET, &path, None, Policy::Default).await?;
            let done = match item_id {
                Some(_) => matches!(reply.body.get("status").and_then(Value::as_str), Some("ready" | "failed")),
                None => {
                    let count =
                        |k: &str| reply.body.get("status_counts").and_then(|c| c.get(k)).and_then(Value::as_u64);
                    count("queued").unwrap_or(0) == 0 && count("processing").unwrap_or(0) == 0
                }
            };
            if done {
                return Ok(reply.body);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let what = item_id.map_or_else(|| format!("collection {collection_id}"), |i| format!("item {i}"));
                return Err(Error::new(
                    ErrorKind::Timeout,
                    format!("the {what} was not ready within {:.1} s", timeout.as_secs_f64()),
                ));
            }
            self.client.wait(interval.min(left)).await;
            interval = interval.mul_f64(1.5).min(MAX_READY_POLL_INTERVAL);
        }
    }
}

impl DecisionNode {
    /// Uploads 1 to 16 documents with the knowledge purpose, whatever their
    /// type and size, and returns one upload id each, in order, to add later
    /// as [`Document::upload_id`] within the hour. A text document is
    /// uploaded as `text/markdown`, or `text/plain` for the `text` format;
    /// a [`Document::upload_id`] is an argument error. One `POST
    /// /v1/uploads` for all of them, then the `PUT`s in parallel. Needs a
    /// full-scope key.
    pub async fn upload_documents(&self, documents: impl IntoIterator<Item = Document>) -> Result<Vec<String>> {
        let documents: Vec<Document> = documents.into_iter().collect();
        if documents.is_empty() || documents.len() > MAX_UPLOAD_FILES {
            return Err(Error::local(
                ErrorKind::BadRequest,
                Some("api_usage_error"),
                format!("files: 1 to {MAX_UPLOAD_FILES} files, got {}.", documents.len()),
            ));
        }
        let mut files = Vec::with_capacity(documents.len());
        for (i, document) in documents.into_iter().enumerate() {
            match document.load_file(i).await? {
                Some(file) => files.push(file),
                None => {
                    return Err(Error::invalid(format!(
                        "documents[{i}] is an upload id; upload_documents takes text and files"
                    )))
                }
            }
        }
        let uploads: Vec<UploadFile<'_>> = files.iter().map(File::upload).collect();
        self.upload_files(&uploads, self.document_upload_timeout()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_by_name_and_bytes() {
        assert_eq!(type_by_name("Policy.PDF"), Some("application/pdf"));
        assert_eq!(type_by_name("notes.markdown"), Some("text/markdown"));
        assert_eq!(type_by_name("scan.TIF"), Some("image/tiff"));
        assert_eq!(type_by_name("archive.zip"), None);
        assert_eq!(type_by_name("README"), None);
        assert_eq!(type_by_bytes(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(type_by_bytes(b"MM\0*rest"), Some("image/tiff"));
        assert_eq!(type_by_bytes(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(type_by_bytes(b"PK\x03\x04"), None);
    }

    #[test]
    fn the_booster_is_always_an_object() {
        let b: Booster = knowledge("col_1").into();
        assert_eq!(b.into_value().unwrap(), json!({"name": "knowledge", "options": {"collection": "col_1"}}));
        let b: Booster = knowledge("col_1").require(false).into();
        assert_eq!(b.into_value().unwrap(), json!({"name": "knowledge", "options": {"collection": "col_1"}}));
        struct Unserialisable;
        impl Serialize for Unserialisable {
            fn serialize<S: serde::Serializer>(&self, _: S) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("no"))
            }
        }
        let b: Booster = knowledge("col_1").filter(Unserialisable).into();
        assert!(b.into_value().unwrap_err().is_argument());
        let b: Booster = knowledge("col_1").scope("M-42").top_k(4).max_tokens("auto").require(true).into();
        assert_eq!(
            b.into_value().unwrap(),
            json!({"name": "knowledge", "require": true,
                   "options": {"collection": "col_1", "scope": "M-42", "top_k": 4, "max_tokens": "auto"}})
        );
    }

    #[test]
    fn readings() {
        let raw = json!({"collection": "col_1", "collection_name": null, "passages": [
            {"n": 1, "pinned": true, "item": "itm_1", "chunk": 0, "byte_start": 0, "byte_end": 10, "score": null}]});
        let r = KnowledgeReading::parse(raw.as_object().unwrap(), &raw).unwrap();
        assert_eq!((r.passages[0].score, r.collection_name.clone(), r.truncated), (None, None, None));
        let bad = json!({"collection": "col_1", "passages": [{"n": "1"}]});
        assert!(KnowledgeReading::parse(bad.as_object().unwrap(), &bad).is_none());
    }

    #[test]
    fn local_entries() {
        assert!(title_entries("  Policy ").is_empty());
        assert_eq!(title_entries("   ")[0]["type"], "string_too_long");
        assert_eq!(title_entries(&"t".repeat(201))[0]["input"], json!(format!("{}...", "t".repeat(64))));
        assert_eq!(pin_rank_entries(1001)[0]["type"], "int_range");
        assert!(pin_rank_entries(0).is_empty());
    }
}
