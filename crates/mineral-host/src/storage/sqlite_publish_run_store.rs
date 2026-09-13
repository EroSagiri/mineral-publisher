use std::{error::Error, fmt, path::Path};

use rusqlite::{Connection, OptionalExtension, params, types::Type};

use crate::{
    domain::{Sha256, SnapshotId},
    publisher::{
        CommitSpecWire, FrozenPublicScope, FrozenPublicScopeError, GitCommitOid, GitRefTarget,
        PublishRun, PublishRunId, PublishRunStore, PublishTargetId, RepositoryLocator,
    },
    workflow::ManagedRoot,
};

const SCHEMA_VERSION: i64 = 5;

/// Local SQLite persistence for immutable publication intents.
pub struct SqlitePublishRunStore {
    connection: Connection,
}

impl SqlitePublishRunStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqlitePublishRunStoreError> {
        let connection = Connection::open(path).map_err(SqlitePublishRunStoreError::Sqlite)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(SqlitePublishRunStoreError::Sqlite)?;
        let detected: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(SqlitePublishRunStoreError::Sqlite)?;
        let mut version = detected;
        match version {
            0 => {
                connection.execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE publish_runs (
                         id INTEGER PRIMARY KEY CHECK (id > 0),
                         snapshot_id INTEGER NOT NULL CHECK (snapshot_id > 0),
                         legacy_projection_sha256 BLOB
                             CHECK (legacy_projection_sha256 IS NULL
                                 OR length(legacy_projection_sha256) = 32),
                         reviewed_text_projection_sha256 BLOB
                             CHECK (reviewed_text_projection_sha256 IS NULL
                                 OR length(reviewed_text_projection_sha256) = 32),
                         delivery_projection_sha256 BLOB
                             CHECK (delivery_projection_sha256 IS NULL
                                 OR length(delivery_projection_sha256) = 32),
                         managed_root TEXT NOT NULL,
                         repository_path TEXT NOT NULL,
                         publish_target_id TEXT NOT NULL DEFAULT '',
                         remote_name TEXT NOT NULL,
                         destination_ref TEXT NOT NULL,
                         base_commit TEXT NOT NULL,
                         reviewed_tree TEXT NOT NULL,
                         publication_kind TEXT NOT NULL CHECK (publication_kind IN ('noop', 'commit_ready')),
                         commit_oid TEXT,
                         created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
                         commit_spec TEXT,
                         CHECK ((publication_kind = 'noop' AND commit_oid IS NULL)
                             OR (publication_kind = 'commit_ready' AND commit_oid IS NOT NULL)),
                         CHECK ((reviewed_text_projection_sha256 IS NULL)
                             = (delivery_projection_sha256 IS NULL))
                     );
                     CREATE INDEX publish_runs_by_target
                         ON publish_runs(remote_name, destination_ref, created_at_unix_ms, id);
                     CREATE INDEX publish_runs_by_creation
                         ON publish_runs(created_at_unix_ms, id);
                     CREATE TABLE publish_run_public_scope (
                         publish_run_id INTEGER PRIMARY KEY
                             REFERENCES publish_runs(id) ON DELETE CASCADE,
                         rules_json TEXT NOT NULL,
                         CHECK (length(rules_json) > 0)
                     );
                     PRAGMA user_version = 5;
                     COMMIT;",
                ).map_err(SqlitePublishRunStoreError::Sqlite)?;
                version = SCHEMA_VERSION;
            }
            // Schema 1 stored the target only as `remote_name`/`destination_ref`.
            // Derive the same default the CLI derives (`{remote}:{ref}`) so rows
            // written before this migration keep matching the runs written after
            // it. `ADD COLUMN` requires a non-null default; the default is never
            // read back as a valid identity because empty target IDs are rejected
            // on load.
            1 => {
                connection
                    .execute_batch(
                        "BEGIN IMMEDIATE;
                     ALTER TABLE publish_runs
                         ADD COLUMN publish_target_id TEXT NOT NULL DEFAULT '';
                     UPDATE publish_runs
                         SET publish_target_id = remote_name || ':' || destination_ref
                         WHERE publish_target_id = '';
                     PRAGMA user_version = 2;
                     COMMIT;",
                    )
                    .map_err(SqlitePublishRunStoreError::Sqlite)?;
                version = 2;
            }
            _ => {}
        }
        if version == 2 {
            // Additive: rows written before this migration keep a NULL
            // `commit_spec`, which means "the runtime must still hold the commit
            // object"; every row written from now on carries one. No CHECK is
            // added here, because `ALTER TABLE` cannot express one — the
            // "specification without a desired commit" state is rejected by
            // `PublishRun` on both fresh and migrated databases alike.
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     ALTER TABLE publish_runs ADD COLUMN commit_spec TEXT;
                     PRAGMA user_version = 3;
                     COMMIT;",
                )
                .map_err(SqlitePublishRunStoreError::Sqlite)?;
            version = 3;
        }
        if version == 3 {
            // Additive in effect, but the identity must be split explicitly rather
            // than reinterpreted: `projection_sha256` held the S5 `PublicProjection`
            // identity for older rows and the S6.1 `TextProjection` identity for
            // newer ones, so the column is renamed to say that its value is opaque
            // historical metadata, and the two identities this engine can actually
            // reason about get columns of their own. Historical rows keep the old
            // bytes under the honest name and are left with no delivery binding —
            // an explicit "no durable delivery projection", never a guessed one.
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE publish_runs_v4 (
                         id INTEGER PRIMARY KEY CHECK (id > 0),
                         snapshot_id INTEGER NOT NULL CHECK (snapshot_id > 0),
                         legacy_projection_sha256 BLOB
                             CHECK (legacy_projection_sha256 IS NULL
                                 OR length(legacy_projection_sha256) = 32),
                         reviewed_text_projection_sha256 BLOB
                             CHECK (reviewed_text_projection_sha256 IS NULL
                                 OR length(reviewed_text_projection_sha256) = 32),
                         delivery_projection_sha256 BLOB
                             CHECK (delivery_projection_sha256 IS NULL
                                 OR length(delivery_projection_sha256) = 32),
                         managed_root TEXT NOT NULL,
                         repository_path TEXT NOT NULL,
                         publish_target_id TEXT NOT NULL DEFAULT '',
                         remote_name TEXT NOT NULL,
                         destination_ref TEXT NOT NULL,
                         base_commit TEXT NOT NULL,
                         reviewed_tree TEXT NOT NULL,
                         publication_kind TEXT NOT NULL CHECK (publication_kind IN ('noop', 'commit_ready')),
                         commit_oid TEXT,
                         created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
                         commit_spec TEXT,
                         CHECK ((publication_kind = 'noop' AND commit_oid IS NULL)
                             OR (publication_kind = 'commit_ready' AND commit_oid IS NOT NULL)),
                         CHECK ((reviewed_text_projection_sha256 IS NULL)
                             = (delivery_projection_sha256 IS NULL))
                     );
                     INSERT INTO publish_runs_v4 (
                         id, snapshot_id, legacy_projection_sha256,
                         reviewed_text_projection_sha256, delivery_projection_sha256,
                         managed_root, repository_path, publish_target_id, remote_name,
                         destination_ref, base_commit, reviewed_tree, publication_kind,
                         commit_oid, created_at_unix_ms, commit_spec
                     )
                     SELECT id, snapshot_id, projection_sha256, NULL, NULL,
                         managed_root, repository_path, publish_target_id, remote_name,
                         destination_ref, base_commit, reviewed_tree, publication_kind,
                         commit_oid, created_at_unix_ms, commit_spec
                     FROM publish_runs;
                     DROP TABLE publish_runs;
                     ALTER TABLE publish_runs_v4 RENAME TO publish_runs;
                     CREATE INDEX publish_runs_by_target
                         ON publish_runs(remote_name, destination_ref, created_at_unix_ms, id);
                     CREATE INDEX publish_runs_by_creation
                         ON publish_runs(created_at_unix_ms, id);
                     PRAGMA user_version = 4;
                     COMMIT;",
                )
                .map_err(SqlitePublishRunStoreError::Sqlite)?;
            version = 4;
        }
        if version == 4 {
            // Additive: the public scope one attempt was decided under is provenance
            // and lives beside the intent, never inside it, so freezing it cannot
            // re-identify or re-interpret anything an earlier run published.
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE publish_run_public_scope (
                         publish_run_id INTEGER PRIMARY KEY
                             REFERENCES publish_runs(id) ON DELETE CASCADE,
                         rules_json TEXT NOT NULL,
                         CHECK (length(rules_json) > 0)
                     );
                     PRAGMA user_version = 5;
                     COMMIT;",
                )
                .map_err(SqlitePublishRunStoreError::Sqlite)?;
            version = 5;
        }
        if version != SCHEMA_VERSION {
            return Err(SqlitePublishRunStoreError::UnsupportedSchemaVersion(
                detected,
            ));
        }
        Ok(Self { connection })
    }

    fn load_many(
        &self,
        sql: &str,
        target: Option<&GitRefTarget>,
    ) -> Result<Vec<PublishRun>, SqlitePublishRunStoreError> {
        let mut statement = self
            .connection
            .prepare(sql)
            .map_err(SqlitePublishRunStoreError::Sqlite)?;
        let rows = match target {
            Some(target) => statement.query_map(
                params![target.remote_name(), target.destination_ref()],
                row_to_publish_run,
            ),
            None => statement.query_map([], row_to_publish_run),
        }
        .map_err(SqlitePublishRunStoreError::Sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SqlitePublishRunStoreError::Sqlite)
    }

    /// Writes one intent without committing; the caller owns the transaction.
    fn save_intent(&self, run: &PublishRun) -> Result<(), SqlitePublishRunStoreError> {
        let (kind, commit_oid): (&str, Option<&str>) = match run.desired_commit() {
            None => ("noop", None),
            Some(commit_oid) => ("commit_ready", Some(commit_oid.as_str())),
        };
        let repository_path = run.repository().as_str();
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO publish_runs (
                 id, snapshot_id, legacy_projection_sha256,
                 reviewed_text_projection_sha256, delivery_projection_sha256,
                 managed_root, repository_path, publish_target_id, remote_name,
                 destination_ref, base_commit, reviewed_tree, publication_kind,
                 commit_oid, created_at_unix_ms, commit_spec
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    integer("publish run ID", run.id().get())?,
                    integer("snapshot ID", run.snapshot_id().get())?,
                    run.legacy_projection_sha256()
                        .map(|identity| identity.as_bytes().to_vec()),
                    run.reviewed_text_projection_sha256()
                        .map(|identity| identity.as_bytes().to_vec()),
                    run.delivery_projection_binding()
                        .map(|binding| binding.delivery_sha256().as_bytes().to_vec()),
                    run.managed_root().as_str(),
                    repository_path,
                    run.target_id().as_str(),
                    run.target().remote_name(),
                    run.target().destination_ref(),
                    run.base_commit(),
                    run.reviewed_tree(),
                    kind,
                    commit_oid,
                    integer("publish timestamp", run.created_at_unix_ms())?,
                    run.commit_spec().map(CommitSpecWire::encode),
                ],
            )
            .map_err(SqlitePublishRunStoreError::Sqlite)?;
        if inserted == 1 {
            return Ok(());
        }
        match self.get(run.id())? {
            Some(stored) if stored == *run => Ok(()),
            Some(_) => Err(SqlitePublishRunStoreError::ConflictingPublishRunId(
                run.id(),
            )),
            None => Err(SqlitePublishRunStoreError::Persistence(
                "publish run insert was ignored without an existing row".to_owned(),
            )),
        }
    }

    /// Records the frozen public scope beside its intent without committing; the
    /// caller owns the transaction.
    fn save_scope(
        &self,
        id: PublishRunId,
        public_scope: &FrozenPublicScope,
    ) -> Result<(), SqlitePublishRunStoreError> {
        let encoded = serde_json::to_string(public_scope.rules())
            .map_err(SqlitePublishRunStoreError::Serialization)?;
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO publish_run_public_scope (publish_run_id, rules_json)
                 VALUES (?1, ?2)",
                params![integer("publish run ID", id.get())?, encoded],
            )
            .map_err(SqlitePublishRunStoreError::Sqlite)?;
        if inserted == 1 {
            return Ok(());
        }
        // Re-recording the same attempt is idempotent; a second, different scope for
        // one attempt is refused, because the frozen provenance is immutable.
        match self.public_scope(id)? {
            Some(stored) if stored == *public_scope => Ok(()),
            Some(_) => Err(SqlitePublishRunStoreError::ConflictingPublicScope(id)),
            None => Err(SqlitePublishRunStoreError::Persistence(
                "public scope insert was ignored without an existing row".to_owned(),
            )),
        }
    }
}

