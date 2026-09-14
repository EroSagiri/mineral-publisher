#![forbid(unsafe_code)]

use std::{error::Error, fmt, path::Path};

use rusqlite::{Connection, OptionalExtension, params};

use mineral_core::backup::{
    BackupDeliveryWire, BackupDeliveryWireError, BackupRun, BackupRunError, BackupRunId,
    BackupRunStore,
};

use crate::{
    domain::{Sha256, SnapshotId},
    publisher::{CommitSpecWire, CommitSpecWireError, GitCommitOid, GitRefTarget},
};

/// The only `user_version` this store writes and reads.
const SCHEMA_VERSION: i64 = 1;

/// Local SQLite persistence for immutable backup intents.
///
/// A backup freezes its intent *before* any remote side effect, and a restart
/// resumes exactly that intent instead of re-deriving one from today's
/// configuration. The row therefore stores the full delivery projection — as the
/// versioned [`BackupDeliveryWire`] text, so the required LFS objects that gate the
/// ref move survive a restart too — beside the base commit, the desired commit, the
/// target ref and the frozen commit specification.
///
/// Recording a fact is idempotent; recording a *different* fact under an id that is
/// already spent fails closed. A row that contradicts itself (a truncated digest, a
/// delivery whose own identity disagrees with the stored one, a commit specification
/// that does not build on the stored base commit) is refused on the way out rather
/// than being handed back as a usable intent.
pub struct SqliteBackupRunStore {
    connection: Connection,
}

impl SqliteBackupRunStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqliteBackupRunStoreError> {
        let connection = Connection::open(path).map_err(SqliteBackupRunStoreError::Sqlite)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(SqliteBackupRunStoreError::Sqlite)?;
        let detected: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(SqliteBackupRunStoreError::Sqlite)?;
        let mut version = detected;
        if version == 0 {
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE backup_runs (
                         id INTEGER PRIMARY KEY CHECK (id > 0),
                         snapshot_id INTEGER NOT NULL CHECK (snapshot_id > 0),
                         backup_projection_sha256 BLOB NOT NULL
                             CHECK (length(backup_projection_sha256) = 32),
                         delivery_sha256 BLOB NOT NULL
                             CHECK (length(delivery_sha256) = 32),
                         base_commit TEXT NOT NULL,
                         desired_commit TEXT NOT NULL,
                         remote_name TEXT NOT NULL,
                         destination_ref TEXT NOT NULL,
                         commit_spec TEXT NOT NULL,
                         delivery TEXT NOT NULL,
                         created_at_unix_ms INTEGER NOT NULL
                             CHECK (created_at_unix_ms >= 0)
                     );
                     CREATE INDEX backup_runs_by_creation
                         ON backup_runs(created_at_unix_ms, id);
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
                .map_err(SqliteBackupRunStoreError::Sqlite)?;
            version = SCHEMA_VERSION;
        }
        // There is exactly one schema this store understands. An unknown version is
        // refused rather than read as if it were this one, so a database written by
        // a different engine can never be silently reinterpreted.
        if version != SCHEMA_VERSION {
            return Err(SqliteBackupRunStoreError::UnsupportedSchemaVersion(
                detected,
            ));
        }
        Ok(Self { connection })
    }

    fn load(&self, id: BackupRunId) -> Result<Option<BackupRun>, SqliteBackupRunStoreError> {
        let stored = self
            .connection
            .query_row(
                "SELECT id, snapshot_id, backup_projection_sha256, delivery_sha256, base_commit,
                        desired_commit, remote_name, destination_ref, commit_spec, delivery,
                        created_at_unix_ms
                 FROM backup_runs WHERE id = ?1",
                [integer("backup run ID", id.get())?],
                stored_row,
            )
            .optional()
            .map_err(SqliteBackupRunStoreError::Sqlite)?;
        stored.map(rehydrate).transpose()
    }
}

impl BackupRunStore for SqliteBackupRunStore {
    type Error = SqliteBackupRunStoreError;

