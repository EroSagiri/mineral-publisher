//! A Git LFS Batch API adapter for the private backup pipeline.
//!
//! Git LFS is how a backup moves the large binary objects its Git commit
//! references. The engine ([`mineral_core::backup`]) owns the facts and the order:
//! which objects a commit requires, that every byte stream is hashed while it is
//! sent, and that a Git ref only moves after every required object is present. This
//! module owns the protocol: the batch request, the Basic credential, the action
//! URLs the endpoint hands back, and the HTTP calls that follow them.
//!
//! Two safety properties shape the code:
//!
//! * **The bytes are the identity.** An upload streams the blob through a bounded
//!   read that folds every byte into a SHA-256 hasher and a counter. A `2xx` the
//!   endpoint returned is not success by itself: the stream must also hash to the
//!   requested oid and be exactly the requested size, so a damaged CAS blob can
//!   never become a remote object that claims to be it.
//! * **The batch answer is a closed set.** Every requested object must appear in the
//!   answer exactly once. An object the endpoint errored on is a failure naming that
//!   oid, and a missing or duplicated oid is malformed rather than something to
//!   interpret generously.
//!
//! # Why an upload spools through a file
//!
//! `reqwest::blocking::Body::sized` requires its reader to be `Send + 'static`, but
//! the engine hands this adapter a `&mut dyn ImmutableBlobSource`, which is neither:
//! [`ImmutableBlobSource`] deliberately carries no `Send` bound, and a borrow is not
//! `'static`. The bounded [`std::io::Read`] written here therefore fills a
//! length-delimited spool file, and the request streams from that file. The working
//! set stays bounded by [`LFS_UPLOAD_CHUNK_BYTES`], the object is never held whole,
//! and every byte is hashed and counted as it leaves the source.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use mineral_core::backup::{LfsRemote, LfsUpload, LfsUploadPlan, RequiredLfsObject};
use reqwest::{
    Method, Url,
    blocking::{Body, Client, RequestBuilder},
    header::{ACCEPT, CONTENT_TYPE},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::{domain::Sha256, publication::asset::ImmutableBlobSource};

/// The media type every Git LFS Batch API request and response uses.
pub const LFS_MEDIA_TYPE: &str = "application/vnd.git-lfs+json";

/// How many bytes are taken from an immutable blob in one bounded read.
///
/// This is the working-set bound: the source is never asked for more than this in
/// one call, so an arbitrarily large object is spooled and sent in fixed pieces.
pub const LFS_UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

/// The one batch operation this adapter performs.
const UPLOAD_OPERATION: &str = "upload";

/// The only transfer adapter this endpoint asks for.
const BASIC_TRANSFER: &str = "basic";

/// The path every Git LFS endpoint serves the batch API from.
const BATCH_PATH: &str = "objects/batch";

/// The exact body the LFS specification sends to a verify action.
const VERIFY_BODY: &str = "{}";

/// Distinguishes the temporary upload spools of concurrent uploads.
static NEXT_SPOOL: AtomicU64 = AtomicU64::new(1);

/// A Git LFS access token.
///
/// The inner value is never printed: a credential must not reach a log, a panic
/// message or an audit record.
#[derive(Clone)]
pub struct LfsToken(String);

impl LfsToken {
    pub fn new(value: impl Into<String>) -> Result<Self, LfsHttpConfigError> {
        let value = value.into();
        if value.is_empty() || value.contains(['\0', '\n', '\r']) {
            return Err(LfsHttpConfigError::InvalidToken);
        }
        Ok(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LfsToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LfsToken(<redacted>)")
    }
}

/// Why a Git LFS endpoint description is not usable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LfsHttpConfigError {
    /// The batch URL is not an absolute http(s) URL, or carries a query or fragment.
    InvalidUrl,
    /// The username is empty, holds a control character, or contains a colon.
    InvalidUsername,
    /// The token is empty or holds a control character.
    InvalidToken,
}

impl fmt::Display for LfsHttpConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl => formatter.write_str(
                "Git LFS batch URL must be an absolute http(s) URL without a query or fragment",
            ),
            Self::InvalidUsername => {
                formatter.write_str("Git LFS username must be non-empty and colon-free")
            }
            Self::InvalidToken => formatter.write_str("Git LFS token is invalid"),
        }
    }
}

impl Error for LfsHttpConfigError {}

/// Everything a runtime needs to reach one Git LFS endpoint.
///
/// This is runtime configuration: it lives in the host, is never handed to the
/// engine, and the engine's durable records never contain any of it.
#[derive(Clone)]
pub struct LfsHttpConfig {
    /// The endpoint root, e.g. `https://github.com/owner/repo.git/info/lfs`. The
    /// batch request is `{batch_url}/objects/batch`, so a trailing slash is
    /// normalized away rather than producing a doubled separator.
    batch_url: String,
    username: String,
    token: LfsToken,
    /// How long one HTTP request may take, including the streaming body.
    timeout: Duration,
}

impl LfsHttpConfig {
    pub fn new(
        batch_url: impl Into<String>,
        username: impl Into<String>,
        token: LfsToken,
        timeout: Duration,
    ) -> Result<Self, LfsHttpConfigError> {
        let batch_url = normalize_batch_url(&batch_url.into())?;
        let username = username.into();
        if username.is_empty() || username.contains(['\0', '\n', '\r', ':']) {
            return Err(LfsHttpConfigError::InvalidUsername);
        }
        Ok(Self {
            batch_url,
            username,
            token,
            timeout,
        })
    }

