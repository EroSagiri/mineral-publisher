use std::{error::Error, fmt, path::Path};

use rusqlite::{Connection, OptionalExtension, params, types::Type};

use crate::{
    asset::{
        AssetByteIdentity, AssetObservationId, AssetObservationStore, AssetTargetFacts,
        AssetTargetObservation, AssetTargetState,
    },
    domain::{ContentPath, Sha256},
    publisher::PublishRunId,
    workflow::{AssetContentType, AssetObjectKey},
};

const SCHEMA_VERSION: i64 = 1;

/// SQLite persistence for the append-only asset-target observation audit trail.
pub struct SqliteAssetObservationStore {
    connection: Connection,
}

impl SqliteAssetObservationStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqliteAssetObservationStoreError> {
        let connection =
            Connection::open(path).map_err(SqliteAssetObservationStoreError::Sqlite)?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(SqliteAssetObservationStoreError::Sqlite)?;
        match version {
            0 => connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE asset_target_observations (
                         id INTEGER PRIMARY KEY CHECK (id > 0),
                         publish_run_id INTEGER NOT NULL CHECK (publish_run_id > 0),
                         delivery_projection_sha256 BLOB NOT NULL
                             CHECK (length(delivery_projection_sha256) = 32),
                         logical_path TEXT NOT NULL,
                         object_key TEXT NOT NULL,
                         observed_kind TEXT NOT NULL CHECK (observed_kind IN ('present', 'missing')),
                         size INTEGER,
                         content_type TEXT,
                         byte_identity_kind TEXT
                             CHECK (byte_identity_kind IS NULL
                                 OR byte_identity_kind IN ('verified', 'attested', 'unavailable')),
                         byte_identity BLOB,
                         observed_at_unix_ms INTEGER NOT NULL CHECK (observed_at_unix_ms >= 0),
                         CHECK (
                             (observed_kind = 'missing'
                                 AND size IS NULL AND content_type IS NULL
                                 AND byte_identity_kind IS NULL AND byte_identity IS NULL)
                             OR (observed_kind = 'present'
                                 AND size IS NOT NULL AND content_type IS NOT NULL
                                 AND byte_identity_kind IS NOT NULL
                                 AND ((byte_identity_kind = 'unavailable' AND byte_identity IS NULL)
                                     OR (byte_identity_kind IN ('verified', 'attested')
                                         AND length(byte_identity) = 32)))
                         )
                     );
                     CREATE INDEX asset_observations_by_publish_run
                         ON asset_target_observations(
                             publish_run_id, observed_at_unix_ms, id
                         );
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
                .map_err(SqliteAssetObservationStoreError::Sqlite)?,
            SCHEMA_VERSION => {}
            value => {
                return Err(SqliteAssetObservationStoreError::UnsupportedSchemaVersion(
                    value,
                ));
            }
        }
        Ok(Self { connection })
    }
}

impl AssetObservationStore for SqliteAssetObservationStore {
    type Error = SqliteAssetObservationStoreError;