    fn save(&self, run: &BackupRun) -> Result<(), Self::Error> {
        let commit_spec = CommitSpecWire::encode(run.commit_spec());
        let delivery = BackupDeliveryWire::encode(run.delivery());
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO backup_runs (
                     id, snapshot_id, backup_projection_sha256, delivery_sha256, base_commit,
                     desired_commit, remote_name, destination_ref, commit_spec, delivery,
                     created_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    integer("backup run ID", run.id().get())?,
                    integer("snapshot ID", run.snapshot_id().get())?,
                    run.backup_projection_sha256().as_bytes().to_vec(),
                    run.delivery_sha256().as_bytes().to_vec(),
                    run.base_commit().as_str(),
                    run.desired_commit().as_str(),
                    run.target().remote_name(),
                    run.target().destination_ref(),
                    commit_spec,
                    delivery,
                    integer("backup timestamp", run.created_at_unix_ms())?,
                ],
            )
            .map_err(SqliteBackupRunStoreError::Sqlite)?;
        if inserted == 1 {
            return Ok(());
        }
        // The id was already spent. Re-recording the identical intent is success; a
        // different fact under one id must never be accepted, because that id is
        // what a later resume uses to find the intent it must continue.
        match self.load(run.id())? {
            Some(stored) if stored == *run => Ok(()),
            Some(_) => Err(SqliteBackupRunStoreError::ConflictingBackupRunId(run.id())),
            None => Err(SqliteBackupRunStoreError::Persistence(
                "backup run insert was ignored without an existing row".to_owned(),
            )),
        }
    }

    fn get(&self, id: BackupRunId) -> Result<Option<BackupRun>, Self::Error> {
        self.load(id)
    }

    fn list(&self) -> Result<Vec<BackupRun>, Self::Error> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, snapshot_id, backup_projection_sha256, delivery_sha256, base_commit,
                        desired_commit, remote_name, destination_ref, commit_spec, delivery,
                        created_at_unix_ms
                 FROM backup_runs ORDER BY created_at_unix_ms ASC, id ASC",
            )
            .map_err(SqliteBackupRunStoreError::Sqlite)?;
        let rows = statement
            .query_map([], stored_row)
            .map_err(SqliteBackupRunStoreError::Sqlite)?;
        let stored = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(SqliteBackupRunStoreError::Sqlite)?;
        stored.into_iter().map(rehydrate).collect()
    }
}

/// One row exactly as SQLite returned it, before any domain value is trusted.
///
/// Keeping the read separate from the interpretation lets the interpretation use
/// the store's own error type (a damaged digest, a contradictory delivery) instead
/// of collapsing every problem into a `rusqlite` conversion failure.
struct StoredRow {
    id: i64,
    snapshot_id: i64,
    backup_projection_sha256: Vec<u8>,
    delivery_sha256: Vec<u8>,
    base_commit: String,
    desired_commit: String,
    remote_name: String,
    destination_ref: String,
    commit_spec: String,
    delivery: String,
    created_at_unix_ms: i64,
}

fn stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRow> {
    Ok(StoredRow {
        id: row.get(0)?,
        snapshot_id: row.get(1)?,
        backup_projection_sha256: row.get(2)?,
        delivery_sha256: row.get(3)?,
        base_commit: row.get(4)?,
        desired_commit: row.get(5)?,
        remote_name: row.get(6)?,
        destination_ref: row.get(7)?,
        commit_spec: row.get(8)?,
        delivery: row.get(9)?,
        created_at_unix_ms: row.get(10)?,
    })
}

