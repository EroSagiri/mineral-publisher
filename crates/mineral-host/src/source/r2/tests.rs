//! Offline tests for the R2 source reader.
//!
//! Every test here runs against an in-process HTTP server that speaks the parts of
//! the S3 API this reader uses: `ListObjectsV2` with a prefix and a continuation
//! token, and an `If-Match` object read. No network, no credentials, no live bucket.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::SystemTime,
};

use mineral_core::{
    domain::{ContentPath, Sha256, Snapshot, SnapshotId, SourceId},
    source::{
        DEFAULT_MAX_SCAN_ATTEMPTS, SourceInventory, SourceMaterialization,
        SourceMaterializationSet, SourceRevision, SourceScan, SourceScanError, StabilizedSource,
        stabilize_scan,
    },
};

use crate::{
    object_store::{R2ObjectStoreConfig, R2SecretKey},
    storage::{LocalContentStore, SqliteSourceMaterializationStore},
};

use super::{
    R2Source, R2SourceError, R2SourcePrefix,
    prefix::R2SourcePrefixError,
    reader::{SOURCE_CHUNK_BYTES, stream_object},
};

const BUCKET: &str = "mineral-vault";
const PREFIX: &str = "vault/";

/// One object the fake bucket holds, together with what a listing reports about it.
#[derive(Clone, Debug)]
struct FakeObject {
    bytes: Vec<u8>,
    etag: String,
    last_modified: String,
    /// When set, listings report this size instead of the real one.
    listed_size: Option<u64>,
    /// When set, an `If-Match` read is answered with this ETag, as an overwrite would.
    served_etag: Option<String>,
    /// When set, the read's response carries this ETag whatever it served.
    response_etag: Option<String>,
    /// When set, the read sends only this many bytes and closes the connection.
    truncated_at: Option<usize>,
    /// When false, the object is listed but cannot be read.
    readable: bool,
}

impl FakeObject {
    fn new(etag: &str, bytes: &[u8], last_modified: &str) -> Self {
        Self {
            bytes: bytes.to_vec(),
            etag: etag.to_owned(),
            last_modified: last_modified.to_owned(),
            listed_size: None,
            served_etag: None,
            response_etag: None,
            truncated_at: None,
            readable: true,
        }
    }
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    authorization: Option<String>,
    if_match: Option<String>,
}

#[derive(Default)]
struct FakeBucket {
    objects: Mutex<BTreeMap<String, FakeObject>>,
    requests: Mutex<Vec<RecordedRequest>>,
    /// How many keys one listing page holds; `0` means "all of them".
    page_size: AtomicUsize,
    /// When true, every page claims to be truncated with one constant cursor.
    loop_cursor: AtomicBool,
    /// When true, a listing reports a key that is not under the requested prefix.
    serve_outside_key: AtomicBool,
}

impl FakeBucket {
    fn put(&self, key: &str, object: FakeObject) {
        self.objects.lock().unwrap().insert(key.to_owned(), object);
    }

    fn remove(&self, key: &str) {
        self.objects.lock().unwrap().remove(key);
    }

    fn mutate(&self, key: &str, change: impl FnOnce(&mut FakeObject)) {
        if let Some(object) = self.objects.lock().unwrap().get_mut(key) {
            change(object);
        }
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn reads(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.method == "GET" && !request.query.contains_key("list-type"))
            .collect()
    }

    fn read_keys(&self) -> Vec<String> {
        self.reads()
            .into_iter()
            .map(|request| {
                request
                    .path
                    .strip_prefix(&format!("/{BUCKET}/"))
                    .unwrap_or(&request.path)
                    .to_owned()
            })
            .collect()
    }

    fn listings(&self) -> usize {
        self.requests()
            .into_iter()
            .filter(|request| request.query.contains_key("list-type"))
            .count()
    }
}

struct FakeServer {
    bucket: Arc<FakeBucket>,
    endpoint: String,
}

