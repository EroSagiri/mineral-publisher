use std::{
    error::Error,
    fmt, fs, io,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST, IF_NONE_MATCH};
use sha2::Digest;

use crate::{
    asset::{
        AssetByteIdentity, AssetTargetFacts, AssetTargetState, AssetVerification,
        ObjectStoreTransport, ObjectWriter,
    },
    domain::Sha256,
    workflow::{AssetContentType, AssetObjectKey, PublishedAsset},
};

use super::signature::{SignableRequest, SigningContext, authorization_header, payload_sha256};

static NEXT_SPOOL: AtomicU64 = AtomicU64::new(1);

/// A secret access key.
///
/// The inner value is never printed: a credential must not reach a log, a panic
/// message or an audit record.
#[derive(Clone)]
pub struct R2SecretKey(String);

impl R2SecretKey {
    pub fn new(value: impl Into<String>) -> Result<Self, R2ObjectStoreConfigError> {
        let value = value.into();
        if value.is_empty() || value.contains(['\0', '\n', '\r']) {
            return Err(R2ObjectStoreConfigError::InvalidSecret);
        }
        Ok(Self(value))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for R2SecretKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("R2SecretKey(<redacted>)")
    }
}

/// Everything the adapter needs to reach one bucket.
///
/// This is runtime configuration: it lives in the host, is never handed to the
/// engine, and the engine's durable records never contain any of it.
#[derive(Clone)]
pub struct R2ObjectStoreConfig {
    endpoint: String,
    bucket: String,
    region: String,
    access_key_id: String,
    secret_access_key: R2SecretKey,
    /// How long one HTTP request may take, including the streaming body.
    timeout: Duration,
}

impl R2ObjectStoreConfig {
    pub fn new(
        endpoint: impl Into<String>,
        bucket: impl Into<String>,
        access_key_id: impl Into<String>,
        secret_access_key: R2SecretKey,
    ) -> Result<Self, R2ObjectStoreConfigError> {
        let endpoint = endpoint.into();
        let bucket = bucket.into();
        let access_key_id = access_key_id.into();
        if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
            return Err(R2ObjectStoreConfigError::InvalidEndpoint);
        }
        if endpoint.ends_with('/') {
            return Err(R2ObjectStoreConfigError::InvalidEndpoint);
        }
        if bucket.is_empty()
            || bucket.contains(['/', '\\', '\0'])
            || access_key_id.is_empty()
            || access_key_id.contains(['\0', '\n', '\r'])
        {
            return Err(R2ObjectStoreConfigError::InvalidIdentity);
        }
        Ok(Self {
            endpoint,
            bucket,
            // R2 accepts `auto`; a generic S3 endpoint may need its own region.
            region: "auto".to_owned(),
            access_key_id,
            secret_access_key,
            timeout: Duration::from_secs(300),
        })
    }

    pub fn with_region(
        mut self,
        region: impl Into<String>,
    ) -> Result<Self, R2ObjectStoreConfigError> {
        let region = region.into();
        if region.is_empty() || region.contains(['\0', '\n', '\r']) {
            return Err(R2ObjectStoreConfigError::InvalidIdentity);
        }
        self.region = region;
        Ok(self)
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl fmt::Debug for R2ObjectStoreConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("R2ObjectStoreConfig")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &self.secret_access_key)
            .finish()
    }
}

/// An S3-compatible object store used as an asset target.
///
/// Requests are path-style (`{endpoint}/{bucket}/{key}`) and signed with Signature
/// Version 4. Inspections hash the object's bytes; writes spool one object to a
/// temporary file so the request can be length-delimited without ever holding the
/// object in memory, and are only sent when the writer is finished — which the
/// driver does after the engine's verification rule has accepted the stream.
pub struct R2ObjectStore {
    config: R2ObjectStoreConfig,
    client: Client,
    spool_directory: PathBuf,
}

impl R2ObjectStore {
    pub fn new(
        config: R2ObjectStoreConfig,
        spool_directory: impl Into<PathBuf>,
    ) -> Result<Self, R2ObjectStoreError> {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(R2ObjectStoreError::Client)?;
        Ok(Self {
            config,
            client,
            spool_directory: spool_directory.into(),
        })
    }

    /// Where this bucket lives, for a report. It never contains a credential.
    pub fn describe(&self) -> String {
        format!("{}/{}", self.config.endpoint(), self.config.bucket())
    }

    fn canonical_uri(&self, object_key: &AssetObjectKey) -> String {
        format!(
            "/{}/{}",
            self.config.bucket,
            encode_path(object_key.as_str())
        )
    }