impl PublishRunStore for SqlitePublishRunStore {
    type Error = SqlitePublishRunStoreError;

    fn save(&self, run: &PublishRun, public_scope: &FrozenPublicScope) -> Result<(), Self::Error> {
        // The intent and the scope it was decided under become durable together: a
        // run this engine records can never be left without the provenance that
        // explains its public scope, and a store that cannot record the scope fails
        // the attempt instead of publishing an unattributable run.
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(SqlitePublishRunStoreError::Sqlite)?;
        let saved = self
            .save_intent(run)
            .and_then(|()| self.save_scope(run.id(), public_scope));
        if let Err(error) = saved {
            // Dropping the transaction rolls back both writes: the intent is not
            // durable without the scope it was decided under.
            drop(transaction);
            return Err(error);
        }
        transaction
            .commit()
            .map_err(SqlitePublishRunStoreError::Sqlite)
    }

    fn public_scope(&self, id: PublishRunId) -> Result<Option<FrozenPublicScope>, Self::Error> {
        let encoded: Option<String> = self
            .connection
            .query_row(
                "SELECT rules_json FROM publish_run_public_scope WHERE publish_run_id = ?1",
                [integer("publish run ID", id.get())?],
                |row| row.get(0),
            )
            .optional()
            .map_err(SqlitePublishRunStoreError::Sqlite)?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        let rules: Vec<String> =
            serde_json::from_str(&encoded).map_err(SqlitePublishRunStoreError::Serialization)?;
        FrozenPublicScope::rehydrate(rules)
            .map(Some)
            .map_err(SqlitePublishRunStoreError::FrozenPublicScope)
    }