impl FakeServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
        let address = listener.local_addr().expect("the bound address");
        let bucket = Arc::new(FakeBucket::default());
        let served = Arc::clone(&bucket);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let bucket = Arc::clone(&served);
                // One request per connection, always closing afterwards.
                let _ = handle(stream, &bucket);
            }
        });
        Self {
            bucket,
            endpoint: format!("http://{address}"),
        }
    }

    fn put_bytes(&self, key: &str, etag: &str, bytes: &[u8]) {
        self.bucket.put(
            key,
            FakeObject::new(etag, bytes, "2024-01-01T00:00:00.000Z"),
        );
    }
}

fn handle(mut stream: TcpStream, bucket: &FakeBucket) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();

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

    let (path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (percent_decode(path), query.to_owned()),
        None => (percent_decode(&target), String::new()),
    };
    let query: BTreeMap<String, String> = raw_query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (name.to_owned(), percent_decode(value)),
            None => (pair.to_owned(), String::new()),
        })
        .collect();

    bucket.requests.lock().unwrap().push(RecordedRequest {
        method: method.clone(),
        path: path.clone(),
        query: query.clone(),
        authorization: header("authorization"),
        if_match: header("if-match"),
    });

    if method == "GET" && query.contains_key("list-type") {
        return serve_listing(&mut stream, bucket, &query);
    }
    if method == "GET" {
        return serve_object(&mut stream, bucket, &path, header("if-match"));
    }
    write_response(&mut stream, 405, &[], b"")
}

fn serve_listing(
    stream: &mut TcpStream,
    bucket: &FakeBucket,
    query: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    let prefix = query.get("prefix").cloned().unwrap_or_default();
    let configured = bucket.page_size.load(Ordering::Relaxed);
    let requested = query
        .get("max-keys")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let page_size = if configured == 0 {
        requested
    } else {
        configured
    };
    let loop_cursor = bucket.loop_cursor.load(Ordering::Relaxed);
    let outside = bucket.serve_outside_key.load(Ordering::Relaxed);

    let mut keys: Vec<String> = bucket
        .objects
        .lock()
        .unwrap()
        .keys()
        .filter(|key| key.starts_with(&prefix))
        .cloned()
        .collect();
    keys.sort();
    if outside {
        keys.push("outside/leak.md".to_owned());
    }

    let mut offset = 0_usize;
    if !loop_cursor
        && let Some(token) = query.get("continuation-token")
        && let Some(index) = token
            .strip_prefix("page:")
            .and_then(|index| index.parse::<usize>().ok())
    {
        offset = index;
    }
    let take = if loop_cursor {
        1
    } else if page_size == 0 {
        keys.len()
    } else {
        page_size
    };
    let page: Vec<String> = keys.iter().skip(offset).take(take).cloned().collect();
    let truncated = loop_cursor || offset + page.len() < keys.len();
    let next_token = if loop_cursor {
        "page:0".to_owned()
    } else if truncated {
        format!("page:{}", offset + page.len())
    } else {
        String::new()
    };

    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult>");
    xml.push_str(&format!(
        "<Name>{BUCKET}</Name><Prefix>{}</Prefix>",
        escape(&prefix)
    ));
    xml.push_str(&format!("<KeyCount>{}</KeyCount>", page.len()));
    xml.push_str(&format!(
        "<IsTruncated>{}</IsTruncated>",
        if truncated { "true" } else { "false" }
    ));
    if truncated {
        xml.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            escape(&next_token)
        ));
    }
    let objects = bucket.objects.lock().unwrap();
    for key in &page {
        xml.push_str("<Contents>");
        xml.push_str(&format!("<Key>{}</Key>", escape(key)));
        let (last_modified, etag, size) = match objects.get(key) {
            Some(object) => (
                object.last_modified.clone(),
                object.etag.clone(),
                object.listed_size.unwrap_or(object.bytes.len() as u64),
            ),
            None => (
                "2024-01-01T00:00:00.000Z".to_owned(),
                "unknown".to_owned(),
                0,
            ),
        };
        xml.push_str(&format!(
            "<LastModified>{}</LastModified><ETag>&quot;{}&quot;</ETag><Size>{size}</Size>",
            escape(&last_modified),
            escape(&etag)
        ));
        xml.push_str("</Contents>");
    }
    drop(objects);
    write_response(stream, 200, &[], xml.as_bytes())
}