    /// Builds a signed request for one endpoint call.
    ///
    /// `create_only` adds `if-none-match: *` to the signed header set, which makes
    /// the upload a create rather than a replace: the store refuses it if anything
    /// appeared under the key after the inspection that found the key absent. This
    /// is the object-storage counterpart of the Git side's exact compare-and-swap.
    fn request(
        &self,
        method: &'static str,
        object_key: &AssetObjectKey,
        payload_sha256_hex: &str,
        content_type: Option<&str>,
        content_length: Option<u64>,
        create_only: bool,
    ) -> Result<RequestBuilder, R2ObjectStoreError> {
        let host = host_of(&self.config.endpoint)?;
        let (date, amz_date) = timestamps()?;
        let mut headers: Vec<(String, String)> = vec![
            ("host".to_owned(), host.clone()),
            (
                "x-amz-content-sha256".to_owned(),
                payload_sha256_hex.to_owned(),
            ),
            ("x-amz-date".to_owned(), amz_date.clone()),
        ];
        if let Some(content_type) = content_type {
            headers.push(("content-type".to_owned(), content_type.to_owned()));
        }
        if create_only {
            headers.push(("if-none-match".to_owned(), "*".to_owned()));
        }
        headers.sort_by(|left, right| left.0.cmp(&right.0));

        let canonical_uri = self.canonical_uri(object_key);
        let request = SignableRequest {
            method,
            canonical_uri: &canonical_uri,
            canonical_query: "",
            headers: &headers,
            payload_sha256: payload_sha256_hex,
        };
        let context = SigningContext {
            access_key_id: &self.config.access_key_id,
            secret_access_key: self.config.secret_access_key.expose(),
            region: &self.config.region,
            service: "s3",
            date: &date,
            amz_date: &amz_date,
        };
        let authorization = authorization_header(&request, &context);
        let url = format!("{}{canonical_uri}", self.config.endpoint);
        let builder = self
            .client
            .request(
                reqwest::Method::from_bytes(method.as_bytes())
                    .expect("the method is a static HTTP verb"),
                &url,
            )
            .header(HOST, host)
            .header("x-amz-content-sha256", payload_sha256_hex)
            .header("x-amz-date", amz_date)
            .header(AUTHORIZATION, authorization);
        let builder = match content_type {
            Some(content_type) => builder.header(CONTENT_TYPE, content_type),
            None => builder,
        };
        let builder = if create_only {
            builder.header(IF_NONE_MATCH, "*")
        } else {
            builder
        };
        Ok(match content_length {
            Some(length) => builder.header(CONTENT_LENGTH, length),
            None => builder,
        })
    }

    fn head(
        &self,
        object_key: &AssetObjectKey,
    ) -> Result<Option<ResponseMetadata>, R2ObjectStoreError> {
        let response = self
            .request("HEAD", object_key, &payload_sha256(b""), None, None, false)?
            .send()
            .map_err(R2ObjectStoreError::Transport)?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => Ok(Some(ResponseMetadata {
                content_type: response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            })),
            status => Err(R2ObjectStoreError::UnexpectedStatus {
                operation: "HEAD",
                status: status.as_u16(),
            }),
        }
    }

    /// Reads one object's bytes, hashing them as they arrive.
    ///
    /// The byte identity reported to the engine comes from these bytes, never from
    /// an ETag or any other header the store happens to send.
    fn get_verified(
        &self,
        object_key: &AssetObjectKey,
    ) -> Result<Option<VerifiedRemoteObject>, R2ObjectStoreError> {
        let Some(metadata) = self.head(object_key)? else {
            return Ok(None);
        };
        let response = self
            .request("GET", object_key, &payload_sha256(b""), None, None, false)?
            .send()
            .map_err(R2ObjectStoreError::Transport)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(R2ObjectStoreError::UnexpectedStatus {
                operation: "GET",
                status: response.status().as_u16(),
            });
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .or(metadata.content_type);
        let mut reader = response;
        let mut hasher = sha2::Sha256::new();
        let mut size = 0_u64;
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|source| R2ObjectStoreError::Read { source })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size += read as u64;
        }
        Ok(Some(VerifiedRemoteObject {
            size,
            content_type,
            sha256: Sha256::new(hasher.finalize().into()),
        }))
    }

    fn spool_path(&self) -> Result<PathBuf, R2ObjectStoreError> {
        fs::create_dir_all(&self.spool_directory).map_err(|source| {
            R2ObjectStoreError::io("create spool directory", &self.spool_directory, source)
        })?;
        let sequence = NEXT_SPOOL.fetch_add(1, Ordering::Relaxed);
        Ok(self.spool_directory.join(format!(
            ".mineral-upload-{}-{sequence}.tmp",
            std::process::id()
        )))
    }

    fn put(&self, asset: &PublishedAsset, spool: &Path) -> Result<(), R2ObjectStoreError> {
        let file = fs::File::open(spool)
            .map_err(|source| R2ObjectStoreError::io("open spooled object", spool, source))?;
        // The signed payload hash is the frozen digest, not a hash recomputed from
        // the spool here. The store therefore checks the bytes it receives against
        // the same identity the engine published, and a spool that changed between
        // the engine's verification pass and this call is rejected by the store
        // instead of being written under a name that would no longer describe it.
        let request = self.request(
            "PUT",
            asset.object_key(),
            &asset.published_sha256().to_string(),
            Some(asset.published_content_type().as_str()),
            Some(asset.published_size()),
            // An object that appeared after the inspection must not be replaced.
            true,
        )?;
        let response = request
            .body(reqwest::blocking::Body::sized(file, asset.published_size()))
            .send()
            .map_err(R2ObjectStoreError::Transport)?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(R2ObjectStoreError::UnexpectedStatus {
            operation: "PUT",
            status: response.status().as_u16(),
        })
    }
}

