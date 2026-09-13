//! Opt-in integration against a real S3-compatible bucket.
//!
//! `cargo test --workspace` never runs these: they are marked `#[ignore]` and need
//! credentials. They seed and remove a dedicated `mineral-source-live-tests/…`
//! prefix, and never touch any other object in the bucket.
//!
//! Run them with:
//!
//! ```text
//! MINERAL_R2_ENDPOINT=… MINERAL_R2_BUCKET=… MINERAL_R2_ACCESS_KEY_ID=… \
//! MINERAL_R2_SECRET_ACCESS_KEY=… \
//!   cargo test -p mineral-publisher source::r2::live_tests -- --ignored --test-threads=1
//! ```

use std::{
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use mineral_core::{
    domain::{ContentPath, Sha256},
    source::{DEFAULT_MAX_SCAN_ATTEMPTS, SourceMaterializationSet, stabilize_scan},
};
use reqwest::StatusCode;

use crate::{
    object_store::{
        R2ObjectStoreConfig, R2SecretKey, SignedRequestSpec, empty_payload_sha256,
        signed_request_builder,
    },
    storage::{LocalContentStore, SqliteSourceMaterializationStore},
};

use super::{
    R2Source, R2SourcePrefix,
    list::parse_list_page,
    reader::{DEFAULT_LIST_PAGE_SIZE, MAX_LIST_BODY_BYTES},
};

/// The dedicated namespace these tests own. Nothing else is ever listed or removed.
const LIVE_TEST_ROOT: &str = "mineral-source-live-tests/";

fn live_config() -> R2ObjectStoreConfig {
    let endpoint = std::env::var("MINERAL_R2_ENDPOINT")
        .expect("MINERAL_R2_ENDPOINT must name the bucket endpoint");
    let bucket =
        std::env::var("MINERAL_R2_BUCKET").expect("MINERAL_R2_BUCKET must name the bucket");
    let access_key_id =
        std::env::var("MINERAL_R2_ACCESS_KEY_ID").expect("MINERAL_R2_ACCESS_KEY_ID must be set");
    let secret = std::env::var("MINERAL_R2_SECRET_ACCESS_KEY")
        .expect("MINERAL_R2_SECRET_ACCESS_KEY must be set");
    R2ObjectStoreConfig::new(
        endpoint,
        bucket,
        access_key_id,
        R2SecretKey::new(secret).expect("the configured secret must be usable"),
    )
    .expect("the configured endpoint must be usable")
}

fn unique_prefix() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!(
        "{LIVE_TEST_ROOT}{}-{nonce}-{}/",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap()
}

fn send(
    config: &R2ObjectStoreConfig,
    method: &'static str,
    path: &str,
    body: Option<Vec<u8>>,
) -> (StatusCode, String) {
    let payload = body
        .as_ref()
        .map(|bytes| crate::object_store::payload_sha256(bytes))
        .unwrap_or_else(empty_payload_sha256);
    let mut spec = SignedRequestSpec::new(method, path, payload.clone());
    if body.is_some() {
        spec = spec.with_header("content-type", "application/octet-stream");
    }
    // The shared builder already sets `x-amz-content-sha256`; adding it again would
    // send two header values and make the signature disagree with the wire.
    let request = signed_request_builder(&client(), config, &spec)
        .expect("the live request must be signable");
    let request = match body {
        Some(bytes) => request.body(bytes),
        None => request,
    };
    let response = request
        .send()
        .expect("the live request must reach the bucket");
    let status = response.status();
    let text = response.text().unwrap_or_default();
    (status, text)
}

fn object_path(config: &R2ObjectStoreConfig, key: &str) -> String {
    format!(
        "/{}/{}",
        config.bucket(),
        crate::object_store::encode_path(key)
    )
}

fn put(config: &R2ObjectStoreConfig, key: &str, bytes: &[u8]) {
    let (status, body) = send(
        config,
        "PUT",
        &object_path(config, key),
        Some(bytes.to_vec()),
    );
    assert!(
        status.is_success(),
        "seeding {key} failed with {status}: {body}"
    );
}

fn remove(config: &R2ObjectStoreConfig, key: &str) {
    let (status, body) = send(config, "DELETE", &object_path(config, key), None);
    assert!(
        status.is_success() || status == StatusCode::NOT_FOUND,
        "removing {key} failed with {status}: {body}"
    );
}

/// Lists the test's own prefix. It is never pointed at anything else.
fn list_prefix(config: &R2ObjectStoreConfig, prefix: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut query = vec![
            ("list-type".to_owned(), "2".to_owned()),
            ("prefix".to_owned(), prefix.to_owned()),
            ("max-keys".to_owned(), DEFAULT_LIST_PAGE_SIZE.to_string()),
        ];
        if let Some(token) = &token {
            query.push(("continuation-token".to_owned(), token.clone()));
        }
        let bucket_path = format!("/{}", config.bucket());
        let spec =
            SignedRequestSpec::new("GET", &bucket_path, empty_payload_sha256()).with_query(&query);
        let response = signed_request_builder(&client(), config, &spec)
            .unwrap()
            .send()
            .unwrap();
        assert!(response.status().is_success(), "{:?}", response.status());
        let body = response.text().unwrap();
        assert!(body.len() as u64 <= MAX_LIST_BODY_BYTES);
        let page = parse_list_page(&body).expect("a real listing must parse");
        keys.extend(page.objects.into_iter().map(|object| object.key));
        match (page.is_truncated, page.next_continuation_token) {
            (true, Some(next)) => token = Some(next),
            _ => break,
        }
    }
    keys
}