fn serve_object(
    stream: &mut TcpStream,
    bucket: &FakeBucket,
    path: &str,
    if_match: Option<String>,
) -> std::io::Result<()> {
    let key = path
        .strip_prefix(&format!("/{BUCKET}/"))
        .unwrap_or(path)
        .to_owned();
    let object = bucket.objects.lock().unwrap().get(&key).cloned();
    let Some(object) = object else {
        return write_response(stream, 404, &[], b"");
    };
    if !object.readable {
        return write_response(stream, 404, &[], b"");
    }

    let served_etag = object
        .served_etag
        .clone()
        .unwrap_or_else(|| object.etag.clone());
    if let Some(if_match) = if_match
        && if_match.trim_matches('"') != served_etag
    {
        return write_response(stream, 412, &[], b"");
    }

    let body = match object.truncated_at {
        Some(limit) => &object.bytes[..limit.min(object.bytes.len())],
        None => &object.bytes[..],
    };
    let response_etag = object.response_etag.clone().unwrap_or(served_etag);
    let mut headers: Vec<(String, String)> = Vec::new();
    headers.push(("etag".to_owned(), format!("\"{response_etag}\"")));
    write_response_with(stream, 200, &headers, body, Some(object.bytes.len() as u64))
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    extra: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let headers = extra
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<Vec<_>>();
    write_response_with(stream, status, &headers, body, None)
}

/// Writes one HTTP/1.1 response. `declared_length` lets a test announce more bytes
/// than it sends, which is exactly what an interrupted transfer looks like.
fn write_response_with(
    stream: &mut TcpStream,
    status: u16,
    extra: &[(String, String)],
    body: &[u8],
    declared_length: Option<u64>,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        412 => "Precondition Failed",
        _ => "Status",
    };
    let length = declared_length.unwrap_or(body.len() as u64);
    let mut response =
        format!("HTTP/1.1 {status} {reason}\r\ncontent-length: {length}\r\nconnection: close\r\n");
    for (name, value) in extra {
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = bytes.get(index + 1..index + 3)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(byte);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).unwrap_or_default()
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "mineral-publisher-r2-source-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn cas(&self) -> PathBuf {
        self.0.join("cas")
    }

    fn materializations(&self) -> PathBuf {
        self.0.join("source-materializations.sqlite3")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One source reading through one fake server, rebuilt from durable state the way a
/// second run would be.
struct Harness {
    server: FakeServer,
    directory: TestDirectory,
}

impl Harness {
    fn new() -> Self {
        Self {
            server: FakeServer::start(),
            directory: TestDirectory::new(),
        }
    }

    fn source(&self) -> R2Source {
        self.source_with_page_size(0)
    }

    fn source_with_page_size(&self, page_size: u32) -> R2Source {
        let config = R2ObjectStoreConfig::new(
            self.server.endpoint.clone(),
            BUCKET,
            "access-key-id",
            R2SecretKey::new("secret-access-key").unwrap(),
        )
        .unwrap();
        let source = R2Source::new(
            config,
            R2SourcePrefix::new(PREFIX).unwrap(),
            LocalContentStore::new(self.directory.cas()),
            SqliteSourceMaterializationStore::open(self.directory.materializations()).unwrap(),
        )
        .unwrap();
        if page_size == 0 {
            source
        } else {
            source.with_page_size(page_size)
        }
    }

    fn scan(&self) -> Result<StabilizedSource, SourceScanError<R2SourceError>> {
        stabilize_scan(&self.source(), DEFAULT_MAX_SCAN_ATTEMPTS)
    }

    fn store(&self) -> LocalContentStore {
        LocalContentStore::new(self.directory.cas())
    }
}

fn snapshot_of(stabilized: &StabilizedSource) -> Snapshot {
    paths_of(stabilized);
    stabilized
        .snapshot(
            SnapshotId::new(1).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("r2-vault").unwrap(),
        )
        .unwrap()
}

fn paths_of(stabilized: &StabilizedSource) -> Vec<String> {
    stabilized
        .materialized()
        .entries()
        .iter()
        .map(|entry| entry.path().as_str().to_owned())
        .collect()
}

#[test]
fn an_empty_namespace_produces_an_empty_source_state() {
    let harness = Harness::new();

    let stabilized = harness.scan().unwrap();

    assert!(stabilized.materialized().is_empty());
    assert!(snapshot_of(&stabilized).files().is_empty());
}

#[test]
fn a_prefix_scopes_the_namespace_exactly() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/index.md", "e1", b"index");
    harness.server.put_bytes("vault/notes/a.md", "e2", b"note");
    harness
        .server
        .put_bytes("vault/旅行/照片.png", "e3", b"photo");
    // Outside the prefix: present in the bucket, never part of this source.
    harness.server.put_bytes("other/index.md", "e4", b"other");

    let stabilized = harness.scan().unwrap();

    assert_eq!(
        paths_of(&stabilized),
        ["index.md", "notes/a.md", "旅行/照片.png"]
    );
    let snapshot = snapshot_of(&stabilized);
    let photo = snapshot
        .files()
        .iter()
        .find(|file| file.path().as_str() == "旅行/照片.png")
        .unwrap();
    assert_eq!(photo.sha256(), Sha256::digest(b"photo"));
}

