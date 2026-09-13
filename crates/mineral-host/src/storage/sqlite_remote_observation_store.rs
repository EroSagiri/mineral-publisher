use std::{error::Error, fmt, path::Path};

use rusqlite::{Connection, OptionalExtension, params, types::Type};

use crate::publisher::{
    GitCommitOid, GitRefTarget, PublishRunId, RemoteObservationId, RemoteObservationStore,
    RemoteRefObservation, RemoteRefState,
};

const SCHEMA_VERSION: i64 = 1;

/// SQLite persistence for the append-only remote observation audit trail.
pub struct SqliteRemoteObservationStore {
    connection: Connection,
}

impl SqliteRemoteObservationStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqliteRemoteObservationStoreError> {
        let connection =
            Connection::open(path).map_err(SqliteRemoteObservationStoreError::Sqlite)?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(SqliteRemoteObservationStoreError::Sqlite)?;
        match version {
            0 => connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE remote_ref_observations (
                         id INTEGER PRIMARY KEY CHECK (id > 0),
                         publish_run_id INTEGER NOT NULL CHECK (publish_run_id > 0),
                         remote_name TEXT NOT NULL,
                         destination_ref TEXT NOT NULL,
                         observed_kind TEXT NOT NULL CHECK (observed_kind IN ('present', 'missing')),
                         commit_oid TEXT,
                         observed_at_unix_ms INTEGER NOT NULL CHECK (observed_at_unix_ms >= 0),
                         CHECK ((observed_kind = 'missing' AND commit_oid IS NULL)
                             OR (observed_kind = 'present' AND commit_oid IS NOT NULL))
                     );
                     CREATE INDEX remote_observations_by_publish_run
                         ON remote_ref_observations(
                             publish_run_id, observed_at_unix_ms, id
                         );
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
                .map_err(SqliteRemoteObservationStoreError::Sqlite)?,
            SCHEMA_VERSION => {}
            value => {
                return Err(SqliteRemoteObservationStoreError::UnsupportedSchemaVersion(
                    value,
                ));
            }
        }
        Ok(Self { connection })
    }
}

impl RemoteObservationStore for SqliteRemoteObservationStore {
    type Error = SqliteRemoteObservationStoreError;

    fn save(&self, observation: &RemoteRefObservation) -> Result<(), Self::Error> {
        let (kind, commit_oid): (&str, Option<&str>) = match observation.observed() {
            RemoteRefState::Present { commit_oid } => ("present", Some(commit_oid.as_str())),
            RemoteRefState::Missing => ("missing", None),
        };
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO remote_ref_observations (
                    id, publish_run_id, remote_name, destination_ref,
                    observed_kind, commit_oid, observed_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    integer("remote observation ID", observation.id().get())?,
                    integer("publish run ID", observation.publish_run_id().get())?,
                    observation.target().remote_name(),
                    observation.target().destination_ref(),
                    kind,
                    commit_oid,
                    integer(
                        "remote observation timestamp",
                        observation.observed_at_unix_ms()
                    )?,
                ],
            )
            .map_err(SqliteRemoteObservationStoreError::Sqlite)?;
        if inserted == 1 {
            return Ok(());
        }
        match self.get(observation.id())? {
            Some(stored) if stored == *observation => Ok(()),
            Some(_) => Err(
                SqliteRemoteObservationStoreError::ConflictingRemoteObservationId(observation.id()),
            ),
            None => Err(SqliteRemoteObservationStoreError::Persistence(
                "remote observation insert was ignored without an existing row".to_owned(),
            )),
        }
    }

    fn get(&self, id: RemoteObservationId) -> Result<Option<RemoteRefObservation>, Self::Error> {
        self.connection
            .query_row(
                "SELECT id, publish_run_id, remote_name, destination_ref,
                        observed_kind, commit_oid, observed_at_unix_ms
                 FROM remote_ref_observations WHERE id = ?1",
                [integer("remote observation ID", id.get())?],
                row_to_observation,
            )
            .optional()
            .map_err(SqliteRemoteObservationStoreError::Sqlite)
    }

    fn list_for_publish_run(
        &self,
        publish_run_id: PublishRunId,
    ) -> Result<Vec<RemoteRefObservation>, Self::Error> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, publish_run_id, remote_name, destination_ref,
                        observed_kind, commit_oid, observed_at_unix_ms
                 FROM remote_ref_observations WHERE publish_run_id = ?1
                 ORDER BY observed_at_unix_ms ASC, id ASC",
            )
            .map_err(SqliteRemoteObservationStoreError::Sqlite)?;
        let rows = statement
            .query_map(
                [integer("publish run ID", publish_run_id.get())?],
                row_to_observation,
            )
            .map_err(SqliteRemoteObservationStoreError::Sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SqliteRemoteObservationStoreError::Sqlite)
    }
}

