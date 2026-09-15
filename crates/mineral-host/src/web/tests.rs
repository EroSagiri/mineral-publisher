//! The HTTP adapter's own tests.
//!
//! They drive the router directly with `oneshot` where the contract is about
//! JSON and status codes, and a real loopback socket where the contract is about
//! the wire itself (SSE, and the fact that the server can be reached at all).
//!
//! Two things are deliberately *not* tested here: that the CLI still works
//! (covered by `scripts/verify-cli-independence.sh` and the CLI's own tests), and
//! that a publication really publishes (covered end to end elsewhere with a
//! temporary Git remote). What is tested here is the adapter: shapes, codes,
//! statuses, the single-flight refusal, and replay.

use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;

use crate::{
    config::{RawConfig, SecretProvider, StaticSecretProvider, ValidatedConfig},
    domain::{ContentPath, Sha256, SnapshotId},
    operations::{
        OperationErrorCode, OperationExecutor, OperationFailure, OperationRequest, OperationResult,
        OperationSupervisor,
    },
    policy::{
        HumanReviewReason, PolicyIdentity, PublicPolicyDecision, ReviewRun, ReviewRunId,
        ReviewRunStore,
    },
    runtime::{Progress, WorkspaceRuntime},
    web::{self, DEFAULT_BIND, WebState, is_loopback},
};

// ---------------------------------------------------------------------------
// A workspace, and a supervisor over a scripted executor
// ---------------------------------------------------------------------------

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mineral-web-{name}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const WORKSPACE: &str = "source:\n  id: local-vault\n  path: ./vault\nstate:\n  path: ./.mineral\ngit:\n  repository: ./publication\n  remote: origin\n  reference: refs/heads/main\n  author_name: Bot\n  author_email: bot@example.invalid\n  message: Publish Mineral content\nassets:\n  public_base_url: https://assets.example.com\n  target_path: ./asset-target\nreview:\n  api_base_url: https://api.deepseek.com\n  markdown_model: deepseek-flash\n  asset_model: deepseek-flash\n  api_key_env: MINERAL_WEB_TEST_KEY\n  timeout_seconds: 45\n";

fn workspace(name: &str) -> (Scratch, Arc<WorkspaceRuntime>) {
    let scratch = Scratch::new(name);
    let path = scratch.0.join("mineral.yaml");
    fs::write(&path, WORKSPACE).unwrap();
    let model: RawConfig = serde_yaml_ng::from_str(WORKSPACE).unwrap();
    let config = ValidatedConfig::from_raw(model, &path).unwrap();
    let secrets =
        Arc::new(StaticSecretProvider::new().with("MINERAL_WEB_TEST_KEY", "web-test-secret-value"))
            as Arc<dyn SecretProvider>;
    let runtime = Arc::new(WorkspaceRuntime::new(config, path, secrets));
    runtime.prepare().unwrap();
    runtime.open_stores().unwrap();
    fs::create_dir_all(scratch.0.join("vault")).unwrap();
    fs::create_dir_all(scratch.0.join("publication")).unwrap();
    (scratch, runtime)
}

/// An executor whose behaviour the test decides, so an operation can be held
/// open while a second request is made.
struct TestExecutor {
    lines: Vec<&'static str>,
    hold: bool,
    gate: Mutex<Receiver<()>>,
    outcome: Outcome,
    started: AtomicUsize,
}

#[derive(Clone, Copy)]
enum Outcome {
    Success,
    Failure(OperationErrorCode),
}

impl OperationExecutor for TestExecutor {
    fn execute(
        &self,
        _request: &OperationRequest,
        progress: Arc<dyn Progress>,
    ) -> Result<OperationResult, OperationFailure> {
        self.started.fetch_add(1, Ordering::SeqCst);
        for line in &self.lines {
            progress.stage(line);
        }
        if self.hold {
            let gate = self.gate.lock().unwrap();
            let _ = gate.recv_timeout(Duration::from_secs(10));
        }
        match self.outcome {
            Outcome::Success => Ok(OperationResult::Diagnosed(
                crate::application::doctor::DoctorOutcome {
                    checks: vec![crate::application::doctor::DoctorCheck {
                        name: "configuration".to_owned(),
                        ok: true,
                        detail: "in-memory".to_owned(),
                    }],
                },
            )),
            Outcome::Failure(code) => Err(OperationFailure::Application {
                code,
                message: "scripted failure".to_owned(),
                causes: vec!["the cause".to_owned()],
            }),
        }
    }
}