#[test]
fn a_listing_that_reports_a_key_outside_the_prefix_fails_closed() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/index.md", "e1", b"index");
    harness
        .server
        .bucket
        .serve_outside_key
        .store(true, Ordering::Relaxed);

    let error = harness.scan().unwrap_err();

    assert!(
        matches!(
            error,
            SourceScanError::Scan(R2SourceError::Key { ref key, .. }) if key == "outside/leak.md"
        ),
        "{error}"
    );
}

#[test]
fn directory_markers_are_not_files_and_a_non_empty_one_fails_closed() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/index.md", "e1", b"index");
    // A zero-byte object whose key ends in a separator is a folder marker.
    harness.server.put_bytes("vault/private/", "e2", b"");
    harness.server.put_bytes("vault/empty.md", "e3", b"");

    let stabilized = harness.scan().unwrap();

    assert_eq!(paths_of(&stabilized), ["empty.md", "index.md"]);

    // A key ending in a separator that holds bytes is not a marker and not a file.
    let broken = Harness::new();
    broken
        .server
        .put_bytes("vault/not-a-marker/", "e1", b"bytes");
    assert!(
        matches!(
            broken.scan().unwrap_err(),
            SourceScanError::Scan(R2SourceError::NonZeroDirectoryMarker { ref key })
                if key == "vault/not-a-marker/"
        ),
        "a non-empty directory marker must fail closed"
    );
}

#[test]
fn every_listing_page_is_read_into_one_deterministic_inventory() {
    let harness = Harness::new();
    for (index, name) in ["c.md", "a.md", "b.md", "d.md"].iter().enumerate() {
        harness.server.put_bytes(
            &format!("vault/{name}"),
            &format!("e{index}"),
            name.as_bytes(),
        );
    }
    let source = harness.source_with_page_size(1);

    let stabilized = stabilize_scan(&source, DEFAULT_MAX_SCAN_ATTEMPTS).unwrap();

    assert_eq!(paths_of(&stabilized), ["a.md", "b.md", "c.md", "d.md"]);
    assert!(
        harness.server.bucket.listings() >= 8,
        "four pages per inventory, two inventories: {}",
        harness.server.bucket.listings()
    );
}

