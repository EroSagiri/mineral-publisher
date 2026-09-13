use std::{error::Error, fmt, path::Path};

use rusqlite::{Connection, OptionalExtension, params, types::Type};

use crate::{
    domain::{ContentPath, Sha256, SnapshotId},
    policy::PolicyIdentity,
    workflow::{
        AssetHumanReviewReason, AssetReviewDecision, AssetReviewDisposition, AssetReviewOutcome,
        AssetReviewRun, AssetReviewRunId, AssetReviewRunStore,
    },
};

const SCHEMA_VERSION: i64 = 1;

/// Local SQLite implementation of the narrow Asset Review Run audit boundary.
pub struct SqliteAssetReviewRunStore {
    connection: Connection,
}

impl SqliteAssetReviewRunStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqliteAssetReviewRunStoreError> {
        let connection = Connection::open(path).map_err(SqliteAssetReviewRunStoreError::Sqlite)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(SqliteAssetReviewRunStoreError::Sqlite)?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(SqliteAssetReviewRunStoreError::Sqlite)?;
        match version {
            0 => connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                 CREATE TABLE asset_review_runs (
                   id INTEGER PRIMARY KEY CHECK (id > 0),
                   snapshot_id INTEGER NOT NULL CHECK (snapshot_id > 0),
                   content_path TEXT NOT NULL,
                   content_sha256 BLOB NOT NULL CHECK (length(content_sha256) = 32),
                   policy_name TEXT NOT NULL CHECK (length(trim(policy_name)) > 0),
                   policy_version TEXT NOT NULL CHECK (length(trim(policy_version)) > 0),
                   policy_hash BLOB NOT NULL CHECK (length(policy_hash) = 32),
                   outcome_kind TEXT NOT NULL CHECK (outcome_kind IN (
                     'blocked', 'reviewer_approved', 'reviewer_rejected',
                     'reviewer_needs_human_review', 'policy_needs_human_review', 'reviewer_failed'
                   )),
                   outcome_json TEXT NOT NULL,
                   reviewer_called INTEGER NOT NULL CHECK (reviewer_called IN (0, 1)),
                   created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0)
                 );
                 CREATE INDEX asset_review_runs_by_snapshot
                   ON asset_review_runs(snapshot_id, content_path, id);
                 CREATE INDEX asset_review_runs_pending_human_review
                   ON asset_review_runs(outcome_kind, snapshot_id, content_path, id);
                 PRAGMA user_version = 1;
                 COMMIT;",
                )
                .map_err(SqliteAssetReviewRunStoreError::Sqlite)?,
            SCHEMA_VERSION => {}
            version => {
                return Err(SqliteAssetReviewRunStoreError::UnsupportedSchemaVersion(
                    version,
                ));
            }
        }
        Ok(Self { connection })
    }

    fn load_many(
        &self,
        sql: &str,
        parameter: Option<i64>,
    ) -> Result<Vec<AssetReviewRun>, SqliteAssetReviewRunStoreError> {
        let mut statement = self
            .connection
            .prepare(sql)
            .map_err(SqliteAssetReviewRunStoreError::Sqlite)?;
        let rows = match parameter {
            Some(value) => statement.query_map([value], row_to_asset_review_run),
            None => statement.query_map([], row_to_asset_review_run),
        }
        .map_err(SqliteAssetReviewRunStoreError::Sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SqliteAssetReviewRunStoreError::Sqlite)
    }
}

impl AssetReviewRunStore for SqliteAssetReviewRunStore {
    type Error = SqliteAssetReviewRunStoreError;

