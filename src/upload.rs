//! Image uploads: `POST /v1/uploads`, the storage `PUT`s, and the client's
//! cache of upload ids by content hash.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use bytes::Bytes;
use futures_util::future::try_join_all;
use indexmap::IndexMap;
use md5::Digest;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::client::{backoff, encoded_len, is_loopback, transport_error, DecisionNode, Policy};
use crate::error::{parse_rfc3339, Error, ErrorKind, Result};
use crate::image::{check_images_without_ids, Image, Local};

/// The most entries the cache holds; the least recently used goes first.
const CACHE_ENTRIES: usize = 256;
/// An entry is used until this long before the upload's `expires_at`.
const CACHE_MARGIN: Duration = Duration::from_secs(300);
/// The most files one `POST /v1/uploads` takes.
pub(crate) const MAX_UPLOAD_FILES: usize = 16;
/// Stands for an upload id not issued yet, at the length of a real one, when
/// the size of a body is worked out before anything is uploaded.
const UPLOAD_ID_PLACEHOLDER: &str = "upl_00000000000000000000000000000000";

/// The SHA-256 of the bytes and the media type.
type Key = ([u8; 32], String);

struct Entry {
    upload_id: String,
    expires_at: SystemTime,
}

/// Upload ids by content hash, in the client's memory only, never shared
/// between clients and never written to disk.
#[derive(Default)]
pub(crate) struct UploadCache {
    entries: Mutex<IndexMap<Key, Entry>>,
    /// One lock per hash while it is being uploaded, so requests holding the
    /// same bytes at the same time share one upload.
    in_flight: Mutex<HashMap<Key, Weak<tokio::sync::Mutex<()>>>>,
}

impl UploadCache {
    fn is_empty(&self) -> bool {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// The live upload id of these bytes, if any. An entry within 5 minutes of
    /// its `expires_at` is dropped.
    fn get(&self, key: &Key, now: SystemTime) -> Option<String> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let index = entries.get_index_of(key)?;
        let usable = entries[index].expires_at.checked_sub(CACHE_MARGIN).is_some_and(|t| now < t);
        if !usable {
            entries.shift_remove_index(index);
            return None;
        }
        // Most recently used last.
        let last = entries.len() - 1;
        entries.move_index(index, last);
        Some(entries[last].upload_id.clone())
    }

    /// [`UploadCache::get`] without touching the entries: the live upload id of
    /// these bytes, if any.
    fn peek(&self, key: &Key, now: SystemTime) -> Option<String> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(key)?;
        let usable = entry.expires_at.checked_sub(CACHE_MARGIN).is_some_and(|t| now < t);
        usable.then(|| entry.upload_id.clone())
    }

    fn put(&self, key: Key, upload_id: String, expires_at: SystemTime) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.shift_remove(&key);
        entries.insert(key, Entry { upload_id, expires_at });
        while entries.len() > CACHE_ENTRIES {
            entries.shift_remove_index(0);
        }
    }

    /// Drops the entries of these upload ids.
    pub(crate) fn forget(&self, upload_ids: &[String]) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, e| !upload_ids.contains(&e.upload_id));
    }

    fn lock_for(&self, key: &Key) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lock) = map.get(key).and_then(Weak::upgrade) {
            return lock;
        }
        map.retain(|_, w| w.strong_count() > 0);
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        map.insert(key.clone(), Arc::downgrade(&lock));
        lock
    }
}

fn key_of(local: &Local) -> Key {
    (*local.sha256(), local.media_type().to_owned())
}

/// The images of a request in their wire form, and the upload ids the client
/// placed (cached or new), for a resend when one turns out stale.
pub(crate) struct Placed {
    pub(crate) wires: Vec<Value>,
    pub(crate) uploaded: Vec<String>,
}

/// One file of a presign: its bytes, media type and SHA-256, and for a
/// knowledge upload its purpose and file name.
pub(crate) struct UploadFile<'a> {
    pub(crate) data: &'a Bytes,
    pub(crate) content_type: &'a str,
    pub(crate) sha256: [u8; 32],
    pub(crate) purpose: Option<&'static str>,
    pub(crate) filename: Option<&'a str>,
}