#[test]
fn a_listing_that_repeats_its_cursor_fails_closed_instead_of_looping() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"a");
    harness
        .server
        .bucket
        .loop_cursor
        .store(true, Ordering::Relaxed);

    let error = harness.scan().unwrap_err();

    assert!(matches!(
        error,
        SourceScanError::Scan(R2SourceError::PaginationLoop)
    ));
}

#[test]
fn a_multi_chunk_object_hashes_to_the_bytes_it_stored() {
    let harness = Harness::new();
    // Deliberately larger than one read buffer, so the ingest has to be chunked.
    let bytes = (0..(SOURCE_CHUNK_BYTES * 2 + 1234))
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect::<Vec<_>>();
    harness
        .server
        .put_bytes("vault/big.bin", "big-etag", &bytes);

    let stabilized = harness.scan().unwrap();

    let file = snapshot_of(&stabilized).files()[0].clone();
    assert_eq!(file.size(), bytes.len() as u64);
    assert_eq!(file.sha256(), Sha256::digest(&bytes));
    assert_eq!(harness.store().read(file.sha256()).unwrap(), bytes);
    assert_eq!(file.content_type(), None);
}

#[test]
fn an_unchanged_namespace_is_never_read_again() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/index.md", "e1", b"index");
    harness.server.put_bytes("vault/notes/a.md", "e2", b"note");

    harness.scan().unwrap();
    let after_first = harness.server.bucket.reads().len();
    let second = harness.scan().unwrap();

    assert_eq!(after_first, 2, "the first run reads every object once");
    assert_eq!(
        harness.server.bucket.reads().len(),
        after_first,
        "an unchanged revision must not be fetched again"
    );
    assert_eq!(snapshot_of(&second).files().len(), 2);
}

#[test]
fn only_the_changed_object_is_read_again() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"a");
    harness.server.put_bytes("vault/b.md", "e2", b"b");
    harness.server.put_bytes("vault/c.md", "e3", b"c");
    harness.scan().unwrap();

    harness.server.bucket.mutate("vault/b.md", |object| {
        object.bytes = b"b changed".to_vec();
        object.etag = "e2-new".to_owned();
        object.last_modified = "2024-02-02T00:00:00.000Z".to_owned();
    });
    let before = harness.server.bucket.reads().len();

    let stabilized = harness.scan().unwrap();

    assert_eq!(harness.server.bucket.read_keys()[before..], ["vault/b.md"]);
    let files = snapshot_of(&stabilized).files().to_vec();
    assert_eq!(files[1].sha256(), Sha256::digest(b"b changed"));
    assert_eq!(files[0].sha256(), Sha256::digest(b"a"));
}

#[test]
fn a_deleted_object_leaves_the_next_source_state() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"a");
    harness.server.put_bytes("vault/b.md", "e2", b"b");
    harness.scan().unwrap();

    harness.server.bucket.remove("vault/b.md");

    let stabilized = harness.scan().unwrap();

    assert_eq!(paths_of(&stabilized), ["a.md"]);
}

#[test]
fn an_object_that_changed_after_the_listing_is_refused() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"a");
    // The listing still reports `e1`, but the read is served a different generation.
    harness.server.bucket.mutate("vault/a.md", |object| {
        object.served_etag = Some("e2".to_owned());
    });

    let error = harness.scan().unwrap_err();

    assert!(
        matches!(
            error,
            SourceScanError::Scan(R2SourceError::RemoteRevisionChanged { .. })
        ),
        "{error}"
    );
    assert_eq!(
        harness.store().probe(Sha256::digest(b"a")).unwrap(),
        None,
        "a refused read must leave no blob behind"
    );
}

#[test]
fn a_read_that_answers_with_a_different_revision_is_refused() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"a");
    // The endpoint honours `If-Match` for `e1` but answers with a newer generation's
    // ETag, so the listing's revision and the served bytes cannot be reconciled.
    harness.server.bucket.mutate("vault/a.md", |object| {
        object.response_etag = Some("e2".to_owned());
    });

    let error = harness.scan().unwrap_err();

    assert!(
        matches!(
            error,
            SourceScanError::Scan(R2SourceError::RevisionChangedOnResponse { .. })
        ),
        "{error}"
    );
}

