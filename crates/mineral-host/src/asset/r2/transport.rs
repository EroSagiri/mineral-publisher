use std::{
    error::Error,
    fmt, fs, io,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use sha2::Digest;

use crate::{
    asset::{
        AssetByteIdentity, AssetTargetFacts, AssetTargetState, AssetVerification,
        ObjectStoreTransport, ObjectWriter,
    },
    domain::Sha256,
    object_store::{
        R2ObjectStoreConfig, R2TransportError, SignedRequestSpec, encode_path, payload_sha256,
        signed_request_builder,
    },
    workflow::{AssetContentType, AssetObjectKey, PublishedAsset},
};

static NEXT_SPOOL: AtomicU64 = AtomicU64::new(1);

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
            .timeout(config.timeout())
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
        self.config.describe()
    }

    fn canonical_uri(&self, object_key: &AssetObjectKey) -> String {
        format!(
            "/{}/{}",
            self.config.bucket(),
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
        let canonical_uri = self.canonical_uri(object_key);
        let mut spec =
            SignedRequestSpec::new(method, &canonical_uri, payload_sha256_hex.to_owned());
        if let Some(content_type) = content_type {
            spec = spec.with_header("content-type", content_type);
        }
        if create_only {
            spec = spec.with_header("if-none-match", "*");
        }

        let builder = signed_request_builder(&self.client, &self.config, &spec)
            .map_err(R2ObjectStoreError::from_transport)?;
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

    /// Maps one low-level request failure onto this adapter's own vocabulary.
    fn from_transport(error: R2TransportError) -> Self {
        match error {
            R2TransportError::Clock => Self::Clock,
            R2TransportError::InvalidEndpoint => Self::InvalidEndpoint,
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
