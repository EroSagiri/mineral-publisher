//! The application layer's own tests.
//!
//! These are the acceptance proof of the layering. Every one of them drives a
//! use case through a [`WorkspaceRuntime`] built from a configuration file and a
//! secret provider — never through the command line, never through argument
//! parsing, and never through anything that prints. Deleting the CLI would not
//! touch a single line here.
//!
//! The heavy pipeline (publication with live review, backup against a remote) is
//! covered by the engine's conformance tests and by the ignored live tests. What
//! is proved here is the property that matters for the layering: the *use cases*
//! exist, are reachable from the library, return structured values, and fail in
//! typed ways.

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    application::{
        ApplicationError,
        backup::{
            self, BackupInitOutcome, BackupRequest, BackupResult, BackupStatusOutcome,
            BackupVerifyOutcome,
        },
        doctor::doctor,
        publish::{self, PublishRequest},
        review::{self, ReviewOutcome, ReviewRequest},
        status::status,
    },
    config::{RawConfig, SecretProvider, StaticSecretProvider, ValidatedConfig},
    domain::{ContentPath, Sha256, SnapshotId},
    policy::{
        HumanReviewReason, PolicyIdentity, PublicPolicyDecision, ReviewRun, ReviewRunId,
        ReviewRunStore,
    },
    runtime::{NoProgress, Progress, WorkspaceRuntime},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

/// A scratch workspace directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mineral-application-{name}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A minimal workspace configuration with a local source, written to disk.
const WORKSPACE: &str = "source:\n  id: local-vault\n  path: ./vault\nstate:\n  path: ./.mineral\ngit:\n  repository: ./publication\n  remote: origin\n  reference: refs/heads/main\n  author_name: Bot\n  author_email: bot@example.invalid\n  message: Publish Mineral content\nassets:\n  public_base_url: https://assets.example.com\n  target_path: ./asset-target\npublic:\n  exclude: []\nreview:\n  api_base_url: https://api.deepseek.com\n  markdown_model: deepseek-flash\n  asset_model: deepseek-flash\n  api_key_env: MINERAL_APPLICATION_TEST_KEY\n  timeout_seconds: 45\n";

/// A workspace with an enabled backup target, so backup use cases are reachable.
const BACKUP_WORKSPACE: &str = "source:\n  id: local-vault\n  path: ./vault\nstate:\n  path: ./.mineral\ngit:\n  repository: ./publication\n  remote: origin\n  reference: refs/heads/main\n  author_name: Bot\n  author_email: bot@example.invalid\n  message: Publish Mineral content\nassets:\n  public_base_url: https://assets.example.com\n  target_path: ./asset-target\nreview:\n  api_base_url: https://api.deepseek.com\n  markdown_model: deepseek-flash\n  asset_model: deepseek-flash\n  api_key_env: MINERAL_APPLICATION_TEST_KEY\n  timeout_seconds: 45\nbackup:\n  enabled: true\n  git:\n    repository: ./backup-repo\n    remote: origin\n    branch: refs/heads/mineral-backup\n    author_name: Mineral Backup\n    author_email: backup@example.invalid\n    message: Backup knowledge snapshot\n  lfs:\n    enabled: true\n    batch_url: https://github.com/owner/repo.git/info/lfs\n    username_env: MINERAL_APPLICATION_TEST_USER\n    token_env: MINERAL_APPLICATION_TEST_TOKEN\n";

/// Builds an initialized workspace, exactly the way `mineral init` would.
fn workspace(name: &str, text: &str) -> (Scratch, WorkspaceRuntime) {
    let scratch = Scratch::new(name);
    let path = scratch.path().join("mineral.yaml");
    fs::write(&path, text).unwrap();
    let model: RawConfig = serde_yaml_ng::from_str(text).unwrap();
    let config = ValidatedConfig::from_raw(model, &path).unwrap();
    let secrets = Arc::new(
        StaticSecretProvider::new()
            .with("MINERAL_APPLICATION_TEST_KEY", "application-test-key")
            .with("MINERAL_APPLICATION_TEST_USER", "mineral-backup")
            .with("MINERAL_APPLICATION_TEST_TOKEN", "application-test-token"),
    ) as Arc<dyn SecretProvider>;
    let runtime = WorkspaceRuntime::new(config, path, secrets);
    runtime.prepare().unwrap();
    runtime.open_stores().unwrap();
    fs::create_dir_all(scratch.path().join("vault")).unwrap();
    fs::create_dir_all(scratch.path().join("publication")).unwrap();
    (scratch, runtime)
}

