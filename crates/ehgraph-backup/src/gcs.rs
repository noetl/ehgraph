//! Google Cloud Storage backup target (register C19, noetl/ehgraph#8).
//!
//! Plain JSON API over HTTPS with a synchronous client (`ureq`), so it runs the
//! same in the standalone server and inside a host's blocking pool without a
//! second async runtime. Semantics match [`crate::object::FsStore`]:
//!
//! - **put-if-absent** is an upload with `ifGenerationMatch=0`. GCS answers 412 when
//!   an object already exists, which is reported as "already there". A retry after
//!   an ambiguous failure (the first attempt may have landed) is therefore safe.
//! - **put** is a single-request media upload. GCS replaces an object atomically:
//!   readers see the old or the new generation, never a mix.
//! - The ordering (immutable objects, then manifest and head, then `checkpoint.json`)
//!   is the caller's, in [`crate::Backup::ship`], and is the same on every target.
//!
//! # Credentials
//!
//! A [`TokenProvider`] supplies the bearer token:
//!
//! - [`MetadataToken`] (the default, `auth = "metadata"`): the GKE metadata server,
//!   i.e. **Workload Identity**. In a pod, that is the pod's Kubernetes service
//!   account's Google identity. An embedded instance inside noetl-server gets the
//!   same identity noetl-server's own `gcp_auth` uses, from the same metadata
//!   server, so there is no second credential to provision. A host that wants to
//!   share its own token cache instead passes a provider through
//!   `ehgraph::OpenOptions::backup_token`.
//! - [`EnvToken`] (`auth = "env:VAR"`): a bearer token read from an environment
//!   variable on every request (for example `gcloud auth print-access-token` in a
//!   local shell).
//! - [`NoAuth`] (`auth = "none"`): no header, for a local emulator.
//!
//! `GCE_METADATA_HOST` overrides the metadata host, as in Google's own libraries.

use std::fmt;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ehgraph_core::EhError;
use serde::Deserialize;

use crate::object::{check_key, ObjectStore};

pub const DEFAULT_ENDPOINT: &str = "https://storage.googleapis.com";
const METADATA_HOST: &str = "metadata.google.internal";
const ATTEMPTS: u32 = 4;

/// Supplies the `Authorization: Bearer` token for each request. `None` sends no
/// header.
pub trait TokenProvider: Send + Sync {
    fn bearer(&self) -> Result<Option<String>, EhError>;
}

/// No credentials (a local emulator).
#[derive(Debug, Default)]
pub struct NoAuth;

impl TokenProvider for NoAuth {
    fn bearer(&self) -> Result<Option<String>, EhError> {
        Ok(None)
    }
}

/// A token read from an environment variable on every request.
#[derive(Debug)]
pub struct EnvToken(pub String);

impl TokenProvider for EnvToken {
    fn bearer(&self) -> Result<Option<String>, EhError> {
        match std::env::var(&self.0) {
            Ok(token) if !token.trim().is_empty() => Ok(Some(token.trim().to_string())),
            _ => Err(EhError::unavailable(format!(
                "GCS backup auth: environment variable {} is not set",
                self.0
            ))),
        }
    }
}

/// The GKE/GCE metadata server (Workload Identity). Tokens are cached until a
/// minute before they expire.
pub struct MetadataToken {
    agent: ureq::Agent,
    url: String,
    cached: Mutex<Option<(String, Instant)>>,
}

impl fmt::Debug for MetadataToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetadataToken")
            .field("url", &self.url)
            .finish()
    }
}

impl Default for MetadataToken {
    fn default() -> Self {
        let host = std::env::var("GCE_METADATA_HOST").unwrap_or_else(|_| METADATA_HOST.into());
        Self::with_host(&host)
    }
}

impl MetadataToken {
    pub fn with_host(host: &str) -> Self {
        let base = if host.starts_with("http://") || host.starts_with("https://") {
            host.trim_end_matches('/').to_string()
        } else {
            format!("http://{host}")
        };
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(10))
                .build(),
            url: format!("{base}/computeMetadata/v1/instance/service-accounts/default/token"),
            cached: Mutex::new(None),
        }
    }
}

impl TokenProvider for MetadataToken {
    fn bearer(&self) -> Result<Option<String>, EhError> {
        let mut cached = self.cached.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((token, until)) = cached.as_ref() {
            if Instant::now() < *until {
                return Ok(Some(token.clone()));
            }
        }
        #[derive(Deserialize)]
        struct Reply {
            access_token: String,
            #[serde(default)]
            expires_in: u64,
        }
        let reply: Reply = self
            .agent
            .get(&self.url)
            .set("Metadata-Flavor", "Google")
            .call()
            .map_err(|err| {
                EhError::unavailable(format!(
                    "GCS backup auth: metadata server token request failed ({err}). On GKE this \
                     needs Workload Identity on the pod's service account; elsewhere set \
                     [backup] auth = \"env:VAR\""
                ))
            })
            .and_then(|resp| {
                serde_json::from_reader(resp.into_reader())
                    .map_err(|err| EhError::unavailable(format!("metadata token reply: {err}")))
            })?;
        let ttl = Duration::from_secs(reply.expires_in.saturating_sub(60).max(1));
        *cached = Some((reply.access_token.clone(), Instant::now() + ttl));
        Ok(Some(reply.access_token))
    }
}

