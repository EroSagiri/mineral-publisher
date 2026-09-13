//! The adapter is exercised against a real HTTP server: an in-process socket that
//! speaks enough of the object-store API for the client, the signing code and the
//! streaming upload path to run unmodified.
//!
//! Nothing here is mocked at the Rust level. `reqwest` opens a TCP connection to a
//! `TcpListener`, the request it sends is parsed from the wire, and the request the
//! test inspects is the bytes that actually crossed the socket. What the fake
//! server does not do is validate the signature the way a real bucket would; that
//! check lives in the ignored live test below.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use crate::{
    asset::{
        AssetByteIdentity, AssetTarget, AssetTargetState, BufferedBlobSource, ObjectStoreTransport,
        StreamingAssetTarget,
    },
    domain::{ContentPath, Sha256},
    workflow::{AssetContentType, AssetDeliveryConfig, PublishedAsset},
};

use super::{R2ObjectStore, R2ObjectStoreConfig, R2ObjectStoreError, R2SecretKey};

const BUCKET: &str = "mineral-assets";
const ACCESS_KEY_ID: &str = "AKIDEXAMPLE";
const SECRET_ACCESS_KEY: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const BODY: &[u8] = b"the one published representation of this asset";

fn asset(body: &[u8]) -> PublishedAsset {
    PublishedAsset::from_parts_for_test(
        ContentPath::new("img/a.bin").unwrap(),
        Sha256::new([1; 32]),
        Sha256::digest(body),
        body.len() as u64,
        AssetContentType::new("image/png").unwrap(),
        &AssetDeliveryConfig::new("https://assets.example.com").unwrap(),
    )
}

/// One request exactly as it arrived on the socket.
#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    content_length: Option<u64>,
    payload_sha256: Option<String>,
    content_type: Option<String>,
    if_none_match: Option<String>,
    body: Vec<u8>,
}

#[derive(Clone, Debug)]
struct StoredObject {
    bytes: Vec<u8>,
    content_type: Option<String>,
}

/// A bucket that answers HEAD, GET and PUT, and remembers what it was asked.
#[derive(Default)]
struct FakeBucket {
    objects: Mutex<HashMap<String, StoredObject>>,
    requests: Mutex<Vec<RecordedRequest>>,
    /// Injected failures, so a test can see how the adapter reports them.
    head_status: Mutex<Option<u16>>,
    get_status: Mutex<Option<u16>>,
    put_status: Mutex<Option<u16>>,
    /// When set, objects are served without a `content-type` header.
    hide_content_type: AtomicBool,
}

impl FakeBucket {
    fn store(&self, key: &str, object: StoredObject) {
        self.objects.lock().unwrap().insert(key.to_owned(), object);
    }

    fn object(&self, key: &str) -> Option<StoredObject> {
        self.objects.lock().unwrap().get(key).cloned()
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn methods(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|request| request.method)
            .collect()
    }

    fn count(&self, method: &str) -> usize {
        self.methods()
            .iter()
            .filter(|other| *other == method)
            .count()
    }
}

