use std::{
    error::Error,
    fmt, io,
    io::Read,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};

use mineral_core::source::{
    SourceIdentity, SourceInventory, SourceInventoryEntry, SourceKind, SourceMaterialization,
    SourceMaterializationError, SourceMaterializationSet, SourceMaterializedEntry,
    SourceRefreshError, SourceRefreshFacts, SourceRefreshStep, SourceRevision, SourceScan,
    plan_source_refresh,
};

use crate::{
    domain::{ContentPath, Sha256},
    object_store::{
        R2ObjectStoreConfig, R2TransportError, SignedRequestSpec, empty_payload_sha256,
        signed_request_builder,
    },
    ports::{BlobStore, BlobWriter},
    storage::{LocalContentStore, SqliteSourceMaterializationStore},
};

use super::{
    prefix::{R2SourceKeyError, R2SourcePrefix},
    revision::{R2ObjectRevision, R2ObjectRevisionError},
};

/// How many keys one listing request asks for.
pub const DEFAULT_LIST_PAGE_SIZE: u32 = 1000;

/// The largest listing page body this reader will accept.
///
/// A listing is metadata, never content: a page larger than this is not something
/// this engine will page through, so the scan stops instead of growing a buffer to
/// fit whatever the endpoint sends.
pub const MAX_LIST_BODY_BYTES: u64 = 8 * 1024 * 1024;

/// The largest object this reader will stream into one blob.
///
/// The ingest itself is bounded in memory; this bound exists so a mis-advertised
/// object cannot fill the disk for an hour before the size check fails. It matches
/// the CAS's own expectations rather than any publication rule.
pub const MAX_SOURCE_OBJECT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// How much of one remote object is read at a time.
///
/// This is the reader's working set: one buffer, whatever the object's size.
pub const SOURCE_CHUNK_BYTES: usize = 64 * 1024;

/// An R2-backed source reader.
///
/// It lists one managed prefix, materializes exactly the revisions that are not
/// already proven, and hands the engine a stabilized inventory. It never classifies
/// content, never decides public scope, and never writes to the bucket: it is a
/// reader, and the only thing it produces is verified bytes in the content store
/// plus the materialization facts that say so.
pub struct R2Source {
    transport: R2SourceTransport,
    prefix: R2SourcePrefix,
    identity: SourceIdentity,
    store: LocalContentStore,
    materializations: SqliteSourceMaterializationStore,
    page_size: u32,
    /// How many objects this reader had to read, and how many durable bindings it
    /// could reuse, so a run can report what it actually did instead of guessing.
    fetched: AtomicUsize,
    reused: AtomicUsize,
}

impl R2Source {
    pub fn new(
        config: R2ObjectStoreConfig,
        prefix: R2SourcePrefix,
        store: LocalContentStore,
        materializations: SqliteSourceMaterializationStore,
    ) -> Result<Self, R2SourceError> {
        let identity = SourceIdentity::of(SourceKind::ObjectStore, &namespace(&config, &prefix))
            .map_err(R2SourceError::Identity)?;
        let transport = R2SourceTransport::new(config)?;
        Ok(Self {
            transport,
            prefix,
            identity,
            store,
            materializations,
            page_size: DEFAULT_LIST_PAGE_SIZE,
            fetched: AtomicUsize::new(0),
            reused: AtomicUsize::new(0),
        })
    }

    /// Narrows one listing request, for runtimes and tests that want smaller pages.
    pub fn with_page_size(mut self, page_size: u32) -> Self {
        self.page_size = page_size.clamp(1, DEFAULT_LIST_PAGE_SIZE);
        self
    }

    pub fn identity(&self) -> SourceIdentity {
        self.identity
    }

    pub fn prefix(&self) -> &R2SourcePrefix {
        &self.prefix
    }

    /// How many remote objects this reader has read into the content store.
    pub fn fetched_objects(&self) -> usize {
        self.fetched.load(Ordering::Relaxed)
    }

    /// How many objects a durable binding proved this reader did not have to read.
    pub fn reused_objects(&self) -> usize {
        self.reused.load(Ordering::Relaxed)
    }

    /// Where this source lives, for a report. It never contains a credential.
    pub fn describe(&self) -> String {
        self.transport.describe()
    }