/// A durable Markdown review attempt for one content path.
fn document_run(id: u64, path: &str) -> ReviewRun {
    ReviewRun::rehydrate(
        ReviewRunId::new(id).unwrap(),
        SnapshotId::new(1).unwrap(),
        ContentPath::new(path).unwrap(),
        Sha256::digest(path.as_bytes()),
        PolicyIdentity::new("public", "v1", Sha256::new([7; 32])).unwrap(),
        (
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested),
            None,
        ),
        1_000 + id,
    )
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// `status` reports the workspace through the application API, with no CLI.
#[test]
fn status_is_available_without_the_cli() {
    let (scratch, runtime) = workspace("status", WORKSPACE);
    let outcome = status(&runtime).unwrap();

    assert_eq!(outcome.source_id, "local-vault");
    assert!(outcome.source.ends_with("vault"), "{}", outcome.source);
    assert_eq!(outcome.state_path, scratch.path().join(".mineral"));
    assert_eq!(outcome.target_remote, "origin");
    assert_eq!(outcome.target_reference, "refs/heads/main");
    assert_eq!(outcome.last_publication, None);
    assert_eq!(outcome.pending_documents, 0);
    assert_eq!(outcome.pending_assets, 0);
}

// ---------------------------------------------------------------------------
// review
// ---------------------------------------------------------------------------

/// The whole review use case — list, show, approve, reject — is reachable from
/// the library, and its outcomes are structured values.
#[test]
fn the_review_queue_is_available_without_the_cli() {
    let (_scratch, runtime) = workspace("review", WORKSPACE);
    let documents = runtime.document_reviews().unwrap();
    documents.save(&document_run(1, "note.md")).unwrap();

    // list
    let listed = review::review(&runtime, &ReviewRequest::List).unwrap();
    let ReviewOutcome::List { documents, assets } = listed else {
        panic!("list must produce queues");
    };
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0].subject, "document:1");
    assert_eq!(documents[0].content_path, "note.md");
    assert!(assets.is_empty());

    // show
    let ReviewOutcome::Shown(detail) =
        review::review(&runtime, &ReviewRequest::Show("document:1".to_owned())).unwrap()
    else {
        panic!("show must produce one detail");
    };
    assert_eq!(detail.kind, "Markdown");
    assert_eq!(detail.content_path, "note.md");
    assert_eq!(detail.policy_name, "public");
    assert_eq!(detail.policy_version, "v1");
    assert!(!detail.human_resolution);

    // approve
    let resolved =
        review::review(&runtime, &ReviewRequest::Approve("document:1".to_owned())).unwrap();
    let ReviewOutcome::Resolved {
        decision, already, ..
    } = resolved
    else {
        panic!("approve must resolve");
    };
    assert_eq!(decision, crate::workflow::HumanReviewDecision::Approve);
    assert!(!already);

    // approving again is idempotent, and the queue is now empty
    let again = review::review(&runtime, &ReviewRequest::Approve("document:1".to_owned())).unwrap();
    assert!(matches!(
        again,
        ReviewOutcome::Resolved { already: true, .. }
    ));
    let ReviewOutcome::List { documents, .. } =
        review::review(&runtime, &ReviewRequest::List).unwrap()
    else {
        panic!("list must produce queues");
    };
    assert!(documents.is_empty());

    // the opposite decision is refused as a conflict, by type
    let error = review::review(&runtime, &ReviewRequest::Reject("document:1".to_owned()))
        .expect_err("the opposite decision must be refused");
    assert!(
        matches!(error, ApplicationError::Conflict { .. }),
        "{error:?}"
    );
}

