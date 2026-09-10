use std::{error::Error, fmt, path::Path};

use rusqlite::{Connection, OptionalExtension, params, types::Type};

use crate::{
    domain::{ContentPath, Sha256, SnapshotId},
    policy::{PolicyIdentity, PublicPolicyDecision, ReviewRun, ReviewRunId, ReviewRunStore},
};

const SCHEMA_VERSION: i64 = 1;

/// Local SQLite implementation of the narrow Review Run persistence boundary.
pub struct SqliteReviewRunStore {
    connection: Connection,
}

impl SqliteReviewRunStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqliteReviewRunStoreError> {
        let connection = Connection::open(path).map_err(SqliteReviewRunStoreError::Sqlite)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(SqliteReviewRunStoreError::Sqlite)?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(SqliteReviewRunStoreError::Sqlite)?;
        match version {
            0 => connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE review_runs (
                         id INTEGER PRIMARY KEY CHECK (id > 0),
                         snapshot_id INTEGER NOT NULL CHECK (snapshot_id > 0),
                         content_path TEXT NOT NULL,
                         content_sha256 BLOB NOT NULL CHECK (length(content_sha256) = 32),
                         policy_name TEXT NOT NULL CHECK (length(trim(policy_name)) > 0),
                         policy_version TEXT NOT NULL CHECK (length(trim(policy_version)) > 0),
                         policy_hash BLOB NOT NULL CHECK (length(policy_hash) = 32),
                         decision_kind TEXT NOT NULL CHECK (
                             decision_kind IN (
                                 'program_issues',
                                 'approved',
                                 'rejected',
                                 'needs_human_review'
                             )
                         ),
                         decision_json TEXT NOT NULL,
                         reviewer_called INTEGER NOT NULL CHECK (reviewer_called IN (0, 1)),
                         created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
                         CHECK (
                             (decision_kind = 'program_issues' AND reviewer_called = 0)
                             OR
                             (decision_kind != 'program_issues' AND reviewer_called = 1)
                         )
                     );
                     CREATE INDEX review_runs_by_snapshot
                         ON review_runs(snapshot_id, content_path, id);
                     CREATE INDEX review_runs_pending_human_review
                         ON review_runs(decision_kind, snapshot_id, content_path, id);
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
                .map_err(SqliteReviewRunStoreError::Sqlite)?,
            SCHEMA_VERSION => {}
            version => return Err(SqliteReviewRunStoreError::UnsupportedSchemaVersion(version)),
        }
        Ok(Self { connection })
    }

    fn load_many(
        &self,
        sql: &str,
        parameter: Option<i64>,
    ) -> Result<Vec<ReviewRun>, SqliteReviewRunStoreError> {
        let mut statement = self
            .connection
            .prepare(sql)
            .map_err(SqliteReviewRunStoreError::Sqlite)?;
        let rows = match parameter {
            Some(value) => statement.query_map([value], row_to_review_run),
            None => statement.query_map([], row_to_review_run),
        }
        .map_err(SqliteReviewRunStoreError::Sqlite)?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SqliteReviewRunStoreError::Sqlite)
    }
}

impl ReviewRunStore for SqliteReviewRunStore {
    type Error = SqliteReviewRunStoreError;

    fn save(&self, run: &ReviewRun) -> Result<(), Self::Error> {
        let decision_json = serde_json::to_string(run.decision())
            .map_err(SqliteReviewRunStoreError::Serialization)?;
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO review_runs (
                     id, snapshot_id, content_path, content_sha256,
                     policy_name, policy_version, policy_hash,
                     decision_kind, decision_json, reviewer_called,
                     created_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    to_sqlite_integer("review run id", run.id().get())?,
                    to_sqlite_integer("snapshot id", run.snapshot_id().get())?,
                    run.content_path().as_str(),
                    run.content_sha256().as_bytes().as_slice(),
                    run.policy().name(),
                    run.policy().version(),
                    run.policy().hash().as_bytes().as_slice(),
                    decision_kind(run.decision()),
                    decision_json,
                    run.reviewer_was_called(),
                    to_sqlite_integer("review timestamp", run.created_at_unix_ms())?,
                ],
            )
            .map_err(SqliteReviewRunStoreError::Sqlite)?;