/// `gs://bucket/prefix` split into its parts. The prefix has no leading or
/// trailing `/` and may be empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcsLocation {
    pub bucket: String,
    pub prefix: String,
}

impl GcsLocation {
    pub fn parse(url: &str) -> Result<Self, EhError> {
        let rest = url
            .strip_prefix("gs://")
            .ok_or_else(|| EhError::invalid(format!("{url:?} is not a gs:// URL")))?;
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        let valid_bucket = !bucket.is_empty()
            && bucket
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c));
        if !valid_bucket {
            return Err(EhError::invalid(format!(
                "{url:?}: bucket name {bucket:?} is not valid"
            )));
        }
        let prefix = prefix.trim_matches('/').to_string();
        if !prefix.is_empty() {
            check_key(&prefix)?;
        }
        Ok(Self {
            bucket: bucket.to_string(),
            prefix,
        })
    }

    fn object(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{key}", self.prefix)
        }
    }
}

impl fmt::Display for GcsLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.prefix.is_empty() {
            write!(f, "gs://{}", self.bucket)
        } else {
            write!(f, "gs://{}/{}", self.bucket, self.prefix)
        }
    }
}

/// A backup prefix in a GCS bucket.
pub struct GcsStore {
    location: GcsLocation,
    endpoint: String,
    agent: ureq::Agent,
    token: Arc<dyn TokenProvider>,
}

impl fmt::Debug for GcsStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcsStore")
            .field("location", &self.location.to_string())
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

enum Outcome {
    Ok(Box<ureq::Response>),
    Status(u16, String),
}

impl GcsStore {
    pub fn new(
        location: GcsLocation,
        endpoint: &str,
        timeout: Duration,
        token: Arc<dyn TokenProvider>,
    ) -> Self {
        Self {
            location,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            agent: ureq::AgentBuilder::new().timeout(timeout).build(),
            token,
        }
    }

    pub fn location(&self) -> &GcsLocation {
        &self.location
    }

    fn object_url(&self, object: &str) -> String {
        format!(
            "{}/storage/v1/b/{}/o/{}",
            self.endpoint,
            self.location.bucket,
            encode_segment(object)
        )
    }

    /// Send with retries on transport errors, 408, 429 and 5xx. Any other status is
    /// returned to the caller to interpret (404, 412, ...).
    fn send(
        &self,
        what: &str,
        build: impl Fn() -> ureq::Request,
        body: Option<&[u8]>,
    ) -> Result<Outcome, EhError> {
        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(200 << (attempt - 1)));
            }
            let mut request = build();
            if let Some(token) = self.token.bearer()? {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }
            let result = match body {
                Some(bytes) => request
                    .set("Content-Type", "application/octet-stream")
                    .send_bytes(bytes),
                None => request.call(),
            };
            match result {
                Ok(resp) => return Ok(Outcome::Ok(Box::new(resp))),
                Err(ureq::Error::Status(code, resp)) => {
                    let text = resp.into_string().unwrap_or_default();
                    if code == 408 || code == 429 || code >= 500 {
                        last = format!("HTTP {code}: {}", truncate(&text));
                        continue;
                    }
                    return Ok(Outcome::Status(code, text));
                }
                Err(ureq::Error::Transport(err)) => {
                    last = err.to_string();
                }
            }
        }
        Err(EhError::unavailable(format!(
            "GCS {what} in {} failed after {ATTEMPTS} attempts: {last}",
            self.location
        )))
    }

    fn upload(&self, key: &str, bytes: &[u8], if_absent: bool) -> Result<bool, EhError> {
        check_key(key)?;
        let object = self.location.object(key);
        let url = format!(
            "{}/upload/storage/v1/b/{}/o",
            self.endpoint, self.location.bucket
        );
        let outcome = self.send(
            &format!("upload of {key}"),
            || {
                let request = self
                    .agent
                    .post(&url)
                    .query("uploadType", "media")
                    .query("name", &object);
                if if_absent {
                    request.query("ifGenerationMatch", "0")
                } else {
                    request
                }
            },
            Some(bytes),
        )?;
        match outcome {
            Outcome::Ok(_) => Ok(true),
            Outcome::Status(412, _) if if_absent => Ok(false),
            Outcome::Status(code, text) => {
                Err(status_error("upload", key, &self.location, code, &text))
            }
        }
    }
}