    pub fn batch_url(&self) -> &str {
        &self.batch_url
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn token(&self) -> &LfsToken {
        &self.token
    }

    /// Where this endpoint lives and who it is used as, for a report. It never
    /// contains the token.
    pub fn describe(&self) -> String {
        format!("Git LFS endpoint {} as {}", self.batch_url, self.username)
    }

    fn batch_endpoint(&self) -> String {
        format!("{}/{}", self.batch_url, BATCH_PATH)
    }
}

impl fmt::Debug for LfsHttpConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LfsHttpConfig")
            .field("batch_url", &self.batch_url)
            .field("username", &self.username)
            .field("token", &self.token)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Validates and normalizes the batch endpoint root.
///
/// Trailing slashes are removed because the batch path is appended; everything
/// else about the URL is kept exactly as the operator wrote it, and anything that
/// is not an absolute http(s) URL without a query or fragment is refused.
fn normalize_batch_url(batch_url: &str) -> Result<String, LfsHttpConfigError> {
    let trimmed = batch_url.trim_end_matches('/');
    let parsed = Url::parse(trimmed).map_err(|_| LfsHttpConfigError::InvalidUrl)?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(LfsHttpConfigError::InvalidUrl);
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(LfsHttpConfigError::InvalidUrl);
    }
    Ok(trimmed.to_owned())
}

/// One upload instruction the endpoint returned, opaque to the engine.
///
/// The engine carries this value from the batch answer to the upload call without
/// looking inside it: an href and headers are protocol, and the portable core must
/// never see a URL or a credential.
#[derive(Clone, Eq, PartialEq)]
pub struct LfsUploadAction {
    href: String,
    headers: Vec<(String, String)>,
}

impl LfsUploadAction {
    fn new(href: String, headers: Vec<(String, String)>) -> Self {
        Self { href, headers }
    }

    pub fn href(&self) -> &str {
        &self.href
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
}

impl fmt::Debug for LfsUploadAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LfsUploadAction")
            .field("href", &self.href)
            .field("headers", &RedactedHeaders(&self.headers))
            .finish()
    }
}

/// One verify instruction the endpoint returned, opaque to the engine.
#[derive(Clone, Eq, PartialEq)]
pub struct LfsVerifyAction {
    href: String,
    headers: Vec<(String, String)>,
}

impl LfsVerifyAction {
    fn new(href: String, headers: Vec<(String, String)>) -> Self {
        Self { href, headers }
    }

    pub fn href(&self) -> &str {
        &self.href
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
}

impl fmt::Debug for LfsVerifyAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LfsVerifyAction")
            .field("href", &self.href)
            .field("headers", &RedactedHeaders(&self.headers))
            .finish()
    }
}

/// Prints action headers without exposing a credential the endpoint handed back.
///
/// A batch answer routinely carries an `Authorization` header for the storage
/// backend behind the action URL. That is a credential too, so it is redacted even
/// though it is not this adapter's own token.
struct RedactedHeaders<'a>(&'a [(String, String)]);

impl fmt::Debug for RedactedHeaders<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = formatter.debug_map();
        for (name, value) in self.0 {
            if name.eq_ignore_ascii_case("authorization") {
                map.entry(name, &"<redacted>");
            } else {
                map.entry(name, value);
            }
        }
        map.finish()
    }
}

/// The Git LFS Batch API over blocking HTTP.
pub struct LfsHttpRemote {
    config: LfsHttpConfig,
    client: Client,
}

impl LfsHttpRemote {
    pub fn new(config: LfsHttpConfig) -> Result<Self, LfsHttpError> {
        let client = Client::builder()
            .timeout(config.timeout())
            .build()
            .map_err(LfsHttpError::Client)?;
        Ok(Self { config, client })
    }

    /// The endpoint and identity this remote talks to, for a report. It never
    /// contains the token.
    pub fn describe(&self) -> String {
        self.config.describe()
    }

    /// Builds a request for one action the endpoint handed back.
    ///
    /// The href is required to be an absolute http(s) URL: a batch answer that
    /// points anywhere else is an unusable instruction, not something to guess at.
    /// A malformed header is a request error, not a reason to drop the header.
    fn action_request(
        &self,
        method: Method,
        href: &str,
        headers: &[(String, String)],
    ) -> Result<RequestBuilder, LfsHttpError> {
        let url = Url::parse(href).map_err(|_| LfsHttpError::Endpoint)?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(LfsHttpError::Endpoint);
        }
        let mut builder = self.client.request(method, url);
        for (name, value) in headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        Ok(builder)
    }
}

impl LfsRemote for LfsHttpRemote {
    type Error = LfsHttpError;
    type UploadAction = LfsUploadAction;
    type VerifyAction = LfsVerifyAction;