    /// Reads every page of one listing into a canonical inventory.
    ///
    /// Directory markers are not files, an object that is not under the prefix is a
    /// contradiction rather than an invisible object, and a cursor that repeats
    /// stops the scan instead of looping.
    pub fn read_inventory(&self) -> Result<SourceInventory, R2SourceError> {
        let mut entries = Vec::new();
        let mut token: Option<String> = None;
        let mut seen_tokens = std::collections::BTreeSet::new();

        loop {
            let page =
                self.transport
                    .list_page(self.prefix.as_str(), token.as_deref(), self.page_size)?;

            for object in page.objects {
                if object.key.ends_with('/') {
                    if object.size == 0 {
                        // A directory marker: a zero-byte object whose key ends in a
                        // separator describes a folder, not a file. It is not part of
                        // the source state and it is not an error.
                        continue;
                    }
                    return Err(R2SourceError::NonZeroDirectoryMarker { key: object.key });
                }
                let path =
                    self.prefix
                        .content_path(&object.key)
                        .map_err(|source| R2SourceError::Key {
                            key: object.key.clone(),
                            source,
                        })?;
                let revision =
                    R2ObjectRevision::new(object.etag, object.last_modified, object.size)
                        .encode()
                        .map_err(R2SourceError::Revision)?;
                entries.push(SourceInventoryEntry::new(path, revision, object.size));
            }

            if !page.is_truncated {
                break;
            }
            let next = page
                .next_continuation_token
                .ok_or(R2SourceError::MissingContinuationToken)?;
            if !seen_tokens.insert(next.clone()) {
                return Err(R2SourceError::PaginationLoop);
            }
            token = Some(next);
        }

        SourceInventory::new(self.identity, entries).map_err(R2SourceError::Inventory)
    }

    /// Reads one object exactly as the inventory observed it.
    ///
    /// The bytes are streamed into the content store while this reader hashes the
    /// same stream, so the identity that names the blob is the identity of the bytes
    /// that were stored — never an ETag, and never a second read. A remote object
    /// that changed since the listing is refused (`If-Match`), not accepted as the
    /// revision the inventory described.
    pub fn fetch_exact(
        &self,
        entry: &SourceInventoryEntry,
    ) -> Result<SourceMaterializedEntry, R2SourceError> {
        let revision =
            R2ObjectRevision::decode(entry.revision()).map_err(R2SourceError::Revision)?;
        let key = self.prefix.object_key(entry.path());
        // The mapping is asserted to be an exact round trip: a key that does not map
        // back to the path it came from must never be read.
        let mapped = self
            .prefix
            .content_path(&key)
            .map_err(|source| R2SourceError::Key {
                key: key.clone(),
                source,
            })?;
        if mapped != *entry.path() {
            return Err(R2SourceError::KeyMapping {
                key,
                path: entry.path().clone(),
            });
        }

        let mut response = self.transport.get_exact(&key, &revision)?;
        self.fetched.fetch_add(1, Ordering::Relaxed);

        let mut writer = self.store.writer().map_err(R2SourceError::ContentStore)?;
        stream_object(
            &mut response,
            writer.as_mut(),
            revision.size(),
            SOURCE_CHUNK_BYTES,
        )
        .map_err(|error| error.into_source_error(&key, entry.path()))?;

        let stored = writer.finish().map_err(R2SourceError::ContentStore)?;
        if stored.size() != revision.size() {
            return Err(R2SourceError::SizeMismatch {
                path: entry.path().clone(),
                reported: revision.size(),
                received: stored.size(),
            });
        }

        let materialization = SourceMaterialization::new(
            self.identity,
            entry.path().clone(),
            entry.revision().clone(),
            stored.identity(),
            stored.size(),
        );
        self.materializations
            .save(&materialization)
            .map_err(R2SourceError::MaterializationStore)?;

        Ok(SourceMaterializedEntry::new(
            entry.path().clone(),
            stored.identity(),
            stored.size(),
        ))
    }

    fn refresh_facts(&self) -> R2RefreshFacts<'_> {
        R2RefreshFacts {
            materializations: &self.materializations,
            store: &self.store,
        }
    }
}

impl SourceScan for R2Source {
    type Error = R2SourceError;

    fn inventory(&self) -> Result<SourceInventory, Self::Error> {
        self.read_inventory()
    }