    fn save(&self, observation: &AssetTargetObservation) -> Result<(), Self::Error> {
        let (kind, size, content_type, identity_kind, identity) = match observation.observed() {
            AssetTargetState::Missing => ("missing", None, None, None, None),
            AssetTargetState::Present(facts) => {
                let (identity_kind, identity) = match facts.bytes() {
                    AssetByteIdentity::Verified(sha256) => ("verified", Some(sha256)),
                    AssetByteIdentity::Attested(sha256) => ("attested", Some(sha256)),
                    AssetByteIdentity::Unavailable => ("unavailable", None),
                };
                (
                    "present",
                    Some(facts.size()),
                    Some(facts.content_type().as_str().to_owned()),
                    Some(identity_kind),
                    identity,
                )
            }
        };
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO asset_target_observations (
                    id, publish_run_id, delivery_projection_sha256, logical_path, object_key,
                    observed_kind, size, content_type, byte_identity_kind, byte_identity,
                    observed_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    integer("asset observation ID", observation.id().get())?,
                    integer("publish run ID", observation.publish_run_id().get())?,
                    observation
                        .delivery_projection_sha256()
                        .as_bytes()
                        .as_slice(),
                    observation.logical_path().as_str(),
                    observation.object_key().as_str(),
                    kind,
                    size.map(|size| integer("observed asset size", size))
                        .transpose()?,
                    content_type,
                    identity_kind,
                    identity.map(|sha256| sha256.as_bytes().to_vec()),
                    integer(
                        "asset observation timestamp",
                        observation.observed_at_unix_ms()
                    )?,
                ],
            )
            .map_err(SqliteAssetObservationStoreError::Sqlite)?;
        if inserted == 1 {
            return Ok(());
        }
        match self.get(observation.id())? {
            Some(stored) if stored == *observation => Ok(()),
            Some(_) => Err(
                SqliteAssetObservationStoreError::ConflictingAssetObservationId(observation.id()),
            ),
            None => Err(SqliteAssetObservationStoreError::Persistence(
                "asset observation insert was ignored without an existing row".to_owned(),
            )),
        }
    }

    fn get(&self, id: AssetObservationId) -> Result<Option<AssetTargetObservation>, Self::Error> {
        self.connection
            .query_row(
                "SELECT id, publish_run_id, delivery_projection_sha256, logical_path, object_key,
                        observed_kind, size, content_type, byte_identity_kind, byte_identity,
                        observed_at_unix_ms
                 FROM asset_target_observations WHERE id = ?1",
                [integer("asset observation ID", id.get())?],
                row_to_observation,
            )
            .optional()
            .map_err(SqliteAssetObservationStoreError::Sqlite)
    }

    fn list_for_publish_run(
        &self,
        publish_run_id: PublishRunId,
    ) -> Result<Vec<AssetTargetObservation>, Self::Error> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, publish_run_id, delivery_projection_sha256, logical_path, object_key,
                        observed_kind, size, content_type, byte_identity_kind, byte_identity,
                        observed_at_unix_ms
                 FROM asset_target_observations WHERE publish_run_id = ?1
                 ORDER BY observed_at_unix_ms ASC, id ASC",
            )
            .map_err(SqliteAssetObservationStoreError::Sqlite)?;
        let rows = statement
            .query_map(
                [integer("publish run ID", publish_run_id.get())?],
                row_to_observation,
            )
            .map_err(SqliteAssetObservationStoreError::Sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SqliteAssetObservationStoreError::Sqlite)
    }
}

fn row_to_observation(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssetTargetObservation> {
    let id = positive_id(
        row.get(0)?,
        0,
        "asset observation ID",
        AssetObservationId::new,
    )?;
    let publish_run_id = positive_id(row.get(1)?, 1, "publish run ID", PublishRunId::new)?;
    let delivery_projection_sha256 = sha256_column(row, 2, "delivery projection identity")?;
    let logical_path = ContentPath::new(row.get::<_, String>(3)?)
        .map_err(|error| conversion_error(3, Type::Text, error.to_string()))?;
    let object_key = AssetObjectKey::rehydrate(row.get::<_, String>(4)?)
        .map_err(|error| conversion_error(4, Type::Text, error.to_string()))?;
    let kind: String = row.get(5)?;
    let observed = match kind.as_str() {
        "missing" => AssetTargetState::Missing,
        "present" => {
            let size = nonnegative(row.get::<_, i64>(6)?, 6, "observed asset size")?;
            let content_type = AssetContentType::new(row.get::<_, String>(7)?)
                .map_err(|error| conversion_error(7, Type::Text, error.to_string()))?;
            let identity_kind: String = row.get(8)?;
            let identity: Option<Vec<u8>> = row.get(9)?;
            let bytes = match (identity_kind.as_str(), identity) {
                ("verified", Some(bytes)) => {
                    AssetByteIdentity::Verified(sha256_bytes(bytes, 9, "observed byte identity")?)
                }
                ("attested", Some(bytes)) => {
                    AssetByteIdentity::Attested(sha256_bytes(bytes, 9, "attested byte identity")?)
                }
                ("unavailable", None) => AssetByteIdentity::Unavailable,
                _ => {
                    return Err(conversion_error(
                        8,
                        Type::Text,
                        "observed byte identity kind does not match its value",
                    ));
                }
            };
            AssetTargetState::Present(AssetTargetFacts::new(
                object_key.clone(),
                size,
                content_type,
                bytes,
            ))
        }
        _ => {
            return Err(conversion_error(
                5,
                Type::Text,
                "asset observation kind is not 'present' or 'missing'",
            ));
        }
    };
    let observed_at_unix_ms = nonnegative(row.get(10)?, 10, "asset observation timestamp")?;
    AssetTargetObservation::rehydrate(
        id,
        publish_run_id,
        delivery_projection_sha256,
        logical_path,
        object_key,
        observed,
        observed_at_unix_ms,
    )
    .map_err(|error| conversion_error(4, Type::Text, error.to_string()))
}

fn sha256_column(
    row: &rusqlite::Row<'_>,
    column: usize,
    field: &'static str,
) -> rusqlite::Result<Sha256> {
    let bytes: Vec<u8> = row.get(column)?;
    sha256_bytes(bytes, column, field)
}

fn sha256_bytes(bytes: Vec<u8>, column: usize, field: &'static str) -> rusqlite::Result<Sha256> {
    let bytes: [u8; 32] = bytes.try_into().map_err(|bytes: Vec<u8>| {
        conversion_error(
            column,
            Type::Blob,
            format!("{field} must contain 32 bytes, got {}", bytes.len()),
        )
    })?;
    Ok(Sha256::new(bytes))
}

fn integer(field: &'static str, value: u64) -> Result<i64, SqliteAssetObservationStoreError> {
    value
        .try_into()
        .map_err(|_| SqliteAssetObservationStoreError::ValueOutOfRange(field))
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
        Box::new(CorruptAssetObservation(message.into())),
    )
}