    fn prepare_upload(
        &self,
        objects: &[RequiredLfsObject],
    ) -> Result<LfsUploadPlan<Self::UploadAction, Self::VerifyAction>, Self::Error> {
        let request = BatchRequest {
            operation: UPLOAD_OPERATION,
            transfers: [BASIC_TRANSFER],
            objects: objects
                .iter()
                .map(|object| BatchRequestObject {
                    oid: object.oid().to_string(),
                    size: object.size(),
                })
                .collect(),
        };
        let body = serde_json::to_vec(&request).map_err(LfsHttpError::Encode)?;
        let response = self
            .client
            .post(self.config.batch_endpoint())
            .header(ACCEPT, LFS_MEDIA_TYPE)
            .header(CONTENT_TYPE, LFS_MEDIA_TYPE)
            .basic_auth(self.config.username(), Some(self.config.token().expose()))
            .body(body)
            .send()
            .map_err(LfsHttpError::Transport)?;
        if !response.status().is_success() {
            return Err(LfsHttpError::UnexpectedStatus {
                operation: "POST batch",
                status: response.status().as_u16(),
            });
        }
        let bytes = response.bytes().map_err(LfsHttpError::Transport)?;
        let answer: BatchResponse =
            serde_json::from_slice(&bytes).map_err(|_| LfsHttpError::MalformedResponse)?;
        plan_from_answer(objects, &answer)
    }

    fn upload(
        &self,
        object: &RequiredLfsObject,
        source: &mut dyn ImmutableBlobSource,
        action: &Self::UploadAction,
    ) -> Result<(), Self::Error> {
        let spool = SpoolFile::create();
        let (file, digest, counted) = spool_object(source, spool.path())?;
        // The length is checked before the request: `Body::sized` promises the
        // endpoint exactly `object.size()` bytes, so a source that is not that
        // long must never be sent as if it were.
        if counted != object.size() {
            return Err(LfsHttpError::SizeMismatch {
                expected: object.size(),
                sent: counted,
            });
        }
        let request = self.action_request(Method::PUT, action.href(), action.headers())?;
        let response = request
            .body(Body::sized(file, object.size()))
            .send()
            .map_err(LfsHttpError::Transport)?;
        if !response.status().is_success() {
            return Err(LfsHttpError::UnexpectedStatus {
                operation: "PUT",
                status: response.status().as_u16(),
            });
        }
        // Only now, after the endpoint accepted the transfer, is the stream judged
        // against the object it claimed to be. The digest is of exactly the bytes
        // that were read from the source and written to the request.
        if digest != object.oid() {
            return Err(LfsHttpError::ContentMismatch {
                oid: object.oid(),
                actual: digest,
            });
        }
        Ok(())
    }

    fn verify(
        &self,
        _object: &RequiredLfsObject,
        action: &Self::VerifyAction,
    ) -> Result<(), Self::Error> {
        let request = self.action_request(Method::POST, action.href(), action.headers())?;
        let response = request
            .body(VERIFY_BODY)
            .send()
            .map_err(LfsHttpError::Transport)?;
        if !response.status().is_success() {
            return Err(LfsHttpError::UnexpectedStatus {
                operation: "POST verify",
                status: response.status().as_u16(),
            });
        }
        Ok(())
    }
}

/// Turns one batch answer into the plan the engine consumes.
///
/// The answer is checked as a closed set: it may not name fewer, more, or the same
/// oid twice as the request did, and an object's echoed size must agree with the
/// one that was asked about. Each object is then either already present, an upload
/// with an optional verify, or a failure naming the oid.
fn plan_from_answer(
    objects: &[RequiredLfsObject],
    answer: &BatchResponse,
) -> Result<LfsUploadPlan<LfsUploadAction, LfsVerifyAction>, LfsHttpError> {
    if answer.objects.len() != objects.len() {
        return Err(LfsHttpError::MalformedResponse);
    }
    let mut present = Vec::with_capacity(objects.len());
    let mut uploads = Vec::new();
    for object in objects {
        let oid = object.oid().to_string();
        let mut matches = answer
            .objects
            .iter()
            .filter(|candidate| candidate.oid == oid);
        let Some(entry) = matches.next() else {
            return Err(LfsHttpError::MalformedResponse);
        };
        if matches.next().is_some() {
            return Err(LfsHttpError::MalformedResponse);
        }
        if entry.size != object.size() {
            return Err(LfsHttpError::MalformedResponse);
        }
        if let Some(error) = &entry.error {
            return Err(LfsHttpError::ServerRefused {
                oid: object.oid(),
                code: error.code,
                message: error.message.clone(),
            });
        }
        let upload = entry
            .actions
            .as_ref()
            .and_then(|actions| actions.upload.as_ref());
        match upload {
            Some(action) => {
                let verify = entry
                    .actions
                    .as_ref()
                    .and_then(|actions| actions.verify.as_ref());
                uploads.push(LfsUpload::new(
                    *object,
                    LfsUploadAction::new(action.href.clone(), header_pairs(&action.header)),
                    verify.map(|action| {
                        LfsVerifyAction::new(action.href.clone(), header_pairs(&action.header))
                    }),
                ));
            }
            // No upload action means the endpoint already holds the object.
            None => present.push(*object),
        }
    }
    Ok(LfsUploadPlan::new(present, uploads))
}