fn row_to_observation(row: &rusqlite::Row<'_>) -> rusqlite::Result<RemoteRefObservation> {
    let id = positive_id(
        row.get(0)?,
        0,
        "remote observation ID",
        RemoteObservationId::new,
    )?;
    let publish_run_id = positive_id(row.get(1)?, 1, "publish run ID", PublishRunId::new)?;
    let target = GitRefTarget::new(row.get::<_, String>(2)?, row.get::<_, String>(3)?)
        .map_err(|error| conversion_error(2, Type::Text, error.to_string()))?;
    let kind: String = row.get(4)?;
    let commit_oid: Option<String> = row.get(5)?;
    let observed = match (kind.as_str(), commit_oid) {
        ("present", Some(commit_oid)) => RemoteRefState::Present {
            commit_oid: GitCommitOid::new(commit_oid)
                .map_err(|error| conversion_error(5, Type::Text, error.to_string()))?,
        },
        ("missing", None) => RemoteRefState::Missing,
        _ => {
            return Err(conversion_error(
                4,
                Type::Text,
                "remote observation kind does not match commit identity",
            ));
        }
    };
    let observed_at_unix_ms = nonnegative(row.get(6)?, 6, "remote observation timestamp")?;
    Ok(RemoteRefObservation::rehydrate(
        id,
        publish_run_id,
        target,
        observed,
        observed_at_unix_ms,
    ))
}

fn integer(field: &'static str, value: u64) -> Result<i64, SqliteRemoteObservationStoreError> {
    value
        .try_into()
        .map_err(|_| SqliteRemoteObservationStoreError::ValueOutOfRange(field))
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

fn conversion_error(column: usize, data_type: Type, message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        data_type,
        Box::new(CorruptRemoteObservation(message.into())),
    )
}

#[derive(Debug)]
struct CorruptRemoteObservation(String);

impl fmt::Display for CorruptRemoteObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CorruptRemoteObservation {}

#[derive(Debug)]
pub enum SqliteRemoteObservationStoreError {
    Sqlite(rusqlite::Error),
    UnsupportedSchemaVersion(i64),
    ConflictingRemoteObservationId(RemoteObservationId),
    ValueOutOfRange(&'static str),
    Persistence(String),
}

impl fmt::Display for SqliteRemoteObservationStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(
                formatter,
                "SQLite remote-observation persistence failed: {error}"
            ),
            Self::UnsupportedSchemaVersion(version) => write!(
                formatter,
                "unsupported remote-observation schema version: {version}"
            ),
            Self::ConflictingRemoteObservationId(id) => write!(
                formatter,
                "remote observation ID {} already stores a different fact",
                id.get()
            ),
            Self::ValueOutOfRange(field) => {
                write!(formatter, "{field} is outside SQLite's integer range")
            }
            Self::Persistence(message) => formatter.write_str(message),
        }
    }
}

impl Error for SqliteRemoteObservationStoreError {
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
    use crate::{
        domain::{Sha256, SnapshotId, TimestampMillis},
        publisher::{GitRepositoryIdentity, PublishRun, PublishTargetId},
        workflow::ManagedRoot,
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
                "mineral-publisher-remote-observation-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn database(&self) -> PathBuf {
            self.0.join("remote-observations.sqlite3")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run(directory: &TestDirectory) -> PublishRun {
        PublishRun::rehydrate(
            PublishRunId::new(7).unwrap(),
            SnapshotId::new(1).unwrap(),
            Some(Sha256::new([1; 32])),
            None,
            None,
            ManagedRoot::new("content").unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            GitRepositoryIdentity::new(&directory.0)
                .unwrap()
                .locator()
                .clone(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            "a".repeat(40),
            "b".repeat(40),
            None,
            None,
            1,
        )
        .unwrap()
    }

    fn observation(
        id: u64,
        run: &PublishRun,
        state: RemoteRefState,
        time: u64,
    ) -> RemoteRefObservation {
        RemoteRefObservation::new(
            RemoteObservationId::new(id).unwrap(),
            run,
            state,
            TimestampMillis::from_unix_millis(time),
        )
    }

    #[test]
    fn observation_survives_restart() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let run = run(&directory);
        let fact = observation(
            1,
            &run,
            RemoteRefState::Present {
                commit_oid: GitCommitOid::new("a".repeat(40)).unwrap(),
            },
            10,
        );
        let store = SqliteRemoteObservationStore::open(&database).unwrap();
        store.save(&fact).unwrap();
        drop(store);
        let reopened = SqliteRemoteObservationStore::open(&database).unwrap();
        assert_eq!(reopened.get(fact.id()).unwrap(), Some(fact));
    }

    #[test]
    fn multiple_observations_are_retained_in_stable_order() {
        let directory = TestDirectory::new();
        let run = run(&directory);
        let first = observation(
            1,
            &run,
            RemoteRefState::Present {
                commit_oid: GitCommitOid::new("a".repeat(40)).unwrap(),
            },
            10,
        );
        let second = observation(
            2,
            &run,
            RemoteRefState::Present {
                commit_oid: GitCommitOid::new("c".repeat(40)).unwrap(),
            },
            20,
        );
        let store = SqliteRemoteObservationStore::open(directory.database()).unwrap();
        store.save(&second).unwrap();
        store.save(&first).unwrap();
        assert_eq!(
            store.list_for_publish_run(run.id()).unwrap(),
            vec![first, second]
        );
    }

    #[test]
    fn save_is_idempotent_but_conflicting_id_fails() {
        let directory = TestDirectory::new();
        let run = run(&directory);
        let first = observation(1, &run, RemoteRefState::Missing, 10);
        let conflict = observation(
            1,
            &run,
            RemoteRefState::Present {
                commit_oid: GitCommitOid::new("a".repeat(40)).unwrap(),
            },
            10,
        );
        let store = SqliteRemoteObservationStore::open(directory.database()).unwrap();
        store.save(&first).unwrap();
        store.save(&first).unwrap();
        assert!(matches!(
            store.save(&conflict),
            Err(SqliteRemoteObservationStoreError::ConflictingRemoteObservationId(id))
                if id == first.id()
        ));
    }
}