        if inserted == 1 {
            return Ok(());
        }
        match self.get(run.id())? {
            Some(stored) if stored == *run => Ok(()),
            Some(_) => Err(SqliteReviewRunStoreError::ConflictingReviewRunId(run.id())),
            None => Err(SqliteReviewRunStoreError::Persistence(
                "review run insert was ignored without an existing row".to_owned(),
            )),
        }
    }

    fn get(&self, id: ReviewRunId) -> Result<Option<ReviewRun>, Self::Error> {
        self.connection
            .query_row(
                "SELECT id, snapshot_id, content_path, content_sha256,
                        policy_name, policy_version, policy_hash,
                        decision_kind, decision_json, reviewer_called,
                        created_at_unix_ms
                 FROM review_runs
                 WHERE id = ?1",
                [to_sqlite_integer("review run id", id.get())?],
                row_to_review_run,
            )
            .optional()
            .map_err(SqliteReviewRunStoreError::Sqlite)
    }

    fn list_by_snapshot(&self, snapshot_id: SnapshotId) -> Result<Vec<ReviewRun>, Self::Error> {
        self.load_many(
            "SELECT id, snapshot_id, content_path, content_sha256,
                    policy_name, policy_version, policy_hash,
                    decision_kind, decision_json, reviewer_called,
                    created_at_unix_ms
             FROM review_runs
             WHERE snapshot_id = ?1
             ORDER BY content_path COLLATE BINARY ASC, id ASC",
            Some(to_sqlite_integer("snapshot id", snapshot_id.get())?),
        )
    }

    fn list_pending_human_review(&self) -> Result<Vec<ReviewRun>, Self::Error> {
        self.load_many(
            "SELECT id, snapshot_id, content_path, content_sha256,
                    policy_name, policy_version, policy_hash,
                    decision_kind, decision_json, reviewer_called,
                    created_at_unix_ms
             FROM review_runs
             WHERE decision_kind = 'needs_human_review'
             ORDER BY snapshot_id ASC, content_path COLLATE BINARY ASC, id ASC",
            None,
        )
    }
}

fn row_to_review_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewRun> {
    let id = positive_id(row.get(0)?, 0, "review run id", ReviewRunId::new)?;
    let snapshot_id = positive_id(row.get(1)?, 1, "snapshot id", SnapshotId::new)?;
    let content_path = ContentPath::new(row.get::<_, String>(2)?).map_err(|error| {
        conversion_error(2, Type::Text, format!("invalid content path: {error}"))
    })?;
    let content_sha256 = sha256_column(row, 3)?;
    let policy_name: String = row.get(4)?;
    let policy_version: String = row.get(5)?;
    let policy_hash = sha256_column(row, 6)?;
    let policy =
        PolicyIdentity::new(policy_name, policy_version, policy_hash).map_err(|error| {
            conversion_error(4, Type::Text, format!("invalid policy identity: {error}"))
        })?;
    let stored_kind: String = row.get(7)?;
    let decision_json: String = row.get(8)?;
    let decision: PublicPolicyDecision = serde_json::from_str(&decision_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(8, Type::Text, Box::new(error))
    })?;
    if stored_kind != decision_kind(&decision) {
        return Err(conversion_error(
            7,
            Type::Text,
            "decision kind does not match decision payload",
        ));
    }
    let reviewer_called: bool = row.get(9)?;
    let derived_reviewer_called = !matches!(decision, PublicPolicyDecision::ProgramIssues(_));
    if reviewer_called != derived_reviewer_called {
        return Err(conversion_error(
            9,
            Type::Integer,
            "reviewer-called flag does not match decision",
        ));
    }
    let created_at_unix_ms = nonnegative_u64(row.get(10)?, 10, "review timestamp")?;

    Ok(ReviewRun::rehydrate(
        id,
        snapshot_id,
        content_path,
        content_sha256,
        policy,
        decision,
        created_at_unix_ms,
    ))
}

fn decision_kind(decision: &PublicPolicyDecision) -> &'static str {
    match decision {
        PublicPolicyDecision::ProgramIssues(_) => "program_issues",
        PublicPolicyDecision::ReviewApproved => "approved",
        PublicPolicyDecision::ReviewRejected => "rejected",
        PublicPolicyDecision::NeedsHumanReview(_) => "needs_human_review",
    }
}

fn to_sqlite_integer(field: &'static str, value: u64) -> Result<i64, SqliteReviewRunStoreError> {
    value
        .try_into()
        .map_err(|_| SqliteReviewRunStoreError::ValueOutOfRange(field))
}

fn positive_id<T, E>(
    value: i64,
    column: usize,
    field: &'static str,
    constructor: impl FnOnce(u64) -> Result<T, E>,
) -> rusqlite::Result<T>
where
    E: fmt::Display,
{
    let value = nonnegative_u64(value, column, field)?;
    constructor(value).map_err(|error| conversion_error(column, Type::Integer, error.to_string()))
}