fn scripted_state(
    runtime: Arc<WorkspaceRuntime>,
    lines: Vec<&'static str>,
    hold: bool,
    outcome: Outcome,
) -> (Arc<WebState>, Arc<TestExecutor>, Sender<()>) {
    let (gate, receiver) = channel();
    let executor = Arc::new(TestExecutor {
        lines,
        hold,
        gate: Mutex::new(receiver),
        outcome,
        started: AtomicUsize::new(0),
    });
    let supervisor = Arc::new(OperationSupervisor::new(executor.clone()));
    (
        Arc::new(WebState::with_supervisor(runtime, supervisor)),
        executor,
        gate,
    )
}

// ---------------------------------------------------------------------------
// Request helpers
// ---------------------------------------------------------------------------

async fn call(app: &Router, method: Method, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("every response body is JSON")
    };
    (status, value)
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    call(app, Method::GET, uri).await
}

async fn post(app: &Router, uri: &str) -> (StatusCode, Value) {
    call(app, Method::POST, uri).await
}

/// Waits until the scripted executor has been entered.
fn wait_until_started(executor: &TestExecutor) {
    for _ in 0..300 {
        if executor.started.load(Ordering::SeqCst) > 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the executor was never entered");
}

/// Waits until an operation reaches a terminal state, and returns its snapshot.
async fn wait_for_terminal(app: &Router, id: &str) -> Value {
    for _ in 0..300 {
        let (_, body) = get(app, &format!("/api/v1/operations/{id}")).await;
        if body["state"] == "failed" || body["state"] == "succeeded" {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("operation {id} never finished");
}

/// Fails when any key anywhere in a response could name a secret.
///
/// The point is structural: it is not enough that a handler remembers to redact,
/// because there must be no field to redact in the first place.
fn assert_no_secret_field(value: &Value) {
    const FORBIDDEN: [&str; 9] = [
        "secret",
        "token",
        "password",
        "api_key",
        "access_key",
        "credential",
        "username",
        "authorization",
        "env",
    ];
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                let lowered = key.to_ascii_lowercase();
                for forbidden in FORBIDDEN {
                    assert!(
                        !lowered.contains(forbidden),
                        "response key {key:?} could carry a secret"
                    );
                }
                assert_no_secret_field(nested);
            }
        }
        Value::Array(items) => items.iter().for_each(assert_no_secret_field),
        Value::String(text) => assert!(
            !text.contains("web-test-secret-value"),
            "a credential value reached a response: {text:?}"
        ),
        _ => {}
    }
}

/// A document review attempt, for the read-only endpoints.
fn seed_document(runtime: &Arc<WorkspaceRuntime>, id: u64, path: &str) {
    runtime
        .document_reviews()
        .unwrap()
        .save(&ReviewRun::rehydrate(
            ReviewRunId::new(id).unwrap(),
            SnapshotId::new(1).unwrap(),
            ContentPath::new(path).unwrap(),
            Sha256::digest(path.as_bytes()),
            PolicyIdentity::new("public", "v1", Sha256::new([5; 32])).unwrap(),
            (
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
                None,
            ),
            1_000,
        ))
        .unwrap();
}

// ---------------------------------------------------------------------------
// Binding and transport
// ---------------------------------------------------------------------------

/// The default address is loopback, because this interface can publish.
#[test]
fn the_default_bind_is_loopback_only() {
    let address: SocketAddr = DEFAULT_BIND.parse().unwrap();
    assert!(is_loopback(&address));
    assert_eq!(address.ip().to_string(), "127.0.0.1");
    assert!(!is_loopback(&"0.0.0.0:8787".parse().unwrap()));
}