impl ObjectStoreTransport for R2ObjectStore {
    type Error = R2ObjectStoreError;

    fn inspect(&self, object_key: &AssetObjectKey) -> Result<AssetTargetState, Self::Error> {
        let Some(remote) = self.get_verified(object_key)? else {
            return Ok(AssetTargetState::Missing);
        };
        let Some(content_type) = remote.content_type else {
            // Without the media type the store cannot describe what it serves, so
            // it reports an object it cannot characterize rather than guessing one.
            return Err(R2ObjectStoreError::MissingContentType {
                object_key: object_key.clone(),
            });
        };
        Ok(AssetTargetState::Present(AssetTargetFacts::new(
            object_key.clone(),
            remote.size,
            AssetContentType::new(content_type.clone()).map_err(|_| {
                R2ObjectStoreError::UnusableContentType {
                    object_key: object_key.clone(),
                    content_type,
                }
            })?,
            AssetByteIdentity::Verified(remote.sha256),
        )))
    }

    fn open_writer(
        &self,
        asset: &PublishedAsset,
    ) -> Result<Box<dyn ObjectWriter<Error = Self::Error> + '_>, Self::Error> {
        // Anything already under the frozen key must satisfy the frozen facts. The
        // judgement is the engine's own rule, so the adapter cannot drift from it.
        match self.inspect(asset.object_key())? {
            AssetTargetState::Missing => {}
            present => match asset.judge(&present) {
                AssetVerification::Ready => {
                    return Ok(Box::new(ReuseRemoteObject));
                }
                AssetVerification::Conflict(conflict) => {
                    return Err(R2ObjectStoreError::ConflictingObject {
                        object_key: asset.object_key().clone(),
                        conflict: Box::new(conflict),
                    });
                }
                AssetVerification::Unverifiable => {
                    return Err(R2ObjectStoreError::UnverifiableObject {
                        object_key: asset.object_key().clone(),
                    });
                }
                AssetVerification::Missing => {}
            },
        }

        let spool = self.spool_path()?;
        Ok(Box::new(R2Upload {
            store: self,
            asset: asset.clone(),
            spool,
            written: 0,
        }))
    }
}

/// A writer that leaves an already-correct remote object alone.
struct ReuseRemoteObject;

impl ObjectWriter for ReuseRemoteObject {
    type Error = R2ObjectStoreError;

    fn write(&mut self, _: &[u8]) -> Result<(), Self::Error> {
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A writer that spools one object locally and only sends it when finished.
///
/// Spooling is what lets the request carry an exact length without holding the
/// object in memory. The upload itself streams from the spool, and the request is
/// never sent if the driver abandons the writer because the bytes it read were not
/// the frozen representation.
struct R2Upload<'a> {
    store: &'a R2ObjectStore,
    asset: PublishedAsset,
    spool: PathBuf,
    written: u64,
}

impl ObjectWriter for R2Upload<'_> {
    type Error = R2ObjectStoreError;

    fn write(&mut self, chunk: &[u8]) -> Result<(), Self::Error> {
        // The spool file is created on first use, so an abandoned write leaves no
        // trace beyond a path that was never created.
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.spool)
            .map_err(|source| R2ObjectStoreError::io("open spool", &self.spool, source))?;
        io::Write::write_all(&mut file, chunk)
            .map_err(|source| R2ObjectStoreError::io("write spool", &self.spool, source))?;
        self.written += chunk.len() as u64;
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), Self::Error> {
        let result = (|| {
            if self.written != self.asset.published_size() {
                return Err(R2ObjectStoreError::SpoolLengthMismatch {
                    expected: self.asset.published_size(),
                    actual: self.written,
                });
            }
            self.store.put(&self.asset, &self.spool)
        })();
        let _ = fs::remove_file(&self.spool);
        result
    }
}