#[test]
fn an_object_that_disappeared_after_the_listing_is_an_error_not_an_empty_file() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"a");
    harness.server.bucket.mutate("vault/a.md", |object| {
        object.readable = false;
    });

    let error = harness.scan().unwrap_err();

    assert!(
        matches!(
            error,
            SourceScanError::Scan(R2SourceError::ObjectDisappeared { .. })
        ),
        "{error}"
    );
}

#[test]
fn a_size_the_remote_does_not_honour_fails_closed() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"four");
    // The listing says four bytes and the read announces five.
    harness.server.bucket.mutate("vault/a.md", |object| {
        object.bytes = b"five!".to_vec();
        object.listed_size = Some(4);
    });

    let error = harness.scan().unwrap_err();

    assert!(
        matches!(
            error,
            SourceScanError::Scan(R2SourceError::ContentLengthMismatch { .. })
        ),
        "{error}"
    );
}

#[test]
fn a_listed_size_that_does_not_match_the_bytes_fails_closed() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"four");
    // The listing overstates the size; the read sends the real bytes and announces
    // them, so the contradiction is caught without a truncated transfer.
    harness.server.bucket.mutate("vault/a.md", |object| {
        object.listed_size = Some(9);
    });

    let error = harness.scan().unwrap_err();

    assert!(
        matches!(
            error,
            SourceScanError::Scan(R2SourceError::ContentLengthMismatch { .. })
                | SourceScanError::Scan(R2SourceError::SizeMismatch { .. })
        ),
        "{error}"
    );
}

#[test]
fn an_interrupted_read_leaves_no_blob_and_no_binding() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"0123456789");
    harness.server.bucket.mutate("vault/a.md", |object| {
        object.truncated_at = Some(4);
    });

    let error = harness.scan().unwrap_err();

    assert!(
        matches!(error, SourceScanError::Scan(R2SourceError::Read { .. })),
        "{error}"
    );
    assert_eq!(
        harness
            .store()
            .probe(Sha256::digest(b"0123456789"))
            .unwrap(),
        None
    );
    assert_eq!(
        std::fs::read_dir(harness.directory.cas())
            .map(|entries| entries.count())
            .unwrap_or(0),
        0,
        "an abandoned ingest must not leave a temporary file behind"
    );
    let store =
        SqliteSourceMaterializationStore::open(harness.directory.materializations()).unwrap();
    assert_eq!(
        store
            .get(
                harness.source().identity(),
                &ContentPath::new("a.md").unwrap(),
                &SourceRevision::versioned("anything").unwrap(),
            )
            .unwrap(),
        None
    );
}

#[test]
fn a_binding_whose_blob_disappeared_is_refetched_rather_than_trusted() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"a");
    let first = harness.scan().unwrap();
    let identity = snapshot_of(&first).files()[0].sha256();
    std::fs::remove_file(harness.directory.cas().join(identity.to_string())).unwrap();
    let before = harness.server.bucket.reads().len();

    let second = harness.scan().unwrap();

    assert_eq!(harness.server.bucket.read_keys()[before..], ["vault/a.md"]);
    assert_eq!(snapshot_of(&second).files()[0].sha256(), identity);
}

#[test]
fn an_excluded_folder_is_still_read_and_materialized() {
    // The reader has no notion of public scope: every object under the prefix is read
    // and materialized, and exclusion happens later in the pipeline.
    let harness = Harness::new();
    harness.server.put_bytes("vault/index.md", "e1", b"index");
    harness
        .server
        .put_bytes("vault/private/passwords.md", "e2", b"secret");

    let stabilized = harness.scan().unwrap();

    assert_eq!(paths_of(&stabilized), ["index.md", "private/passwords.md"]);
    assert_eq!(harness.server.bucket.reads().len(), 2);
    assert_eq!(
        harness.store().read(Sha256::digest(b"secret")).unwrap(),
        b"secret"
    );
}