impl ObjectStore for GcsStore {
    fn describe(&self) -> String {
        self.location.to_string()
    }

    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<bool, EhError> {
        self.upload(key, bytes, true)
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), EhError> {
        self.upload(key, bytes, false).map(|_| ())
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, EhError> {
        check_key(key)?;
        let url = self.object_url(&self.location.object(key));
        match self.send(
            &format!("download of {key}"),
            || self.agent.get(&url).query("alt", "media"),
            None,
        )? {
            Outcome::Ok(resp) => {
                let mut bytes = Vec::new();
                resp.into_reader()
                    .read_to_end(&mut bytes)
                    .map_err(|err| EhError::unavailable(format!("GCS read of {key}: {err}")))?;
                Ok(Some(bytes))
            }
            Outcome::Status(404, _) => Ok(None),
            Outcome::Status(code, text) => {
                Err(status_error("download", key, &self.location, code, &text))
            }
        }
    }

    fn exists(&self, key: &str) -> Result<bool, EhError> {
        check_key(key)?;
        let url = self.object_url(&self.location.object(key));
        match self.send(
            &format!("stat of {key}"),
            || self.agent.get(&url).query("fields", "name"),
            None,
        )? {
            Outcome::Ok(_) => Ok(true),
            Outcome::Status(404, _) => Ok(false),
            Outcome::Status(code, text) => {
                Err(status_error("stat", key, &self.location, code, &text))
            }
        }
    }

    fn list(&self) -> Result<Vec<String>, EhError> {
        #[derive(Deserialize)]
        struct Item {
            name: String,
        }
        #[derive(Deserialize)]
        struct Page {
            #[serde(default)]
            items: Vec<Item>,
            #[serde(default, rename = "nextPageToken")]
            next_page_token: Option<String>,
        }
        let url = format!("{}/storage/v1/b/{}/o", self.endpoint, self.location.bucket);
        let list_prefix = if self.location.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.location.prefix)
        };
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let outcome = self.send(
                "list",
                || {
                    let mut request = self
                        .agent
                        .get(&url)
                        .query("prefix", &list_prefix)
                        .query("fields", "items(name),nextPageToken");
                    if let Some(page) = &token {
                        request = request.query("pageToken", page);
                    }
                    request
                },
                None,
            )?;
            let page: Page = match outcome {
                Outcome::Ok(resp) => serde_json::from_reader(resp.into_reader())
                    .map_err(|err| EhError::unavailable(format!("GCS list reply: {err}")))?,
                Outcome::Status(code, text) => {
                    return Err(status_error("list", "", &self.location, code, &text))
                }
            };
            for item in page.items {
                if let Some(key) = item.name.strip_prefix(&list_prefix) {
                    if !key.is_empty() && !key.ends_with('/') {
                        keys.push(key.to_string());
                    }
                }
            }
            match page.next_page_token {
                Some(next) if !next.is_empty() => token = Some(next),
                _ => break,
            }
        }
        keys.sort();
        Ok(keys)
    }
}

fn status_error(op: &str, key: &str, location: &GcsLocation, code: u16, text: &str) -> EhError {
    let message = format!(
        "GCS {op} of {key:?} in {location}: HTTP {code}: {}",
        truncate(text)
    );
    match code {
        401 | 403 => EhError::unauthorized(format!(
            "{message}. The backup identity needs storage.objects.create/get/list on the bucket"
        )),
        404 => EhError::not_found(message),
        _ => EhError::unavailable(message),
    }
}

fn truncate(text: &str) -> String {
    let text = text.trim();
    if text.len() > 300 {
        let mut end = 300;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &text[..end])
    } else {
        text.to_string()
    }
}

/// Percent-encode one URL path segment (an object name, `/` included).
fn encode_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gs_urls() {
        let loc = GcsLocation::parse("gs://noetl-ehgraph/ehgraph-network/").unwrap();
        assert_eq!(loc.bucket, "noetl-ehgraph");
        assert_eq!(loc.prefix, "ehgraph-network");
        assert_eq!(
            loc.object("checkpoint.json"),
            "ehgraph-network/checkpoint.json"
        );
        assert_eq!(loc.to_string(), "gs://noetl-ehgraph/ehgraph-network");
        let bare = GcsLocation::parse("gs://b").unwrap();
        assert_eq!(bare.object("x"), "x");
        for bad in ["s3://b/p", "gs://", "gs://Bad/p", "gs://b/../p"] {
            assert!(GcsLocation::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn encodes_object_names_as_one_segment() {
        assert_eq!(
            encode_segment("p/namespaces/lab/head.json"),
            "p%2Fnamespaces%2Flab%2Fhead.json"
        );
        assert_eq!(encode_segment("a b"), "a%20b");
    }

    #[test]
    fn env_token_requires_the_variable() {
        let missing = EnvToken("EHGRAPH_TEST_TOKEN_THAT_IS_NOT_SET".into());
        assert!(missing.bearer().is_err());
    }
}