/// The API is reachable on a real socket, and every response is JSON.
#[tokio::test(flavor = "multi_thread")]
async fn the_api_answers_on_a_real_loopback_socket() {
    let (_scratch, runtime) = workspace("socket");
    let state = Arc::new(WebState::new(runtime));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    assert!(is_loopback(&address));
    tokio::spawn(web::serve_listener(Arc::clone(&state), listener));

    let response = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET /api/v1/status HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    })
    .await
    .unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("application/json"), "{response}");
    assert!(response.contains("\"source\""), "{response}");
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Status is a structured value, not a rendering of CLI text.
#[tokio::test(flavor = "multi_thread")]
async fn status_is_structured() {
    let (_scratch, runtime) = workspace("status");
    let app = web::router(Arc::new(WebState::new(runtime)));

    let (status, body) = get(&app, "/api/v1/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], Value::Bool(true));
    assert_eq!(body["source"]["id"], "local-vault");
    assert_eq!(body["source"]["kind"], "local");
    assert_eq!(body["publication"]["remote"], "origin");
    assert_eq!(body["publication"]["reference"], "refs/heads/main");
    assert_eq!(body["reviews"]["pending_markdown"], 0);
    assert_eq!(body["backup"]["configured"], Value::Bool(false));
    assert_no_secret_field(&body);
}

/// The status shape is the wire contract, so its key set is asserted.
#[tokio::test(flavor = "multi_thread")]
async fn the_status_shape_is_the_contract() {
    let (_scratch, runtime) = workspace("shapes");
    let app = web::router(Arc::new(WebState::new(runtime)));

    let (_, body) = get(&app, "/api/v1/status").await;
    let keys = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        BTreeSet::from([
            "configured",
            "source",
            "state_path",
            "publication",
            "reviews",
            "backup",
        ])
    );
}

/// Doctor answers every check as data, including the failures.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_is_structured() {
    let (_scratch, runtime) = workspace("doctor");
    let app = web::router(Arc::new(WebState::new(runtime)));

    let (status, body) = get(&app, "/api/v1/doctor").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["checks"]
            .as_array()
            .is_some_and(|checks| !checks.is_empty())
    );
    assert!(body["passed"].as_u64().is_some_and(|passed| passed > 0));
    assert_eq!(
        body["ok"],
        Value::Bool(body["failed"].as_u64().unwrap_or(1) == 0)
    );
    assert_no_secret_field(&body);
}

/// The review queue lists attempts and shows one in full.
#[tokio::test(flavor = "multi_thread")]
async fn reviews_are_structured() {
    let (_scratch, runtime) = workspace("reviews");
    seed_document(&runtime, 1, "note.md");
    let app = web::router(Arc::new(WebState::new(runtime)));

    let (status, body) = get(&app, "/api/v1/reviews").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["markdown"][0]["attempt"], "document:1");
    assert_eq!(body["markdown"][0]["path"], "note.md");
    assert_eq!(body["assets"].as_array().unwrap().len(), 0);

    let (status, body) = get(&app, "/api/v1/reviews/document:1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["attempt"], "document:1");
    assert_eq!(body["subject"], "markdown");
    assert_eq!(body["decision"]["code"], "needs_human_review");
    assert_eq!(body["policy"]["name"], "public");
    assert_eq!(body["human_resolution"], "pending");
    assert_no_secret_field(&body);
}

/// An unknown attempt is a typed 404; a malformed one is a typed 400.
#[tokio::test(flavor = "multi_thread")]
async fn review_errors_are_typed() {
    let (_scratch, runtime) = workspace("review-errors");
    let app = web::router(Arc::new(WebState::new(runtime)));

    let (status, body) = get(&app, "/api/v1/reviews/document:99").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "review_not_found");
    assert!(body["message"].as_str().is_some_and(|m| !m.is_empty()));

    let (status, body) = get(&app, "/api/v1/reviews/banana").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");
}