impl<'a> UploadFile<'a> {
    fn image(local: &'a Local) -> UploadFile<'a> {
        UploadFile {
            data: &local.data,
            content_type: local.media_type(),
            sha256: *local.sha256(),
            purpose: None,
            filename: None,
        }
    }
}

/// One entry of the presign answer.
struct Presigned {
    upload_id: String,
    url: String,
    headers: HeaderMap,
    expires_at: Option<SystemTime>,
}

impl DecisionNode {
    /// Chooses each image's wire form. URLs and upload ids go as given. Local
    /// bytes with a live cache entry go by their upload id, whatever their
    /// size. The other local bytes go inline when all the local images
    /// together total under the inline threshold, and are uploaded (one
    /// presign, the `PUT`s in parallel) at or above it, or always with
    /// `always_upload`.
    pub(crate) async fn place_images(&self, images: &[Image], with_id: bool, always_upload: bool) -> Result<Placed> {
        let mut wires: Vec<Option<Value>> = vec![None; images.len()];
        let mut uploaded = Vec::new();
        let mut pending = Vec::new();
        let cache = &self.inner.uploads;
        let cache_empty = cache.is_empty();
        let mut total = 0usize;
        for (i, image) in images.iter().enumerate() {
            let Some(local) = image.as_local() else {
                wires[i] = Some(image.wire(with_id));
                continue;
            };
            total += local.data.len();
            if !cache_empty {
                if let Some(id) = cache.get(&key_of(local), self.now()) {
                    wires[i] = Some(image.wire_upload(&id, with_id));
                    uploaded.push(id);
                    continue;
                }
            }
            pending.push(i);
        }
        if !pending.is_empty() && (always_upload || total >= self.inner.inline_threshold) {
            let locals: Vec<&Arc<Local>> = pending.iter().filter_map(|&i| images[i].as_local()).collect();
            let ids = self.upload_locals(&locals).await?;
            for (&i, id) in pending.iter().zip(ids) {
                wires[i] = Some(images[i].wire_upload(&id, with_id));
                uploaded.push(id);
            }
        } else {
            for &i in &pending {
                wires[i] = Some(images[i].wire(with_id));
            }
        }
        Ok(Placed { wires: wires.into_iter().map(|w| w.unwrap_or(Value::Null)).collect(), uploaded })
    }

    /// The length of the encoded `images` array [`DecisionNode::place_images`]
    /// would build (with ids, not always uploading), worked out without
    /// uploading or encoding anything: an image it would upload is named by a
    /// placeholder id of a real id's length.
    pub(crate) fn projected_images_len(&self, images: &[Image]) -> Result<usize> {
        let cache = &self.inner.uploads;
        let cache_empty = cache.is_empty();
        let mut len = images.len().saturating_sub(1) + 2;
        let mut pending = Vec::new();
        let mut total = 0usize;
        for image in images {
            let Some(local) = image.as_local() else {
                len += encoded_len(&image.wire(true))?;
                continue;
            };
            total += local.data.len();
            match (!cache_empty).then(|| cache.peek(&key_of(local), self.now())).flatten() {
                Some(id) => len += encoded_len(&image.wire_upload(&id, true))?,
                None => pending.push((image, local)),
            }
        }
        let upload = total >= self.inner.inline_threshold;
        for (image, local) in pending {
            len += if upload {
                encoded_len(&image.wire_upload(UPLOAD_ID_PLACEHOLDER, true))?
            } else {
                image.inline_len(local)?
            };
        }
        Ok(len)
    }