fn nonnegative_u64(value: i64, column: usize, field: &str) -> rusqlite::Result<u64> {
    value.try_into().map_err(|_| {
        conversion_error(
            column,
            Type::Integer,
            format!("{field} must be non-negative"),
        )
    })
}

fn sha256_column(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Sha256> {
    let bytes: Vec<u8> = row.get(column)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|bytes: Vec<u8>| {
        conversion_error(
            column,
            Type::Blob,
            format!("SHA-256 must contain 32 bytes, got {}", bytes.len()),
        )
    })?;
    Ok(Sha256::new(bytes))
}

fn conversion_error(column: usize, data_type: Type, message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        data_type,
        Box::new(CorruptReviewRun(message.into())),
    )
}

#[derive(Debug)]
struct CorruptReviewRun(String);

impl fmt::Display for CorruptReviewRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CorruptReviewRun {}

#[derive(Debug)]
pub enum SqliteReviewRunStoreError {
    Sqlite(rusqlite::Error),
    Serialization(serde_json::Error),
    UnsupportedSchemaVersion(i64),
    ConflictingReviewRunId(ReviewRunId),
    ValueOutOfRange(&'static str),
    Persistence(String),
}

impl fmt::Display for SqliteReviewRunStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => {
                write!(formatter, "SQLite review-run persistence failed: {error}")
            }
            Self::Serialization(error) => {
                write!(formatter, "review decision serialization failed: {error}")
            }
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "unsupported review-run schema version: {version}"
                )
            }
            Self::ConflictingReviewRunId(id) => {
                write!(
                    formatter,
                    "review run id {} already stores a different fact",
                    id.get()
                )
            }
            Self::ValueOutOfRange(field) => {
                write!(formatter, "{field} is outside SQLite's integer range")
            }
            Self::Persistence(message) => formatter.write_str(message),
        }
    }
}