    fn get(&self, id: PublishRunId) -> Result<Option<PublishRun>, Self::Error> {
        self.connection
            .query_row(
                "SELECT id, snapshot_id, legacy_projection_sha256,
                    reviewed_text_projection_sha256, delivery_projection_sha256, managed_root,
                    repository_path, publish_target_id, remote_name, destination_ref, base_commit,
                    reviewed_tree, publication_kind, commit_oid, created_at_unix_ms, commit_spec
             FROM publish_runs WHERE id = ?1",
                [integer("publish run ID", id.get())?],
                row_to_publish_run,
            )
            .optional()
            .map_err(SqlitePublishRunStoreError::Sqlite)
    }

    fn list(&self) -> Result<Vec<PublishRun>, Self::Error> {
        self.load_many(
            "SELECT id, snapshot_id, legacy_projection_sha256,
                    reviewed_text_projection_sha256, delivery_projection_sha256, managed_root,
                    repository_path, publish_target_id, remote_name, destination_ref, base_commit,
                    reviewed_tree, publication_kind, commit_oid, created_at_unix_ms, commit_spec
             FROM publish_runs ORDER BY created_at_unix_ms ASC, id ASC",
            None,
        )
    }

    fn list_for_target(&self, target: &GitRefTarget) -> Result<Vec<PublishRun>, Self::Error> {
        self.load_many(
            "SELECT id, snapshot_id, legacy_projection_sha256,
                    reviewed_text_projection_sha256, delivery_projection_sha256, managed_root,
                    repository_path, publish_target_id, remote_name, destination_ref, base_commit,
                    reviewed_tree, publication_kind, commit_oid, created_at_unix_ms, commit_spec
             FROM publish_runs WHERE remote_name = ?1 AND destination_ref = ?2
             ORDER BY created_at_unix_ms ASC, id ASC",
            Some(target),
        )
    }
}

fn row_to_publish_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<PublishRun> {
    let id = positive_id(row.get(0)?, 0, "publish run ID", PublishRunId::new)?;
    let snapshot_id = positive_id(row.get(1)?, 1, "snapshot ID", SnapshotId::new)?;
    let legacy_projection_sha256 = optional_sha256_column(row, 2)?;
    let reviewed_text_projection_sha256 = optional_sha256_column(row, 3)?;
    let delivery_projection_sha256 = optional_sha256_column(row, 4)?;
    let managed_root = ManagedRoot::new(row.get::<_, String>(5)?)
        .map_err(|error| conversion_error(5, Type::Text, error.to_string()))?;
    let repository = RepositoryLocator::new(row.get::<_, String>(6)?)
        .map_err(|error| conversion_error(6, Type::Text, error.to_string()))?;
    let target_id = PublishTargetId::new(row.get::<_, String>(7)?)
        .map_err(|error| conversion_error(7, Type::Text, error.to_string()))?;
    let target = GitRefTarget::new(row.get::<_, String>(8)?, row.get::<_, String>(9)?)
        .map_err(|error| conversion_error(8, Type::Text, error.to_string()))?;
    let base_commit = row.get(10)?;
    let reviewed_tree = row.get(11)?;
    let kind: String = row.get(12)?;
    let commit_oid: Option<String> = row.get(13)?;
    let desired_commit = match (kind.as_str(), commit_oid) {
        ("noop", None) => None,
        ("commit_ready", Some(commit_oid)) => Some(
            GitCommitOid::new(commit_oid)
                .map_err(|error| conversion_error(13, Type::Text, error.to_string()))?,
        ),
        _ => {
            return Err(conversion_error(
                12,
                Type::Text,
                "publication kind does not match commit identity",
            ));
        }
    };
    let created_at_unix_ms = nonnegative(row.get(14)?, 14, "publish timestamp")?;
    let commit_spec = match row.get::<_, Option<String>>(15)? {
        None => None,
        Some(encoded) => Some(
            CommitSpecWire::decode(&encoded)
                .map_err(|error| conversion_error(15, Type::Text, error.to_string()))?,
        ),
    };
    PublishRun::rehydrate(
        id,
        snapshot_id,
        legacy_projection_sha256,
        reviewed_text_projection_sha256,
        delivery_projection_sha256,
        managed_root,
        target_id,
        repository,
        target,
        base_commit,
        reviewed_tree,
        desired_commit,
        commit_spec,
        created_at_unix_ms,
    )
    .map_err(|error| conversion_error(0, Type::Text, error.to_string()))
}

/// A nullable SHA-256 column: `NULL` means the fact was never captured.
fn optional_sha256_column(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<Option<Sha256>> {
    match row.get::<_, Option<Vec<u8>>>(column)? {
        None => Ok(None),
        Some(_) => sha256_column(row, column).map(Some),
    }
}

fn integer(field: &'static str, value: u64) -> Result<i64, SqlitePublishRunStoreError> {
    value
        .try_into()
        .map_err(|_| SqlitePublishRunStoreError::ValueOutOfRange(field))
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
    constructor(nonnegative(value, column, field)?)
        .map_err(|error| conversion_error(column, Type::Integer, error.to_string()))
}
fn nonnegative(value: i64, column: usize, field: &str) -> rusqlite::Result<u64> {
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
        Box::new(CorruptPublishRun(message.into())),
    )
}