    fn materialize(
        &self,
        inventory: &SourceInventory,
    ) -> Result<SourceMaterializationSet, Self::Error> {
        let facts = self.refresh_facts();
        let plan = plan_source_refresh(inventory, &facts)
            .map_err(|error| R2SourceError::Refresh(Box::new(error)))?;

        let mut entries = Vec::with_capacity(plan.len());
        for step in plan.steps() {
            match step {
                SourceRefreshStep::Reuse {
                    entry,
                    content_sha256,
                    content_size,
                } => {
                    self.reused.fetch_add(1, Ordering::Relaxed);
                    entries.push(SourceMaterializedEntry::new(
                        entry.path().clone(),
                        *content_sha256,
                        *content_size,
                    ))
                }
                SourceRefreshStep::Fetch { entry } => entries.push(self.fetch_exact(entry)?),
            }
        }
        SourceMaterializationSet::new(entries).map_err(R2SourceError::Materialization)
    }
}

/// The durable facts one R2 refresh decision rests on.
struct R2RefreshFacts<'a> {
    materializations: &'a SqliteSourceMaterializationStore,
    store: &'a LocalContentStore,
}

impl SourceRefreshFacts for R2RefreshFacts<'_> {
    type Error = R2SourceError;

    fn materialization(
        &self,
        source: SourceIdentity,
        path: &ContentPath,
        revision: &SourceRevision,
    ) -> Result<Option<SourceMaterialization>, Self::Error> {
        self.materializations
            .get(source, path, revision)
            .map_err(R2SourceError::MaterializationStore)
    }

    fn stored_blob_size(&self, identity: Sha256) -> Result<Option<u64>, Self::Error> {
        self.store
            .probe(identity)
            .map_err(R2SourceError::ContentStore)
    }
}

/// The connection one R2 source reads through.
///
/// It shares endpoint configuration, URL encoding and Signature Version 4 with the
/// asset target, and it shares nothing else: this type only ever issues listings and
/// reads, and it has no concept of a published object.
struct R2SourceTransport {
    config: R2ObjectStoreConfig,
    client: Client,
}

impl R2SourceTransport {
    fn new(config: R2ObjectStoreConfig) -> Result<Self, R2SourceError> {
        let timeout = config.timeout();
        let client = Client::builder()
            .timeout(timeout.max(Duration::from_secs(1)))
            .build()
            .map_err(R2SourceError::Client)?;
        Ok(Self { config, client })
    }

    fn describe(&self) -> String {
        self.config.describe()
    }

    fn list_page(
        &self,
        prefix: &str,
        token: Option<&str>,
        page_size: u32,
    ) -> Result<super::list::ListPage, R2SourceError> {
        let mut query = vec![
            ("list-type".to_owned(), "2".to_owned()),
            ("prefix".to_owned(), prefix.to_owned()),
            ("max-keys".to_owned(), page_size.to_string()),
        ];
        if let Some(token) = token {
            query.push(("continuation-token".to_owned(), token.to_owned()));
        }
        let path = format!("/{}", self.config.bucket());
        let spec = SignedRequestSpec::new("GET", &path, empty_payload_sha256()).with_query(&query);
        let response = signed_request_builder(&self.client, &self.config, &spec)
            .map_err(R2SourceError::Request)?
            .send()
            .map_err(R2SourceError::Transport)?;

        if !response.status().is_success() {
            return Err(R2SourceError::UnexpectedStatus {
                operation: "LIST",
                status: response.status().as_u16(),
            });
        }

        let body = read_bounded(response, MAX_LIST_BODY_BYTES, "LIST")?;
        let body = String::from_utf8(body).map_err(|_| R2SourceError::ListingNotUtf8)?;
        super::list::parse_list_page(&body).map_err(R2SourceError::Listing)
    }

    /// Reads one object under an `If-Match` precondition.
    ///
    /// `If-Match` is what makes the read *exact*: if the object changed after the
    /// listing, the endpoint refuses with 412 and this engine retries the scan
    /// instead of accepting bytes that are not the revision it observed.
    fn get_exact(&self, key: &str, revision: &R2ObjectRevision) -> Result<Response, R2SourceError> {
        let path = format!(
            "/{}/{}",
            self.config.bucket(),
            crate::object_store::encode_path(key)
        );
        let spec = SignedRequestSpec::new("GET", &path, empty_payload_sha256())
            .with_header("if-match", revision.if_match());
        let response = signed_request_builder(&self.client, &self.config, &spec)
            .map_err(R2SourceError::Request)?
            .send()
            .map_err(R2SourceError::Transport)?;

        match response.status() {
            status if status.is_success() => {}
            StatusCode::NOT_FOUND => {
                return Err(R2SourceError::ObjectDisappeared {
                    key: key.to_owned(),
                });
            }
            StatusCode::PRECONDITION_FAILED => {
                return Err(R2SourceError::RemoteRevisionChanged {
                    key: key.to_owned(),
                });
            }
            status => {
                return Err(R2SourceError::UnexpectedStatus {
                    operation: "GET",
                    status: status.as_u16(),
                });
            }
        }

        if let Some(length) = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            && length != revision.size()
        {
            return Err(R2SourceError::ContentLengthMismatch {
                key: key.to_owned(),
                reported: revision.size(),
                received: length,
            });
        }
        if let Some(etag) = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
        {
            let etag = etag.trim_matches('"');
            if etag != revision.etag() {
                return Err(R2SourceError::RevisionChangedOnResponse {
                    key: key.to_owned(),
                });
            }
        }
        Ok(response)
    }
}