#[test]
fn a_namespace_that_never_holds_still_produces_no_source_state() {
    /// A source whose every listing reports a new ETag for the same key.
    struct Churning<'a> {
        source: &'a R2Source,
        bucket: &'a Arc<FakeBucket>,
        listings: AtomicUsize,
    }

    impl SourceScan for Churning<'_> {
        type Error = R2SourceError;

        fn inventory(&self) -> Result<SourceInventory, Self::Error> {
            let round = self.listings.fetch_add(1, Ordering::Relaxed);
            let etag = format!("e{round}");
            self.bucket.mutate("vault/a.md", |object| {
                object.etag = etag;
            });
            SourceScan::inventory(self.source)
        }

        fn materialize(
            &self,
            inventory: &SourceInventory,
        ) -> Result<SourceMaterializationSet, Self::Error> {
            SourceScan::materialize(self.source, inventory)
        }
    }

    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e0", b"a");
    let source = harness.source();
    let churning = Churning {
        source: &source,
        bucket: &harness.server.bucket,
        listings: AtomicUsize::new(0),
    };

    let error = stabilize_scan(&churning, DEFAULT_MAX_SCAN_ATTEMPTS).unwrap_err();

    assert!(
        matches!(error, SourceScanError::Unstable { attempts: 3 }),
        "{error}"
    );
}

/// The reader hands the store bounded chunks: a reader that refuses to be asked for
/// the whole object at once still succeeds.
#[test]
fn the_ingest_asks_its_source_for_bounded_chunks_only() {
    struct BoundedOnly {
        remaining: Vec<u8>,
        maximum_request: usize,
        requests: usize,
    }

    impl Read for BoundedOnly {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            assert!(
                buffer.len() <= self.maximum_request,
                "the ingest asked for {} bytes at once",
                buffer.len()
            );
            self.requests += 1;
            let take = buffer.len().min(self.remaining.len());
            buffer[..take].copy_from_slice(&self.remaining[..take]);
            self.remaining.drain(..take);
            Ok(take)
        }
    }

    #[derive(Default)]
    struct RecordingWriter {
        chunks: Vec<usize>,
    }

    impl mineral_core::ports::BlobWriter for RecordingWriter {
        fn write(&mut self, chunk: &[u8]) -> Result<(), mineral_core::ports::ContentStoreError> {
            self.chunks.push(chunk.len());
            Ok(())
        }

        fn finish(
            self: Box<Self>,
        ) -> Result<mineral_core::ports::StoredBlob, mineral_core::ports::ContentStoreError>
        {
            Ok(mineral_core::ports::StoredBlob::new(
                Sha256::digest(b"unused"),
                0,
            ))
        }
    }

    let total = SOURCE_CHUNK_BYTES * 3 + 17;
    let mut reader = BoundedOnly {
        remaining: vec![7_u8; total],
        maximum_request: SOURCE_CHUNK_BYTES,
        requests: 0,
    };
    let mut writer = RecordingWriter::default();

    let received =
        stream_object(&mut reader, &mut writer, total as u64, SOURCE_CHUNK_BYTES).unwrap();

    assert_eq!(received, total as u64);
    assert!(writer.chunks.len() >= 4, "{:?}", writer.chunks);
    assert!(
        writer.chunks.iter().all(|size| *size <= SOURCE_CHUNK_BYTES),
        "{:?}",
        writer.chunks
    );
    assert!(reader.requests >= 4);
}

#[test]
fn the_bucket_root_must_be_asked_for_explicitly() {
    let root = R2SourcePrefix::new("").unwrap();

    assert!(root.is_root());
    assert_eq!(
        root.content_path("daily/a.md").unwrap().as_str(),
        "daily/a.md"
    );
    assert!(matches!(
        R2SourcePrefix::new("/"),
        Err(R2SourcePrefixError::NotCanonical(_))
    ));
    assert!(R2SourcePrefix::new("//").is_err());
}

