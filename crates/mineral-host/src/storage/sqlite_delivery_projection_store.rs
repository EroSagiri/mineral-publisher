use std::{error::Error, fmt, path::Path};

use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    domain::Sha256,
    workflow::{
        DeliveryProjection, DeliveryProjectionStore, DeliveryProjectionWire,
        DeliveryProjectionWireError,
    },
};

const SCHEMA_VERSION: i64 = 1;

/// Local SQLite persistence for immutable delivery projections.
///
/// This is a store of its own, like every other durable Mineral fact: one file,
/// one table, one schema version. A projection is content-addressed, so the row
/// key is the projection's own canonical identity and the payload can be checked
/// against it on every read.
pub struct SqliteDeliveryProjectionStore {
    connection: Connection,
}

impl SqliteDeliveryProjectionStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqliteDeliveryProjectionStoreError> {
        let connection =
            Connection::open(path).map_err(SqliteDeliveryProjectionStoreError::Sqlite)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(SqliteDeliveryProjectionStoreError::Sqlite)?;
        let detected: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(SqliteDeliveryProjectionStoreError::Sqlite)?;
        let mut version = detected;
        if version == 0 {
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE delivery_projections (
                         delivery_sha256 BLOB PRIMARY KEY
                             CHECK (length(delivery_sha256) = 32),
                         wire TEXT NOT NULL
                     );
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
                .map_err(SqliteDeliveryProjectionStoreError::Sqlite)?;
            version = SCHEMA_VERSION;
        }
        if version != SCHEMA_VERSION {
            return Err(SqliteDeliveryProjectionStoreError::UnsupportedSchemaVersion(detected));
        }
        Ok(Self { connection })
    }

    /// Reads the exact projection a key names, or `None` when it was never saved.
    fn load(
        &self,
        id: Sha256,
    ) -> Result<Option<DeliveryProjection>, SqliteDeliveryProjectionStoreError> {
        let wire: Option<String> = self
            .connection
            .query_row(
                "SELECT wire FROM delivery_projections WHERE delivery_sha256 = ?1",
                [id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(SqliteDeliveryProjectionStoreError::Sqlite)?;
        let Some(wire) = wire else {
            return Ok(None);
        };
        let projection = DeliveryProjectionWire::decode(&wire).map_err(|source| {
            SqliteDeliveryProjectionStoreError::Corrupt {
                id,
                source: Box::new(source),
            }
        })?;
        // The row key is a claim; the payload has to prove it. A database where
        // `key = HASH_A` and `payload = HASH_B` decodes to nothing usable.
        if projection.delivery_sha256() != id {
            return Err(SqliteDeliveryProjectionStoreError::IdentityMismatch {
                requested: id,
                stored: projection.delivery_sha256(),
            });
        }
        Ok(Some(projection))
    }
}

impl DeliveryProjectionStore for SqliteDeliveryProjectionStore {
    type Error = SqliteDeliveryProjectionStoreError;

    fn save(&self, projection: &DeliveryProjection) -> Result<(), Self::Error> {
        let id = projection.delivery_sha256();
        let wire = DeliveryProjectionWire::encode(projection);
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO delivery_projections (delivery_sha256, wire)
                 VALUES (?1, ?2)",
                params![id.as_bytes().as_slice(), wire],
            )
            .map_err(SqliteDeliveryProjectionStoreError::Sqlite)?;
        if inserted == 1 {
            return Ok(());
        }
        // Re-saving the same immutable intent is idempotent; anything else under an
        // existing identity is a conflict this store must report, never resolve.
        match self.load(id)? {
            Some(stored) if stored == *projection => Ok(()),
            Some(_) => Err(SqliteDeliveryProjectionStoreError::ConflictingDeliveryProjection(id)),
            None => Err(SqliteDeliveryProjectionStoreError::Persistence(
                "delivery projection insert was ignored without an existing row".to_owned(),
            )),
        }
    }

    fn get(&self, id: Sha256) -> Result<Option<DeliveryProjection>, Self::Error> {
        self.load(id)
    }
}

#[derive(Debug)]
pub enum SqliteDeliveryProjectionStoreError {
    Sqlite(rusqlite::Error),
    UnsupportedSchemaVersion(i64),
    /// The same identity already stores a different delivery intent.
    ConflictingDeliveryProjection(Sha256),
    /// The stored payload's own identity is not the key it was stored under.
    IdentityMismatch {
        requested: Sha256,
        stored: Sha256,
    },
    /// The stored payload is not a delivery projection this engine can read.
    Corrupt {
        id: Sha256,
        source: Box<DeliveryProjectionWireError>,
    },
    Persistence(String),
}