/// An endpoint that does not exist is a typed 404, not an HTML page.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_endpoint_is_json() {
    let (_scratch, runtime) = workspace("unknown");
    let app = web::router(Arc::new(WebState::new(runtime)));

    let (status, body) = get(&app, "/api/v1/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "endpoint_not_found");

    // A path that exists but not for this method is a 405, which is what the
    // protocol says and what a client should expect.
    let (status, _) = get(&app, "/api/v1/operations/backup").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// Starting an operation returns its identity immediately, without waiting.
#[tokio::test(flavor = "multi_thread")]
async fn starting_an_operation_returns_an_identity_immediately() {
    let (_scratch, runtime) = workspace("accept");
    let (state, executor, gate) = scripted_state(runtime, vec!["working"], true, Outcome::Success);
    let app = web::router(state);

    let (status, body) = post(&app, "/api/v1/operations/backup").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["kind"], "backup");
    let id = body["operation_id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("op-"), "{id}");

    // The work is held open, and the endpoint already answered. The worker is
    // started by another thread, so wait for it rather than racing it.
    wait_until_started(&executor);
    let (status, snapshot) = get(&app, &format!("/api/v1/operations/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["state"], "running");
    assert_eq!(snapshot["progress"][0]["message"], "working");
    assert_eq!(snapshot["progress"][0]["kind"], "stage");
    assert_no_secret_field(&snapshot);

    gate.send(()).unwrap();
}

/// A second mutating operation is refused with 409 and the holder's identity,
/// and the engine is never entered a second time.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_mutation_is_a_409_and_never_reaches_the_engine() {
    let (_scratch, runtime) = workspace("busy");
    let (state, executor, gate) = scripted_state(runtime, vec!["holding"], true, Outcome::Success);
    let app = web::router(state);

    let (status, accepted) = post(&app, "/api/v1/operations/backup").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let holder = accepted["operation_id"].as_str().unwrap().to_owned();
    // The worker is started by another thread. Wait for it, so the refusals
    // below are proven to happen while it is running — and so the "exactly
    // once" assertion is about the refusals rather than a race.
    wait_until_started(&executor);

    for uri in [
        "/api/v1/operations/publish",
        "/api/v1/operations/backup/init",
        "/api/v1/operations/backup",
    ] {
        let (status, body) = post(&app, uri).await;
        assert_eq!(status, StatusCode::CONFLICT, "{uri}");
        assert_eq!(body["code"], "workspace_busy", "{uri}");
        assert_eq!(body["active_operation_id"], holder.as_str(), "{uri}");
        assert_eq!(body["active_operation_kind"], "backup", "{uri}");
    }

    // The engine ran exactly once: the refusals happened before it.
    assert_eq!(executor.started.load(Ordering::SeqCst), 1);

    // A read-only operation is not refused while the workspace is held.
    let (status, body) = post(&app, "/api/v1/operations/backup/verify").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["kind"], "verify_backup");

    gate.send(()).unwrap();
}

/// A human decision goes through the supervisor, so it takes the same gate.
#[tokio::test(flavor = "multi_thread")]
async fn a_review_decision_takes_the_workspace_gate() {
    let (_scratch, runtime) = workspace("decision");
    seed_document(&runtime, 1, "note.md");
    let (state, _executor, gate) =
        scripted_state(runtime, vec!["deciding"], true, Outcome::Success);
    let app = web::router(state);

    let (status, accepted) = post(&app, "/api/v1/reviews/document:1/approve").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(accepted["kind"], "review_decision");
    let first = accepted["operation_id"].as_str().unwrap().to_owned();

    // The decision holds the workspace, so a publication is refused — which is
    // the whole reason the handler goes through the supervisor.
    let (status, body) = post(&app, "/api/v1/operations/publish").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "workspace_busy");
    assert_eq!(body["active_operation_kind"], "review_decision");

    // Release the first decision and wait for it to finish. The workspace is
    // free the moment the operation is finished, so the next one is accepted.
    gate.send(()).unwrap();
    wait_for_terminal(&app, &first).await;

    let (status, rejected) = post(&app, "/api/v1/reviews/document:1/reject").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(rejected["kind"], "review_decision");
    gate.send(()).unwrap();
}