/// Rebuilds one intent from a stored row, refusing anything that is not exactly the
/// fact a live [`BackupRun`] could have produced.
///
/// Every column is validated and the value is rebuilt through [`BackupRun::new`], so
/// the cross-column invariants (delivery belongs to the snapshot, delivery was built
/// from the projection, the specification builds on the base commit) hold for a
/// restored run just as they held for the run that was saved.
fn rehydrate(stored: StoredRow) -> Result<BackupRun, SqliteBackupRunStoreError> {
    let id = BackupRunId::new(nonnegative(stored.id, "backup run ID is negative")?)
        .map_err(|_| SqliteBackupRunStoreError::DamagedRow("backup run ID is zero"))?;
    let snapshot_id = SnapshotId::new(nonnegative(stored.snapshot_id, "snapshot ID is negative")?)
        .map_err(|_| SqliteBackupRunStoreError::DamagedRow("snapshot ID is zero"))?;
    let backup_projection_sha256 = sha256(
        stored.backup_projection_sha256,
        "backup projection SHA-256 is not 32 bytes",
    )?;
    let stored_delivery_sha256 =
        sha256(stored.delivery_sha256, "delivery SHA-256 is not 32 bytes")?;
    let base_commit = GitCommitOid::new(stored.base_commit)
        .map_err(|_| SqliteBackupRunStoreError::DamagedRow("base commit is not a Git object ID"))?;
    let desired_commit = GitCommitOid::new(stored.desired_commit).map_err(|_| {
        SqliteBackupRunStoreError::DamagedRow("desired commit is not a Git object ID")
    })?;
    let target = GitRefTarget::new(stored.remote_name, stored.destination_ref).map_err(|_| {
        SqliteBackupRunStoreError::DamagedRow("stored remote and destination ref are not usable")
    })?;
    let commit_spec = CommitSpecWire::decode(&stored.commit_spec)
        .map_err(SqliteBackupRunStoreError::CommitSpec)?;
    let delivery = BackupDeliveryWire::decode(&stored.delivery)
        .map_err(SqliteBackupRunStoreError::Delivery)?;
    // The delivery text already recomputes its own identity; a stored column that
    // disagrees with it means the row was edited or written by something else.
    if delivery.delivery_sha256() != stored_delivery_sha256 {
        return Err(SqliteBackupRunStoreError::DamagedRow(
            "stored delivery identity does not match the stored delivery",
        ));
    }
    BackupRun::new(
        id,
        snapshot_id,
        backup_projection_sha256,
        delivery,
        base_commit,
        desired_commit,
        target,
        commit_spec,
        nonnegative(stored.created_at_unix_ms, "backup timestamp is negative")?,
    )
    .map_err(SqliteBackupRunStoreError::Run)
}

/// Converts one non-negative domain identity into SQLite's signed integer range.
fn integer(field: &'static str, value: u64) -> Result<i64, SqliteBackupRunStoreError> {
    value
        .try_into()
        .map_err(|_| SqliteBackupRunStoreError::ValueOutOfRange(field))
}

/// Reads one SQLite integer that a stored row promises to be non-negative.
fn nonnegative(value: i64, field: &'static str) -> Result<u64, SqliteBackupRunStoreError> {
    value
        .try_into()
        .map_err(|_| SqliteBackupRunStoreError::DamagedRow(field))
}

/// Reads one stored digest, refusing a BLOB that is not exactly 32 bytes.
fn sha256(bytes: Vec<u8>, field: &'static str) -> Result<Sha256, SqliteBackupRunStoreError> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| SqliteBackupRunStoreError::DamagedRow(field))?;
    Ok(Sha256::new(bytes))
}

#[derive(Debug)]
pub enum SqliteBackupRunStoreError {
    Sqlite(rusqlite::Error),
    UnsupportedSchemaVersion(i64),
    ConflictingBackupRunId(BackupRunId),
    ValueOutOfRange(&'static str),
    /// A stored row is not the shape this engine wrote, or contradicts itself.
    DamagedRow(&'static str),
    /// The stored parts are individually readable but do not form a valid intent.
    Run(BackupRunError),
    CommitSpec(CommitSpecWireError),
    Delivery(BackupDeliveryWireError),
    Persistence(String),
}

impl fmt::Display for SqliteBackupRunStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => {
                write!(formatter, "SQLite backup-run persistence failed: {error}")
            }
            Self::UnsupportedSchemaVersion(version) => write!(
                formatter,
                "unsupported backup-run schema version: {version}"
            ),
            Self::ConflictingBackupRunId(id) => write!(
                formatter,
                "backup run ID {} already stores a different fact",
                id.get()
            ),
            Self::ValueOutOfRange(field) => {
                write!(formatter, "{field} is outside SQLite's integer range")
            }
            Self::DamagedRow(reason) => {
                write!(formatter, "stored backup run row is damaged: {reason}")
            }
            Self::Run(error) => write!(formatter, "stored backup run is unusable: {error}"),
            Self::CommitSpec(error) => {
                write!(
                    formatter,
                    "stored backup commit specification is unusable: {error}"
                )
            }
            Self::Delivery(error) => {
                write!(formatter, "stored backup delivery is unusable: {error}")
            }
            Self::Persistence(message) => formatter.write_str(message),
        }
    }
}