impl fmt::Display for SqliteDeliveryProjectionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => {
                write!(
                    formatter,
                    "SQLite delivery-projection persistence failed: {error}"
                )
            }
            Self::UnsupportedSchemaVersion(version) => write!(
                formatter,
                "unsupported delivery-projection schema version: {version}"
            ),
            Self::ConflictingDeliveryProjection(id) => write!(
                formatter,
                "delivery projection {id} already stores a different intent"
            ),
            Self::IdentityMismatch { requested, stored } => write!(
                formatter,
                "stored delivery projection claims identity {stored} but is stored under {requested}"
            ),
            Self::Corrupt { id, .. } => {
                write!(formatter, "stored delivery projection is unreadable: {id}")
            }
            Self::Persistence(message) => formatter.write_str(message),
        }
    }
}

impl Error for SqliteDeliveryProjectionStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Corrupt { source, .. } => Some(source.as_ref()),
            Self::UnsupportedSchemaVersion(_)
            | Self::ConflictingDeliveryProjection(_)
            | Self::IdentityMismatch { .. }
            | Self::Persistence(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::SystemTime,
    };

    use crate::{
        domain::{ContentPath, Snapshot, SnapshotFile, SnapshotId, SourceId},
        ports::{BlobStore, ContentStoreError},
        workflow::{
            AssetContentType, AssetDeliveryConfig, AssetReviewRunId, DeliveryProjectionBuilder,
            FinalPublicationSet, ManagedRoot, PublicProjection, SanitizationTransformation,
            SanitizedAsset,
        },
    };

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-delivery-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn database(&self) -> PathBuf {
            self.0.join("delivery-projections.sqlite3")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct MemoryStore(HashMap<Sha256, Vec<u8>>);

    impl MemoryStore {
        fn insert(&mut self, bytes: &[u8]) -> Sha256 {
            let identity = Sha256::digest(bytes);
            self.0.insert(identity, bytes.to_vec());
            identity
        }
    }

    impl BlobStore for MemoryStore {
        fn read(&self, identity: Sha256) -> Result<Vec<u8>, ContentStoreError> {
            self.0
                .get(&identity)
                .cloned()
                .ok_or(ContentStoreError::Missing(identity))
        }

        fn store(&self, content: &[u8]) -> Result<Sha256, ContentStoreError> {
            Ok(Sha256::digest(content))
        }
    }

    fn path(value: &str) -> ContentPath {
        ContentPath::new(value).unwrap()
    }

    /// A real projection built through the production builder, with one rewritten
    /// document and one published asset.
    fn projection(document: &str, published_seed: u8) -> DeliveryProjection {
        projection_with_unpublished(document, published_seed, None)
    }

    /// The same delivery, optionally published from a snapshot that also contains a
    /// file nothing references.
    fn projection_with_unpublished(
        document: &str,
        published_seed: u8,
        unpublished: Option<&str>,
    ) -> DeliveryProjection {
        let mut store = MemoryStore::default();
        let mut files = vec![SnapshotFile::new(
            path("a.md"),
            document.len() as u64,
            store.insert(document.as_bytes()),
            None,
        )];
        files.push(SnapshotFile::new(
            path("img/a.png"),
            100,
            Sha256::new([published_seed; 32]),
            None,
        ));
        if let Some(unpublished) = unpublished {
            files.push(SnapshotFile::new(
                path("unrelated.txt"),
                unpublished.len() as u64,
                store.insert(unpublished.as_bytes()),
                None,
            ));
        }
        // A snapshot identity is derived from the file set it captured, so a vault
        // that also holds an unrelated file captures a different one.
        let snapshot_id = if unpublished.is_some() {
            SnapshotId::new(8).unwrap()
        } else {
            SnapshotId::new(7).unwrap()
        };
        let snapshot = Snapshot::new(
            snapshot_id,
            SystemTime::UNIX_EPOCH,
            SourceId::new("test").unwrap(),
            files,
        )
        .unwrap();
        let set = FinalPublicationSet::from_parts_for_test(
            snapshot.id(),
            vec![path("a.md")],
            vec![SanitizedAsset::from_parts(
                path("img/a.png"),
                AssetReviewRunId::new(1).unwrap(),
                Sha256::new([published_seed; 32]),
                Sha256::new([published_seed; 32]),
                100,
                AssetContentType::new("image/png").unwrap(),
                vec![SanitizationTransformation::Identity],
            )],
        );
        let public =
            PublicProjection::build(&set, &snapshot, ManagedRoot::new("content").unwrap()).unwrap();
        DeliveryProjectionBuilder::build(
            &public,
            &snapshot,
            &AssetDeliveryConfig::new("https://assets.example.com").unwrap(),
            &store,
        )
        .unwrap()
    }

    #[test]
    fn a_projection_survives_a_restart_and_is_idempotent_to_save() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let projection = projection("![[img/a.png]]\n", 2);
        let store = SqliteDeliveryProjectionStore::open(&database).unwrap();
        store.save(&projection).unwrap();
        store.save(&projection).unwrap();
        drop(store);

        let reopened = SqliteDeliveryProjectionStore::open(&database).unwrap();
        assert_eq!(
            reopened.get(projection.delivery_sha256()).unwrap(),
            Some(projection)
        );
        assert_eq!(reopened.get(Sha256::new([0xaa; 32])).unwrap(), None);
    }

    /// The identity keys the payload, so two intents that publish the same bytes
    /// from different snapshots are two rows — not a conflict.
    ///
    /// A real workspace hit the opposite: adding an unrelated file to the vault
    /// changed the snapshot but not the delivered content, the identity did not
    /// cover the snapshot, and the second publication could not be persisted at all.
    #[test]
    fn two_snapshots_of_the_same_delivery_are_two_intents() {
        let directory = TestDirectory::new();
        let store = SqliteDeliveryProjectionStore::open(directory.database()).unwrap();
        let first = projection("body", 9);
        let second = projection_with_unpublished("body", 9, Some("an unrelated file"));

        assert_ne!(
            first.snapshot_id(),
            second.snapshot_id(),
            "the snapshot really is a different one"
        );
        assert_eq!(
            first.text().projection_sha256(),
            second.text().projection_sha256(),
            "the delivered text tree is identical"
        );
        assert_eq!(
            first.assets().assets(),
            second.assets().assets(),
            "the delivered assets are identical"
        );
        assert_ne!(
            first.delivery_sha256(),
            second.delivery_sha256(),
            "different audited inputs are different intents"
        );

        store.save(&first).unwrap();
        store.save(&second).unwrap();
        assert_eq!(
            store.get(first.delivery_sha256()).unwrap(),
            Some(first.clone())
        );
        assert_eq!(store.get(second.delivery_sha256()).unwrap(), Some(second));
        // Saving either one again is still idempotent.
        store.save(&first).unwrap();
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
            SqliteDeliveryProjectionStore::open(&database),
            Err(SqliteDeliveryProjectionStoreError::UnsupportedSchemaVersion(99))
        ));
    }

    /// The key is a claim: a row whose payload describes another projection is
    /// refused instead of being trusted because of its primary key.
    #[test]
    fn a_row_whose_payload_is_another_projection_fails_closed() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let stored = projection("![[img/a.png]]\n", 2);
        let other = projection("![[img/a.png]]\n![[img/a.png]]\n", 3);
        let store = SqliteDeliveryProjectionStore::open(&database).unwrap();
        store.save(&other).unwrap();
        // Store the other projection's wire under this projection's identity.
        store
            .connection
            .execute(
                "INSERT INTO delivery_projections (delivery_sha256, wire) VALUES (?1, ?2)",
                params![
                    stored.delivery_sha256().as_bytes().as_slice(),
                    DeliveryProjectionWire::encode(&other)
                ],
            )
            .unwrap();

        assert!(matches!(
            store.get(stored.delivery_sha256()),
            Err(SqliteDeliveryProjectionStoreError::IdentityMismatch { .. })
        ));
        // Saving a different intent under an existing identity is a conflict.
        assert!(matches!(
            store.save(&stored),
            Err(SqliteDeliveryProjectionStoreError::IdentityMismatch { .. })
        ));
    }

    #[test]
    fn an_unreadable_stored_payload_fails_closed() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let projection = projection("![[img/a.png]]\n", 2);
        let store = SqliteDeliveryProjectionStore::open(&database).unwrap();
        store.save(&projection).unwrap();
        store
            .connection
            .execute(
                "UPDATE delivery_projections SET wire = '{\"version\":1}' WHERE delivery_sha256 = ?1",
                [projection.delivery_sha256().as_bytes().as_slice()],
            )
            .unwrap();

        assert!(matches!(
            store.get(projection.delivery_sha256()),
            Err(SqliteDeliveryProjectionStoreError::Corrupt { .. })
        ));
        // Re-saving the same intent cannot "repair" or silently replace it.
        assert!(matches!(
            store.save(&projection),
            Err(SqliteDeliveryProjectionStoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn a_payload_stored_under_another_identity_cannot_be_saved_over() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let first = projection("![[img/a.png]]\n", 2);
        let second = projection("![[img/a.png]]\n![[img/a.png]]\n", 3);
        let store = SqliteDeliveryProjectionStore::open(&database).unwrap();
        store.save(&second).unwrap();
        store
            .connection
            .execute(
                "UPDATE delivery_projections SET wire = ?1 WHERE delivery_sha256 = ?2",
                params![
                    DeliveryProjectionWire::encode(&first),
                    second.delivery_sha256().as_bytes().as_slice()
                ],
            )
            .unwrap();

        // The row key still says `second`, but the payload proves `first`.
        assert!(matches!(
            store.save(&second),
            Err(SqliteDeliveryProjectionStoreError::IdentityMismatch { .. })
        ));
        assert!(matches!(
            store.get(second.delivery_sha256()),
            Err(SqliteDeliveryProjectionStoreError::IdentityMismatch { .. })
        ));
    }
}