/// A running fake bucket, plus the endpoint that reaches it.
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
        // The accept loop ends when the process does: it holds no state a test
        // needs to release, and every test binds its own port.
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let bucket = Arc::clone(&served);
                // One request per connection, always closing afterwards, which is
                // the simplest thing that cannot desynchronize a keep-alive stream.
                let _ = handle(stream, &bucket);
            }
        });
        Self {
            bucket,
            endpoint: format!("http://{address}"),
        }
    }

    fn store(&self, object_key: &str, bytes: &[u8], content_type: Option<&str>) {
        self.bucket.store(
            &format!("/{BUCKET}/{object_key}"),
            StoredObject {
                bytes: bytes.to_vec(),
                content_type: content_type.map(str::to_owned),
            },
        );
    }

    fn object(&self, object_key: &str) -> Option<StoredObject> {
        self.bucket.object(&format!("/{BUCKET}/{object_key}"))
    }

    fn store_for(&self, asset: &PublishedAsset, bytes: &[u8], content_type: &str) {
        self.store(asset.object_key().as_str(), bytes, Some(content_type));
    }

    fn transport(&self, spool_directory: &std::path::Path) -> R2ObjectStore {
        self.transport_with(spool_directory.to_path_buf())
    }

    fn transport_with(&self, spool_directory: std::path::PathBuf) -> R2ObjectStore {
        let config = R2ObjectStoreConfig::new(
            self.endpoint.clone(),
            BUCKET,
            ACCESS_KEY_ID,
            R2SecretKey::new(SECRET_ACCESS_KEY).unwrap(),
        )
        .unwrap();
        R2ObjectStore::new(config, spool_directory).unwrap()
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

    bucket.requests.lock().unwrap().push(RecordedRequest {
        method: method.clone(),
        path: path.clone(),
        authorization: header("authorization"),
        content_length: header("content-length").and_then(|value| value.parse().ok()),
        payload_sha256: header("x-amz-content-sha256"),
        content_type: header("content-type"),
        if_none_match: header("if-none-match"),
        body: body.clone(),
    });

    let object = bucket.objects.lock().unwrap().get(&path).cloned();
    let hide_content_type = bucket.hide_content_type.load(Ordering::Relaxed);
    let injected = |slot: &Mutex<Option<u16>>| *slot.lock().unwrap();

    let (status, content_type, response_body): (u16, Option<String>, Vec<u8>) =
        match method.as_str() {
            "HEAD" => match injected(&bucket.head_status) {
                Some(status) => (status, None, Vec::new()),
                None => match object {
                    Some(object) => (
                        200,
                        (!hide_content_type)
                            .then_some(object.content_type)
                            .flatten(),
                        Vec::new(),
                    ),
                    None => (404, None, Vec::new()),
                },
            },
            "GET" => match injected(&bucket.get_status) {
                Some(status) => (status, None, Vec::new()),
                None => match object {
                    Some(object) => (
                        200,
                        (!hide_content_type)
                            .then_some(object.content_type)
                            .flatten(),
                        object.bytes,
                    ),
                    None => (404, None, Vec::new()),
                },
            },
            "PUT" => match injected(&bucket.put_status) {
                Some(status) => (status, None, Vec::new()),
                None => {
                    bucket.objects.lock().unwrap().insert(
                        path.clone(),
                        StoredObject {
                            bytes: body,
                            content_type: header("content-type"),
                        },
                    );
                    (200, None, Vec::new())
                }
            },
            _ => (405, None, Vec::new()),
        };

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

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

fn spool_directory(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("mineral-r2-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    directory
}

fn spool_files(directory: &std::path::Path) -> Vec<String> {
    match std::fs::read_dir(directory) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn present_identity(
    state: &AssetTargetState,
) -> Option<(u64, AssetContentType, AssetByteIdentity)> {
    match state {
        AssetTargetState::Present(facts) => {
            Some((facts.size(), facts.content_type().clone(), facts.bytes()))
        }
        AssetTargetState::Missing => None,
    }
}

#[test]
fn a_missing_object_is_reported_as_missing_after_a_single_head() {
    let server = FakeServer::start();
    let store = server.transport(&spool_directory("missing"));
    let published = asset(BODY);

    let state = store.inspect(published.object_key()).unwrap();

    assert_eq!(state, AssetTargetState::Missing);
    // Missing is decided by the HEAD alone: no bytes are fetched to learn that
    // there are none.
    assert_eq!(server.bucket.methods(), vec!["HEAD".to_owned()]);
}

#[test]
fn a_present_object_is_described_by_its_bytes_not_by_a_header() {
    let server = FakeServer::start();
    let store = server.transport(&spool_directory("present"));
    let published = asset(BODY);
    server.store_for(&published, BODY, "image/png");

    let state = store.inspect(published.object_key()).unwrap();
    let (size, content_type, identity) = present_identity(&state).expect("a present object");

    assert_eq!(size, BODY.len() as u64);
    assert_eq!(content_type.as_str(), "image/png");
    // The identity is the digest of the bytes that were served, so the engine's
    // own judgement can accept it.
    assert_eq!(identity, AssetByteIdentity::Verified(Sha256::digest(BODY)));
    assert!(published.judge(&state).is_ready());
}

#[test]
fn an_object_without_a_content_type_cannot_be_described() {
    let server = FakeServer::start();
    let store = server.transport(&spool_directory("no-content-type"));
    let published = asset(BODY);
    server.store_for(&published, BODY, "image/png");
    server
        .bucket
        .hide_content_type
        .store(true, Ordering::Relaxed);

    let error = store.inspect(published.object_key()).unwrap_err();

    assert!(matches!(
        error,
        R2ObjectStoreError::MissingContentType { .. }
    ));
}

#[test]
fn a_failed_head_is_reported_with_its_status() {
    let server = FakeServer::start();
    let store = server.transport(&spool_directory("head-status"));
    let published = asset(BODY);
    *server.bucket.head_status.lock().unwrap() = Some(500);

    let error = store.inspect(published.object_key()).unwrap_err();

    assert!(matches!(
        error,
        R2ObjectStoreError::UnexpectedStatus {
            operation: "HEAD",
            status: 500
        }
    ));
}

#[test]
fn an_absent_object_is_uploaded_with_the_frozen_facts() {
    let server = FakeServer::start();
    let spool = spool_directory("upload");
    let store = server.transport(&spool);
    let published = asset(BODY);

    let mut writer = store.open_writer(&published).unwrap();
    for chunk in BODY.chunks(7) {
        writer.write(chunk).unwrap();
    }
    writer.finish().unwrap();

    let object = server
        .object(published.object_key().as_str())
        .expect("the object");
    assert_eq!(object.bytes, BODY);
    assert_eq!(object.content_type.as_deref(), Some("image/png"));

    let puts: Vec<RecordedRequest> = server
        .bucket
        .requests()
        .into_iter()
        .filter(|request| request.method == "PUT")
        .collect();
    assert_eq!(puts.len(), 1);
    let put = &puts[0];
    assert_eq!(
        put.path,
        format!("/{BUCKET}/{}", published.object_key().as_str())
    );
    assert_eq!(put.content_length, Some(BODY.len() as u64));
    assert_eq!(put.content_type.as_deref(), Some("image/png"));
    // The signed payload hash is the frozen identity, so the bucket itself checks
    // the body it received against the digest the engine published.
    assert_eq!(
        put.payload_sha256.as_deref(),
        Some(published.published_sha256().to_string().as_str())
    );
    // The upload is a create, not a replace: the bucket must still hold nothing
    // under the key when it accepts the body.
    assert_eq!(put.if_none_match.as_deref(), Some("*"));
    let authorization = put.authorization.clone().expect("a signed request");
    assert!(authorization.starts_with(&format!("AWS4-HMAC-SHA256 Credential={ACCESS_KEY_ID}/")));
    // The media type is a frozen fact, so it is signed; the length is carried by
    // the request but is not part of the signed set, as in every S3 client.
    assert!(
        authorization.contains(
            ", SignedHeaders=content-type;host;if-none-match;x-amz-content-sha256;x-amz-date, "
        ),
        "{authorization}"
    );

    // The spool is not a published object and does not survive the upload.
    assert!(spool_files(&spool).is_empty(), "{:?}", spool_files(&spool));
}

#[test]
fn an_existing_identical_object_is_reused_and_never_rewritten() {
    let server = FakeServer::start();
    let store = server.transport(&spool_directory("reuse"));
    let published = asset(BODY);
    server.store_for(&published, BODY, "image/png");

    let mut writer = store.open_writer(&published).unwrap();
    writer.write(BODY).unwrap();
    writer.finish().unwrap();

    assert_eq!(server.bucket.count("PUT"), 0);
    let object = server
        .object(published.object_key().as_str())
        .expect("the object");
    assert_eq!(object.bytes, BODY);
}

#[test]
fn an_object_that_disagrees_with_the_frozen_facts_is_refused_before_any_upload() {
    let server = FakeServer::start();
    let store = server.transport(&spool_directory("conflict"));
    let published = asset(BODY);
    server.store_for(&published, b"some other representation", "image/png");

    let error = match store.open_writer(&published) {
        Ok(_) => panic!("a conflicting object must not produce a writer"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        R2ObjectStoreError::ConflictingObject { .. }
    ));
    // The wrong object is left exactly as it was: this adapter never overwrites.
    assert_eq!(
        server
            .object(published.object_key().as_str())
            .unwrap()
            .bytes,
        b"some other representation"
    );
    assert_eq!(server.bucket.count("PUT"), 0);
}

#[test]
fn an_unreadable_object_is_refused_rather_than_replaced() {
    let server = FakeServer::start();
    let store = server.transport(&spool_directory("get-status"));
    let published = asset(BODY);
    server.store_for(&published, BODY, "image/png");
    *server.bucket.get_status.lock().unwrap() = Some(500);

    let error = store.inspect(published.object_key()).unwrap_err();

    assert!(matches!(
        error,
        R2ObjectStoreError::UnexpectedStatus {
            operation: "GET",
            status: 500
        }
    ));
}

#[test]
fn a_failed_upload_is_reported_and_leaves_no_spool_behind() {
    let server = FakeServer::start();
    let spool = spool_directory("put-status");
    let store = server.transport(&spool);
    let published = asset(BODY);
    *server.bucket.put_status.lock().unwrap() = Some(500);

    let mut writer = store.open_writer(&published).unwrap();
    writer.write(BODY).unwrap();
    let error = writer.finish().unwrap_err();

    assert!(matches!(
        error,
        R2ObjectStoreError::UnexpectedStatus {
            operation: "PUT",
            status: 500
        }
    ));
    assert!(server.object(published.object_key().as_str()).is_none());
    assert!(spool_files(&spool).is_empty(), "{:?}", spool_files(&spool));
}

#[test]
fn an_abandoned_writer_never_uploads_and_removes_its_spool() {
    let server = FakeServer::start();
    let spool = spool_directory("abandoned");
    let store = server.transport(&spool);
    let published = asset(BODY);

    let mut writer = store.open_writer(&published).unwrap();
    writer.write(BODY).unwrap();
    drop(writer);

    assert_eq!(server.bucket.count("PUT"), 0);
    assert!(spool_files(&spool).is_empty(), "{:?}", spool_files(&spool));
}

#[test]
fn a_driver_upload_of_a_multi_chunk_object_is_verified_again_by_reading_it_back() {
    let server = FakeServer::start();
    let spool = spool_directory("large");
    let store = server.transport(&spool);
    // Several times the driver's chunk size, so the object crosses the socket in
    // many pieces and is never held as one buffer by the driver.
    let body: Vec<u8> = (0..200_000_u32).map(|index| (index % 251) as u8).collect();
    let published = asset(&body);

    let driver = StreamingAssetTarget::with_transport(store).with_chunk_size(4096);
    let mut source = BufferedBlobSource::new(published.published_sha256(), body.clone());
    driver.publish(&published, &mut source).unwrap();

    // The upload is only accepted because the driver verified the stream it sent.
    let stored = server
        .object(published.object_key().as_str())
        .expect("the object");
    assert_eq!(stored.bytes, body);
    assert_eq!(stored.content_type.as_deref(), Some("image/png"));
    let put = server
        .bucket
        .requests()
        .into_iter()
        .find(|request| request.method == "PUT")
        .expect("the upload");
    // One length-delimited request: the object was never held whole to send it.
    assert_eq!(put.content_length, Some(body.len() as u64));
    assert_eq!(put.body.len(), body.len());

    // Re-inspecting reads the object back and reports the frozen identity.
    let state = driver.inspect(published.object_key()).unwrap();
    let (size, content_type, identity) = present_identity(&state).expect("a present object");
    assert_eq!(size, body.len() as u64);
    assert_eq!(content_type.as_str(), "image/png");
    assert_eq!(identity, AssetByteIdentity::Verified(Sha256::digest(&body)));
    assert!(published.judge(&state).is_ready());
    assert!(spool_files(&spool).is_empty(), "{:?}", spool_files(&spool));
}

#[test]
fn a_source_that_is_not_the_frozen_representation_never_reaches_the_bucket() {
    let server = FakeServer::start();
    let spool = spool_directory("wrong-source");
    let store = server.transport(&spool);
    let published = asset(BODY);

    let driver = StreamingAssetTarget::with_transport(store).with_chunk_size(4);
    // Same length, different bytes: only the engine's own rule catches this.
    let mut source = BufferedBlobSource::new(published.published_sha256(), vec![b'x'; BODY.len()]);
    let error = driver.publish(&published, &mut source).unwrap_err();

    assert!(matches!(
        error,
        crate::asset::StreamingAssetTargetError::Content(_)
    ));
    assert_eq!(server.bucket.count("PUT"), 0);
    assert!(server.object(published.object_key().as_str()).is_none());
    assert!(spool_files(&spool).is_empty(), "{:?}", spool_files(&spool));
}

#[test]
fn a_source_with_another_identity_is_refused_before_the_store_is_touched() {
    let server = FakeServer::start();
    let store = server.transport(&spool_directory("identity"));
    let published = asset(BODY);

    let driver = StreamingAssetTarget::with_transport(store);
    let mut source = BufferedBlobSource::new(Sha256::digest(b"another blob"), BODY.to_vec());
    let error = driver.publish(&published, &mut source).unwrap_err();

    assert!(matches!(
        error,
        crate::asset::StreamingAssetTargetError::SourceIdentityMismatch { .. }
    ));
    assert!(server.bucket.requests().is_empty());
}

#[test]
fn the_signature_covers_the_real_request_that_was_sent() {
    let server = FakeServer::start();
    let store = server.transport(&spool_directory("signature"));
    let published = asset(BODY);

    let state = store.inspect(published.object_key()).unwrap();
    assert_eq!(state, AssetTargetState::Missing);

    let head = server.bucket.requests().first().cloned().expect("the HEAD");
    let authorization = head.authorization.expect("a signed request");
    // A canonical request is deterministic, so the same request signed twice at
    // the same instant yields the same signature; what this asserts is that the
    // header set the signature names is exactly the set that was sent.
    let signed_headers = authorization
        .split("SignedHeaders=")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .expect("the signed header list");
    assert_eq!(signed_headers, "host;x-amz-content-sha256;x-amz-date");
    assert_eq!(
        head.path,
        format!("/{BUCKET}/{}", published.object_key().as_str())
    );
    // A bodyless request signs the hash of no bytes.
    assert_eq!(
        head.payload_sha256.as_deref(),
        Some(Sha256::digest(b"").to_string().as_str())
    );
}

#[test]
fn an_endpoint_without_a_scheme_or_a_trailing_slash_is_refused() {
    for endpoint in ["127.0.0.1:9000", "https://bucket.example.com/"] {
        let config = R2ObjectStoreConfig::new(
            endpoint,
            BUCKET,
            ACCESS_KEY_ID,
            R2SecretKey::new(SECRET_ACCESS_KEY).unwrap(),
        );
        assert!(matches!(
            config,
            Err(crate::asset::r2::R2ObjectStoreConfigError::InvalidEndpoint)
        ));
    }
    assert!(R2SecretKey::new("").is_err());
}

#[test]
fn an_unreachable_endpoint_is_an_explicit_transport_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let config = R2ObjectStoreConfig::new(
        format!("http://{address}"),
        BUCKET,
        ACCESS_KEY_ID,
        R2SecretKey::new(SECRET_ACCESS_KEY).unwrap(),
    )
    .unwrap()
    .with_timeout(std::time::Duration::from_millis(500));
    let store = R2ObjectStore::new(config, spool_directory("unreachable")).unwrap();
    let published = asset(BODY);

    let error = store.inspect(published.object_key()).unwrap_err();

    assert!(matches!(
        error,
        R2ObjectStoreError::Transport(_) | R2ObjectStoreError::Client(_)
    ));
}

#[test]
fn a_secret_is_never_printed() {
    let config = R2ObjectStoreConfig::new(
        "https://bucket.example.com",
        BUCKET,
        ACCESS_KEY_ID,
        R2SecretKey::new(SECRET_ACCESS_KEY).unwrap(),
    )
    .unwrap();

    let printed = format!("{config:?}");

    assert!(!printed.contains(SECRET_ACCESS_KEY));
    assert!(
        !format!("{:?}", R2SecretKey::new(SECRET_ACCESS_KEY).unwrap()).contains(SECRET_ACCESS_KEY)
    );
    assert!(printed.contains(ACCESS_KEY_ID));
}

/// A live bucket, when one is configured.
///
/// The default test run touches no network and needs no credentials. Set
/// `MINERAL_R2_ENDPOINT`, `MINERAL_R2_BUCKET`, `MINERAL_R2_ACCESS_KEY_ID` and
/// `MINERAL_R2_SECRET_ACCESS_KEY` and run with `--ignored` to exercise the
/// adapter against the real service, where the signature is actually validated.
#[test]
#[ignore = "requires a live bucket and credentials"]
fn a_live_bucket_accepts_a_publish_and_serves_the_frozen_bytes_back() {
    let (Ok(endpoint), Ok(bucket), Ok(access_key_id), Ok(secret)) = (
        std::env::var("MINERAL_R2_ENDPOINT"),
        std::env::var("MINERAL_R2_BUCKET"),
        std::env::var("MINERAL_R2_ACCESS_KEY_ID"),
        std::env::var("MINERAL_R2_SECRET_ACCESS_KEY"),
    ) else {
        panic!("the live bucket test needs MINERAL_R2_* configuration");
    };
    let config = R2ObjectStoreConfig::new(
        endpoint,
        bucket,
        access_key_id,
        R2SecretKey::new(secret).unwrap(),
    )
    .unwrap();
    let store = R2ObjectStore::new(config, spool_directory("live")).unwrap();

    // A unique key per run: the live test must never depend on state left behind.
    let body: Vec<u8> = (0..150_000_u32).map(|index| (index % 253) as u8).collect();
    let key = format!(
        "mineral-live/{}/object.bin",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let published = PublishedAsset::from_parts_for_test(
        ContentPath::new(key.as_str()).unwrap(),
        Sha256::new([2; 32]),
        Sha256::digest(&body),
        body.len() as u64,
        AssetContentType::new("application/octet-stream").unwrap(),
        &AssetDeliveryConfig::new("https://assets.example.com").unwrap(),
    );

    // The key is content-addressed, so an earlier run against the same bucket left
    // the very same object under it. Either state is a valid starting point:
    // publishing is idempotent, and anything else under the key is a real failure.
    match store.inspect(published.object_key()).unwrap() {
        AssetTargetState::Missing => {}
        present => assert!(
            published.judge(&present).is_ready(),
            "the live bucket holds something else under the frozen key: {present:?}"
        ),
    }

    let driver = StreamingAssetTarget::with_transport(store).with_chunk_size(8 * 1024);
    let mut source = BufferedBlobSource::new(published.published_sha256(), body.clone());
    driver.publish(&published, &mut source).unwrap();

    // Read back: the served bytes, their size, their media type and their digest.
    let state = driver.inspect(published.object_key()).unwrap();
    let (size, content_type, identity) = present_identity(&state).expect("a present object");
    assert_eq!(size, body.len() as u64);
    assert_eq!(content_type.as_str(), "application/octet-stream");
    assert_eq!(identity, AssetByteIdentity::Verified(Sha256::digest(&body)));
    assert!(published.judge(&state).is_ready());

    // Publishing again is a no-op: the object already satisfies the frozen facts.
    let mut source = BufferedBlobSource::new(published.published_sha256(), body.clone());
    driver.publish(&published, &mut source).unwrap();
    let state = driver.inspect(published.object_key()).unwrap();
    assert!(published.judge(&state).is_ready());
}

/// Compile-time proof that the adapter's error type stays inside the runtime.
#[allow(dead_code)]
fn r2_error_is_a_std_error(error: R2ObjectStoreError) -> Box<dyn std::error::Error> {
    Box::new(error)
}