    fn save(&self, run: &AssetReviewRun) -> Result<(), Self::Error> {
        let outcome_json = serde_json::to_string(run.outcome())
            .map_err(SqliteAssetReviewRunStoreError::Serialization)?;
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO asset_review_runs (
               id, snapshot_id, content_path, content_sha256, policy_name, policy_version, policy_hash,
               outcome_kind, outcome_json, reviewer_called, created_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                integer("asset review run id", run.id().get())?, integer("snapshot id", run.snapshot_id().get())?,
                run.content_path().as_str(), run.content_sha256().as_bytes().as_slice(),
                run.policy().name(), run.policy().version(), run.policy().hash().as_bytes().as_slice(),
                outcome_kind(run.outcome()), outcome_json, run.reviewer_was_called(),
                integer("asset review timestamp", run.created_at_unix_ms())?,
            ],
        ).map_err(SqliteAssetReviewRunStoreError::Sqlite)?;
        if inserted == 1 {
            return Ok(());
        }
        match self.get(run.id())? {
            Some(stored) if stored == *run => Ok(()),
            Some(_) => Err(SqliteAssetReviewRunStoreError::ConflictingAssetReviewRunId(
                run.id(),
            )),
            None => Err(SqliteAssetReviewRunStoreError::Persistence(
                "asset review run insert was ignored without an existing row".to_owned(),
            )),
        }
    }

    fn get(&self, id: AssetReviewRunId) -> Result<Option<AssetReviewRun>, Self::Error> {
        self.connection.query_row(
            "SELECT id, snapshot_id, content_path, content_sha256, policy_name, policy_version, policy_hash,
                    outcome_kind, outcome_json, reviewer_called, created_at_unix_ms
             FROM asset_review_runs WHERE id = ?1", [integer("asset review run id", id.get())?], row_to_asset_review_run,
        ).optional().map_err(SqliteAssetReviewRunStoreError::Sqlite)
    }

    fn list_by_snapshot(
        &self,
        snapshot_id: SnapshotId,
    ) -> Result<Vec<AssetReviewRun>, Self::Error> {
        self.load_many(
            "SELECT id, snapshot_id, content_path, content_sha256, policy_name, policy_version, policy_hash,
                    outcome_kind, outcome_json, reviewer_called, created_at_unix_ms
             FROM asset_review_runs WHERE snapshot_id = ?1 ORDER BY content_path COLLATE BINARY ASC, id ASC",
            Some(integer("snapshot id", snapshot_id.get())?),
        )
    }

    fn list_pending_human_review(&self) -> Result<Vec<AssetReviewRun>, Self::Error> {
        self.load_many(
            "SELECT id, snapshot_id, content_path, content_sha256, policy_name, policy_version, policy_hash,
                    outcome_kind, outcome_json, reviewer_called, created_at_unix_ms
             FROM asset_review_runs
             WHERE outcome_kind IN ('reviewer_needs_human_review', 'policy_needs_human_review', 'reviewer_failed')
             ORDER BY snapshot_id ASC, content_path COLLATE BINARY ASC, id ASC", None,
        )
    }
}

fn row_to_asset_review_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssetReviewRun> {
    let id = positive_id(row.get(0)?, 0, "asset review run id", AssetReviewRunId::new)?;
    let snapshot_id = positive_id(row.get(1)?, 1, "snapshot id", SnapshotId::new)?;
    let content_path = ContentPath::new(row.get::<_, String>(2)?)
        .map_err(|e| conversion(2, Type::Text, format!("invalid content path: {e}")))?;
    let content_sha256 = sha256(row, 3)?;
    let policy = PolicyIdentity::new(
        row.get::<_, String>(4)?,
        row.get::<_, String>(5)?,
        sha256(row, 6)?,
    )
    .map_err(|e| conversion(4, Type::Text, format!("invalid policy identity: {e}")))?;
    let kind: String = row.get(7)?;
    let outcome: AssetReviewOutcome = serde_json::from_str(&row.get::<_, String>(8)?)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(8, Type::Text, Box::new(e)))?;
    if outcome.path() != &content_path
        || outcome.sha256() != Some(content_sha256)
        || kind != outcome_kind(&outcome)
    {
        return Err(conversion(
            8,
            Type::Text,
            "asset review identity or outcome kind does not match payload",
        ));
    }
    let reviewer_called: bool = row.get(9)?;
    let derived_reviewer_called =
        matches!(outcome.disposition(), AssetReviewDisposition::Reviewed(_))
            || matches!(
                outcome.disposition(),
                AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::ReviewerFailed(_))
            );
    if reviewer_called != derived_reviewer_called {
        return Err(conversion(
            9,
            Type::Integer,
            "reviewer-called flag does not match outcome",
        ));
    }
    let timestamp = nonnegative(row.get(10)?, 10, "asset review timestamp")?;
    Ok(AssetReviewRun::rehydrate(
        id,
        snapshot_id,
        content_path,
        content_sha256,
        policy,
        outcome,
        timestamp,
    ))
}