impl Error for SqliteReviewRunStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime},
    };

    use crate::{
        content::{AnalyzedMarkdown, MarkdownReferenceParser, Resolution, ResolvedReference},
        domain::{Sha256, Snapshot, SnapshotFile, SourceId},
        policy::{
            HumanReviewReason, MarkdownFrontmatterParser, PolicyIdentity, PrivacyFilter,
            PublicPolicy, ReviewCandidate, ReviewDecision, ReviewRunError, Reviewer, ReviewerError,
        },
    };

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-review-store-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn database(&self) -> PathBuf {
            self.0.join("reviews.sqlite3")
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct FixedReviewer {
        calls: Cell<usize>,
        response: Result<ReviewDecision, ReviewerError>,
    }

    impl Reviewer for FixedReviewer {
        fn review(&self, _candidate: &ReviewCandidate) -> Result<ReviewDecision, ReviewerError> {
            self.calls.set(self.calls.get() + 1);
            self.response.clone()
        }
    }

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    fn analyzed(
        document_path: &str,
        markdown: &str,
        resolutions: Vec<Resolution>,
    ) -> AnalyzedMarkdown {
        let references = MarkdownReferenceParser::parse(markdown);
        assert_eq!(references.len(), resolutions.len());
        let references = references
            .into_iter()
            .zip(resolutions)
            .map(|(reference, resolution)| ResolvedReference::new(reference, resolution))
            .collect();
        AnalyzedMarkdown::with_frontmatter(
            SnapshotFile::new(
                path(document_path),
                markdown.len() as u64,
                Sha256::digest(markdown.as_bytes()),
                None,
            ),
            MarkdownFrontmatterParser::parse(markdown),
            references,
        )
    }

    fn outcome(
        document: AnalyzedMarkdown,
        response: Result<ReviewDecision, ReviewerError>,
    ) -> crate::policy::PublicPolicyOutcome {
        let filtered = PrivacyFilter::filter(vec![document]);
        let reviewer = FixedReviewer {
            calls: Cell::new(0),
            response,
        };
        PublicPolicy::evaluate(filtered.into_public_candidates(), &reviewer)
            .pop()
            .unwrap()
    }

    fn snapshot(id: u64, files: Vec<SnapshotFile>) -> Snapshot {
        Snapshot::new(
            SnapshotId::new(id).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test-vault").unwrap(),
            files,
        )
        .unwrap()
    }

    fn policy() -> PolicyIdentity {
        PolicyIdentity::new("public", "public-v1", Sha256::new([42; 32])).unwrap()
    }

    fn reviewed_file(document_path: &str, markdown: &str) -> SnapshotFile {
        SnapshotFile::new(
            path(document_path),
            markdown.len() as u64,
            Sha256::digest(markdown.as_bytes()),
            None,
        )
    }

    fn run(
        id: u64,
        snapshot: &Snapshot,
        outcome: &crate::policy::PublicPolicyOutcome,
        created_at_ms: u64,
    ) -> ReviewRun {
        ReviewRun::from_policy_outcome(
            ReviewRunId::new(id).unwrap(),
            snapshot,
            outcome,
            policy(),
            SystemTime::UNIX_EPOCH + Duration::from_millis(created_at_ms),
        )
        .unwrap()
    }

    #[test]
    fn approved_survives_closing_and_reopening_the_store() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let snapshot = snapshot(1, vec![reviewed_file("note.md", "body")]);
        let outcome = outcome(
            analyzed("note.md", "body", vec![]),
            Ok(ReviewDecision::Approve),
        );
        let expected = run(1, &snapshot, &outcome, 1_000);

        SqliteReviewRunStore::open(&database)
            .unwrap()
            .save(&expected)
            .unwrap();
        let reopened = SqliteReviewRunStore::open(&database).unwrap();

        assert_eq!(reopened.get(expected.id()).unwrap(), Some(expected));
    }

    #[test]
    fn rejected_round_trips_without_becoming_a_boolean() {
        let directory = TestDirectory::new();
        let store = SqliteReviewRunStore::open(directory.database()).unwrap();
        let snapshot = snapshot(1, vec![reviewed_file("note.md", "body")]);
        let outcome = outcome(
            analyzed("note.md", "body", vec![]),
            Ok(ReviewDecision::Reject),
        );
        let expected = run(1, &snapshot, &outcome, 1_000);

        store.save(&expected).unwrap();

        assert_eq!(
            store.get(expected.id()).unwrap().unwrap().decision(),
            &PublicPolicyDecision::ReviewRejected
        );
    }

    #[test]
    fn program_issues_and_reviewer_not_called_fact_round_trip_intact() {
        let directory = TestDirectory::new();
        let store = SqliteReviewRunStore::open(directory.database()).unwrap();
        let markdown = "![[missing.png]]";
        let snapshot = snapshot(1, vec![reviewed_file("note.md", markdown)]);
        let outcome = outcome(
            analyzed(
                "note.md",
                markdown,
                vec![Resolution::Missing {
                    target: "missing.png".to_owned(),
                }],
            ),
            Ok(ReviewDecision::Approve),
        );
        let expected = run(1, &snapshot, &outcome, 1_000);

        store.save(&expected).unwrap();
        let actual = store.get(expected.id()).unwrap().unwrap();

        assert_eq!(actual, expected);
        assert!(!actual.reviewer_was_called());
        assert!(matches!(
            actual.decision(),
            PublicPolicyDecision::ProgramIssues(issues)
                if issues.len() == 1
                    && issues[0].origin().target() == "missing.png"
        ));
    }

    #[test]
    fn needs_human_review_query_includes_requested_and_failed_with_error_reason() {
        let directory = TestDirectory::new();
        let store = SqliteReviewRunStore::open(directory.database()).unwrap();
        let snapshot = snapshot(
            1,
            vec![
                reviewed_file("requested.md", "body"),
                reviewed_file("failed.md", "body"),
                reviewed_file("approved.md", "body"),
            ],
        );
        let requested = outcome(
            analyzed("requested.md", "body", vec![]),
            Ok(ReviewDecision::NeedsHumanReview),
        );
        let failed = outcome(
            analyzed("failed.md", "body", vec![]),
            Err(ReviewerError::new("provider unavailable")),
        );
        let approved = outcome(
            analyzed("approved.md", "body", vec![]),
            Ok(ReviewDecision::Approve),
        );
        for run in [
            run(3, &snapshot, &requested, 3_000),
            run(2, &snapshot, &failed, 2_000),
            run(1, &snapshot, &approved, 1_000),
        ] {
            store.save(&run).unwrap();
        }

        let pending = store.list_pending_human_review().unwrap();

        assert_eq!(
            pending
                .iter()
                .map(|run| run.content_path().as_str())
                .collect::<Vec<_>>(),
            ["failed.md", "requested.md"]
        );
        assert!(matches!(
            pending[0].decision(),
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerFailed(error))
                if error.message() == "provider unavailable"
        ));
        assert!(matches!(
            pending[1].decision(),
            PublicPolicyDecision::NeedsHumanReview(HumanReviewReason::ReviewerRequested)
        ));
    }

    #[test]
    fn snapshots_and_documents_remain_independent_and_queries_are_deterministic() {
        let directory = TestDirectory::new();
        let store = SqliteReviewRunStore::open(directory.database()).unwrap();
        let reverse_store =
            SqliteReviewRunStore::open(directory.path().join("reverse.sqlite3")).unwrap();
        let snapshot_one = snapshot(
            1,
            vec![
                reviewed_file("z.md", "first z"),
                reviewed_file("note.md", "first note"),
            ],
        );
        let snapshot_two = snapshot(2, vec![reviewed_file("note.md", "second note")]);
        let z = outcome(
            analyzed("z.md", "first z", vec![]),
            Ok(ReviewDecision::Approve),
        );
        let first_note = outcome(
            analyzed("note.md", "first note", vec![]),
            Ok(ReviewDecision::Reject),
        );
        let second_note = outcome(
            analyzed("note.md", "second note", vec![]),
            Ok(ReviewDecision::Approve),
        );
        let inserted = [
            run(30, &snapshot_one, &z, 3_000),
            run(20, &snapshot_two, &second_note, 2_000),
            run(10, &snapshot_one, &first_note, 1_000),
        ];
        for run in &inserted {
            store.save(run).unwrap();
        }
        for run in inserted.iter().rev() {
            reverse_store.save(run).unwrap();
        }

        let first = store.list_by_snapshot(snapshot_one.id()).unwrap();
        let second = store.list_by_snapshot(snapshot_two.id()).unwrap();

        assert_eq!(
            first
                .iter()
                .map(|run| (run.content_path().as_str(), run.id().get()))
                .collect::<Vec<_>>(),
            [("note.md", 10), ("z.md", 30)]
        );
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].content_path().as_str(), "note.md");
        assert_ne!(first[0].content_sha256(), second[0].content_sha256());
        assert_eq!(
            first,
            reverse_store.list_by_snapshot(snapshot_one.id()).unwrap()
        );
    }

    #[test]
    fn identical_retry_is_idempotent_but_conflicting_id_is_rejected() {
        let directory = TestDirectory::new();
        let store = SqliteReviewRunStore::open(directory.database()).unwrap();
        let snapshot = snapshot(1, vec![reviewed_file("note.md", "body")]);
        let approved = outcome(
            analyzed("note.md", "body", vec![]),
            Ok(ReviewDecision::Approve),
        );
        let rejected = outcome(
            analyzed("note.md", "body", vec![]),
            Ok(ReviewDecision::Reject),
        );
        let first = run(1, &snapshot, &approved, 1_000);
        let second_attempt = run(2, &snapshot, &rejected, 2_000);
        let conflicting = run(1, &snapshot, &rejected, 1_000);

        store.save(&first).unwrap();
        store.save(&first).unwrap();
        store.save(&second_attempt).unwrap();
        let error = store.save(&conflicting).unwrap_err();

        assert!(matches!(
            error,
            SqliteReviewRunStoreError::ConflictingReviewRunId(id) if id == first.id()
        ));
        assert_eq!(
            store.list_by_snapshot(snapshot.id()).unwrap(),
            [first, second_attempt]
        );
    }

    #[test]
    fn review_run_rejects_a_policy_outcome_from_different_content() {
        let snapshot = snapshot(1, vec![reviewed_file("note.md", "snapshot body")]);
        let outcome = outcome(
            analyzed("note.md", "reviewed body", vec![]),
            Ok(ReviewDecision::Approve),
        );

        let error = ReviewRun::from_policy_outcome(
            ReviewRunId::new(1).unwrap(),
            &snapshot,
            &outcome,
            policy(),
            SystemTime::UNIX_EPOCH,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ReviewRunError::ContentIdentityMismatch { .. }
        ));
    }

    #[test]
    fn open_and_write_failures_are_reported() {
        let directory = TestDirectory::new();
        let open_error = match SqliteReviewRunStore::open(directory.path()) {
            Ok(_) => panic!("opening a directory as SQLite unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(matches!(open_error, SqliteReviewRunStoreError::Sqlite(_)));

        let store = SqliteReviewRunStore::open(directory.database()).unwrap();
        store
            .connection
            .execute("DROP TABLE review_runs", [])
            .unwrap();
        let snapshot = snapshot(1, vec![reviewed_file("note.md", "body")]);
        let outcome = outcome(
            analyzed("note.md", "body", vec![]),
            Ok(ReviewDecision::Approve),
        );
        let error = store.save(&run(1, &snapshot, &outcome, 1_000)).unwrap_err();

        assert!(matches!(error, SqliteReviewRunStoreError::Sqlite(_)));
    }
}