/// A failure is reported with its stable code, its message and its chain.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_operation_reports_a_code() {
    let (_scratch, runtime) = workspace("failure");
    let (state, _executor, _gate) = scripted_state(
        runtime,
        vec!["working"],
        false,
        Outcome::Failure(OperationErrorCode::BackupNoBaseCommit),
    );
    let app = web::router(state);

    let (_, accepted) = post(&app, "/api/v1/operations/backup").await;
    let id = accepted["operation_id"].as_str().unwrap().to_owned();
    let body = wait_for_terminal(&app, &id).await;

    assert_eq!(body["state"], "failed");
    assert_eq!(body["failure"]["code"], "backup_no_base_commit");
    assert_eq!(body["failure"]["message"], "scripted failure");
    assert_eq!(body["failure"]["causes"][0], "the cause");
    assert_no_secret_field(&body);
}

/// An identity the server never allocated is a typed 404; a malformed one is a
/// typed 400.
#[tokio::test(flavor = "multi_thread")]
async fn operation_identities_are_checked() {
    let (_scratch, runtime) = workspace("ids");
    let (state, _executor, _gate) = scripted_state(runtime, vec![], false, Outcome::Success);
    let app = web::router(state);

    let (status, body) = get(&app, "/api/v1/operations/op-999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "operation_not_found");

    let (status, body) = get(&app, "/api/v1/operations/not-an-operation").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");

    let (status, body) = get(&app, "/api/v1/operations/op-0").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");
}

/// The list endpoint summarises what the supervisor holds.
#[tokio::test(flavor = "multi_thread")]
async fn operations_are_listable() {
    let (_scratch, runtime) = workspace("list");
    let (state, _executor, _gate) = scripted_state(runtime, vec!["one"], false, Outcome::Success);
    let app = web::router(state);

    let (_, accepted) = post(&app, "/api/v1/operations/backup/verify").await;
    let id = accepted["operation_id"].as_str().unwrap().to_owned();
    wait_for_terminal(&app, &id).await;

    let (status, body) = get(&app, "/api/v1/operations").await;
    assert_eq!(status, StatusCode::OK);
    let entries = body.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], id.as_str());
    assert_eq!(entries[0]["kind"], "verify_backup");
    assert_eq!(entries[0]["state"], "succeeded");
}

// ---------------------------------------------------------------------------
// SSE
// ---------------------------------------------------------------------------

/// Undoes HTTP chunked framing.
///
/// An SSE response is streaming, so its length is unknown and each write becomes
/// its own chunk. A test that searched the raw bytes for an event would be
/// testing the framing rather than the stream.
fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some(position) = rest.find("\r\n") {
        let Ok(size) = usize::from_str_radix(rest[..position].trim(), 16) else {
            // Not a chunk header: the body was not chunked after all.
            out.push_str(rest);
            break;
        };
        if size == 0 {
            break;
        }
        let start = position + 2;
        if rest.len() < start + size {
            out.push_str(&rest[start..]);
            break;
        }
        out.push_str(&rest[start..start + size]);
        rest = &rest[start + size..];
        if let Some(tail) = rest.strip_prefix("\r\n") {
            rest = tail;
        }
    }
    out
}

/// The body of a response, with chunked framing removed if it was used.
fn response_body(response: &str) -> String {
    match response.split_once("\r\n\r\n") {
        Some((_, body)) => dechunk(body),
        None => String::new(),
    }
}

/// Reads an event stream from a real socket until the stream ends.
fn read_events(address: SocketAddr, path: &str, last_event_id: Option<u64>) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let last = last_event_id
        .map(|id| format!("Last-Event-ID: {id}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nAccept: text/event-stream\r\n{last}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                response.push_str(&String::from_utf8_lossy(&buffer[..read]));
                // The stream closes after the terminal event, and `Connection:
                // close` means EOF is the end of the body.
                let body = response_body(&response);
                if body.contains("event: completed") || body.contains("event: failed") {
                    let _ = stream.read(&mut buffer);
                    break;
                }
            }
            Err(_) => break,
        }
    }
    response
}