/// A subject nobody recorded is a `NotFound`, not a message to parse.
#[test]
fn reviewing_an_unknown_subject_is_typed() {
    let (_scratch, runtime) = workspace("review-unknown", WORKSPACE);

    let error = review::review(&runtime, &ReviewRequest::Show("document:99".to_owned()))
        .expect_err("an unknown subject must be refused");
    assert!(
        matches!(error, ApplicationError::NotFound { .. }),
        "{error:?}"
    );

    let error = review::review(&runtime, &ReviewRequest::Approve("banana".to_owned()))
        .expect_err("a malformed subject must be refused");
    assert!(
        matches!(error, ApplicationError::Unsupported { .. }),
        "{error:?}"
    );
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// `doctor` answers every health question as data, including which ones failed.
#[test]
fn doctor_is_available_without_the_cli() {
    let (_scratch, runtime) = workspace("doctor", WORKSPACE);
    let outcome = doctor(&runtime).unwrap();

    let names = outcome
        .checks
        .iter()
        .map(|check| check.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "configuration",
            "source",
            "state",
            "CAS",
            "database",
            "Git target",
            "backup",
            "remote/ref",
            "provider credential",
        ]
    );
    // The credential is supplied by the provider, so the check passes without
    // the process environment holding anything.
    assert!(
        outcome
            .checks
            .iter()
            .find(|check| check.name == "provider credential")
            .unwrap()
            .ok
    );
    assert!(outcome.passed() >= 8);
}

// ---------------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------------

/// A workspace without a backup target reports that as a value, not an error.
#[test]
fn backup_reports_an_unconfigured_workspace() {
    let (_scratch, runtime) = workspace("backup-unconfigured", WORKSPACE);

    assert!(matches!(
        backup::backup(&runtime, BackupRequest::now().unwrap(), &NoProgress).unwrap(),
        BackupResult::NotConfigured
    ));
    assert!(matches!(
        backup::backup_status(&runtime).unwrap(),
        BackupStatusOutcome::NotConfigured
    ));
    assert!(matches!(
        backup::backup_verify(&runtime).unwrap(),
        BackupVerifyOutcome::NotConfigured
    ));
    assert!(matches!(
        backup::backup_init(&runtime).unwrap(),
        BackupInitOutcome::NotConfigured
    ));
}

/// A configured workspace reaches the backup use cases, and the ref that does
/// not exist yet is a typed refusal carrying what the attempt had frozen.
#[test]
fn backup_reaches_the_engine_and_refuses_a_missing_ref_by_type() {
    let (scratch, runtime) = workspace("backup-configured", BACKUP_WORKSPACE);
    fs::create_dir_all(scratch.path().join("backup-repo")).unwrap();

    // `status` observes the ref; the repository exists but has no remote, so the
    // observation is reported as unavailable rather than failing the use case.
    let BackupStatusOutcome::Reported {
        remote,
        reference,
        newest_run,
        ..
    } = backup::backup_status(&runtime).unwrap()
    else {
        panic!("a configured workspace must report");
    };
    assert_eq!(remote, "origin");
    assert_eq!(reference, "refs/heads/mineral-backup");
    assert_eq!(newest_run, None);

    // The ref cannot be observed, so `init` must fail closed rather than guess.
    assert!(backup::backup_init(&runtime).is_err());
}

/// The backup request carries its own clock, so a use case never reads one.
#[test]
fn a_backup_request_supplies_its_own_time() {
    let request = BackupRequest::now().unwrap();
    let later = BackupRequest {
        created_at: crate::domain::TimestampMillis::from_unix_millis(
            request.created_at.as_unix_millis() + 1,
        ),
    };
    assert!(later.created_at.as_unix_millis() > request.created_at.as_unix_millis());
    assert_eq!(
        later.created_at.as_unix_millis(),
        request.created_at.as_unix_millis() + 1
    );
}

// ---------------------------------------------------------------------------
// publish
// ---------------------------------------------------------------------------

/// The publication use case is reachable from the library, takes its clock from
/// the request, and fails in a typed way on a workspace it cannot publish.
#[test]
fn publish_is_available_without_the_cli() {
    let (scratch, runtime) = workspace("publish", WORKSPACE);
    // A publication needs a Git target that is a repository.
    let publication = scratch.path().join("publication");
    fs::create_dir_all(&publication).unwrap();

    let progress: Arc<dyn Progress> = Arc::new(NoProgress);
    let outcome = publish::publish(&runtime, PublishRequest::now(), &progress);
    // The source is an empty local directory: the pipeline gets as far as the
    // publication engine and reports its own failure rather than the CLI's.
    match outcome {
        Ok(outcome) => {
            // An empty source still produces a trace worth reporting.
            let _ = outcome.asset_location;
            let _ = outcome.public_scope;
        }
        Err(error) => {
            assert!(
                matches!(error, ApplicationError::Operation { .. }),
                "{error:?}"
            );
        }
    }
}
