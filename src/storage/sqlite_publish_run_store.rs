use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, params, types::Type};

use crate::{
    domain::{Sha256, SnapshotId},
    publisher::{
        GitRepositoryIdentity, PublicationTarget, PublishRun, PublishRunId, PublishRunPublication,
        PublishRunStore,
    },
    workflow::ManagedRoot,
};

const SCHEMA_VERSION: i64 = 1;

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
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(SqlitePublishRunStoreError::Sqlite)?;
        match version {
            0 => connection.execute_batch(
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
            ).map_err(SqlitePublishRunStoreError::Sqlite)?,
            SCHEMA_VERSION => {},
            value => return Err(SqlitePublishRunStoreError::UnsupportedSchemaVersion(value)),
        }
        Ok(Self { connection })
    }

    fn load_many(
        &self,
        sql: &str,
        target: Option<&PublicationTarget>,
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
}

impl PublishRunStore for SqlitePublishRunStore {
    type Error = SqlitePublishRunStoreError;

    fn save(&self, run: &PublishRun) -> Result<(), Self::Error> {
        let (kind, commit_oid): (&str, Option<&str>) = match run.publication() {
            PublishRunPublication::Noop => ("noop", None),
            PublishRunPublication::CommitReady { commit_oid } => ("commit_ready", Some(commit_oid)),
        };
        let repository_path = run.repository().path().to_string_lossy();
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO publish_runs (
                 id, snapshot_id, projection_sha256, managed_root, repository_path,
                 remote_name, destination_ref, base_commit, reviewed_tree,
                 publication_kind, commit_oid, created_at_unix_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    integer("publish run ID", run.id().get())?,
                    integer("snapshot ID", run.snapshot_id().get())?,
                    run.projection_sha256().as_bytes().as_slice(),
                    run.managed_root().as_str(),
                    repository_path.as_ref(),
                    run.target().remote_name(),
                    run.target().destination_ref(),
                    run.base_commit(),
                    run.reviewed_tree(),
                    kind,
                    commit_oid,
                    integer("publish timestamp", run.created_at_unix_ms())?,
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

    fn get(&self, id: PublishRunId) -> Result<Option<PublishRun>, Self::Error> {
        self.connection
            .query_row(
                "SELECT id, snapshot_id, projection_sha256, managed_root, repository_path,
                    remote_name, destination_ref, base_commit, reviewed_tree,
                    publication_kind, commit_oid, created_at_unix_ms
             FROM publish_runs WHERE id = ?1",
                [integer("publish run ID", id.get())?],
                row_to_publish_run,
            )
            .optional()
            .map_err(SqlitePublishRunStoreError::Sqlite)
    }

    fn list(&self) -> Result<Vec<PublishRun>, Self::Error> {
        self.load_many(
            "SELECT id, snapshot_id, projection_sha256, managed_root, repository_path,
                    remote_name, destination_ref, base_commit, reviewed_tree,
                    publication_kind, commit_oid, created_at_unix_ms
             FROM publish_runs ORDER BY created_at_unix_ms ASC, id ASC",
            None,
        )
    }

    fn list_for_target(&self, target: &PublicationTarget) -> Result<Vec<PublishRun>, Self::Error> {
        self.load_many(
            "SELECT id, snapshot_id, projection_sha256, managed_root, repository_path,
                    remote_name, destination_ref, base_commit, reviewed_tree,
                    publication_kind, commit_oid, created_at_unix_ms
             FROM publish_runs WHERE remote_name = ?1 AND destination_ref = ?2
             ORDER BY created_at_unix_ms ASC, id ASC",
            Some(target),
        )
    }
}

fn row_to_publish_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<PublishRun> {
    let id = positive_id(row.get(0)?, 0, "publish run ID", PublishRunId::new)?;
    let snapshot_id = positive_id(row.get(1)?, 1, "snapshot ID", SnapshotId::new)?;
    let projection_sha256 = sha256_column(row, 2)?;
    let managed_root = ManagedRoot::new(row.get::<_, String>(3)?)
        .map_err(|error| conversion_error(3, Type::Text, error.to_string()))?;
    let repository =
        GitRepositoryIdentity::from_canonical_path(PathBuf::from(row.get::<_, String>(4)?))
            .map_err(|error| conversion_error(4, Type::Text, error.to_string()))?;
    let target = PublicationTarget::new(row.get::<_, String>(5)?, row.get::<_, String>(6)?)
        .map_err(|error| conversion_error(5, Type::Text, error.to_string()))?;
    let base_commit = row.get(7)?;
    let reviewed_tree = row.get(8)?;
    let kind: String = row.get(9)?;
    let commit_oid: Option<String> = row.get(10)?;
    let publication = match (kind.as_str(), commit_oid) {
        ("noop", None) => PublishRunPublication::Noop,
        ("commit_ready", Some(commit_oid)) => PublishRunPublication::CommitReady { commit_oid },
        _ => {
            return Err(conversion_error(
                9,
                Type::Text,
                "publication kind does not match commit identity",
            ));
        }
    };
    let created_at_unix_ms = nonnegative(row.get(11)?, 11, "publish timestamp")?;
    PublishRun::rehydrate(
        id,
        snapshot_id,
        projection_sha256,
        managed_root,
        repository,
        target,
        base_commit,
        reviewed_tree,
        publication,
        created_at_unix_ms,
    )
    .map_err(|error| conversion_error(0, Type::Text, error.to_string()))
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
            Self::Persistence(message) => formatter.write_str(message),
        }
    }
}
impl Error for SqlitePublishRunStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let publication = match commit {
            Some(value) => PublishRunPublication::CommitReady {
                commit_oid: oid(value),
            },
            None => PublishRunPublication::Noop,
        };
        PublishRun::rehydrate(
            PublishRunId::new(id).unwrap(),
            SnapshotId::new(7).unwrap(),
            Sha256::new([9; 32]),
            ManagedRoot::new("content").unwrap(),
            GitRepositoryIdentity::new(&directory.0).unwrap(),
            PublicationTarget::new("origin", target).unwrap(),
            oid('a'),
            oid('b'),
            publication,
            time,
        )
        .unwrap()
    }

    #[test]
    fn created_and_noop_survive_restart_without_fabricating_a_commit() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let created = run(&directory, 1, "refs/heads/main", Some('c'), 10);
        let noop = run(&directory, 2, "refs/heads/main", None, 11);
        let store = SqlitePublishRunStore::open(&database).unwrap();
        store.save(&created).unwrap();
        store.save(&noop).unwrap();
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
        store.save(&first).unwrap();
        store.save(&first).unwrap();
        let conflict = run(&directory, 1, "refs/heads/main", Some('d'), 10);
        assert!(
            matches!(store.save(&conflict), Err(SqlitePublishRunStoreError::ConflictingPublishRunId(id)) if id == first.id())
        );
    }

    #[test]
    fn multiple_attempts_and_targets_are_retained_in_stable_order() {
        let directory = TestDirectory::new();
        let store = SqlitePublishRunStore::open(directory.database()).unwrap();
        let late = run(&directory, 2, "refs/heads/main", Some('c'), 20);
        let first = run(&directory, 1, "refs/heads/main", Some('c'), 10);
        let staging = run(&directory, 3, "refs/heads/staging", Some('c'), 10);
        store.save(&late).unwrap();
        store.save(&first).unwrap();
        store.save(&staging).unwrap();
        assert_eq!(
            store.list().unwrap(),
            vec![first.clone(), staging.clone(), late]
        );
        assert_eq!(
            store.list_for_target(staging.target()).unwrap(),
            vec![staging]
        );
    }
}