/// Progress is streamed with its sequence as the event id, and the stream ends
/// after a terminal event.
#[tokio::test(flavor = "multi_thread")]
async fn events_are_streamed_and_end_after_a_terminal_event() {
    let (_scratch, runtime) = workspace("sse");
    let (state, _executor, gate) =
        scripted_state(runtime, vec!["first", "second"], true, Outcome::Success);
    let app = web::router(Arc::clone(&state));

    let (_, accepted) = post(&app, "/api/v1/operations/backup").await;
    let id = accepted["operation_id"].as_str().unwrap().to_owned();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(web::serve_listener(Arc::clone(&state), listener));

    // Release the operation while the client is already waiting.
    let released = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        gate.send(()).unwrap();
    });
    let streamed = tokio::task::spawn_blocking(move || {
        read_events(address, &format!("/api/v1/operations/{id}/events"), None)
    })
    .await
    .unwrap();
    released.join().unwrap();

    assert!(streamed.starts_with("HTTP/1.1 200 OK"), "{streamed}");
    assert!(streamed.contains("text/event-stream"), "{streamed}");
    let body = response_body(&streamed);
    assert!(body.contains("id: 1\nevent: progress\ndata: "), "{body}");
    assert!(body.contains("\"message\":\"first\""), "{body}");
    assert!(body.contains("\"message\":\"second\""), "{body}");
    assert!(body.contains("event: completed"), "{body}");
    assert!(body.contains("\"state\":\"succeeded\""), "{body}");
}

/// A reconnecting client is replayed only what it has not seen.
#[tokio::test(flavor = "multi_thread")]
async fn events_honour_last_event_id() {
    let (_scratch, runtime) = workspace("sse-replay");
    let (state, _executor, _gate) =
        scripted_state(runtime, vec!["first", "second"], false, Outcome::Success);
    let app = web::router(Arc::clone(&state));

    let (_, accepted) = post(&app, "/api/v1/operations/backup").await;
    let id = accepted["operation_id"].as_str().unwrap().to_owned();
    let path = format!("/api/v1/operations/{id}/events");
    wait_for_terminal(&app, &id).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(web::serve_listener(Arc::clone(&state), listener));

    let everything = tokio::task::spawn_blocking({
        let path = path.clone();
        move || read_events(address, &path, None)
    })
    .await
    .unwrap();
    let everything_body = response_body(&everything);
    assert!(
        everything_body.contains("\"message\":\"first\""),
        "{everything_body}"
    );
    assert!(
        everything_body.contains("\"message\":\"second\""),
        "{everything_body}"
    );

    let after_first = tokio::task::spawn_blocking(move || read_events(address, &path, Some(1)))
        .await
        .unwrap();
    let after_first_body = response_body(&after_first);
    assert!(
        !after_first_body.contains("\"message\":\"first\""),
        "{after_first_body}"
    );
    assert!(
        after_first_body.contains("\"message\":\"second\""),
        "{after_first_body}"
    );
    assert!(
        after_first_body.contains("event: completed"),
        "{after_first_body}"
    );
}

/// An event stream for an identity nobody allocated is a 404.
#[tokio::test(flavor = "multi_thread")]
async fn events_for_an_unknown_operation_are_refused() {
    let (_scratch, runtime) = workspace("sse-unknown");
    let app = web::router(Arc::new(WebState::new(runtime)));

    let (status, body) = get(&app, "/api/v1/operations/op-77/events").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "operation_not_found");
}

// ---------------------------------------------------------------------------
// The UI and the API share one origin, and never each other's fallback
// ---------------------------------------------------------------------------

/// A built UI, without needing Node to make one.
///
/// The fixture is two files with recognisable contents, which is all the router
/// cares about. Rust tests must not depend on npm having run.
struct FakeUi {
    /// Owned, because a `Scratch` deletes itself when it is dropped.
    _scratch: Scratch,
    dist: PathBuf,
}

impl FakeUi {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let dist = scratch.0.join("dist");
        fs::create_dir_all(dist.join("assets")).unwrap();
        fs::write(
            dist.join("index.html"),
            "<!doctype html><title>mineral ui</title><div id=root></div>",
        )
        .unwrap();
        fs::write(dist.join("assets/app.js"), "console.log('mineral')\n").unwrap();
        Self {
            _scratch: scratch,
            dist,
        }
    }

    fn path(&self) -> &Path {
        &self.dist
    }
}

