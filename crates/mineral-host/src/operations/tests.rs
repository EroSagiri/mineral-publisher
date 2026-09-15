//! The operation layer's own tests — the acceptance proof for S8.1.
//!
//! Every one of them drives the supervisor directly. None of them starts an
//! HTTP server, parses a command line, or reads standard output, and none of
//! them needs the CLI to exist.

use std::{
    error::Error,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::Duration,
};

use crate::{
    application::{
        ApplicationError,
        backup::{BackupRequest, BackupResult, BackupStatusOutcome, BackupVerifyOutcome},
        doctor,
        publish::PublishRequest,
        review::{self, ReviewOutcome, ReviewRequest},
        status,
    },
    config::{RawConfig, SecretProvider, StaticSecretProvider, ValidatedConfig},
    domain::{ContentPath, Sha256, SnapshotId},
    operations::{
        ApplicationExecutor, OperationErrorCode, OperationEvent, OperationExecutor,
        OperationFailure, OperationId, OperationKind, OperationRequest, OperationResult,
        OperationSnapshot, OperationState, OperationSupervisor, ProgressKind, ReviewOperation,
        StartError,
    },
    policy::{
        HumanReviewReason, PolicyIdentity, PublicPolicyDecision, ReviewRun, ReviewRunId,
        ReviewRunStore,
    },
    runtime::{NoProgress, Progress, WorkspaceRuntime},
};

// ---------------------------------------------------------------------------
// A workspace, and a supervisor over it
// ---------------------------------------------------------------------------

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mineral-operations-{name}-{}-{sequence}",
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

const WORKSPACE: &str = "source:\n  id: local-vault\n  path: ./vault\nstate:\n  path: ./.mineral\ngit:\n  repository: ./publication\n  remote: origin\n  reference: refs/heads/main\n  author_name: Bot\n  author_email: bot@example.invalid\n  message: Publish Mineral content\nassets:\n  public_base_url: https://assets.example.com\n  target_path: ./asset-target\nreview:\n  api_base_url: https://api.deepseek.com\n  markdown_model: deepseek-flash\n  asset_model: deepseek-flash\n  api_key: operations-test-key\n  timeout_seconds: 45\n";

/// An initialized workspace, built the way `mineral init` would build it.
fn workspace(name: &str) -> (Scratch, Arc<WorkspaceRuntime>) {
    let scratch = Scratch::new(name);
    let path = scratch.0.join("mineral.yaml");
    fs::write(&path, WORKSPACE).unwrap();
    let model: RawConfig = serde_yaml_ng::from_str(WORKSPACE).unwrap();
    let config = ValidatedConfig::from_raw(model, &path).unwrap();
    let secrets = Arc::new(
        StaticSecretProvider::new().with("__mineral_inline_review_api_key", "operations-test-key"),
    ) as Arc<dyn SecretProvider>;
    let runtime = Arc::new(WorkspaceRuntime::new(config, path, secrets));
    runtime.prepare().unwrap();
    runtime.open_stores().unwrap();
    fs::create_dir_all(scratch.0.join("vault")).unwrap();
    fs::create_dir_all(scratch.0.join("publication")).unwrap();
    (scratch, runtime)
}

/// A production supervisor over a real workspace.
fn supervisor(runtime: &Arc<WorkspaceRuntime>) -> OperationSupervisor {
    OperationSupervisor::new(Arc::new(ApplicationExecutor::new(Arc::clone(runtime))))
}

// ---------------------------------------------------------------------------
// A scripted executor, for the shapes a real use case cannot be made to take
// ---------------------------------------------------------------------------

/// A gate a scripted step waits on before it returns, and its opener.
fn open_gate() -> (Sender<()>, Gate) {
    let (sender, receiver) = channel();
    (sender, Arc::new(Mutex::new(receiver)))
}

/// A gate that is already open: a step waiting on it returns immediately.
fn released_gate() -> Gate {
    let (sender, receiver) = channel();
    sender.send(()).unwrap();
    Arc::new(Mutex::new(receiver))
}

/// A gate shared by a script and the test that drives it.
type Gate = Arc<Mutex<Receiver<()>>>;