fn outcome_kind(outcome: &AssetReviewOutcome) -> &'static str {
    match outcome.disposition() {
        AssetReviewDisposition::Blocked => "blocked",
        AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve) => "reviewer_approved",
        AssetReviewDisposition::Reviewed(AssetReviewDecision::Reject) => "reviewer_rejected",
        AssetReviewDisposition::Reviewed(AssetReviewDecision::NeedsHumanReview) => {
            "reviewer_needs_human_review"
        }
        AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings) => {
            "policy_needs_human_review"
        }
        AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::ReviewerFailed(_)) => {
            "reviewer_failed"
        }
    }
}
fn integer(field: &'static str, value: u64) -> Result<i64, SqliteAssetReviewRunStoreError> {
    value
        .try_into()
        .map_err(|_| SqliteAssetReviewRunStoreError::ValueOutOfRange(field))
}
fn positive_id<T, E>(
    value: i64,
    column: usize,
    field: &'static str,
    make: impl FnOnce(u64) -> Result<T, E>,
) -> rusqlite::Result<T>
where
    E: fmt::Display,
{
    make(nonnegative(value, column, field)?)
        .map_err(|e| conversion(column, Type::Integer, e.to_string()))
}
fn nonnegative(value: i64, column: usize, field: &str) -> rusqlite::Result<u64> {
    value.try_into().map_err(|_| {
        conversion(
            column,
            Type::Integer,
            format!("{field} must be non-negative"),
        )
    })
}
fn sha256(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Sha256> {
    let bytes: Vec<u8> = row.get(column)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
        conversion(
            column,
            Type::Blob,
            format!("SHA-256 must contain 32 bytes, got {}", v.len()),
        )
    })?;
    Ok(Sha256::new(bytes))
}
fn conversion(column: usize, ty: Type, message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        ty,
        Box::new(CorruptAssetReviewRun(message.into())),
    )
}
#[derive(Debug)]
struct CorruptAssetReviewRun(String);
impl fmt::Display for CorruptAssetReviewRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl Error for CorruptAssetReviewRun {}