async fn raw_get(app: &Router, uri: &str) -> (StatusCode, String, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// A client-side route is served the application shell, so a reload works.
#[tokio::test(flavor = "multi_thread")]
async fn client_routes_are_served_the_application_shell() {
    let (_scratch, runtime) = workspace("spa");
    let ui = FakeUi::new("spa-assets");
    let state = Arc::new(WebState::with_assets(
        runtime,
        Some(ui.path().to_path_buf()),
    ));
    let app = web::router(state);

    for route in ["/", "/reviews", "/operations/op-17", "/doctor"] {
        let (status, content_type, body) = raw_get(&app, route).await;
        assert_eq!(status, StatusCode::OK, "{route}");
        assert!(
            content_type.starts_with("text/html"),
            "{route}: {content_type}"
        );
        assert!(body.contains("mineral ui"), "{route}: {body}");
    }

    // A real asset is served as itself, not as the shell.
    let (status, content_type, body) = raw_get(&app, "/assets/app.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("javascript"), "{content_type}");
    assert!(body.contains("console.log"), "{body}");
}

/// The API's own 404 is never swallowed by the UI fallback.
///
/// This is the guarantee that keeps a client able to tell "no such endpoint"
/// from "here is a page": an API caller that receives HTML cannot read a code.
#[tokio::test(flavor = "multi_thread")]
async fn a_broken_api_call_stays_a_json_404() {
    let (_scratch, runtime) = workspace("api-404");
    let ui = FakeUi::new("api-404-assets");
    let state = Arc::new(WebState::with_assets(
        runtime,
        Some(ui.path().to_path_buf()),
    ));
    let app = web::router(state);

    for route in ["/api/v1/nonsense", "/api/nope", "/api"] {
        let (status, content_type, body) = raw_get(&app, route).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{route}");
        assert!(
            content_type.starts_with("application/json"),
            "{route}: {content_type}"
        );
        let parsed: Value = serde_json::from_str(&body).expect("an API 404 is JSON");
        assert_eq!(parsed["code"], "endpoint_not_found", "{route}");
    }

    // And the API still works with the UI in front of it.
    let (status, _, body) = raw_get(&app, "/api/v1/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"configured\""), "{body}");
}

/// Without a build, the API works and the pages explain what to do.
#[tokio::test(flavor = "multi_thread")]
async fn an_unbuilt_ui_says_so_and_leaves_the_api_alone() {
    let (_scratch, runtime) = workspace("no-ui");
    let app = web::router(Arc::new(WebState::with_assets(runtime, None)));

    let (status, _, body) = raw_get(&app, "/").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("npm --prefix web-ui"), "{body}");

    let (status, _, _) = raw_get(&app, "/api/v1/status").await;
    assert_eq!(status, StatusCode::OK);
}

/// A directory without an index is not a built UI, so it is not served as one.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_directory_is_not_a_built_ui() {
    let (_scratch, runtime) = workspace("empty-ui");
    let empty = Scratch::new("empty-ui-assets");
    let state = Arc::new(WebState::with_assets(runtime, Some(empty.0.clone())));
    assert!(state.assets().is_none());
    let app = web::router(state);

    let (status, _, _) = raw_get(&app, "/").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

// ---------------------------------------------------------------------------
// The DTO boundary
// ---------------------------------------------------------------------------

/// No response contains a key that could name a secret, anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn no_response_can_carry_a_secret() {
    let (_scratch, runtime) = workspace("secrets");
    seed_document(&runtime, 1, "note.md");
    let (state, _executor, _gate) =
        scripted_state(runtime, vec!["working"], false, Outcome::Success);
    let app = web::router(state);

    let mut bodies = Vec::new();
    for uri in [
        "/api/v1/status",
        "/api/v1/doctor",
        "/api/v1/reviews",
        "/api/v1/reviews/document:1",
        "/api/v1/operations",
    ] {
        bodies.push(get(&app, uri).await.1);
    }
    let (_, accepted) = post(&app, "/api/v1/operations/backup").await;
    bodies.push(accepted);
    let id = bodies.last().unwrap()["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    bodies.push(wait_for_terminal(&app, &id).await);

    for body in &bodies {
        assert_no_secret_field(body);
    }
}