    /// Uploads local images and returns one upload id per image, in order.
    ///
    /// Same bytes are uploaded once. While bytes are being uploaded, another
    /// request holding them waits for that upload and reuses its id.
    async fn upload_locals(&self, locals: &[&Arc<Local>]) -> Result<Vec<String>> {
        let cache = &self.inner.uploads;
        let keys: Vec<Key> = locals.iter().map(|l| key_of(l)).collect();
        let mut distinct: Vec<usize> = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            if !distinct.iter().any(|&j| keys[j] == *k) {
                distinct.push(i);
            }
        }
        // Lock in hash order, so two requests never wait on each other.
        let mut order = distinct.clone();
        order.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
        let locks: Vec<_> = order.iter().map(|&i| cache.lock_for(&keys[i])).collect();
        let mut guards = Vec::with_capacity(locks.len());
        for lock in &locks {
            guards.push(lock.lock().await);
        }

        let mut ids: HashMap<&Key, String> = HashMap::new();
        let mut todo = Vec::new();
        for &i in &distinct {
            match cache.get(&keys[i], self.now()) {
                Some(id) => {
                    ids.insert(&keys[i], id);
                }
                None => todo.push(i),
            }
        }
        if !todo.is_empty() {
            let group: Vec<UploadFile<'_>> = todo.iter().map(|&i| UploadFile::image(locals[i])).collect();
            let timeout = self.inner.upload_timeout;
            let (presigned, urls_expire) = self.presign(&group).await?;
            let presigned = try_join_all(
                group.iter().zip(presigned).map(|(file, p)| self.put_or_presign_again(p, file, urls_expire, timeout)),
            )
            .await?;
            for (&i, p) in todo.iter().zip(presigned) {
                // An upload whose expiry cannot be read is used once, not cached.
                if let Some(at) = p.expires_at {
                    cache.put(keys[i].clone(), p.upload_id.clone(), at);
                }
                ids.insert(&keys[i], p.upload_id);
            }
        }
        drop(guards);
        Ok(keys.iter().map(|k| ids[k].clone()).collect())
    }

    /// The `PUT` of one file, and when it is answered `403` once the URL's
    /// lifetime has passed by the client's clock, one new presign and `PUT`
    /// (a `403` inside the lifetime is a refusal and is raised). Returns the
    /// upload the bytes went to.
    async fn put_or_presign_again(
        &self,
        presigned: Presigned,
        file: &UploadFile<'_>,
        urls_expire: SystemTime,
        timeout: Duration,
    ) -> Result<Presigned> {
        match self.put(&presigned, file.data, timeout).await {
            Ok(()) => Ok(presigned),
            Err(e) if e.status() == Some(403) && self.now() >= urls_expire => {
                log::debug!("storage URL expired before its PUT; presigning again");
                let (again, _) = self.presign(std::slice::from_ref(file)).await?;
                let again = again
                    .into_iter()
                    .next()
                    .ok_or_else(|| Error::new(ErrorKind::Server, "the upload answer holds no upload for the file"))?;
                self.put(&again, file.data, timeout).await?;
                Ok(again)
            }
            Err(e) => Err(e),
        }
    }