/// Reads a response body into memory, refusing anything larger than `limit`.
fn read_bounded(
    mut response: Response,
    limit: u64,
    operation: &'static str,
) -> Result<Vec<u8>, R2SourceError> {
    let mut body = Vec::new();
    let mut buffer = vec![0_u8; SOURCE_CHUNK_BYTES];
    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|source| R2SourceError::Read { source })?;
        if read == 0 {
            break;
        }
        if u64::try_from(body.len() + read).unwrap_or(u64::MAX) > limit {
            return Err(R2SourceError::ResponseTooLarge { operation });
        }
        body.extend_from_slice(&buffer[..read]);
    }
    Ok(body)
}

/// The canonical, credential-free description of one source namespace.
fn namespace(config: &R2ObjectStoreConfig, prefix: &R2SourcePrefix) -> String {
    format!(
        "{}/{}/{}",
        config.endpoint(),
        config.bucket(),
        prefix.as_str()
    )
}

/// Copies one remote body into a content store in bounded chunks.
///
/// The same bytes are written and hashed: the caller's writer computes the content
/// identity of exactly what it stored, so nothing here can disagree with what ends up
/// in the store. The copy never asks its reader for more than `chunk_bytes`, which is
/// what keeps an object of any size from being materialized in memory.
pub(crate) fn stream_object(
    reader: &mut impl Read,
    writer: &mut dyn BlobWriter,
    expected_size: u64,
    chunk_bytes: usize,
) -> Result<u64, StreamError> {
    let mut buffer = vec![0_u8; chunk_bytes.max(1)];
    let mut received: u64 = 0;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| StreamError::Read { source })?;
        if read == 0 {
            break;
        }
        writer
            .write(&buffer[..read])
            .map_err(|source| StreamError::Write { source })?;
        received = received
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or(StreamError::TooLarge)?;
        if received > expected_size {
            return Err(StreamError::SizeMismatch {
                reported: expected_size,
                received,
            });
        }
        if received > MAX_SOURCE_OBJECT_BYTES {
            return Err(StreamError::TooLarge);
        }
    }
    if received != expected_size {
        return Err(StreamError::SizeMismatch {
            reported: expected_size,
            received,
        });
    }
    Ok(received)
}

/// Why one remote body could not be copied into the content store.
#[derive(Debug)]
pub(crate) enum StreamError {
    Read {
        source: io::Error,
    },
    Write {
        source: crate::storage::ContentStoreError,
    },
    SizeMismatch {
        reported: u64,
        received: u64,
    },
    TooLarge,
}

impl StreamError {
    fn into_source_error(self, key: &str, path: &ContentPath) -> R2SourceError {
        match self {
            Self::Read { source } => R2SourceError::Read { source },
            Self::Write { source } => R2SourceError::ContentStore(source),
            Self::SizeMismatch { reported, received } => R2SourceError::SizeMismatch {
                path: path.clone(),
                reported,
                received,
            },
            Self::TooLarge => R2SourceError::ObjectTooLarge {
                key: key.to_owned(),
            },
        }
    }
}