impl Drop for R2Upload<'_> {
    fn drop(&mut self) {
        // An abandoned write never reached the bucket, and its spool is not a
        // published object.
        let _ = fs::remove_file(&self.spool);
    }
}

struct ResponseMetadata {
    content_type: Option<String>,
}

struct VerifiedRemoteObject {
    size: u64,
    content_type: Option<String>,
    sha256: Sha256,
}

/// `YYYYMMDD` and `YYYYMMDDTHHMMSSZ` for the current instant.
fn timestamps() -> Result<(String, String), R2ObjectStoreError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| R2ObjectStoreError::Clock)?;
    let seconds = now.as_secs();
    let (year, month, day, hour, minute, second) = civil_from_unix(seconds);
    Ok((
        format!("{year:04}{month:02}{day:02}"),
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
    ))
}

/// Days-from-civil, inverted (Howard Hinnant's algorithm), so no date dependency
/// is needed for a timestamp.
fn civil_from_unix(seconds: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (seconds / 86_400) as i64;
    let remainder = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        m,
        d,
        (remainder / 3_600) as u32,
        ((remainder % 3_600) / 60) as u32,
        (remainder % 60) as u32,
    )
}

fn host_of(endpoint: &str) -> Result<String, R2ObjectStoreError> {
    let without_scheme = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .ok_or(R2ObjectStoreError::InvalidEndpoint)?;
    let host = without_scheme.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err(R2ObjectStoreError::InvalidEndpoint);
    }
    Ok(host.to_owned())
}

/// Percent-encodes one object key for a canonical URI, keeping `/` as a separator.
fn encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum R2ObjectStoreConfigError {
    InvalidEndpoint,
    InvalidIdentity,
    InvalidSecret,
}

impl fmt::Display for R2ObjectStoreConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint => formatter.write_str(
                "object store endpoint must be an absolute http(s) URL without a trailing slash",
            ),
            Self::InvalidIdentity => {
                formatter.write_str("object store bucket or access key id is invalid")
            }
            Self::InvalidSecret => formatter.write_str("object store secret access key is invalid"),
        }
    }
}

impl Error for R2ObjectStoreConfigError {}

#[derive(Debug)]
pub enum R2ObjectStoreError {
    Client(reqwest::Error),
    Transport(reqwest::Error),
    Read {
        source: io::Error,
    },
    Clock,
    InvalidEndpoint,
    UnexpectedStatus {
        operation: &'static str,
        status: u16,
    },
    MissingContentType {
        object_key: AssetObjectKey,
    },
    UnusableContentType {
        object_key: AssetObjectKey,
        content_type: String,
    },
    ConflictingObject {
        object_key: AssetObjectKey,
        /// Boxed: the exact disagreement is the largest fact here, and every early
        /// return of the adapter would otherwise pay for it.
        conflict: Box<crate::asset::AssetTargetConflict>,
    },
    UnverifiableObject {
        object_key: AssetObjectKey,
    },
    SpoolLengthMismatch {
        expected: u64,
        actual: u64,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl R2ObjectStoreError {
    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for R2ObjectStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) | Self::Transport(error) => {
                write!(formatter, "object store request failed: {error}")
            }
            Self::Read { source } => {
                write!(formatter, "could not read the remote object: {source}")
            }
            Self::Clock => formatter.write_str("system clock cannot be used to sign a request"),
            Self::InvalidEndpoint => formatter.write_str("object store endpoint is invalid"),
            Self::UnexpectedStatus { operation, status } => {
                write!(
                    formatter,
                    "object store {operation} returned status {status}"
                )
            }
            Self::MissingContentType { object_key } => write!(
                formatter,
                "remote object has no content type, so its facts cannot be described: {object_key}"
            ),
            Self::UnusableContentType { object_key, .. } => write!(
                formatter,
                "remote object reports an unusable content type: {object_key}"
            ),
            Self::ConflictingObject { object_key, .. } => write!(
                formatter,
                "object store already holds conflicting content under the frozen key: {object_key}"
            ),
            Self::UnverifiableObject { object_key } => write!(
                formatter,
                "object store cannot verify the bytes it serves: {object_key}"
            ),
            Self::SpoolLengthMismatch { expected, actual } => write!(
                formatter,
                "spooled object is {actual} bytes, not the frozen {expected}"
            ),
            Self::Io {
                operation, path, ..
            } => write!(formatter, "could not {operation}: {}", path.display()),
        }
    }
}

impl Error for R2ObjectStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Client(error) | Self::Transport(error) => Some(error),
            Self::Read { source } => Some(source),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