impl Error for SqliteBackupRunStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Run(error) => Some(error),
            Self::CommitSpec(error) => Some(error),
            Self::Delivery(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{ContentPath, Snapshot, SnapshotFile, SourceId, TimestampMillis},
        ports::{BlobStore, ContentStoreError},
        publisher::{GitCommitSpec, GitTreeOid},
    };
    use mineral_core::backup::{
        BackupProjection, TypeFirstBackupRepresentationPolicy, build_backup_delivery,
        build_backup_tree,
    };
    use std::{
        cell::RefCell,
        collections::HashMap,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::SystemTime,
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-backup-run-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn database(&self) -> PathBuf {
            self.0.join("backup-runs.sqlite3")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// An in-memory CAS, enough for the delivery builder to store the pointer,
    /// attribute and manifest blobs.
    #[derive(Default)]
    struct MemoryStore {
        blobs: RefCell<HashMap<Sha256, Vec<u8>>>,
    }

    impl BlobStore for MemoryStore {
        fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
            self.blobs
                .borrow()
                .get(&identity)
                .cloned()
                .ok_or(ContentStoreError::Missing(identity))
        }

        fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
            let identity = Sha256::digest(content);
            self.blobs.borrow_mut().insert(identity, content.to_vec());
            Ok(identity)
        }
    }

    fn oid(value: char) -> String {
        std::iter::repeat_n(value, 40).collect()
    }

    /// One intent whose delivery holds both a Git blob and an LFS object, so the
    /// round trip exercises every part of the durable delivery text — including the
    /// required LFS object that gates the ref move.
    fn run(id: u64, time: u64) -> BackupRun {
        let blobs = MemoryStore::default();
        let snapshot = Snapshot::new(
            SnapshotId::new(3).unwrap(),
            SystemTime::UNIX_EPOCH,
            SourceId::new("vault").unwrap(),
            vec![
                SnapshotFile::new(
                    ContentPath::new("notes/a.md").unwrap(),
                    6,
                    Sha256::digest(b"# note"),
                    None,
                ),
                SnapshotFile::new(
                    ContentPath::new("attachments/a.jpg").unwrap(),
                    5,
                    Sha256::digest(b"jpeg!"),
                    None,
                ),
            ],
        )
        .unwrap();
        let projection = BackupProjection::build(&snapshot).unwrap();
        let delivery =
            build_backup_delivery(&projection, &TypeFirstBackupRepresentationPolicy, &blobs)
                .unwrap();
        // Built here so the fixture goes through the same tree mapping a real attempt
        // uses before the delivery becomes an intent.
        let tree = build_backup_tree(&delivery).unwrap();
        assert_eq!(tree.len(), 4, "two files plus the manifest and attributes");
        assert_eq!(delivery.required_lfs_objects().len(), 1);
        BackupRun::new(
            BackupRunId::new(id).unwrap(),
            snapshot.id(),
            projection.projection_sha256(),
            delivery,
            GitCommitOid::new(oid('a')).unwrap(),
            GitCommitOid::new(oid('c')).unwrap(),
            GitRefTarget::new("origin", "refs/heads/backup").unwrap(),
            GitCommitSpec::new(
                GitCommitOid::new(oid('a')).unwrap(),
                GitTreeOid::new(oid('b')).unwrap(),
                "Mineral Publisher",
                "publisher@example.invalid",
                TimestampMillis::from_unix_millis(time),
                "Mineral Publisher",
                "publisher@example.invalid",
                TimestampMillis::from_unix_millis(time),
                "Back up Mineral content",
            )
            .unwrap(),
            time,
        )
        .unwrap()
    }

    #[test]
    fn a_run_survives_a_restart_with_every_frozen_field() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let original = run(1, 10);
        {
            let store = SqliteBackupRunStore::open(&database).unwrap();
            store.save(&original).unwrap();
        }

        let reopened = SqliteBackupRunStore::open(&database).unwrap();
        let restored = reopened.get(original.id()).unwrap().unwrap();
        assert_eq!(restored, original);
        assert_eq!(
            restored.required_lfs_objects(),
            original.required_lfs_objects(),
            "the required LFS objects are part of the durable delivery"
        );
        assert!(!restored.required_lfs_objects().is_empty());
        assert_eq!(restored.delivery_sha256(), original.delivery_sha256());
        assert_eq!(restored.commit_spec(), original.commit_spec());
        assert_eq!(
            reopened.get(BackupRunId::new(9).unwrap()).unwrap(),
            None,
            "an unknown id is absent, not an error"
        );
    }

    #[test]
    fn an_unknown_schema_version_fails_closed() {
        let directory = TestDirectory::new();
        let database = directory.database();
        {
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch("PRAGMA user_version = 4;")
                .unwrap();
        }

        assert!(matches!(
            SqliteBackupRunStore::open(&database),
            Err(SqliteBackupRunStoreError::UnsupportedSchemaVersion(4))
        ));
    }

    #[test]
    fn saving_the_same_run_twice_is_idempotent() {
        let directory = TestDirectory::new();
        let store = SqliteBackupRunStore::open(directory.database()).unwrap();
        let original = run(1, 10);

        store.save(&original).unwrap();
        store.save(&original).unwrap();

        assert_eq!(store.list().unwrap(), vec![original]);
    }

    #[test]
    fn a_different_run_under_one_id_fails_closed() {
        let directory = TestDirectory::new();
        let store = SqliteBackupRunStore::open(directory.database()).unwrap();
        let first = run(1, 10);
        let conflict = run(1, 11);
        assert_ne!(first, conflict);

        store.save(&first).unwrap();

        assert!(matches!(
            store.save(&conflict),
            Err(SqliteBackupRunStoreError::ConflictingBackupRunId(id)) if id == first.id()
        ));
        assert_eq!(store.get(first.id()).unwrap(), Some(first));
    }

    #[test]
    fn a_damaged_row_fails_closed() {
        let directory = TestDirectory::new();
        let store = SqliteBackupRunStore::open(directory.database()).unwrap();
        let truncated = run(1, 10);
        let corrupt_delivery = run(2, 11);
        store.save(&truncated).unwrap();
        store.save(&corrupt_delivery).unwrap();

        // A digest that is not 32 bytes can only be written with the CHECK
        // constraints disabled: exactly the state a damaged database is in.
        store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE backup_runs SET backup_projection_sha256 = ?1 WHERE id = ?2",
                params![vec![0_u8; 16], truncated.id().get() as i64],
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE backup_runs SET delivery = 'not a backup delivery' WHERE id = ?1",
                params![corrupt_delivery.id().get() as i64],
            )
            .unwrap();

        assert!(matches!(
            store.get(truncated.id()),
            Err(SqliteBackupRunStoreError::DamagedRow(_))
        ));
        assert!(matches!(
            store.get(corrupt_delivery.id()),
            Err(SqliteBackupRunStoreError::Delivery(_))
        ));
        assert!(
            store.list().is_err(),
            "a row that cannot be trusted must also fail the listing"
        );
    }

    #[test]
    fn multiple_runs_are_retained_in_stable_order() {
        let directory = TestDirectory::new();
        let store = SqliteBackupRunStore::open(directory.database()).unwrap();
        let late = run(2, 20);
        let first = run(1, 10);
        let same_time = run(3, 10);

        store.save(&late).unwrap();
        store.save(&first).unwrap();
        store.save(&same_time).unwrap();

        assert_eq!(store.list().unwrap(), vec![first, same_time, late]);
    }
}