#[derive(Debug)]
struct CorruptPublishRun(String);
impl fmt::Display for CorruptPublishRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl Error for CorruptPublishRun {}

#[derive(Debug)]
pub enum SqlitePublishRunStoreError {
    Sqlite(rusqlite::Error),
    UnsupportedSchemaVersion(i64),
    ConflictingPublishRunId(PublishRunId),
    ValueOutOfRange(&'static str),
    /// A frozen public scope could not be encoded or was stored unreadably.
    Serialization(serde_json::Error),
    /// A stored frozen public scope is not a usable canonical scope.
    FrozenPublicScope(FrozenPublicScopeError),
    /// An attempt already records a different public scope.
    ConflictingPublicScope(PublishRunId),
    Persistence(String),
}
impl fmt::Display for SqlitePublishRunStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => {
                write!(formatter, "SQLite publish-run persistence failed: {error}")
            }
            Self::UnsupportedSchemaVersion(version) => write!(
                formatter,
                "unsupported publish-run schema version: {version}"
            ),
            Self::ConflictingPublishRunId(id) => write!(
                formatter,
                "publish run ID {} already stores a different fact",
                id.get()
            ),
            Self::ValueOutOfRange(field) => {
                write!(formatter, "{field} is outside SQLite's integer range")
            }
            Self::Serialization(error) => {
                write!(formatter, "frozen public scope is not readable: {error}")
            }
            Self::FrozenPublicScope(error) => write!(formatter, "{error}"),
            Self::ConflictingPublicScope(id) => write!(
                formatter,
                "publish run {} already records a different public scope",
                id.get()
            ),
            Self::Persistence(message) => formatter.write_str(message),
        }
    }
}
impl Error for SqlitePublishRunStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Serialization(error) => Some(error),
            Self::FrozenPublicScope(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::TimestampMillis,
        publisher::{DeliveryProjectionBinding, GitCommitSpec, GitRepositoryIdentity, GitTreeOid},
    };
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);
    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-publish-run-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn database(&self) -> PathBuf {
            self.0.join("publish-runs.sqlite3")
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn oid(value: char) -> String {
        std::iter::repeat_n(value, 40).collect()
    }
    fn run(
        directory: &TestDirectory,
        id: u64,
        target: &str,
        commit: Option<char>,
        time: u64,
    ) -> PublishRun {
        run_with_target_id(directory, id, target, target, commit, time)
    }
    fn run_with_target_id(
        directory: &TestDirectory,
        id: u64,
        target: &str,
        target_id: &str,
        commit: Option<char>,
        time: u64,
    ) -> PublishRun {
        let desired_commit = commit.map(|value| GitCommitOid::new(oid(value)).unwrap());
        run_with(
            directory,
            id,
            target,
            target_id,
            &oid('a'),
            &oid('b'),
            desired_commit,
            None,
            time,
        )
    }

    /// The one specification that agrees with `run_with` about parent, tree, and
    /// both times.
    fn spec(base: &str, tree: &str, time: u64) -> GitCommitSpec {
        let time = TimestampMillis::from_unix_millis(time);
        GitCommitSpec::new(
            GitCommitOid::new(base).unwrap(),
            GitTreeOid::new(tree).unwrap(),
            "Mineral Publisher",
            "publisher@example.invalid",
            time,
            "Mineral Publisher",
            "publisher@example.invalid",
            time,
            "Publish Mineral content",
        )
        .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn run_with(
        directory: &TestDirectory,
        id: u64,
        target: &str,
        target_id: &str,
        base_commit: &str,
        reviewed_tree: &str,
        desired_commit: Option<GitCommitOid>,
        commit_spec: Option<GitCommitSpec>,
        time: u64,
    ) -> PublishRun {
        PublishRun::rehydrate(
            PublishRunId::new(id).unwrap(),
            SnapshotId::new(7).unwrap(),
            Some(Sha256::new([9; 32])),
            None,
            None,
            ManagedRoot::new("content").unwrap(),
            PublishTargetId::new(target_id).unwrap(),
            GitRepositoryIdentity::new(&directory.0)
                .unwrap()
                .locator()
                .clone(),
            GitRefTarget::new("origin", target).unwrap(),
            base_commit.to_owned(),
            reviewed_tree.to_owned(),
            desired_commit,
            commit_spec,
            time,
        )
        .unwrap()
    }

    /// Writes a row directly, bypassing every Rust-side invariant, so the load
    /// path can be shown to fail closed on facts it did not write itself.
    #[allow(clippy::too_many_arguments)]
    fn insert_raw(
        connection: &Connection,
        directory: &TestDirectory,
        id: u64,
        publication_kind: &str,
        commit_oid: Option<&str>,
        commit_spec: Option<&str>,
        base_commit: &str,
        reviewed_tree: &str,
        time: u64,
    ) {
        connection
            .execute(
                "INSERT INTO publish_runs (
                     id, snapshot_id, legacy_projection_sha256,
                     reviewed_text_projection_sha256, delivery_projection_sha256, managed_root,
                     repository_path, publish_target_id, remote_name, destination_ref, base_commit,
                     reviewed_tree, publication_kind, commit_oid, created_at_unix_ms, commit_spec
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    id as i64,
                    7_i64,
                    [9_u8; 32].as_slice(),
                    Option::<Vec<u8>>::None,
                    Option::<Vec<u8>>::None,
                    "content",
                    directory.0.to_str().unwrap(),
                    "origin:refs/heads/main",
                    "origin",
                    "refs/heads/main",
                    base_commit,
                    reviewed_tree,
                    publication_kind,
                    commit_oid,
                    time as i64,
                    commit_spec,
                ],
            )
            .unwrap();
    }

    #[test]
    fn created_and_noop_survive_restart_without_fabricating_a_commit() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let created = run(&directory, 1, "refs/heads/main", Some('c'), 10);
        let noop = run(&directory, 2, "refs/heads/main", None, 11);
        let store = SqlitePublishRunStore::open(&database).unwrap();
        store.save(&created, &FrozenPublicScope::empty()).unwrap();
        store.save(&noop, &FrozenPublicScope::empty()).unwrap();
        drop(store);
        let reopened = SqlitePublishRunStore::open(&database).unwrap();
        assert_eq!(reopened.get(created.id()).unwrap(), Some(created));
        assert_eq!(reopened.get(noop.id()).unwrap(), Some(noop));
    }

    #[test]
    fn save_is_idempotent_but_conflicting_id_fails() {
        let directory = TestDirectory::new();
        let store = SqlitePublishRunStore::open(directory.database()).unwrap();
        let first = run(&directory, 1, "refs/heads/main", Some('c'), 10);
        store.save(&first, &FrozenPublicScope::empty()).unwrap();
        store.save(&first, &FrozenPublicScope::empty()).unwrap();
        let conflict = run(&directory, 1, "refs/heads/main", Some('d'), 10);
        assert!(
            matches!(store.save(&conflict, &FrozenPublicScope::empty()), Err(SqlitePublishRunStoreError::ConflictingPublishRunId(id)) if id == first.id())
        );
    }

    #[test]
    fn multiple_attempts_and_targets_are_retained_in_stable_order() {
        let directory = TestDirectory::new();
        let store = SqlitePublishRunStore::open(directory.database()).unwrap();
        let late = run(&directory, 2, "refs/heads/main", Some('c'), 20);
        let first = run(&directory, 1, "refs/heads/main", Some('c'), 10);
        let staging = run(&directory, 3, "refs/heads/staging", Some('c'), 10);
        store.save(&late, &FrozenPublicScope::empty()).unwrap();
        store.save(&first, &FrozenPublicScope::empty()).unwrap();
        store.save(&staging, &FrozenPublicScope::empty()).unwrap();
        assert_eq!(
            store.list().unwrap(),
            vec![first.clone(), staging.clone(), late]
        );
        assert_eq!(
            store.list_for_target(staging.target()).unwrap(),
            vec![staging]
        );
    }

    #[test]
    fn publish_target_identity_is_independent_of_the_remote_and_ref() {
        let directory = TestDirectory::new();
        let store = SqlitePublishRunStore::open(directory.database()).unwrap();
        let run = run_with_target_id(
            &directory,
            1,
            "refs/heads/main",
            "public-production",
            Some('c'),
            10,
        );
        store.save(&run, &FrozenPublicScope::empty()).unwrap();
        let restored = store.get(run.id()).unwrap().unwrap();
        assert_eq!(restored.target_id().as_str(), "public-production");
        assert_eq!(restored.target().remote_name(), "origin");
        assert_eq!(restored, run);
    }

    #[test]
    fn schema_one_rows_gain_the_derived_publish_target_id() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let projection = [9_u8; 32];
        let base_commit = oid('a');
        let reviewed_tree = oid('b');
        let commit_oid = oid('c');
        {
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE publish_runs (
                         id INTEGER PRIMARY KEY CHECK (id > 0),
                         snapshot_id INTEGER NOT NULL CHECK (snapshot_id > 0),
                         projection_sha256 BLOB NOT NULL CHECK (length(projection_sha256) = 32),
                         managed_root TEXT NOT NULL,
                         repository_path TEXT NOT NULL,
                         remote_name TEXT NOT NULL,
                         destination_ref TEXT NOT NULL,
                         base_commit TEXT NOT NULL,
                         reviewed_tree TEXT NOT NULL,
                         publication_kind TEXT NOT NULL CHECK (publication_kind IN ('noop', 'commit_ready')),
                         commit_oid TEXT,
                         created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
                         CHECK ((publication_kind = 'noop' AND commit_oid IS NULL)
                             OR (publication_kind = 'commit_ready' AND commit_oid IS NOT NULL))
                     );
                     CREATE INDEX publish_runs_by_target
                         ON publish_runs(remote_name, destination_ref, created_at_unix_ms, id);
                     CREATE INDEX publish_runs_by_creation
                         ON publish_runs(created_at_unix_ms, id);
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO publish_runs (
                         id, snapshot_id, projection_sha256, managed_root, repository_path,
                         remote_name, destination_ref, base_commit, reviewed_tree,
                         publication_kind, commit_oid, created_at_unix_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        1_i64,
                        7_i64,
                        projection.as_slice(),
                        "content",
                        "C:/publication",
                        "origin",
                        "refs/heads/main",
                        base_commit,
                        reviewed_tree,
                        "commit_ready",
                        commit_oid,
                        10_i64,
                    ],
                )
                .unwrap();
        }
        let store = SqlitePublishRunStore::open(&database).unwrap();
        let restored = store.get(PublishRunId::new(1).unwrap()).unwrap().unwrap();
        assert_eq!(restored.target_id().as_str(), "origin:refs/heads/main");
        assert_eq!(restored.repository().as_str(), "C:/publication");
        assert_eq!(restored.target().destination_ref(), "refs/heads/main");
        assert_eq!(restored.reviewed_tree(), reviewed_tree);
        assert_eq!(restored.commit_spec(), None);
    }

    /// The public scope one attempt was decided under is frozen together with the
    /// intent, and a row written before scopes were frozen reads as `None` rather
    /// than as empty.
    #[test]
    fn the_public_scope_of_an_attempt_is_frozen_with_the_intent() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let store = SqlitePublishRunStore::open(&database).unwrap();
        let recorded = run(&directory, 1, "refs/heads/main", Some('b'), 10);
        let scope = FrozenPublicScope::rehydrate(["private/**", "secret.md"]).unwrap();
        store.save(&recorded, &scope).unwrap();

        // A row that predates frozen scopes is "unknown", not "empty": it is written
        // the way a database migrated from schema v4 looks.
        let legacy = run(&directory, 2, "refs/heads/main", Some('b'), 11);
        store.save(&legacy, &FrozenPublicScope::empty()).unwrap();
        store
            .connection
            .execute(
                "DELETE FROM publish_run_public_scope WHERE publish_run_id = ?1",
                [integer("publish run ID", legacy.id().get()).unwrap()],
            )
            .unwrap();

        assert_eq!(store.public_scope(legacy.id()).unwrap(), None);
        assert_eq!(
            store.public_scope(recorded.id()).unwrap(),
            Some(scope.clone()),
            "an empty scope is recorded as an empty scope, not as unknown"
        );
        drop(store);

        let reopened = SqlitePublishRunStore::open(&database).unwrap();
        assert_eq!(reopened.public_scope(recorded.id()).unwrap(), Some(scope));
        // The frozen scope is provenance: it is not part of the intent it describes.
        assert_eq!(reopened.get(recorded.id()).unwrap(), Some(recorded));
    }

    /// Re-recording the same attempt is idempotent, but the frozen provenance is
    /// immutable: a second, different scope for one attempt fails closed.
    #[test]
    fn re_saving_one_attempt_with_a_different_scope_fails_closed() {
        let directory = TestDirectory::new();
        let store = SqlitePublishRunStore::open(directory.database()).unwrap();
        let run = run(&directory, 1, "refs/heads/main", Some('b'), 10);
        let scope = FrozenPublicScope::rehydrate(["private/**"]).unwrap();
        let other = FrozenPublicScope::rehydrate(["secret.md"]).unwrap();

        store.save(&run, &scope).unwrap();
        store.save(&run, &scope).unwrap();

        assert!(matches!(
            store.save(&run, &other),
            Err(SqlitePublishRunStoreError::ConflictingPublicScope(id)) if id == run.id()
        ));
        assert!(matches!(
            store.save(&run, &FrozenPublicScope::empty()),
            Err(SqlitePublishRunStoreError::ConflictingPublicScope(id)) if id == run.id()
        ));
        assert_eq!(store.public_scope(run.id()).unwrap(), Some(scope));
    }

    /// The intent and its scope are one durable fact: when the scope cannot be
    /// recorded, the intent is not recorded either.
    #[test]
    fn an_unrecordable_scope_rolls_back_the_intent() {
        let directory = TestDirectory::new();
        let store = SqlitePublishRunStore::open(directory.database()).unwrap();
        let run = run(&directory, 1, "refs/heads/main", Some('b'), 10);
        store
            .connection
            .execute_batch("DROP TABLE publish_run_public_scope;")
            .unwrap();

        assert!(store.save(&run, &FrozenPublicScope::empty()).is_err());
        assert_eq!(
            store.get(run.id()).unwrap(),
            None,
            "the intent must not survive a failed scope write"
        );
        assert_eq!(store.list().unwrap(), Vec::new());
    }

    /// A stored record is exactly the input that can be damaged, so it is validated
    /// again on the way out instead of quietly meaning something else.
    #[test]
    fn a_stored_scope_that_is_not_canonical_is_refused() {
        let directory = TestDirectory::new();
        let store = SqlitePublishRunStore::open(directory.database()).unwrap();
        let run = run(&directory, 1, "refs/heads/main", Some('b'), 10);
        store.save(&run, &FrozenPublicScope::empty()).unwrap();

        for damaged in [
            r#"["b/**","a/**"]"#,
            r#"["/absolute.md"]"#,
            r#"["..\\escape.md"]"#,
            r#"[""]"#,
        ] {
            store
                .connection
                .execute(
                    "UPDATE publish_run_public_scope SET rules_json = ?1 WHERE publish_run_id = ?2",
                    params![damaged, integer("publish run ID", run.id().get()).unwrap()],
                )
                .unwrap();
            assert!(
                matches!(
                    store.public_scope(run.id()),
                    Err(SqlitePublishRunStoreError::FrozenPublicScope(_))
                ),
                "damaged scope {damaged} was accepted"
            );
        }
    }

    #[test]
    fn a_reconstructible_run_survives_restart_with_its_frozen_spec() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let run = run_with(
            &directory,
            1,
            "refs/heads/main",
            "origin:refs/heads/main",
            &oid('a'),
            &oid('b'),
            Some(GitCommitOid::new(oid('c')).unwrap()),
            Some(spec(&oid('a'), &oid('b'), 10)),
            10,
        );
        let store = SqlitePublishRunStore::open(&database).unwrap();
        store.save(&run, &FrozenPublicScope::empty()).unwrap();
        drop(store);

        let reopened = SqlitePublishRunStore::open(&database).unwrap();
        let restored = reopened.get(run.id()).unwrap().unwrap();
        assert_eq!(restored, run);
        assert_eq!(
            restored.commit_spec(),
            Some(&spec(&oid('a'), &oid('b'), 10))
        );
        assert_eq!(
            restored.desired_commit(),
            Some(&GitCommitOid::new(oid('c')).unwrap())
        );
    }

    /// §8: the migration that matters is upgrading a real database written by the
    /// previous schema, not creating a fresh current one.
    #[test]
    fn a_real_version_two_database_upgrades_without_losing_or_inventing_a_spec() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let base_commit = oid('a');
        let reviewed_tree = oid('b');
        let historic_commit = oid('c');
        {
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE publish_runs (
                         id INTEGER PRIMARY KEY CHECK (id > 0),
                         snapshot_id INTEGER NOT NULL CHECK (snapshot_id > 0),
                         projection_sha256 BLOB NOT NULL CHECK (length(projection_sha256) = 32),
                         managed_root TEXT NOT NULL,
                         repository_path TEXT NOT NULL,
                         publish_target_id TEXT NOT NULL DEFAULT '',
                         remote_name TEXT NOT NULL,
                         destination_ref TEXT NOT NULL,
                         base_commit TEXT NOT NULL,
                         reviewed_tree TEXT NOT NULL,
                         publication_kind TEXT NOT NULL CHECK (publication_kind IN ('noop', 'commit_ready')),
                         commit_oid TEXT,
                         created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
                         CHECK ((publication_kind = 'noop' AND commit_oid IS NULL)
                             OR (publication_kind = 'commit_ready' AND commit_oid IS NOT NULL))
                     );
                     CREATE INDEX publish_runs_by_target
                         ON publish_runs(remote_name, destination_ref, created_at_unix_ms, id);
                     CREATE INDEX publish_runs_by_creation
                         ON publish_runs(created_at_unix_ms, id);
                     PRAGMA user_version = 2;
                     COMMIT;",
                )
                .unwrap();
            for (id, kind, commit) in [
                (1_i64, "noop", None),
                (2_i64, "commit_ready", Some(historic_commit.as_str())),
            ] {
                connection
                    .execute(
                        "INSERT INTO publish_runs (
                             id, snapshot_id, projection_sha256, managed_root, repository_path,
                             publish_target_id, remote_name, destination_ref, base_commit,
                             reviewed_tree, publication_kind, commit_oid, created_at_unix_ms
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                        params![
                            id,
                            7_i64,
                            [9_u8; 32].as_slice(),
                            "content",
                            directory.0.to_str().unwrap(),
                            "origin:refs/heads/main",
                            "origin",
                            "refs/heads/main",
                            base_commit,
                            reviewed_tree,
                            kind,
                            commit,
                            id * 10,
                        ],
                    )
                    .unwrap();
            }
        }

        {
            let store = SqlitePublishRunStore::open(&database).unwrap();
            let noop = store.get(PublishRunId::new(1).unwrap()).unwrap().unwrap();
            let historical = store.get(PublishRunId::new(2).unwrap()).unwrap().unwrap();

            // The historical rows keep every field they had, gain no fabricated
            // specification, and (Some, None) is a legal degraded shape rather
            // than corruption.
            assert_eq!(noop.desired_commit(), None);
            assert_eq!(noop.commit_spec(), None);
            assert_eq!(noop.created_at_unix_ms(), 10);
            assert_eq!(
                historical.desired_commit(),
                Some(&GitCommitOid::new(historic_commit.clone()).unwrap())
            );
            assert_eq!(historical.commit_spec(), None);
            assert_eq!(historical.base_commit(), base_commit);
            assert_eq!(historical.reviewed_tree(), reviewed_tree);
            assert_eq!(historical.created_at_unix_ms(), 20);
            assert_eq!(historical.target_id().as_str(), "origin:refs/heads/main");
            assert_eq!(historical.managed_root().as_str(), "content");
            // The old opaque hash survives under an honest name, and the two
            // identities this engine can reason about stay absent rather than being
            // guessed from it.
            for run in [&noop, &historical] {
                assert_eq!(run.legacy_projection_sha256(), Some(Sha256::new([9; 32])));
                assert_eq!(run.reviewed_text_projection_sha256(), None);
                assert_eq!(run.delivery_projection_binding(), None);
            }
        }

        let reopened = SqlitePublishRunStore::open(&database).unwrap();
        assert_eq!(
            reopened
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            5
        );
        let stored_spec: Option<String> = reopened
            .connection
            .query_row(
                "SELECT commit_spec FROM publish_runs WHERE id = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_spec, None);
        assert_eq!(
            reopened.get(PublishRunId::new(2).unwrap()).unwrap(),
            reopened.get(PublishRunId::new(2).unwrap()).unwrap()
        );
    }

    /// §7: the migration that matters is upgrading a real database written by the
    /// schema in production, not creating a fresh current one. Version 3 is the
    /// shape this engine last wrote before delivery projections existed, and its
    /// `projection_sha256` is ambiguous by construction — the public identity for
    /// S5 rows, the text identity for S6.1 rows. The migration must not try to
    /// decide which one a row holds.
    #[test]
    fn a_real_version_three_database_splits_the_ambiguous_projection_identity() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let base_commit = oid('a');
        let reviewed_tree = oid('b');
        let historic_commit = oid('c');
        // One row per era: the value below is deliberately the same bytes for both,
        // because the point is that the schema cannot tell them apart.
        let ambiguous = [0x5a_u8; 32];
        {
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE publish_runs (
                         id INTEGER PRIMARY KEY CHECK (id > 0),
                         snapshot_id INTEGER NOT NULL CHECK (snapshot_id > 0),
                         projection_sha256 BLOB NOT NULL CHECK (length(projection_sha256) = 32),
                         managed_root TEXT NOT NULL,
                         repository_path TEXT NOT NULL,
                         publish_target_id TEXT NOT NULL DEFAULT '',
                         remote_name TEXT NOT NULL,
                         destination_ref TEXT NOT NULL,
                         base_commit TEXT NOT NULL,
                         reviewed_tree TEXT NOT NULL,
                         publication_kind TEXT NOT NULL CHECK (publication_kind IN ('noop', 'commit_ready')),
                         commit_oid TEXT,
                         created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
                         commit_spec TEXT,
                         CHECK ((publication_kind = 'noop' AND commit_oid IS NULL)
                             OR (publication_kind = 'commit_ready' AND commit_oid IS NOT NULL))
                     );
                     CREATE INDEX publish_runs_by_target
                         ON publish_runs(remote_name, destination_ref, created_at_unix_ms, id);
                     CREATE INDEX publish_runs_by_creation
                         ON publish_runs(created_at_unix_ms, id);
                     PRAGMA user_version = 3;
                     COMMIT;",
                )
                .unwrap();
            for (id, kind, commit, spec) in [
                (1_i64, "noop", None, None),
                (
                    2_i64,
                    "commit_ready",
                    Some(historic_commit.as_str()),
                    Some(CommitSpecWire::encode(&spec(
                        &base_commit,
                        &reviewed_tree,
                        20,
                    ))),
                ),
            ] {
                connection
                    .execute(
                        "INSERT INTO publish_runs (
                             id, snapshot_id, projection_sha256, managed_root, repository_path,
                             publish_target_id, remote_name, destination_ref, base_commit,
                             reviewed_tree, publication_kind, commit_oid, created_at_unix_ms,
                             commit_spec
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                        params![
                            id,
                            7_i64,
                            ambiguous.as_slice(),
                            "content",
                            directory.0.to_str().unwrap(),
                            "origin:refs/heads/main",
                            "origin",
                            "refs/heads/main",
                            base_commit,
                            reviewed_tree,
                            kind,
                            commit,
                            id * 10,
                            spec,
                        ],
                    )
                    .unwrap();
            }
        }

        let store = SqlitePublishRunStore::open(&database).unwrap();
        assert_eq!(
            store
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            5
        );
        let noop = store.get(PublishRunId::new(1).unwrap()).unwrap().unwrap();
        let historical = store.get(PublishRunId::new(2).unwrap()).unwrap().unwrap();

        // Every pre-existing field is unchanged.
        assert_eq!(noop.snapshot_id(), SnapshotId::new(7).unwrap());
        assert_eq!(noop.desired_commit(), None);
        assert_eq!(noop.created_at_unix_ms(), 10);
        assert_eq!(historical.base_commit(), base_commit);
        assert_eq!(historical.reviewed_tree(), reviewed_tree);
        assert_eq!(
            historical.desired_commit(),
            Some(&GitCommitOid::new(historic_commit.clone()).unwrap())
        );
        assert_eq!(
            historical.commit_spec(),
            Some(&spec(&base_commit, &reviewed_tree, 20))
        );
        assert_eq!(historical.created_at_unix_ms(), 20);

        // The old value is preserved as opaque metadata, and neither interpreted
        // identity is fabricated from it.
        for run in [&noop, &historical] {
            assert_eq!(run.legacy_projection_sha256(), Some(Sha256::new(ambiguous)));
            assert_eq!(run.reviewed_text_projection_sha256(), None);
            assert_eq!(run.delivery_projection_binding(), None);
        }

        // A run that predates frozen scopes has an unknown scope rather than an
        // empty one: the upgrade must not invent provenance it never recorded.
        assert_eq!(store.public_scope(noop.id()).unwrap(), None);
        assert_eq!(store.public_scope(historical.id()).unwrap(), None);

        // Nothing was invented in the delivery projection store; a historical run
        // simply has no durable delivery intent, which is a degraded but honest
        // state rather than corruption.
        let columns: Vec<String> = store
            .connection
            .prepare("SELECT name FROM pragma_table_info('publish_runs')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(columns.contains(&"legacy_projection_sha256".to_owned()));
        assert!(columns.contains(&"reviewed_text_projection_sha256".to_owned()));
        assert!(columns.contains(&"delivery_projection_sha256".to_owned()));
        assert!(!columns.contains(&"projection_sha256".to_owned()));

        // Reopening is idempotent.
        drop(store);
        let reopened = SqlitePublishRunStore::open(&database).unwrap();
        assert_eq!(
            reopened.get(PublishRunId::new(2).unwrap()).unwrap(),
            Some(historical)
        );
    }

    #[test]
    fn a_new_run_round_trips_its_delivery_binding() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let binding =
            DeliveryProjectionBinding::new(Sha256::new([0x11; 32]), Sha256::new([0x22; 32]));
        let run = PublishRun::rehydrate(
            PublishRunId::new(1).unwrap(),
            SnapshotId::new(7).unwrap(),
            None,
            Some(binding.text_projection_sha256()),
            Some(binding.delivery_sha256()),
            ManagedRoot::new("content").unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            GitRepositoryIdentity::new(&directory.0)
                .unwrap()
                .locator()
                .clone(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            oid('a'),
            oid('b'),
            None,
            None,
            10,
        )
        .unwrap();
        let store = SqlitePublishRunStore::open(&database).unwrap();
        store.save(&run, &FrozenPublicScope::empty()).unwrap();
        drop(store);

        let reopened = SqlitePublishRunStore::open(&database).unwrap();
        let restored = reopened.get(run.id()).unwrap().unwrap();

        assert_eq!(restored, run);
        assert_eq!(restored.delivery_projection_binding(), Some(binding));
        assert_eq!(restored.legacy_projection_sha256(), None);
    }

    /// The schema itself refuses half a binding, so no adapter can create the
    /// state the domain model calls illegal.
    #[test]
    fn the_schema_refuses_half_a_delivery_binding() {
        let directory = TestDirectory::new();
        let store = SqlitePublishRunStore::open(directory.database()).unwrap();

        let error = store
            .connection
            .execute(
                "INSERT INTO publish_runs (
                     id, snapshot_id, legacy_projection_sha256,
                     reviewed_text_projection_sha256, delivery_projection_sha256, managed_root,
                     repository_path, publish_target_id, remote_name, destination_ref, base_commit,
                     reviewed_tree, publication_kind, commit_oid, created_at_unix_ms, commit_spec
                 ) VALUES (?1, ?2, NULL, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'noop', NULL, ?11, NULL)",
                params![
                    1_i64,
                    7_i64,
                    [1_u8; 32].as_slice(),
                    "content",
                    directory.0.to_str().unwrap(),
                    "origin:refs/heads/main",
                    "origin",
                    "refs/heads/main",
                    oid('a'),
                    oid('b'),
                    10_i64,
                ],
            )
            .unwrap_err();

        assert!(
            error.to_string().contains("CHECK"),
            "the schema accepted half a delivery binding: {error}"
        );
    }

    /// §6.6.2: `(None, Some)` is never legal, even when it reaches the database
    /// through something other than this store.
    #[test]
    fn a_stored_spec_without_a_desired_commit_fails_closed() {
        let directory = TestDirectory::new();
        let store = SqlitePublishRunStore::open(directory.database()).unwrap();
        insert_raw(
            &store.connection,
            &directory,
            1,
            "noop",
            None,
            Some(&CommitSpecWire::encode(&spec(&oid('a'), &oid('b'), 10))),
            &oid('a'),
            &oid('b'),
            10,
        );

        assert!(store.get(PublishRunId::new(1).unwrap()).is_err());
    }

    /// §7: a stored specification that disagrees with the run it belongs to is
    /// rejected when the intent is loaded, not later.
    #[test]
    fn a_stored_spec_that_disagrees_with_its_run_fails_closed() {
        let directory = TestDirectory::new();
        let store = SqlitePublishRunStore::open(directory.database()).unwrap();
        insert_raw(
            &store.connection,
            &directory,
            1,
            "commit_ready",
            Some(&oid('c')),
            // The specification freezes another reviewed tree.
            Some(&CommitSpecWire::encode(&spec(&oid('a'), &oid('d'), 10))),
            &oid('a'),
            &oid('b'),
            10,
        );
        insert_raw(
            &store.connection,
            &directory,
            2,
            "commit_ready",
            Some(&oid('c')),
            // spec built at another instant
            Some(&CommitSpecWire::encode(&spec(&oid('a'), &oid('b'), 11))),
            &oid('a'),
            &oid('b'),
            10,
        );
        insert_raw(
            &store.connection,
            &directory,
            3,
            "commit_ready",
            Some(&oid('c')),
            // spec built on another parent
            Some(&CommitSpecWire::encode(&spec(&oid('d'), &oid('b'), 10))),
            &oid('a'),
            &oid('b'),
            10,
        );

        for id in [1_u64, 2, 3] {
            assert!(
                store.get(PublishRunId::new(id).unwrap()).is_err(),
                "accepted a disagreeing specification for run {id}"
            );
        }
    }

    #[test]
    fn an_unreadable_stored_spec_fails_closed() {
        let directory = TestDirectory::new();
        let store = SqlitePublishRunStore::open(directory.database()).unwrap();
        let usable = CommitSpecWire::encode(&spec(&oid('a'), &oid('b'), 10));
        for (id, encoded) in [
            (1_u64, "not json".to_owned()),
            (2_u64, usable.replace("\"version\":1", "\"version\":2")),
            (
                3_u64,
                usable.replace("\"message\"", "\"unexpected\":1,\"message\""),
            ),
        ] {
            insert_raw(
                &store.connection,
                &directory,
                id,
                "commit_ready",
                Some(&oid('c')),
                Some(&encoded),
                &oid('a'),
                &oid('b'),
                10,
            );
        }

        for id in [1_u64, 2, 3] {
            assert!(
                store.get(PublishRunId::new(id).unwrap()).is_err(),
                "accepted an unreadable specification for run {id}"
            );
        }
    }
}