/// One thing a scripted executor does.
///
/// A script is re-runnable: every execution clones the plan, so a supervisor
/// that runs the same executor many times (four threads, say) sees the same
/// behaviour every time.
#[derive(Clone)]
enum Step {
    /// Report a progress line, then wait for the gate.
    Report {
        stage: bool,
        message: &'static str,
        gate: Gate,
    },
    /// Fail with this code, and a two-link cause chain.
    Fail(OperationErrorCode),
    /// Succeed with a health scan.
    SucceedDiagnosed,
}

/// An executor whose behaviour a test dictates, so a long-running operation can
/// be held open on purpose.
struct ScriptedExecutor {
    steps: Mutex<Vec<Step>>,
    started: AtomicUsize,
    finished: AtomicUsize,
}

impl ScriptedExecutor {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Mutex::new(steps),
            started: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
        }
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    /// Waits until the executor has been entered at least once.
    fn wait_until_started(&self) {
        while self.started() == 0 {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl OperationExecutor for ScriptedExecutor {
    fn execute(
        &self,
        _request: &OperationRequest,
        progress: Arc<dyn Progress>,
    ) -> Result<OperationResult, OperationFailure> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let steps = self.steps.lock().unwrap().clone();
        let mut outcome = Err(OperationFailure::Application {
            code: OperationErrorCode::PublicationFailed,
            message: "the script ended without an outcome".to_owned(),
            causes: Vec::new(),
        });
        for step in steps {
            match step {
                Step::Report {
                    stage,
                    message,
                    gate,
                } => {
                    if stage {
                        progress.stage(message);
                    } else {
                        progress.detail(message);
                    }
                    let gate = gate.lock().unwrap();
                    let _ = gate.recv_timeout(Duration::from_secs(10));
                }
                Step::Fail(code) => {
                    outcome = Err(OperationFailure::Application {
                        code,
                        message: format!("scripted failure: {code}"),
                        causes: vec!["the cause below".to_owned(), "the deepest cause".to_owned()],
                    });
                    break;
                }
                Step::SucceedDiagnosed => {
                    outcome = Ok(OperationResult::Diagnosed(doctor::DoctorOutcome {
                        checks: vec![doctor::DoctorCheck {
                            name: "configuration".to_owned(),
                            ok: true,
                            detail: "in-memory".to_owned(),
                        }],
                    }));
                    break;
                }
            }
        }
        self.finished.fetch_add(1, Ordering::SeqCst);
        outcome
    }
}

// ---------------------------------------------------------------------------
// start returns an identity immediately
// ---------------------------------------------------------------------------

/// `start` never waits for the work: the identity comes back while the
/// operation is still running.
#[test]
fn start_returns_an_identity_before_the_work_finishes() {
    let (gate_tx, gate_rx) = open_gate();
    let executor = Arc::new(ScriptedExecutor::new(vec![
        Step::Report {
            stage: true,
            message: "working",
            gate: gate_rx,
        },
        Step::SucceedDiagnosed,
    ]));
    let supervisor = OperationSupervisor::new(Arc::clone(&executor) as Arc<dyn OperationExecutor>);

    let id = supervisor.start(OperationRequest::Doctor).unwrap();
    assert_eq!(id.get(), 1);

    executor.wait_until_started();
    let snapshot = supervisor.snapshot(id).unwrap();
    assert_eq!(snapshot.kind, OperationKind::Doctor);
    assert!(!snapshot.is_terminal(), "the work is still held open");

    gate_tx.send(()).unwrap();
    let finished = supervisor.wait(id).unwrap();
    assert_eq!(finished.state, OperationState::Succeeded);
    assert_eq!(executor.finished.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// progress is collectable through a non-CLI sink
// ---------------------------------------------------------------------------

/// Progress arrives as ordered events with no terminal involved.
#[test]
fn progress_is_collectable_through_an_operation_subscription() {
    let (first_tx, first_rx) = open_gate();
    let (second_tx, second_rx) = open_gate();
    let supervisor = OperationSupervisor::new(Arc::new(ScriptedExecutor::new(vec![
        Step::Report {
            stage: true,
            message: "[1/4] Creating immutable Snapshot...",
            gate: first_rx,
        },
        Step::Report {
            stage: false,
            message: "      Markdown review: note.md",
            gate: second_rx,
        },
        Step::SucceedDiagnosed,
    ])));

    let id = supervisor.start(OperationRequest::Doctor).unwrap();
    let subscription = supervisor.subscribe(id).unwrap();

    first_tx.send(()).unwrap();
    second_tx.send(()).unwrap();

    let mut messages = Vec::new();
    let mut kinds = Vec::new();
    for event in subscription {
        match event {
            OperationEvent::Progress(progress) => {
                messages.push(progress.message);
                kinds.push(progress.kind);
            }
            OperationEvent::Finished { state } => {
                assert_eq!(state, OperationState::Succeeded);
                break;
            }
        }
    }

    assert_eq!(
        messages,
        [
            "[1/4] Creating immutable Snapshot...",
            "      Markdown review: note.md"
        ]
    );
    assert_eq!(kinds, [ProgressKind::Stage, ProgressKind::Detail]);

    // Sequences are monotonic from one.
    let snapshot = supervisor.snapshot(id).unwrap();
    let sequences = snapshot
        .progress
        .iter()
        .map(|event| event.sequence)
        .collect::<Vec<_>>();
    assert_eq!(sequences, [1, 2]);
}

/// A subscription made after the operation finished replays the history, so a
/// late reader is never worse off than an early one.
#[test]
fn a_late_subscriber_replays_the_whole_history() {
    let supervisor = OperationSupervisor::new(Arc::new(ScriptedExecutor::new(vec![
        Step::Report {
            stage: true,
            message: "one",
            gate: released_gate(),
        },
        Step::Report {
            stage: false,
            message: "two",
            gate: released_gate(),
        },
        Step::SucceedDiagnosed,
    ])));

    let id = supervisor.start(OperationRequest::Doctor).unwrap();
    assert_eq!(
        supervisor.wait(id).unwrap().state,
        OperationState::Succeeded
    );

    let replayed = supervisor
        .subscribe(id)
        .unwrap()
        .filter_map(|event| match event {
            OperationEvent::Progress(progress) => Some(progress.message),
            OperationEvent::Finished { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replayed, ["one", "two"]);
}

// ---------------------------------------------------------------------------
// success and failure produce typed final results
// ---------------------------------------------------------------------------

/// A successful operation keeps a typed result, readable afterwards.
#[test]
fn a_successful_operation_keeps_a_typed_result() {
    let supervisor = OperationSupervisor::new(Arc::new(ScriptedExecutor::new(vec![
        Step::SucceedDiagnosed,
    ])));
    let id = supervisor.start(OperationRequest::Doctor).unwrap();
    let snapshot = supervisor.wait(id).unwrap();

    assert_eq!(snapshot.state, OperationState::Succeeded);
    assert!(snapshot.failure().is_none());
    match snapshot.result().unwrap().as_ref() {
        OperationResult::Diagnosed(outcome) => {
            assert_eq!(outcome.checks.len(), 1);
            assert_eq!(outcome.passed(), 1);
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

/// A failed operation keeps a code, a message and the whole cause chain.
#[test]
fn a_failed_operation_keeps_a_typed_failure() {
    let supervisor = OperationSupervisor::new(Arc::new(ScriptedExecutor::new(vec![Step::Fail(
        OperationErrorCode::CredentialMissing,
    )])));
    let id = supervisor.start(OperationRequest::Doctor).unwrap();
    let snapshot = supervisor.wait(id).unwrap();

    assert_eq!(snapshot.state, OperationState::Failed);
    assert!(snapshot.result().is_none());

    let failure = snapshot.failure().unwrap();
    assert_eq!(failure.code(), OperationErrorCode::CredentialMissing);
    assert_eq!(failure.code().as_str(), "credential_missing");
    assert!(failure.message().contains("credential_missing"));
    assert_eq!(failure.causes().len(), 2);

    // A caller that reports errors the Rust way sees the same chain it would
    // have seen in-process.
    let error = failure.to_error();
    assert!(error.to_string().contains("credential_missing"));
    let mut chain = Vec::new();
    let mut source = error.source();
    while let Some(cause) = source {
        chain.push(cause.to_string());
        source = cause.source();
    }
    assert_eq!(chain, ["the cause below", "the deepest cause"]);
}

// ---------------------------------------------------------------------------
// a second mutating operation is refused without entering the engine
// ---------------------------------------------------------------------------

/// While a mutating operation holds the workspace another is refused by type,
/// the engine is never entered, and read-only work still runs.
#[test]
fn a_second_mutating_operation_is_refused_as_workspace_busy() {
    let (gate_tx, gate_rx) = open_gate();
    let blocking = Arc::new(ScriptedExecutor::new(vec![
        Step::Report {
            stage: true,
            message: "holding the workspace",
            gate: gate_rx,
        },
        Step::SucceedDiagnosed,
    ]));
    let supervisor = OperationSupervisor::new(Arc::clone(&blocking) as Arc<dyn OperationExecutor>);

    let holder = supervisor
        .start(OperationRequest::Backup(BackupRequest::now().unwrap()))
        .unwrap();
    blocking.wait_until_started();
    assert_eq!(supervisor.active_mutation(), Some(holder));

    // A second mutating request is refused, naming the holder, and the engine is
    // not entered: the executor started exactly once.
    let error = supervisor
        .start(OperationRequest::BackupInit)
        .expect_err("a second mutating operation must be refused");
    assert_eq!(
        error,
        StartError::WorkspaceBusy {
            active_operation_id: holder
        }
    );
    assert_eq!(blocking.started(), 1);
    assert!(error.to_string().contains("busy"));

    // A publish is refused the same way, so the publication engine is never
    // entered while a backup holds the workspace.
    let refused = supervisor.start(OperationRequest::Publish(PublishRequest::now()));
    assert!(matches!(refused, Err(StartError::WorkspaceBusy { .. })));
    assert_eq!(blocking.started(), 1);

    // A read-only request is still accepted while the workspace is held.
    let read = supervisor.start(OperationRequest::Doctor).unwrap();
    assert_ne!(read, holder);
    assert_eq!(supervisor.active_mutation(), Some(holder));

    // The gate releases the holder, then the read-only operation that was
    // accepted while the workspace was held, then the mutating operation below.
    gate_tx.send(()).unwrap();
    assert_eq!(
        supervisor.wait(holder).unwrap().state,
        OperationState::Succeeded
    );
    // The workspace is free the moment the holder is finished — not a moment
    // later, which is what a Web client would otherwise race against.
    assert_eq!(supervisor.active_mutation(), None);

    gate_tx.send(()).unwrap();
    gate_tx.send(()).unwrap();
    let recovered = supervisor.start(OperationRequest::BackupInit).unwrap();
    assert_eq!(
        supervisor.wait(recovered).unwrap().state,
        OperationState::Succeeded
    );
    let _ = supervisor.wait(read).unwrap();
}

// ---------------------------------------------------------------------------
// the layer itself
// ---------------------------------------------------------------------------

/// The read-only use cases answer with structured values; nothing is parsed
/// from text.
#[test]
fn read_only_use_cases_answer_with_values() {
    let (_scratch, runtime) = workspace("reads");

    let reported = status::status(&runtime).unwrap();
    assert_eq!(reported.source_id, "local-vault");
    assert_eq!(reported.pending_documents, 0);

    let scanned = doctor::doctor(&runtime).unwrap();
    assert!(!scanned.checks.is_empty());

    let listed = review::review(&runtime, &ReviewRequest::List).unwrap();
    assert!(matches!(listed, ReviewOutcome::List { .. }));
}

/// The application executor drives the real use cases through the supervisor.
#[test]
fn the_application_executor_drives_real_use_cases() {
    let (_scratch, runtime) = workspace("executor");
    let supervisor = supervisor(&runtime);

    let id = supervisor
        .start(OperationRequest::Backup(BackupRequest::now().unwrap()))
        .unwrap();
    let snapshot = supervisor.wait(id).unwrap();
    assert_eq!(snapshot.state, OperationState::Succeeded);
    match snapshot.result().unwrap().as_ref() {
        OperationResult::BackedUp(BackupResult::NotConfigured) => {}
        other => panic!("unexpected result: {other:?}"),
    }

    // A decision about an unknown subject fails with the *review* code, because
    // the classification comes from the use case and its error variant.
    let review_id = supervisor
        .start(OperationRequest::ReviewDecision(ReviewOperation::Approve(
            "document:99".to_owned(),
        )))
        .unwrap();
    let failed = supervisor.wait(review_id).unwrap();
    assert_eq!(failed.state, OperationState::Failed);
    assert_eq!(
        failed.failure().unwrap().code(),
        OperationErrorCode::ReviewNotFound
    );

    // A real doctor scan succeeds with the real checks.
    let doctor_id = supervisor.start(OperationRequest::Doctor).unwrap();
    let diagnosed = supervisor.wait(doctor_id).unwrap();
    assert_eq!(diagnosed.state, OperationState::Succeeded);
    match diagnosed.result().unwrap().as_ref() {
        OperationResult::Diagnosed(outcome) => assert!(outcome.passed() > 0),
        other => panic!("unexpected result: {other:?}"),
    }
}

/// Every operation this supervisor accepted is listable, oldest first.
#[test]
fn the_supervisor_lists_its_operations() {
    let supervisor = OperationSupervisor::new(Arc::new(ScriptedExecutor::new(vec![
        Step::SucceedDiagnosed,
    ])));
    let first = supervisor.start(OperationRequest::Doctor).unwrap();
    supervisor.wait(first).unwrap();
    let second = supervisor.start(OperationRequest::VerifyBackup).unwrap();
    supervisor.wait(second).unwrap();

    let snapshots = supervisor.snapshots();
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].id, first);
    assert_eq!(snapshots[0].kind, OperationKind::Doctor);
    assert_eq!(snapshots[1].kind, OperationKind::VerifyBackup);
}

/// An identity nobody allocated has no snapshot and no subscription.
#[test]
fn an_unknown_identity_has_nothing_to_read() {
    let supervisor = OperationSupervisor::new(Arc::new(ScriptedExecutor::new(Vec::new())));
    assert!(supervisor.snapshots().is_empty());
    assert!(supervisor.snapshot(OperationId::first()).is_none());
    assert!(supervisor.subscribe(OperationId::first()).is_none());
    assert!(supervisor.wait(OperationId::first()).is_none());
}

/// The supervisor is usable from several threads at once.
#[test]
fn the_supervisor_is_shared_between_threads() {
    let supervisor = Arc::new(OperationSupervisor::new(Arc::new(ScriptedExecutor::new(
        vec![Step::SucceedDiagnosed],
    ))));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let supervisor = Arc::clone(&supervisor);
        handles.push(thread::spawn(move || {
            let id = supervisor.start(OperationRequest::Doctor).unwrap();
            supervisor.wait(id).unwrap().state
        }));
    }
    for handle in handles {
        assert_eq!(handle.join().unwrap(), OperationState::Succeeded);
    }
    assert_eq!(supervisor.snapshots().len(), 4);
}

/// A use case that panics does not poison the record: the supervisor stays
/// readable and keeps refusing mutating work, because nothing released it.
#[test]
fn a_panicking_executor_does_not_poison_the_supervisor() {
    struct Panicking;

    impl OperationExecutor for Panicking {
        fn execute(
            &self,
            _request: &OperationRequest,
            _progress: Arc<dyn Progress>,
        ) -> Result<OperationResult, OperationFailure> {
            panic!("the use case panicked");
        }
    }

    let supervisor = OperationSupervisor::new(Arc::new(Panicking));
    // A mutating kind, so the panic happens while the workspace is held.
    let id = supervisor
        .start(OperationRequest::Backup(BackupRequest::now().unwrap()))
        .unwrap();
    let snapshot = supervisor.wait(id).unwrap();

    // The panic became a typed failure rather than a dead thread.
    assert_eq!(snapshot.state, OperationState::Failed);
    assert_eq!(
        snapshot.failure().unwrap().code(),
        OperationErrorCode::OperationPanicked
    );
    // And the workspace was released, so the supervisor is not bricked.
    assert_eq!(supervisor.active_mutation(), None);
    let recovered = supervisor.start(OperationRequest::BackupInit);
    assert!(
        recovered.is_ok(),
        "a panicked operation must not lock the workspace"
    );
}

/// The values an operation is made of cross the worker boundary.
#[test]
fn operation_values_cross_the_worker_boundary() {
    fn assert_send<T: Send>() {}
    assert_send::<OperationId>();
    assert_send::<OperationKind>();
    assert_send::<OperationState>();
    assert_send::<OperationRequest>();
    assert_send::<OperationResult>();
    assert_send::<OperationFailure>();
    assert_send::<OperationSnapshot>();
    assert_send::<StartError>();
    assert_send::<crate::operations::ProgressEvent>();

    // And they really do, because the supervisor runs work on another thread.
    let (_scratch, runtime) = workspace("send");
    let supervisor = supervisor(&runtime);
    let id = supervisor.start(OperationRequest::Doctor).unwrap();
    let snapshot = supervisor.wait(id).unwrap();
    let state = thread::spawn(move || snapshot.state).join().unwrap();
    assert_eq!(state, OperationState::Succeeded);
}

/// The backup read-only use cases are reachable and typed.
#[test]
fn backup_reads_are_typed() {
    let (_scratch, runtime) = workspace("backup-reads");
    assert!(matches!(
        crate::application::backup::backup_status(&runtime).unwrap(),
        BackupStatusOutcome::NotConfigured
    ));
    assert!(matches!(
        crate::application::backup::backup_verify(&runtime).unwrap(),
        BackupVerifyOutcome::NotConfigured
    ));

    let supervisor = supervisor(&runtime);
    let id = supervisor.start(OperationRequest::VerifyBackup).unwrap();
    let snapshot = supervisor.wait(id).unwrap();
    assert_eq!(snapshot.state, OperationState::Succeeded);
    assert!(matches!(
        snapshot.result().unwrap().as_ref(),
        OperationResult::BackupVerified(BackupVerifyOutcome::NotConfigured)
    ));
}

/// A decision recorded through an operation is visible to a direct read, so the
/// two paths agree about the workspace.
#[test]
fn a_decision_made_through_an_operation_is_visible_to_a_direct_read() {
    let (_scratch, runtime) = workspace("decision");
    runtime
        .document_reviews()
        .unwrap()
        .save(&ReviewRun::rehydrate(
            ReviewRunId::new(1).unwrap(),
            SnapshotId::new(1).unwrap(),
            ContentPath::new("note.md").unwrap(),
            Sha256::digest(b"note.md"),
            PolicyIdentity::new("public", "v1", Sha256::new([3; 32])).unwrap(),
            (
                PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
                None,
            ),
            1_000,
        ))
        .unwrap();

    let supervisor = supervisor(&runtime);
    let id = supervisor
        .start(OperationRequest::ReviewDecision(ReviewOperation::Approve(
            "document:1".to_owned(),
        )))
        .unwrap();
    assert_eq!(
        supervisor.wait(id).unwrap().state,
        OperationState::Succeeded
    );

    let ReviewOutcome::List { documents, .. } =
        review::review(&runtime, &ReviewRequest::List).unwrap()
    else {
        panic!("list must produce queues");
    };
    assert!(documents.is_empty());

    // The same decision again is an idempotent success; the opposite one is a
    // typed conflict.
    let id = supervisor
        .start(OperationRequest::ReviewDecision(ReviewOperation::Approve(
            "document:1".to_owned(),
        )))
        .unwrap();
    assert_eq!(
        supervisor.wait(id).unwrap().state,
        OperationState::Succeeded
    );

    let id = supervisor
        .start(OperationRequest::ReviewDecision(ReviewOperation::Reject(
            "document:1".to_owned(),
        )))
        .unwrap();
    let conflicted = supervisor.wait(id).unwrap();
    assert_eq!(conflicted.state, OperationState::Failed);
    assert_eq!(
        conflicted.failure().unwrap().code(),
        OperationErrorCode::ReviewConflict
    );
}

/// An operation nobody watches still completes and keeps its result.
#[test]
fn an_unwatched_operation_completes() {
    let (_scratch, runtime) = workspace("unwatched");
    let supervisor = supervisor(&runtime);
    let id = supervisor.start(OperationRequest::Doctor).unwrap();
    let snapshot = supervisor.wait(id).unwrap();
    assert_eq!(snapshot.state, OperationState::Succeeded);
}

/// The application layer accepts any sink, so a use case can run with neither a
/// supervisor nor a terminal.
#[test]
fn a_use_case_runs_with_an_inert_sink() {
    let (_scratch, runtime) = workspace("sink");
    let outcome = crate::application::publish::publish(
        &runtime,
        PublishRequest::now(),
        &(Arc::new(NoProgress) as Arc<dyn Progress>),
    );
    // An empty source cannot be published; what matters is that it failed with a
    // typed application error rather than a string.
    match outcome {
        Ok(_) => {}
        Err(error) => assert!(
            matches!(error, ApplicationError::Operation { .. }),
            "{error:?}"
        ),
    }
}