/// Copies the endpoint's action headers into an owned list.
fn header_pairs(header: &Option<BTreeMap<String, String>>) -> Vec<(String, String)> {
    header
        .as_ref()
        .map(|header| {
            header
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// The request body of one batch call, as the LFS specification defines it.
#[derive(Serialize)]
struct BatchRequest {
    operation: &'static str,
    transfers: [&'static str; 1],
    objects: Vec<BatchRequestObject>,
}

/// One object named in a batch request.
///
/// The oid is serialized as the 64-character lowercase hex string the protocol
/// uses, not as the 32-byte array `Sha256` is transparent over.
#[derive(Serialize)]
struct BatchRequestObject {
    oid: String,
    size: u64,
}

/// The subset of a batch answer this adapter depends on.
#[derive(Deserialize)]
struct BatchResponse {
    #[serde(default)]
    objects: Vec<BatchResponseObject>,
}

#[derive(Deserialize)]
struct BatchResponseObject {
    oid: String,
    size: u64,
    #[serde(default)]
    actions: Option<BatchActions>,
    #[serde(default)]
    error: Option<BatchError>,
}

#[derive(Deserialize)]
struct BatchActions {
    #[serde(default)]
    upload: Option<BatchAction>,
    #[serde(default)]
    verify: Option<BatchAction>,
}

#[derive(Deserialize)]
struct BatchAction {
    href: String,
    #[serde(default)]
    header: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct BatchError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

/// Why one Git LFS protocol call failed.
#[derive(Debug)]
pub enum LfsHttpError {
    /// The HTTP client could not be built.
    Client(reqwest::Error),
    /// A request could not be sent or a response could not be read.
    Transport(reqwest::Error),
    /// The batch request could not be encoded.
    Encode(serde_json::Error),
    /// An endpoint call returned a status that is not a success.
    UnexpectedStatus {
        operation: &'static str,
        status: u16,
    },
    /// An action URL in a batch answer is not an absolute http(s) URL.
    Endpoint,
    /// Reading the backup blob failed.
    Read { source: io::Error },
    /// The bytes the source produced are not the object they claimed to be.
    ContentMismatch { oid: Sha256, actual: Sha256 },
    /// The source did not produce the object's declared size.
    SizeMismatch { expected: u64, sent: u64 },
    /// The endpoint refused to serve one requested object.
    ServerRefused {
        oid: Sha256,
        code: i64,
        message: String,
    },
    /// A batch answer does not describe the requested objects exactly once.
    MalformedResponse,
    /// A local file the spool needs could not be used.
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl LfsHttpError {
    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for LfsHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => {
                write!(
                    formatter,
                    "could not build the Git LFS HTTP client: {error}"
                )
            }
            Self::Transport(error) => write!(formatter, "Git LFS request failed: {error}"),
            Self::Encode(error) => {
                write!(
                    formatter,
                    "could not encode the Git LFS batch request: {error}"
                )
            }
            Self::UnexpectedStatus { operation, status } => {
                write!(formatter, "Git LFS {operation} returned status {status}")
            }
            Self::Endpoint => {
                formatter.write_str("Git LFS endpoint returned an unusable action URL")
            }
            Self::Read { source } => write!(formatter, "could not read the backup blob: {source}"),
            Self::ContentMismatch { oid, actual } => write!(
                formatter,
                "backup blob failed integrity verification: expected {oid}, got {actual}"
            ),
            Self::SizeMismatch { expected, sent } => write!(
                formatter,
                "backup blob is {sent} bytes, not the required {expected}"
            ),
            Self::ServerRefused { oid, code, message } => write!(
                formatter,
                "Git LFS endpoint refused object {oid}: {message} (code {code})"
            ),
            Self::MalformedResponse => formatter.write_str(
                "Git LFS batch response does not describe the requested objects exactly once",
            ),
            Self::Io {
                operation, path, ..
            } => write!(formatter, "could not {operation}: {}", path.display()),
        }
    }
}

impl Error for LfsHttpError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Client(error) | Self::Transport(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::Read { source } => Some(source),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// One temporary file the upload spool is written to, removed on every exit path.
struct SpoolFile {
    path: PathBuf,
}

impl SpoolFile {
    fn create() -> Self {
        let sequence = NEXT_SPOOL.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            ".mineral-lfs-{}-{sequence}.tmp",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SpoolFile {
    fn drop(&mut self) {
        // The spool is not a backup object and does not survive the upload.
        let _ = fs::remove_file(&self.path);
    }
}

/// Spools one blob, hashing and counting exactly the bytes read from the source.
///
/// The returned handle is rewound and ready to be the request body; the digest and
/// the count describe precisely the bytes that were written to it.
fn spool_object(
    source: &mut dyn ImmutableBlobSource,
    path: &Path,
) -> Result<(fs::File, Sha256, u64), LfsHttpError> {
    // Read *and* write: the same handle is written, rewound and then read as the
    // request body, so the spool never has to be reopened (and cannot be raced).
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|source| LfsHttpError::io("create the LFS upload spool", path, source))?;
    let mut reader = SourceReader::new(source);
    io::copy(&mut reader, &mut file).map_err(|source| LfsHttpError::Read { source })?;
    file.flush()
        .map_err(|source| LfsHttpError::io("flush the LFS upload spool", path, source))?;
    let (digest, counted) = reader.finish();
    file.seek(SeekFrom::Start(0))
        .map_err(|source| LfsHttpError::io("rewind the LFS upload spool", path, source))?;
    Ok((file, digest, counted))
}

/// A bounded [`Read`] over one immutable blob that hashes and counts its bytes.
///
/// The internal buffer is what bounds the working set: [`ImmutableBlobSource`] is
/// only ever asked for [`LFS_UPLOAD_CHUNK_BYTES`] at a time, whatever buffer the
/// caller of `read` offers, and only the bytes actually handed out are folded into
/// the digest.
struct SourceReader<'a> {
    source: &'a mut dyn ImmutableBlobSource,
    buffer: Vec<u8>,
    filled: usize,
    position: usize,
    hasher: Sha256Hasher,
    counted: u64,
}

impl<'a> SourceReader<'a> {
    fn new(source: &'a mut dyn ImmutableBlobSource) -> Self {
        Self {
            source,
            buffer: vec![0_u8; LFS_UPLOAD_CHUNK_BYTES],
            filled: 0,
            position: 0,
            hasher: Sha256Hasher::new(),
            counted: 0,
        }
    }

    /// The digest and count of the bytes that were handed out.
    fn finish(self) -> (Sha256, u64) {
        (Sha256::new(self.hasher.finalize().into()), self.counted)
    }

    fn refill(&mut self) -> io::Result<()> {
        if self.position < self.filled {
            return Ok(());
        }
        let read = self
            .source
            .read_chunk(&mut self.buffer)
            .map_err(io::Error::other)?;
        self.position = 0;
        self.filled = read;
        Ok(())
    }
}

impl Read for SourceReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        self.refill()?;
        if self.filled == 0 {
            return Ok(0);
        }
        let take = (self.filled - self.position).min(out.len());
        out[..take].copy_from_slice(&self.buffer[self.position..self.position + take]);
        self.position += take;
        self.hasher.update(&out[..take]);
        self.counted += take as u64;
        Ok(take)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;

    use crate::{ports::ContentStoreError, publication::asset::BufferedBlobSource};

    use super::*;

    const USERNAME: &str = "mineral";
    const TOKEN: &str = "test-lfs-token-that-must-never-be-printed";
    const BODY: &[u8] = b"the frozen bytes of one backup object";
    const BATCH_REQUEST_PATH: &str = "/info/lfs/objects/batch";

    /// One request exactly as it arrived on the socket.
    #[derive(Clone, Debug)]
    struct RecordedRequest {
        method: String,
        path: String,
        accept: Option<String>,
        content_type: Option<String>,
        authorization: Option<String>,
        content_length: Option<u64>,
        body: Vec<u8>,
    }

    /// An LFS endpoint that answers the batch API and follows the actions it hands
    /// back, remembering the raw bytes of every request.
    #[derive(Default)]
    struct FakeLfs {
        requests: Mutex<Vec<RecordedRequest>>,
        present: Mutex<HashSet<String>>,
        omit: Mutex<Option<String>>,
        duplicate: Mutex<Option<String>>,
        refuse: Mutex<Option<String>>,
        offer_verify: AtomicBool,
        batch_status: Mutex<Option<u16>>,
        upload_status: Mutex<Option<u16>>,
        verify_status: Mutex<Option<u16>>,
    }

    impl FakeLfs {
        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn requests_for(&self, method: &str, prefix: &str) -> Vec<RecordedRequest> {
            self.requests()
                .into_iter()
                .filter(|request| request.method == method && request.path.starts_with(prefix))
                .collect()
        }

        fn mark_present(&self, oid: Sha256) {
            self.present.lock().unwrap().insert(oid.to_string());
        }
    }

    /// A running fake endpoint, plus the batch URL that reaches it.
    struct FakeServer {
        lfs: Arc<FakeLfs>,
        batch_url: String,
    }

    impl FakeServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
            let address = listener.local_addr().expect("the bound address");
            let base_url = format!("http://{address}");
            let lfs = Arc::new(FakeLfs::default());
            let served = Arc::clone(&lfs);
            let served_base = base_url.clone();
            // The accept loop ends when the process does: every test binds its own
            // port and holds no state the thread needs released.
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    let lfs = Arc::clone(&served);
                    let _ = handle(stream, &lfs, &served_base);
                }
            });
            Self {
                lfs,
                batch_url: format!("{base_url}/info/lfs"),
            }
        }

        fn remote(&self) -> LfsHttpRemote {
            self.remote_with_timeout(Duration::from_secs(10))
        }

        fn remote_with_timeout(&self, timeout: Duration) -> LfsHttpRemote {
            let config = LfsHttpConfig::new(
                self.batch_url.clone(),
                USERNAME,
                LfsToken::new(TOKEN).unwrap(),
                timeout,
            )
            .unwrap();
            LfsHttpRemote::new(config).unwrap()
        }
    }

    fn handle(mut stream: TcpStream, lfs: &FakeLfs, base_url: &str) -> io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();

        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
            }
        }
        let header = |name: &str| {
            headers
                .iter()
                .find(|(other, _)| other == name)
                .map(|(_, value)| value.clone())
        };

        let length = header("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0_u8; length];
        if length > 0 {
            reader.read_exact(&mut body)?;
        }

        lfs.requests.lock().unwrap().push(RecordedRequest {
            method: method.clone(),
            path: path.clone(),
            accept: header("accept"),
            content_type: header("content-type"),
            authorization: header("authorization"),
            content_length: header("content-length").and_then(|value| value.parse().ok()),
            body: body.clone(),
        });

        let (status, content_type, response_body) =
            response_for(&method, &path, &body, lfs, base_url);
        let mut response = format!("HTTP/1.1 {status} {}\r\n", reason(status));
        if let Some(content_type) = content_type {
            response.push_str(&format!("content-type: {content_type}\r\n"));
        }
        response.push_str(&format!("content-length: {}\r\n", response_body.len()));
        response.push_str("connection: close\r\n\r\n");
        stream.write_all(response.as_bytes())?;
        stream.write_all(&response_body)?;
        stream.flush()
    }

    fn response_for(
        method: &str,
        path: &str,
        body: &[u8],
        lfs: &FakeLfs,
        base_url: &str,
    ) -> (u16, Option<String>, Vec<u8>) {
        if method == "POST" && path == BATCH_REQUEST_PATH {
            if let Some(status) = *lfs.batch_status.lock().unwrap() {
                return (status, None, Vec::new());
            }
            return (
                200,
                Some(LFS_MEDIA_TYPE.to_owned()),
                batch_response(body, lfs, base_url),
            );
        }
        if method == "PUT" && path.starts_with("/upload/") {
            return match *lfs.upload_status.lock().unwrap() {
                Some(status) => (status, None, Vec::new()),
                None => (200, None, Vec::new()),
            };
        }
        if method == "POST" && path.starts_with("/verify/") {
            return match *lfs.verify_status.lock().unwrap() {
                Some(status) => (status, None, Vec::new()),
                None => (200, None, Vec::new()),
            };
        }
        (404, None, Vec::new())
    }

    /// Builds the batch answer the request's objects describe.
    fn batch_response(body: &[u8], lfs: &FakeLfs, base_url: &str) -> Vec<u8> {
        let request: serde_json::Value = serde_json::from_slice(body).unwrap();
        let present = lfs.present.lock().unwrap().clone();
        let omit = lfs.omit.lock().unwrap().clone();
        let duplicate = lfs.duplicate.lock().unwrap().clone();
        let refuse = lfs.refuse.lock().unwrap().clone();
        let offer_verify = lfs.offer_verify.load(Ordering::Relaxed);
        let mut objects = Vec::new();
        for requested in request["objects"].as_array().unwrap() {
            let oid = requested["oid"].as_str().unwrap().to_owned();
            let size = requested["size"].as_u64().unwrap();
            if omit.as_deref() == Some(oid.as_str()) {
                continue;
            }
            let mut entry = json!({"oid": oid, "size": size});
            if refuse.as_deref() == Some(oid.as_str()) {
                entry["error"] = json!({"code": 404, "message": "object not found"});
            } else if !present.contains(&oid) {
                let mut actions = json!({
                    "upload": {
                        "href": format!("{base_url}/upload/{oid}"),
                        "header": {"content-type": "application/octet-stream"},
                    },
                });
                if offer_verify {
                    actions["verify"] = json!({
                        "href": format!("{base_url}/verify/{oid}"),
                        "header": {"accept": LFS_MEDIA_TYPE},
                    });
                }
                entry["actions"] = actions;
            }
            // A duplicated oid is one of the malformed answers a test injects.
            if duplicate.as_deref() == Some(oid.as_str()) {
                objects.push(entry.clone());
            }
            objects.push(entry);
        }
        serde_json::to_vec(&json!({"objects": objects})).unwrap()
    }

    fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            404 => "Not Found",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Status",
        }
    }

    fn object_for(bytes: &[u8]) -> RequiredLfsObject {
        RequiredLfsObject::new(Sha256::digest(bytes), bytes.len() as u64)
    }

    fn buffered(object: &RequiredLfsObject, bytes: &[u8]) -> BufferedBlobSource {
        BufferedBlobSource::new(object.oid(), bytes.to_vec())
    }

    fn basic_authorization() -> String {
        format!("Basic {}", STANDARD.encode(format!("{USERNAME}:{TOKEN}")))
    }

    /// Asks for a plan that the fake endpoint's answer cannot produce, so the error
    /// itself is what the test inspects.
    ///
    /// `LfsUploadPlan` deliberately has no `Debug`, so `unwrap_err` cannot be used.
    fn batch_error(remote: &LfsHttpRemote, object: &RequiredLfsObject) -> LfsHttpError {
        match remote.prepare_upload(std::slice::from_ref(object)) {
            Ok(_) => panic!("a batch answer that is not a closed set must fail"),
            Err(error) => error,
        }
    }

    /// A source that records every buffer it is asked to fill and refuses to be
    /// read in anything larger than one chunk.
    struct RecordingSource {
        identity: Sha256,
        bytes: Vec<u8>,
        position: usize,
        chunk_sizes: Arc<Mutex<Vec<usize>>>,
    }

    impl RecordingSource {
        fn new(identity: Sha256, bytes: Vec<u8>, chunk_sizes: Arc<Mutex<Vec<usize>>>) -> Self {
            Self {
                identity,
                bytes,
                position: 0,
                chunk_sizes,
            }
        }
    }

    impl ImmutableBlobSource for RecordingSource {
        fn identity(&self) -> Sha256 {
            self.identity
        }

        fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize, ContentStoreError> {
            assert!(
                buffer.len() <= LFS_UPLOAD_CHUNK_BYTES,
                "the source was asked for {} bytes at once",
                buffer.len()
            );
            self.chunk_sizes.lock().unwrap().push(buffer.len());
            let remaining = self.bytes.len().saturating_sub(self.position);
            let take = remaining.min(buffer.len());
            buffer[..take].copy_from_slice(&self.bytes[self.position..self.position + take]);
            self.position += take;
            Ok(take)
        }
    }

    #[test]
    fn a_batch_request_carries_the_media_types_credential_and_exact_objects() {
        let server = FakeServer::start();
        let object = object_for(BODY);
        server.lfs.mark_present(object.oid());
        let remote = server.remote();

        let plan = remote
            .prepare_upload(std::slice::from_ref(&object))
            .unwrap();

        assert!(plan.is_complete());
        assert_eq!(plan.present().len(), 1);
        assert_eq!(plan.present()[0], object);

        let requests = server.lfs.requests();
        assert_eq!(requests.len(), 1);
        let batch = &requests[0];
        assert_eq!(batch.method, "POST");
        assert_eq!(batch.path, BATCH_REQUEST_PATH);
        assert_eq!(batch.accept.as_deref(), Some(LFS_MEDIA_TYPE));
        assert_eq!(batch.content_type.as_deref(), Some(LFS_MEDIA_TYPE));
        assert_eq!(
            batch.authorization.as_deref(),
            Some(basic_authorization().as_str())
        );
        let expected = format!(
            "{{\"operation\":\"upload\",\"transfers\":[\"basic\"],\"objects\":[{{\"oid\":\"{}\",\"size\":{}}}]}}",
            object.oid(),
            object.size()
        );
        assert_eq!(batch.body, expected.as_bytes());
    }

    #[test]
    fn an_object_the_endpoint_already_holds_produces_no_upload_and_no_verify() {
        let server = FakeServer::start();
        server.lfs.offer_verify.store(true, Ordering::Relaxed);
        let object = object_for(BODY);
        server.lfs.mark_present(object.oid());
        let remote = server.remote();

        let plan = remote
            .prepare_upload(std::slice::from_ref(&object))
            .unwrap();

        assert!(plan.is_complete());
        assert!(plan.uploads().is_empty());
        assert_eq!(plan.present().len(), 1);
        assert!(server.lfs.requests_for("PUT", "/upload/").is_empty());
        assert!(server.lfs.requests_for("POST", "/verify/").is_empty());
    }

    #[test]
    fn a_missing_object_is_uploaded_with_the_exact_bytes_and_content_length() {
        let server = FakeServer::start();
        let object = object_for(BODY);
        let remote = server.remote();

        let plan = remote
            .prepare_upload(std::slice::from_ref(&object))
            .unwrap();
        assert_eq!(plan.uploads().len(), 1);
        let mut source = buffered(&object, BODY);
        remote
            .upload(&object, &mut source, plan.uploads()[0].action())
            .unwrap();

        let uploads = server.lfs.requests_for("PUT", "/upload/");
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].path, format!("/upload/{}", object.oid()));
        assert_eq!(uploads[0].content_length, Some(object.size()));
        assert_eq!(
            uploads[0].content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(uploads[0].body, BODY);
    }

    #[test]
    fn a_verify_action_is_called_once_with_an_empty_json_body() {
        let server = FakeServer::start();
        server.lfs.offer_verify.store(true, Ordering::Relaxed);
        let object = object_for(BODY);
        let remote = server.remote();

        let plan = remote
            .prepare_upload(std::slice::from_ref(&object))
            .unwrap();
        let upload = &plan.uploads()[0];
        let verify = upload
            .verify()
            .expect("the endpoint offered a verify action");
        remote
            .upload(&object, &mut buffered(&object, BODY), upload.action())
            .unwrap();
        remote.verify(&object, verify).unwrap();

        let verifications = server.lfs.requests_for("POST", "/verify/");
        assert_eq!(verifications.len(), 1);
        assert_eq!(verifications[0].path, format!("/verify/{}", object.oid()));
        assert_eq!(verifications[0].body, b"{}");
        assert_eq!(verifications[0].accept.as_deref(), Some(LFS_MEDIA_TYPE));
    }

    #[test]
    fn a_batch_answer_that_omits_a_requested_oid_fails_closed() {
        let server = FakeServer::start();
        let object = object_for(BODY);
        *server.lfs.omit.lock().unwrap() = Some(object.oid().to_string());
        let remote = server.remote();

        let error = batch_error(&remote, &object);

        assert!(
            matches!(error, LfsHttpError::MalformedResponse),
            "{error:?}"
        );
        assert!(server.lfs.requests_for("PUT", "/upload/").is_empty());
    }

    #[test]
    fn a_batch_answer_that_repeats_an_oid_fails_closed() {
        let server = FakeServer::start();
        let object = object_for(BODY);
        *server.lfs.duplicate.lock().unwrap() = Some(object.oid().to_string());
        let remote = server.remote();

        let error = batch_error(&remote, &object);

        assert!(
            matches!(error, LfsHttpError::MalformedResponse),
            "{error:?}"
        );
    }

    #[test]
    fn a_batch_answer_that_errors_an_object_fails_closed_naming_it() {
        let server = FakeServer::start();
        let object = object_for(BODY);
        *server.lfs.refuse.lock().unwrap() = Some(object.oid().to_string());
        let remote = server.remote();

        let error = batch_error(&remote, &object);

        match &error {
            LfsHttpError::ServerRefused { oid, code, message } => {
                assert_eq!(*oid, object.oid());
                assert_eq!(*code, 404);
                assert_eq!(message, "object not found");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(server.lfs.requests_for("PUT", "/upload/").is_empty());
    }

    #[test]
    fn a_damaged_blob_that_does_not_hash_to_its_oid_fails_closed_after_the_put() {
        let server = FakeServer::start();
        // The declared oid is not the digest of the bytes the source holds, so the
        // PUT itself succeeds and only the adapter's own hashing catches it.
        let object = RequiredLfsObject::new(Sha256::new([0xAB; 32]), BODY.len() as u64);
        let remote = server.remote();

        let plan = remote
            .prepare_upload(std::slice::from_ref(&object))
            .unwrap();
        let error = remote
            .upload(
                &object,
                &mut buffered(&object, BODY),
                plan.uploads()[0].action(),
            )
            .unwrap_err();

        match &error {
            LfsHttpError::ContentMismatch { oid, actual } => {
                assert_eq!(*oid, object.oid());
                assert_eq!(*actual, Sha256::digest(BODY));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        // The endpoint accepted the transfer with 200; the response alone is not
        // treated as success.
        assert_eq!(server.lfs.requests_for("PUT", "/upload/").len(), 1);
        assert!(!format!("{error}").contains(TOKEN));
        assert!(!format!("{error:?}").contains(TOKEN));
    }

    #[test]
    fn an_upload_streams_the_source_in_bounded_chunks() {
        let server = FakeServer::start();
        let body: Vec<u8> = (0..(LFS_UPLOAD_CHUNK_BYTES + 4096))
            .map(|index| (index % 251) as u8)
            .collect();
        let object = object_for(&body);
        let remote = server.remote();

        let plan = remote
            .prepare_upload(std::slice::from_ref(&object))
            .unwrap();
        let chunk_sizes = Arc::new(Mutex::new(Vec::new()));
        let mut source = RecordingSource::new(object.oid(), body.clone(), Arc::clone(&chunk_sizes));
        remote
            .upload(&object, &mut source, plan.uploads()[0].action())
            .unwrap();

        let sizes = chunk_sizes.lock().unwrap().clone();
        assert!(
            sizes.len() > 1,
            "an object larger than one chunk must be read in several reads: {sizes:?}"
        );
        assert!(
            sizes.iter().all(|size| *size <= LFS_UPLOAD_CHUNK_BYTES),
            "{sizes:?}"
        );
        assert!(sizes.contains(&LFS_UPLOAD_CHUNK_BYTES));
        let uploads = server.lfs.requests_for("PUT", "/upload/");
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].content_length, Some(body.len() as u64));
        assert_eq!(uploads[0].body, body);
    }

    #[test]
    fn a_failed_upload_is_reported_with_its_status() {
        let server = FakeServer::start();
        *server.lfs.upload_status.lock().unwrap() = Some(500);
        let object = object_for(BODY);
        let remote = server.remote();

        let plan = remote
            .prepare_upload(std::slice::from_ref(&object))
            .unwrap();
        let error = remote
            .upload(
                &object,
                &mut buffered(&object, BODY),
                plan.uploads()[0].action(),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            LfsHttpError::UnexpectedStatus {
                operation: "PUT",
                status: 500
            }
        ));
    }

    #[test]
    fn a_failed_batch_is_reported_with_its_status() {
        let server = FakeServer::start();
        *server.lfs.batch_status.lock().unwrap() = Some(503);
        let object = object_for(BODY);
        let remote = server.remote();

        let error = batch_error(&remote, &object);

        assert!(matches!(
            error,
            LfsHttpError::UnexpectedStatus {
                operation: "POST batch",
                status: 503
            }
        ));
    }

    #[test]
    fn an_unreachable_endpoint_is_a_transport_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let config = LfsHttpConfig::new(
            format!("http://{address}/info/lfs"),
            USERNAME,
            LfsToken::new(TOKEN).unwrap(),
            Duration::from_millis(500),
        )
        .unwrap();
        let remote = LfsHttpRemote::new(config).unwrap();
        let object = object_for(BODY);

        let error = batch_error(&remote, &object);

        assert!(
            matches!(error, LfsHttpError::Transport(_) | LfsHttpError::Client(_)),
            "{error:?}"
        );
    }

    #[test]
    fn a_token_never_appears_in_a_configuration_or_a_description() {
        let server = FakeServer::start();
        let config = LfsHttpConfig::new(
            server.batch_url.clone(),
            USERNAME,
            LfsToken::new(TOKEN).unwrap(),
            Duration::from_secs(5),
        )
        .unwrap();
        let remote = LfsHttpRemote::new(config.clone()).unwrap();

        assert!(!format!("{config:?}").contains(TOKEN));
        assert!(!config.describe().contains(TOKEN));
        assert!(!format!("{:?}", LfsToken::new(TOKEN).unwrap()).contains(TOKEN));
        assert!(!remote.describe().contains(TOKEN));
        assert!(remote.describe().contains(&server.batch_url));
    }

    #[test]
    fn a_configuration_refuses_a_url_that_is_not_an_absolute_http_endpoint() {
        for url in [
            "lfs.example.invalid/info/lfs",
            "ftp://lfs.example.invalid/info/lfs",
            "https://lfs.example.invalid/info/lfs?query=1",
            "https://lfs.example.invalid/info/lfs#fragment",
            "",
        ] {
            let config = LfsHttpConfig::new(
                url,
                USERNAME,
                LfsToken::new(TOKEN).unwrap(),
                Duration::from_secs(5),
            );
            assert!(
                matches!(config, Err(LfsHttpConfigError::InvalidUrl)),
                "{url} was accepted"
            );
        }
        assert!(matches!(
            LfsToken::new(""),
            Err(LfsHttpConfigError::InvalidToken)
        ));
        assert!(matches!(
            LfsHttpConfig::new(
                "https://lfs.example.invalid/info/lfs",
                "",
                LfsToken::new(TOKEN).unwrap(),
                Duration::from_secs(5)
            ),
            Err(LfsHttpConfigError::InvalidUsername)
        ));
    }

    #[test]
    fn a_trailing_slash_on_the_batch_url_is_normalized_away() {
        let config = LfsHttpConfig::new(
            "https://lfs.example.invalid/info/lfs/",
            USERNAME,
            LfsToken::new(TOKEN).unwrap(),
            Duration::from_secs(5),
        )
        .unwrap();

        assert_eq!(config.batch_url(), "https://lfs.example.invalid/info/lfs");
    }
}