fn cleanup(config: &R2ObjectStoreConfig, prefix: &str) {
    for key in list_prefix(config, prefix) {
        remove(config, &key);
    }
}

fn reader(config: &R2ObjectStoreConfig, prefix: &str, directory: &Path) -> R2Source {
    R2Source::new(
        config.clone(),
        R2SourcePrefix::new(prefix).unwrap(),
        LocalContentStore::new(directory.join("cas")),
        SqliteSourceMaterializationStore::open(directory.join("source-materializations.sqlite3"))
            .unwrap(),
    )
    .unwrap()
}

fn sha_of(materialized: &SourceMaterializationSet, path: &str) -> Sha256 {
    materialized
        .entries()
        .iter()
        .find(|entry| entry.path() == &ContentPath::new(path).unwrap())
        .unwrap_or_else(|| panic!("{path} was not materialized"))
        .content_sha256()
}

fn sizes_of(materialized: &SourceMaterializationSet) -> BTreeMap<String, u64> {
    materialized
        .entries()
        .iter()
        .map(|entry| (entry.path().as_str().to_owned(), entry.content_size()))
        .collect()
}

#[test]
#[ignore = "requires a live bucket and credentials"]
fn a_live_bucket_seeds_materializes_and_restabilizes_one_source_prefix() {
    let config = live_config();
    let prefix = unique_prefix();
    let directory =
        std::env::temp_dir().join(format!("mineral-live-source-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();

    // Refuse to run inside a prefix that already holds anything: cleanup below is
    // only ever aimed at this test's own namespace.
    cleanup(&config, LIVE_TEST_ROOT);

    let index = b"# index\n".to_vec();
    let note = b"# note\n".to_vec();
    let photo = vec![0x89_u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3];
    let empty = Vec::new();
    let unicode = "旅行 照片\n".as_bytes().to_vec();
    for (path, bytes) in [
        ("index.md", &index),
        ("notes/a.md", &note),
        ("attachments/photo.png", &photo),
        ("empty.md", &empty),
        ("旅行/照片.txt", &unicode),
    ] {
        put(&config, &format!("{prefix}{path}"), bytes);
    }

    let source = reader(&config, &prefix, &directory);
    let first = stabilize_scan(&source, DEFAULT_MAX_SCAN_ATTEMPTS).unwrap();
    assert_eq!(first.materialized().len(), 5);
    assert_eq!(
        sha_of(first.materialized(), "index.md"),
        Sha256::digest(&index)
    );
    assert_eq!(
        sha_of(first.materialized(), "notes/a.md"),
        Sha256::digest(&note)
    );
    assert_eq!(
        sha_of(first.materialized(), "attachments/photo.png"),
        Sha256::digest(&photo)
    );
    assert_eq!(
        sha_of(first.materialized(), "empty.md"),
        Sha256::digest(&empty)
    );
    assert_eq!(
        sha_of(first.materialized(), "旅行/照片.txt"),
        Sha256::digest(&unicode),
        "a non-ASCII key must round-trip exactly"
    );
    assert_eq!(
        sizes_of(first.materialized()),
        BTreeMap::from([
            ("attachments/photo.png".to_owned(), photo.len() as u64),
            ("empty.md".to_owned(), 0),
            ("index.md".to_owned(), index.len() as u64),
            ("notes/a.md".to_owned(), note.len() as u64),
            ("旅行/照片.txt".to_owned(), unicode.len() as u64),
        ])
    );
    assert_eq!(source.fetched_objects(), 5);
    assert_eq!(source.reused_objects(), 0);

    // An unchanged namespace costs no object reads at all.
    let unchanged = reader(&config, &prefix, &directory);
    let second = stabilize_scan(&unchanged, DEFAULT_MAX_SCAN_ATTEMPTS).unwrap();
    assert_eq!(
        unchanged.fetched_objects(),
        0,
        "no object may be read twice"
    );
    assert_eq!(unchanged.reused_objects(), 5);
    assert_eq!(second.materialized(), first.materialized());

    // One overwrite: exactly one object is read again, and its identity changes.
    put(&config, &format!("{prefix}notes/a.md"), b"# note changed\n");
    let changed = reader(&config, &prefix, &directory);
    let third = stabilize_scan(&changed, DEFAULT_MAX_SCAN_ATTEMPTS).unwrap();
    assert_eq!(changed.fetched_objects(), 1);
    assert_eq!(changed.reused_objects(), 4);
    assert_eq!(
        sha_of(third.materialized(), "notes/a.md"),
        Sha256::digest(b"# note changed\n")
    );
    assert_ne!(
        sha_of(third.materialized(), "index.md"),
        Sha256::digest(b"# note changed\n")
    );

    // An addition and a deletion: the source state follows the namespace.
    put(&config, &format!("{prefix}added.md"), b"# added\n");
    remove(&config, &format!("{prefix}empty.md"));
    let fourth = reader(&config, &prefix, &directory);
    let stabilized = stabilize_scan(&fourth, DEFAULT_MAX_SCAN_ATTEMPTS).unwrap();
    let paths = stabilized
        .materialized()
        .entries()
        .iter()
        .map(|entry| entry.path().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "added.md",
            "attachments/photo.png",
            "index.md",
            "notes/a.md",
            "旅行/照片.txt"
        ]
    );
    assert_eq!(fourth.fetched_objects(), 1, "only the added object is new");

    cleanup(&config, &prefix);
    assert!(list_prefix(&config, &prefix).is_empty());
    let _ = std::fs::remove_dir_all(&directory);
}