#[derive(Debug)]
struct CorruptAssetObservation(String);

impl fmt::Display for CorruptAssetObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CorruptAssetObservation {}

#[derive(Debug)]
pub enum SqliteAssetObservationStoreError {
    Sqlite(rusqlite::Error),
    UnsupportedSchemaVersion(i64),
    ConflictingAssetObservationId(AssetObservationId),
    ValueOutOfRange(&'static str),
    Persistence(String),
}

impl fmt::Display for SqliteAssetObservationStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(
                formatter,
                "SQLite asset-observation persistence failed: {error}"
            ),
            Self::UnsupportedSchemaVersion(version) => write!(
                formatter,
                "unsupported asset-observation schema version: {version}"
            ),
            Self::ConflictingAssetObservationId(id) => write!(
                formatter,
                "asset observation ID {} already stores a different fact",
                id.get()
            ),
            Self::ValueOutOfRange(field) => {
                write!(formatter, "{field} is outside SQLite's integer range")
            }
            Self::Persistence(message) => formatter.write_str(message),
        }
    }
}

impl Error for SqliteAssetObservationStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::{
        asset::{
            AssetByteIdentity, AssetObservationIdGenerator, AssetTargetFacts, AssetTargetState,
            SequentialAssetObservationIdGenerator,
        },
        domain::{ContentPath, SnapshotId, TimestampMillis},
        publisher::{
            DeliveryProjectionBinding, GitRefTarget, PublishRun, PublishTargetId, RepositoryLocator,
        },
        workflow::{AssetDeliveryConfig, PublishedAsset},
    };

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-asset-observations-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn database(&self) -> PathBuf {
            self.0.join("asset-observations.sqlite3")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn asset(logical_path: &str, body: &[u8]) -> PublishedAsset {
        PublishedAsset::from_parts_for_test(
            ContentPath::new(logical_path).unwrap(),
            Sha256::new([1; 32]),
            Sha256::digest(body),
            body.len() as u64,
            AssetContentType::new("image/png").unwrap(),
            &AssetDeliveryConfig::new("https://assets.example.com").unwrap(),
        )
    }

    fn run(id: u64) -> PublishRun {
        PublishRun::rehydrate(
            PublishRunId::new(id).unwrap(),
            SnapshotId::new(7).unwrap(),
            None,
            Some(Sha256::new([2; 32])),
            Some(Sha256::new([3; 32])),
            crate::workflow::ManagedRoot::new("content").unwrap(),
            PublishTargetId::new("origin:refs/heads/main").unwrap(),
            RepositoryLocator::new("/srv/public-repo").unwrap(),
            GitRefTarget::new("origin", "refs/heads/main").unwrap(),
            "a".repeat(40),
            "b".repeat(40),
            None,
            None,
            10,
        )
        .unwrap()
    }

    fn present(asset: &PublishedAsset) -> AssetTargetState {
        AssetTargetState::Present(AssetTargetFacts::new(
            asset.object_key().clone(),
            asset.published_size(),
            asset.published_content_type().clone(),
            AssetByteIdentity::Verified(asset.published_sha256()),
        ))
    }

    fn observation(
        id: u64,
        run: &PublishRun,
        asset: &PublishedAsset,
        observed: AssetTargetState,
        at: u64,
    ) -> AssetTargetObservation {
        AssetTargetObservation::new(
            AssetObservationId::new(id).unwrap(),
            run,
            Sha256::new([3; 32]),
            asset,
            observed,
            TimestampMillis::from_unix_millis(at),
        )
        .unwrap()
    }

    #[test]
    fn observations_survive_a_restart_and_are_append_only() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let run = run(1);
        let first = asset("img/a.png", b"first body");
        let second = asset("img/b.png", b"second body");
        let missing = observation(1, &run, &first, AssetTargetState::Missing, 10);
        let ready = observation(2, &run, &second, present(&second), 11);
        {
            let store = SqliteAssetObservationStore::open(&database).unwrap();
            store.save(&missing).unwrap();
            store.save(&ready).unwrap();
            store.save(&ready).unwrap();
        }

        let reopened = SqliteAssetObservationStore::open(&database).unwrap();
        assert_eq!(reopened.get(missing.id()).unwrap(), Some(missing.clone()));
        assert_eq!(reopened.get(ready.id()).unwrap(), Some(ready.clone()));
        let listed = reopened.list_for_publish_run(run.id()).unwrap();
        assert_eq!(listed, vec![missing, ready]);
        // A second run's facts are not mixed in.
        assert!(
            reopened
                .list_for_publish_run(PublishRunId::new(2).unwrap())
                .unwrap()
                .is_empty()
        );
    }

    /// §18.11, host side: an observation written before filename-bearing keys
    /// existed still loads after a restart.
    ///
    /// The durable asset-observation trail stores the object key as text, and both
    /// canonical shapes remain readable: a legacy row must not become unreadable
    /// just because current publications name a filename too.
    #[test]
    fn an_observation_recorded_under_a_legacy_key_still_loads() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let run = run(1);
        let current = asset("img/a.png", b"legacy body");
        let legacy_key = AssetObjectKey::legacy_for_published_sha256(&current.published_sha256());
        assert!(legacy_key.is_legacy());
        let legacy = AssetTargetObservation::rehydrate(
            AssetObservationId::new(9).unwrap(),
            run.id(),
            Sha256::new([3; 32]),
            current.logical_path().clone(),
            legacy_key,
            AssetTargetState::Present(AssetTargetFacts::new(
                current.object_key().clone(),
                current.published_size(),
                current.published_content_type().clone(),
                AssetByteIdentity::Verified(current.published_sha256()),
            )),
            12,
        );
        // The facts above name the current key, so they cannot be recorded against
        // the legacy one: an observation never mixes the two schemes.
        assert!(
            legacy.is_err(),
            "an observation must state facts about the key it records"
        );

        let legacy_key = AssetObjectKey::legacy_for_published_sha256(&current.published_sha256());
        let legacy = AssetTargetObservation::rehydrate(
            AssetObservationId::new(9).unwrap(),
            run.id(),
            Sha256::new([3; 32]),
            current.logical_path().clone(),
            legacy_key.clone(),
            AssetTargetState::Present(AssetTargetFacts::new(
                legacy_key.clone(),
                current.published_size(),
                current.published_content_type().clone(),
                AssetByteIdentity::Verified(current.published_sha256()),
            )),
            12,
        )
        .unwrap();
        {
            let store = SqliteAssetObservationStore::open(&database).unwrap();
            store.save(&legacy).unwrap();
        }

        let reopened = SqliteAssetObservationStore::open(&database).unwrap();
        let loaded = reopened.get(legacy.id()).unwrap().unwrap();
        assert_eq!(loaded, legacy);
        assert!(loaded.object_key().is_legacy());
        assert!(loaded.object_key().public_filename().is_none());
        assert_eq!(
            reopened
                .list_for_publish_run(run.id())
                .unwrap()
                .first()
                .map(|observation| observation.object_key().as_str().to_owned()),
            Some(legacy_key.as_str().to_owned())
        );
    }

    #[test]
    fn every_observed_shape_round_trips() {
        let directory = TestDirectory::new();
        let store = SqliteAssetObservationStore::open(directory.database()).unwrap();
        let run = run(1);
        let exact = asset("img/exact.png", b"exact body");
        let attested = asset("img/attested.png", b"attested body");
        let unavailable = asset("img/unavailable.png", b"unavailable body");
        let shapes = vec![
            (&exact, AssetTargetState::Missing),
            (&exact, present(&exact)),
            (
                &attested,
                AssetTargetState::Present(AssetTargetFacts::new(
                    attested.object_key().clone(),
                    attested.published_size(),
                    attested.published_content_type().clone(),
                    AssetByteIdentity::Attested(attested.published_sha256()),
                )),
            ),
            (
                &unavailable,
                AssetTargetState::Present(AssetTargetFacts::new(
                    unavailable.object_key().clone(),
                    unavailable.published_size(),
                    unavailable.published_content_type().clone(),
                    AssetByteIdentity::Unavailable,
                )),
            ),
        ];

        for (index, (asset, observed)) in shapes.into_iter().enumerate() {
            let entry = observation(index as u64 + 1, &run, asset, observed, 10 + index as u64);
            store.save(&entry).unwrap();
            assert_eq!(store.get(entry.id()).unwrap(), Some(entry));
        }
    }

    #[test]
    fn a_reused_observation_identity_is_rejected() {
        let directory = TestDirectory::new();
        let store = SqliteAssetObservationStore::open(directory.database()).unwrap();
        let run = run(1);
        let first = asset("img/a.png", b"first body");
        let second = asset("img/b.png", b"second body");

        store
            .save(&observation(1, &run, &first, AssetTargetState::Missing, 10))
            .unwrap();

        assert!(matches!(
            store.save(&observation(1, &run, &second, present(&second), 11)),
            Err(SqliteAssetObservationStoreError::ConflictingAssetObservationId(id))
                if id == AssetObservationId::new(1).unwrap()
        ));
    }

    #[test]
    fn an_observation_carries_the_run_and_delivery_it_belongs_to() {
        let directory = TestDirectory::new();
        let store = SqliteAssetObservationStore::open(directory.database()).unwrap();
        let run = run(1);
        let asset = asset("img/a.png", b"body");
        let entry = observation(1, &run, &asset, present(&asset), 10);

        store.save(&entry).unwrap();

        let stored = store.get(entry.id()).unwrap().unwrap();
        assert_eq!(stored.publish_run_id(), run.id());
        assert_eq!(
            stored.delivery_projection_sha256(),
            DeliveryProjectionBinding::new(Sha256::new([3; 32]), Sha256::new([2; 32]))
                .delivery_sha256()
        );
        assert_eq!(stored.logical_path(), asset.logical_path());
        assert_eq!(stored.object_key(), asset.object_key());
        assert_eq!(stored.observed(), &present(&asset));
        assert_eq!(stored.observed_at().as_unix_millis(), 10);
    }

    #[test]
    fn an_observation_about_another_object_is_never_recorded() {
        let directory = TestDirectory::new();
        let store = SqliteAssetObservationStore::open(directory.database()).unwrap();
        let run = run(1);
        let keyed = asset("img/a.png", b"first body");
        let other = asset("img/b.png", b"second body");

        let error = AssetTargetObservation::new(
            AssetObservationId::new(1).unwrap(),
            &run,
            Sha256::new([3; 32]),
            &keyed,
            present(&other),
            TimestampMillis::from_unix_millis(10),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            crate::asset::AssetTargetObservationError::ObservedObjectKeyMismatch { .. }
        ));
        assert!(store.list_for_publish_run(run.id()).unwrap().is_empty());
    }

    #[test]
    fn an_unknown_schema_version_fails_closed() {
        let directory = TestDirectory::new();
        let database = directory.database();
        {
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch("PRAGMA user_version = 99;")
                .unwrap();
        }

        assert!(matches!(
            SqliteAssetObservationStore::open(&database),
            Err(SqliteAssetObservationStoreError::UnsupportedSchemaVersion(
                99
            ))
        ));
    }

    #[test]
    fn the_schema_refuses_a_present_observation_without_facts() {
        let directory = TestDirectory::new();
        let store = SqliteAssetObservationStore::open(directory.database()).unwrap();
        let run = run(1);
        let asset = asset("img/a.png", b"body");

        let error = store
            .connection
            .execute(
                "INSERT INTO asset_target_observations (
                     id, publish_run_id, delivery_projection_sha256, logical_path, object_key,
                     observed_kind, size, content_type, byte_identity_kind, byte_identity,
                     observed_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'present', NULL, NULL, NULL, NULL, ?6)",
                params![
                    1_i64,
                    run.id().get() as i64,
                    [3_u8; 32].as_slice(),
                    asset.logical_path().as_str(),
                    asset.object_key().as_str(),
                    10_i64,
                ],
            )
            .unwrap_err();

        assert!(
            error.to_string().contains("CHECK"),
            "the schema accepted a present observation without facts: {error}"
        );
    }

    #[test]
    fn the_allocator_never_reuses_an_identity() {
        let mut ids =
            SequentialAssetObservationIdGenerator::new(AssetObservationId::new(1).unwrap());
        let first = ids.next_id().unwrap();
        let second = ids.next_id().unwrap();

        assert_ne!(first, second);
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 2);
    }
}