    /// Uploads files with a purpose (the knowledge files of documents): one
    /// `POST /v1/uploads` for all of them, then one `PUT` each, in parallel,
    /// each attempt within `timeout`. Returns one upload id per file, in
    /// order. Nothing is cached: such an upload is consumed by the item it
    /// makes.
    pub(crate) async fn upload_files(&self, files: &[UploadFile<'_>], timeout: Duration) -> Result<Vec<String>> {
        let (presigned, urls_expire) = self.presign(files).await?;
        let placed = try_join_all(
            files.iter().zip(presigned).map(|(f, p)| self.put_or_presign_again(p, f, urls_expire, timeout)),
        )
        .await?;
        Ok(placed.into_iter().map(|p| p.upload_id).collect())
    }

    /// One `POST /v1/uploads` for these files. Returns the uploads, in order,
    /// and when their URLs stop taking a `PUT` by the client's clock
    /// (`expires_in`, 300 seconds, from the answer).
    async fn presign(&self, group: &[UploadFile<'_>]) -> Result<(Vec<Presigned>, SystemTime)> {
        let files: Vec<Value> = group
            .iter()
            .map(|f| {
                let mut file = Map::new();
                if let Some(purpose) = f.purpose {
                    file.insert("purpose".into(), json!(purpose));
                }
                file.insert("content_type".into(), json!(f.content_type));
                file.insert("byte_size".into(), json!(f.data.len()));
                file.insert(
                    "checksum".into(),
                    json!(base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(f.data))),
                );
                file.insert("sha256".into(), json!(base64::engine::general_purpose::STANDARD.encode(f.sha256)));
                if let Some(name) = f.filename {
                    file.insert("filename".into(), json!(name));
                }
                Value::Object(file)
            })
            .collect();
        let body = json!({ "files": files });
        let reply = self.request_json(Method::POST, "/v1/uploads", Some(&body), Policy::Default).await?;
        let lifetime = reply.body.get("expires_in").and_then(Value::as_f64).filter(|s| *s > 0.0).unwrap_or(300.0);
        let urls_expire = self.now() + Duration::from_secs_f64(lifetime.min(86_400.0));
        let invalid = || {
            Error::new(ErrorKind::Server, "the response is not a valid upload answer")
                .with_response(reply.status, &reply.headers)
        };
        let uploads = reply.body.get("uploads").and_then(Value::as_array).ok_or_else(invalid)?;
        uploads
            .iter()
            .map(|u| {
                let text = |k: &str| u.get(k).and_then(Value::as_str).map(str::to_owned);
                let mut headers = HeaderMap::new();
                for (name, value) in u.get("headers").and_then(Value::as_object).into_iter().flatten() {
                    let (Ok(n), Some(Ok(v))) =
                        (HeaderName::from_bytes(name.as_bytes()), value.as_str().map(HeaderValue::from_str))
                    else {
                        return Err(invalid());
                    };
                    headers.append(n, v);
                }
                Ok(Presigned {
                    upload_id: text("upload_id").ok_or_else(invalid)?,
                    url: text("url").ok_or_else(invalid)?,
                    headers,
                    expires_at: text("expires_at").as_deref().and_then(parse_rfc3339),
                })
            })
            .collect::<Result<Vec<_>>>()
            .and_then(|uploads| {
                if uploads.len() == group.len() {
                    Ok((uploads, urls_expire))
                } else {
                    Err(Error::new(
                        ErrorKind::Server,
                        format!("the upload answer holds {} uploads for {} files", uploads.len(), group.len()),
                    ))
                }
            })
    }

    /// The `PUT` of one file: to the returned URL with exactly the returned
    /// headers and the bytes. Never the key or the client's own headers, and
    /// never a redirect followed. Retried on 5xx and connection errors; a
    /// `412` on a retry means an earlier attempt stored the object, which is
    /// written once: success.
    async fn put(&self, presigned: &Presigned, data: &Bytes, timeout: Duration) -> Result<()> {
        let inner = &*self.inner;
        let url = reqwest::Url::parse(&presigned.url)
            .map_err(|e| Error::invalid(format!("the storage URL is not valid: {e}")).with_source(e))?;
        match url.scheme() {
            "https" => {}
            "http" if is_loopback(&url) => {}
            other => {
                return Err(Error::invalid(format!(
                    "the storage URL must use https, not {other}; nothing was sent to it"
                )))
            }
        }
        let mut attempt: u32 = 0;
        loop {
            let started = Instant::now();
            let rb = inner.http.put(url.clone()).timeout(timeout).headers(presigned.headers.clone()).body(data.clone());
            let attempt_future = async {
                let resp = rb.send().await.map_err(|e| transport_error(e, timeout))?;
                let status = resp.status().as_u16();
                let body = resp.bytes().await.map_err(|e| transport_error(e, timeout))?;
                Ok::<_, Error>((status, body))
            };
            let result = tokio::time::timeout(timeout, attempt_future).await.unwrap_or_else(|_| {
                Err(Error::new(ErrorKind::Timeout, format!("no response within {:.1} s", timeout.as_secs_f64())))
            });
            log::debug!(
                "PUT storage {} in {} ms (attempt {})",
                match &result {
                    Ok((status, _)) => status.to_string(),
                    Err(e) => e.kind().to_string(),
                },
                started.elapsed().as_millis(),
                attempt + 1
            );
            let more = attempt < inner.max_retries;
            match result {
                Err(e) if e.is_transport() && more => {}
                Err(e) => return Err(e),
                Ok((status, _)) if (200..=299).contains(&status) => return Ok(()),
                Ok((412, _)) if attempt > 0 => return Ok(()),
                Ok((status, _)) if (500..=599).contains(&status) && more => {}
                Ok((status, body)) => return Err(Error::upload_failed(status, &body)),
            }
            self.wait(backoff(attempt)).await;
            attempt += 1;
        }
    }

    /// Uploads images and returns one upload id per image, in order, to send
    /// later as [`Image::upload_id`] in any request, session frame or batch
    /// line of the same workspace for an hour.
    ///
    /// Local images are checked as in a request, then uploaded whatever their
    /// size: one `POST /v1/uploads` for all of them, then one `PUT` each, in
    /// parallel. Bytes the client uploaded before are not uploaded again: their
    /// cached id is returned. An [`Image::upload_id`] is returned as given. A
    /// URL image is an argument error, since the API fetches it itself.
    pub async fn upload(&self, images: impl IntoIterator<Item = Image>) -> Result<Vec<String>> {
        let images: Vec<Image> = images.into_iter().collect();
        if images.len() > MAX_UPLOAD_FILES {
            // One presign takes 1 to 16 files: its own 400, before anything is sent.
            return Err(Error::local(
                ErrorKind::BadRequest,
                Some("api_usage_error"),
                format!("files: 1 to {MAX_UPLOAD_FILES} files, got {}.", images.len()),
            ));
        }
        for image in &images {
            if image.is_url() {
                return Err(Error::invalid(format!(
                    "image {:?} is a URL; the API fetches URLs itself, there is nothing to upload",
                    image.id()
                )));
            }
        }
        check_images_without_ids(&images, &self.limits(&self.inner.model))?;
        let placed = self.place_images(&images, true, true).await?;
        Ok(placed
            .wires
            .iter()
            .map(|w| w.get("upload_id").and_then(Value::as_str).unwrap_or_default().to_owned())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> Key {
        ([n; 32], "image/png".to_owned())
    }

    #[test]
    fn cache_entries_live_until_five_minutes_before_expiry() {
        let cache = UploadCache::default();
        let now = SystemTime::now();
        cache.put(key(1), "upl_1".into(), now + Duration::from_secs(3600));
        cache.put(key(2), "upl_2".into(), now + Duration::from_secs(200));
        assert_eq!(cache.get(&key(1), SystemTime::now()).as_deref(), Some("upl_1"));
        assert_eq!(cache.get(&key(2), SystemTime::now()), None);
        assert_eq!(cache.entries.lock().unwrap().len(), 1);
        cache.forget(&["upl_1".to_owned()]);
        assert_eq!(cache.get(&key(1), SystemTime::now()), None);
    }

    #[test]
    fn cache_drops_the_least_recently_used() {
        let cache = UploadCache::default();
        let later = SystemTime::now() + Duration::from_secs(3600);
        for n in 0..=255u8 {
            cache.put(key(n), format!("upl_{n}"), later);
        }
        assert!(cache.get(&key(0), SystemTime::now()).is_some()); // now the most recently used
        cache.put(([9; 32], "image/jpeg".to_owned()), "upl_new".into(), later);
        assert_eq!(cache.entries.lock().unwrap().len(), CACHE_ENTRIES);
        assert!(cache.get(&key(0), SystemTime::now()).is_some());
        assert!(cache.get(&key(1), SystemTime::now()).is_none());
    }

    #[test]
    fn one_lock_per_hash_while_in_use() {
        let cache = UploadCache::default();
        let a = cache.lock_for(&key(1));
        let b = cache.lock_for(&key(1));
        assert!(Arc::ptr_eq(&a, &b));
        drop((a, b));
        let c = cache.lock_for(&key(2));
        assert_eq!(cache.in_flight.lock().unwrap().len(), 1);
        drop(c);
    }
}