#[derive(Debug)]
pub enum R2SourceError {
    Client(reqwest::Error),
    Transport(reqwest::Error),
    Request(R2TransportError),
    Read {
        source: io::Error,
    },
    UnexpectedStatus {
        operation: &'static str,
        status: u16,
    },
    ResponseTooLarge {
        operation: &'static str,
    },
    ListingNotUtf8,
    Listing(super::list::ListPageError),
    MissingContinuationToken,
    PaginationLoop,
    NonZeroDirectoryMarker {
        key: String,
    },
    Key {
        key: String,
        source: R2SourceKeyError,
    },
    KeyMapping {
        key: String,
        path: ContentPath,
    },
    Revision(R2ObjectRevisionError),
    RevisionChangedOnResponse {
        key: String,
    },
    RemoteRevisionChanged {
        key: String,
    },
    ObjectDisappeared {
        key: String,
    },
    ObjectTooLarge {
        key: String,
    },
    SizeMismatch {
        path: ContentPath,
        reported: u64,
        received: u64,
    },
    ContentLengthMismatch {
        key: String,
        reported: u64,
        received: u64,
    },
    ContentStore(crate::storage::ContentStoreError),
    MaterializationStore(crate::storage::SqliteSourceMaterializationStoreError),
    Materialization(SourceMaterializationError),
    Refresh(Box<SourceRefreshError<R2SourceError>>),
    Inventory(mineral_core::source::SourceInventoryError),
    Identity(mineral_core::source::SourceIdentityError),
}

impl fmt::Display for R2SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "could not build the R2 client: {error}"),
            Self::Transport(error) => write!(formatter, "R2 request failed: {error}"),
            Self::Request(error) => write!(formatter, "could not sign an R2 request: {error}"),
            Self::Read { source } => {
                write!(formatter, "could not read the remote object: {source}")
            }
            Self::UnexpectedStatus { operation, status } => {
                write!(formatter, "R2 {operation} returned status {status}")
            }
            Self::ResponseTooLarge { operation } => {
                write!(
                    formatter,
                    "R2 {operation} response is larger than this reader accepts"
                )
            }
            Self::ListingNotUtf8 => formatter.write_str("object listing is not valid UTF-8"),
            Self::Listing(error) => write!(formatter, "{error}"),
            Self::MissingContinuationToken => {
                formatter.write_str("object listing is truncated without a continuation token")
            }
            Self::PaginationLoop => {
                formatter.write_str("object listing repeated a continuation token")
            }
            Self::NonZeroDirectoryMarker { key } => write!(
                formatter,
                "remote key {key} ends in a separator but is not an empty directory marker"
            ),
            Self::Key { key, source } => {
                write!(formatter, "remote key {key} is unusable: {source}")
            }
            Self::KeyMapping { key, path } => {
                write!(formatter, "remote key {key} does not map back to {path}")
            }
            Self::Revision(error) => write!(formatter, "{error}"),
            Self::RevisionChangedOnResponse { key } => write!(
                formatter,
                "R2 served a different revision than the listing observed: {key}"
            ),
            Self::RemoteRevisionChanged { key } => write!(
                formatter,
                "remote object changed after it was listed: {key}"
            ),
            Self::ObjectDisappeared { key } => {
                write!(
                    formatter,
                    "remote object disappeared after it was listed: {key}"
                )
            }
            Self::ObjectTooLarge { key } => {
                write!(
                    formatter,
                    "remote object is larger than this reader accepts: {key}"
                )
            }
            Self::SizeMismatch {
                path,
                reported,
                received,
            } => write!(
                formatter,
                "remote object {path} is {received} bytes but the listing reported {reported}"
            ),
            Self::ContentLengthMismatch {
                key,
                reported,
                received,
            } => write!(
                formatter,
                "R2 announced {received} bytes for {key} but the listing reported {reported}"
            ),
            Self::ContentStore(error) => write!(formatter, "content store failed: {error}"),
            Self::MaterializationStore(error) => {
                write!(
                    formatter,
                    "could not record source materialization: {error}"
                )
            }
            Self::Materialization(error) => write!(formatter, "{error}"),
            Self::Refresh(error) => write!(formatter, "{error}"),
            Self::Inventory(error) => write!(formatter, "{error}"),
            Self::Identity(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for R2SourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Client(error) | Self::Transport(error) => Some(error),
            Self::Request(error) => Some(error),
            Self::Read { source } => Some(source),
            Self::Listing(error) => Some(error),
            Self::Key { source, .. } => Some(source),
            Self::Revision(error) => Some(error),
            Self::ContentStore(error) => Some(error),
            Self::MaterializationStore(error) => Some(error),
            Self::Materialization(error) => Some(error),
            Self::Refresh(error) => Some(error.as_ref()),
            Self::Inventory(error) => Some(error),
            Self::Identity(error) => Some(error),
            _ => None,
        }
    }
}