#[derive(Debug)]
pub enum SqliteAssetReviewRunStoreError {
    Sqlite(rusqlite::Error),
    Serialization(serde_json::Error),
    UnsupportedSchemaVersion(i64),
    ConflictingAssetReviewRunId(AssetReviewRunId),
    ValueOutOfRange(&'static str),
    Persistence(String),
}
impl fmt::Display for SqliteAssetReviewRunStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "SQLite asset-review persistence failed: {e}"),
            Self::Serialization(e) => write!(f, "asset review outcome serialization failed: {e}"),
            Self::UnsupportedSchemaVersion(v) => {
                write!(f, "unsupported asset-review schema version: {v}")
            }
            Self::ConflictingAssetReviewRunId(id) => write!(
                f,
                "asset review run id {} already stores a different fact",
                id.get()
            ),
            Self::ValueOutOfRange(field) => write!(f, "{field} is outside SQLite's integer range"),
            Self::Persistence(message) => f.write_str(message),
        }
    }
}
impl Error for SqliteAssetReviewRunStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            Self::Serialization(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
        time::SystemTime,
    };

    use crate::{
        domain::{Snapshot, SnapshotFile, SourceId},
        workflow::{AssetCheckFinding, AssetReviewOutcome},
    };

    use super::*;

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-asset-review-store-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn database(&self) -> PathBuf {
            self.0.join("asset-review.sqlite3")
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

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }
    fn sha(value: &str) -> Sha256 {
        Sha256::digest(value.as_bytes())
    }
    fn snapshot(id: u64, asset: &str, content: &str) -> Snapshot {
        Snapshot::new(
            SnapshotId::new(id).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            vec![SnapshotFile::new(path(asset), 1, sha(content), None)],
        )
        .unwrap()
    }
    fn policy() -> PolicyIdentity {
        PolicyIdentity::new("asset", "1", sha("policy")).unwrap()
    }
    fn run(
        id: u64,
        snapshot: &Snapshot,
        asset: &str,
        content: &str,
        disposition: AssetReviewDisposition,
        findings: Vec<AssetCheckFinding>,
    ) -> AssetReviewRun {
        AssetReviewRun::from_review_outcome(
            AssetReviewRunId::new(id).unwrap(),
            snapshot,
            AssetReviewOutcome::from_parts_for_test(
                path(asset),
                vec![path("a.md"), path("b.md")],
                Some(sha(content)),
                findings,
                disposition,
            ),
            policy(),
            SystemTime::UNIX_EPOCH,
        )
        .unwrap()
    }

    #[test]
    fn restart_restores_approved_pending_findings_and_dependents() {
        let directory = TestDirectory::new();
        let approved_snapshot = snapshot(1, "image.png", "one");
        let pending_snapshot = snapshot(2, "photo.png", "two");
        let approved = run(
            3,
            &approved_snapshot,
            "image.png",
            "one",
            AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve),
            vec![],
        );
        let pending = run(
            4,
            &pending_snapshot,
            "photo.png",
            "two",
            AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::ReviewerFailed(
                crate::workflow::AssetReviewerError::new("offline"),
            )),
            vec![
                AssetCheckFinding::ExifMetadataPresent,
                AssetCheckFinding::GpsMetadataPresent,
                AssetCheckFinding::XmpMetadataPresent,
            ],
        );
        let store = SqliteAssetReviewRunStore::open(directory.database()).unwrap();
        store.save(&pending).unwrap();
        store.save(&approved).unwrap();
        drop(store);

        let reopened = SqliteAssetReviewRunStore::open(directory.database()).unwrap();
        assert_eq!(reopened.get(approved.id()).unwrap(), Some(approved));
        assert_eq!(reopened.get(pending.id()).unwrap(), Some(pending.clone()));
        assert_eq!(
            reopened.list_pending_human_review().unwrap(),
            vec![pending.clone()]
        );
        assert_eq!(
            pending.outcome().dependents(),
            &[path("a.md"), path("b.md")]
        );
        assert_eq!(pending.outcome().findings().len(), 3);
    }

    #[test]
    fn stores_every_outcome_kind_and_orders_queries() {
        let directory = TestDirectory::new();
        let primary_snapshot = snapshot(7, "z.png", "z");
        let other = snapshot(8, "a.png", "a");
        let blocked = run(
            2,
            &primary_snapshot,
            "z.png",
            "z",
            AssetReviewDisposition::Blocked,
            vec![AssetCheckFinding::DecodeFailed],
        );
        let rejected = run(
            1,
            &other,
            "a.png",
            "a",
            AssetReviewDisposition::Reviewed(AssetReviewDecision::Reject),
            vec![],
        );
        let reviewer_human = run(
            4,
            &primary_snapshot,
            "z.png",
            "z",
            AssetReviewDisposition::Reviewed(AssetReviewDecision::NeedsHumanReview),
            vec![],
        );
        let policy_human = run(
            3,
            &primary_snapshot,
            "z.png",
            "z",
            AssetReviewDisposition::NeedsHumanReview(AssetHumanReviewReason::PolicyFindings),
            vec![AssetCheckFinding::UnknownType],
        );
        let store = SqliteAssetReviewRunStore::open(directory.database()).unwrap();
        for item in [&reviewer_human, &blocked, &policy_human, &rejected] {
            store.save(item).unwrap();
        }
        assert_eq!(
            store.list_by_snapshot(primary_snapshot.id()).unwrap(),
            vec![blocked, policy_human.clone(), reviewer_human.clone()]
        );
        assert_eq!(
            store.list_pending_human_review().unwrap(),
            vec![policy_human, reviewer_human]
        );
    }

    #[test]
    fn save_is_idempotent_but_conflicting_ids_fail() {
        let directory = TestDirectory::new();
        let first_snapshot = snapshot(1, "image.png", "one");
        let second_snapshot = snapshot(2, "image.png", "two");
        let first = run(
            9,
            &first_snapshot,
            "image.png",
            "one",
            AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve),
            vec![],
        );
        let conflict = run(
            9,
            &second_snapshot,
            "image.png",
            "two",
            AssetReviewDisposition::Reviewed(AssetReviewDecision::Approve),
            vec![],
        );
        let store = SqliteAssetReviewRunStore::open(directory.database()).unwrap();
        store.save(&first).unwrap();
        store.save(&first).unwrap();
        assert!(
            matches!(store.save(&conflict), Err(SqliteAssetReviewRunStoreError::ConflictingAssetReviewRunId(id)) if id == first.id())
        );
    }

    #[test]
    fn constructor_rejects_wrong_identity_markdown_and_unstable_dependents() {
        let asset_snapshot = snapshot(1, "image.png", "one");
        let outcome = AssetReviewOutcome::from_parts_for_test(
            path("image.png"),
            vec![path("a.md")],
            Some(sha("wrong")),
            vec![],
            AssetReviewDisposition::Blocked,
        );
        assert!(matches!(
            AssetReviewRun::from_review_outcome(
                AssetReviewRunId::new(1).unwrap(),
                &asset_snapshot,
                outcome,
                policy(),
                SystemTime::UNIX_EPOCH
            ),
            Err(crate::workflow::AssetReviewRunError::ContentIdentityMismatch { .. })
        ));
        let markdown = snapshot(2, "wrong.md", "two");
        let outcome = AssetReviewOutcome::from_parts_for_test(
            path("wrong.md"),
            vec![path("a.md")],
            Some(sha("two")),
            vec![],
            AssetReviewDisposition::Blocked,
        );
        assert!(matches!(
            AssetReviewRun::from_review_outcome(
                AssetReviewRunId::new(2).unwrap(),
                &markdown,
                outcome,
                policy(),
                SystemTime::UNIX_EPOCH
            ),
            Err(crate::workflow::AssetReviewRunError::AssetIsMarkdown(_))
        ));
        let outcome = AssetReviewOutcome::from_parts_for_test(
            path("image.png"),
            vec![path("b.md"), path("a.md")],
            Some(sha("one")),
            vec![],
            AssetReviewDisposition::Blocked,
        );
        assert!(matches!(
            AssetReviewRun::from_review_outcome(
                AssetReviewRunId::new(3).unwrap(),
                &asset_snapshot,
                outcome,
                policy(),
                SystemTime::UNIX_EPOCH
            ),
            Err(crate::workflow::AssetReviewRunError::DependentsNotInStableOrder(_))
        ));
    }

    #[test]
    fn persistence_failure_is_explicit() {
        let directory = TestDirectory::new();
        assert!(matches!(
            SqliteAssetReviewRunStore::open(directory.path()),
            Err(SqliteAssetReviewRunStoreError::Sqlite(_))
        ));
    }
}