/// The namespace the reader is configured for is part of every durable binding, and
/// a different namespace is a different identity.
#[test]
fn the_namespace_identity_covers_endpoint_bucket_and_prefix() {
    let harness = Harness::new();
    let first = harness.source();
    let config = || {
        R2ObjectStoreConfig::new(
            harness.server.endpoint.clone(),
            BUCKET,
            "access-key-id",
            R2SecretKey::new("secret-access-key").unwrap(),
        )
        .unwrap()
    };
    let other_prefix = R2Source::new(
        config(),
        R2SourcePrefix::new("other").unwrap(),
        LocalContentStore::new(harness.directory.cas()),
        SqliteSourceMaterializationStore::open(harness.directory.materializations()).unwrap(),
    )
    .unwrap();
    let other_bucket = R2Source::new(
        R2ObjectStoreConfig::new(
            harness.server.endpoint.clone(),
            "another-bucket",
            "access-key-id",
            R2SecretKey::new("secret-access-key").unwrap(),
        )
        .unwrap(),
        R2SourcePrefix::new(PREFIX).unwrap(),
        LocalContentStore::new(harness.directory.cas()),
        SqliteSourceMaterializationStore::open(harness.directory.materializations()).unwrap(),
    )
    .unwrap();

    assert_ne!(first.identity(), other_prefix.identity());
    assert_ne!(first.identity(), other_bucket.identity());
    assert_eq!(first.identity(), harness.source().identity());
    assert!(!first.describe().contains("secret-access-key"));
}

#[test]
fn a_conditional_read_is_signed_and_carries_the_listed_revision() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"a");

    harness.scan().unwrap();

    let reads = harness.server.bucket.reads();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].if_match.as_deref(), Some("\"e1\""));
    assert!(
        reads[0]
            .authorization
            .as_deref()
            .unwrap()
            .contains("SignedHeaders=")
    );
    assert!(reads[0].query.is_empty());
    let listing = harness
        .server
        .bucket
        .requests()
        .into_iter()
        .find(|request| request.query.contains_key("list-type"))
        .unwrap();
    assert_eq!(
        listing.query.get("prefix").map(String::as_str),
        Some(PREFIX)
    );
    assert_eq!(
        listing.query.get("list-type").map(String::as_str),
        Some("2")
    );
}

#[test]
fn a_binding_records_the_revision_it_read() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"a");
    let stabilized = harness.scan().unwrap();

    let entry = stabilized.inventory().entries()[0].clone();
    let source = harness.source();
    let store =
        SqliteSourceMaterializationStore::open(harness.directory.materializations()).unwrap();
    let binding = store
        .get(
            source.identity(),
            &ContentPath::new("a.md").unwrap(),
            entry.revision(),
        )
        .unwrap()
        .unwrap();

    assert_eq!(binding.content_sha256(), Sha256::digest(b"a"));
    assert_eq!(binding.content_size(), 1);
    assert_eq!(binding.revision(), entry.revision());
    let encoded = entry.revision().encoded();
    assert!(encoded.starts_with("v1:"));
    assert!(encoded.contains("etag="));
    let decoded = SourceRevision::decode(encoded).unwrap();
    assert_eq!(decoded.version(), "v1");
    let _ = SourceMaterialization::new(
        binding.source(),
        binding.path().clone(),
        binding.revision().clone(),
        binding.content_sha256(),
        binding.content_size(),
    );
}

#[test]
fn a_second_run_over_the_same_durable_state_reuses_every_binding() {
    let harness = Harness::new();
    harness.server.put_bytes("vault/a.md", "e1", b"a");
    harness.server.put_bytes("vault/b.md", "e2", b"b");
    harness.scan().unwrap();

    // A brand-new reader over the same CAS, database and namespace: this is what the
    // next process looks like.
    let second = harness.scan().unwrap();

    assert_eq!(harness.server.bucket.reads().len(), 2);
    assert_eq!(snapshot_of(&second).files().len(), 2);
    assert!(harness.server.bucket.listings() >= 4);
}
