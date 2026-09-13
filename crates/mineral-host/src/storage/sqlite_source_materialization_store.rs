use std::{error::Error, fmt, path::Path};

use rusqlite::{Connection, OptionalExtension, params};

use crate::domain::{ContentPath, Sha256};
use mineral_core::source::{SourceIdentity, SourceMaterialization, SourceRevision};

const SCHEMA_VERSION: i64 = 1;

/// Durable record of which remote revisions this engine has already read.
///
/// It answers exactly one question — "have I read *this* revision of *this* path
/// from *this* namespace, and what did the bytes hash to?" — and it is the only
/// thing that lets a later run skip a download. The store never decides whether a
/// binding may be reused; it records and returns facts, and refuses a second,
/// different answer for the same key.
pub struct SqliteSourceMaterializationStore {
    connection: Connection,
}

impl SqliteSourceMaterializationStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqliteSourceMaterializationStoreError> {
        let connection =
            Connection::open(path).map_err(SqliteSourceMaterializationStoreError::Sqlite)?;
        let detected: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(SqliteSourceMaterializationStoreError::Sqlite)?;
        if detected == 0 {
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE source_materializations (
                         source_identity BLOB NOT NULL
                             CHECK (length(source_identity) = 32),
                         logical_path TEXT NOT NULL CHECK (length(logical_path) > 0),
                         source_revision TEXT NOT NULL CHECK (length(source_revision) > 0),
                         content_sha256 BLOB NOT NULL CHECK (length(content_sha256) = 32),
                         content_size INTEGER NOT NULL CHECK (content_size >= 0),
                         PRIMARY KEY (source_identity, logical_path, source_revision)
                     );
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
                .map_err(SqliteSourceMaterializationStoreError::Sqlite)?;
        } else if detected != SCHEMA_VERSION {
            return Err(SqliteSourceMaterializationStoreError::UnsupportedSchemaVersion(detected));
        }
        Ok(Self { connection })
    }

    /// Records one materialization.
    ///
    /// Recording the same fact twice is success; recording a different hash or size
    /// for the same namespace, path and revision fails closed, because the binding
    /// is the only evidence that a later run may skip a download.
    pub fn save(
        &self,
        materialization: &SourceMaterialization,
    ) -> Result<(), SqliteSourceMaterializationStoreError> {
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO source_materializations (
                     source_identity, logical_path, source_revision, content_sha256, content_size
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    materialization.source().as_sha256().as_bytes().to_vec(),
                    materialization.path().as_str(),
                    materialization.revision().encoded(),
                    materialization.content_sha256().as_bytes().to_vec(),
                    integer("content size", materialization.content_size())?,
                ],
            )
            .map_err(SqliteSourceMaterializationStoreError::Sqlite)?;
        if inserted == 1 {
            return Ok(());
        }
        match self.get(
            materialization.source(),
            materialization.path(),
            materialization.revision(),
        )? {
            Some(stored) if stored == *materialization => Ok(()),
            Some(_) => Err(
                SqliteSourceMaterializationStoreError::ConflictingMaterialization {
                    path: materialization.path().clone(),
                    revision: materialization.revision().clone(),
                },
            ),
            None => Err(SqliteSourceMaterializationStoreError::Persistence(
                "materialization insert was ignored without an existing row".to_owned(),
            )),
        }
    }

    /// Reads the binding for exactly one namespace, path and revision.
    pub fn get(
        &self,
        source: SourceIdentity,
        path: &ContentPath,
        revision: &SourceRevision,
    ) -> Result<Option<SourceMaterialization>, SqliteSourceMaterializationStoreError> {
        let row: Option<(Vec<u8>, Vec<u8>, i64)> = self
            .connection
            .query_row(
                "SELECT source_identity, content_sha256, content_size
                 FROM source_materializations
                 WHERE source_identity = ?1 AND logical_path = ?2 AND source_revision = ?3",
                params![
                    source.as_sha256().as_bytes().to_vec(),
                    path.as_str(),
                    revision.encoded(),
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(SqliteSourceMaterializationStoreError::Sqlite)?;

        let Some((stored_source, content_sha256, content_size)) = row else {
            return Ok(None);
        };
        // A stored row is exactly the input that can be damaged, so every column is
        // validated again: a truncated digest or an unusable revision must stop the
        // run, not become a binding that authorizes skipping a download.
        let source_identity = SourceIdentity::rehydrate(sha256_column(stored_source, "source")?);
        let content_sha256 = sha256_column(content_sha256, "content")?;
        let content_size = nonnegative(content_size, "content size")?;

        Ok(Some(SourceMaterialization::new(
            source_identity,
            path.clone(),
            revision.clone(),
            content_sha256,
            content_size,
        )))
    }
}

fn sha256_column(
    bytes: Vec<u8>,
    column: &'static str,
) -> Result<Sha256, SqliteSourceMaterializationStoreError> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| SqliteSourceMaterializationStoreError::DamagedRow(column))?;
    Ok(Sha256::new(bytes))
}

fn nonnegative(
    value: i64,
    column: &'static str,
) -> Result<u64, SqliteSourceMaterializationStoreError> {
    u64::try_from(value).map_err(|_| SqliteSourceMaterializationStoreError::ValueOutOfRange(column))
}

fn integer(field: &'static str, value: u64) -> Result<i64, SqliteSourceMaterializationStoreError> {
    i64::try_from(value).map_err(|_| SqliteSourceMaterializationStoreError::ValueOutOfRange(field))
}

#[derive(Debug)]
pub enum SqliteSourceMaterializationStoreError {
    Sqlite(rusqlite::Error),
    UnsupportedSchemaVersion(i64),
    ValueOutOfRange(&'static str),
    /// A stored column does not carry the shape it must have.
    DamagedRow(&'static str),
    /// One namespace, path and revision already records other bytes.
    ConflictingMaterialization {
        path: ContentPath,
        revision: SourceRevision,
    },
    Persistence(String),
}

impl fmt::Display for SqliteSourceMaterializationStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => {
                write!(
                    formatter,
                    "SQLite source-materialization persistence failed: {error}"
                )
            }
            Self::UnsupportedSchemaVersion(version) => write!(
                formatter,
                "unsupported source-materialization schema version: {version}"
            ),
            Self::ValueOutOfRange(field) => {
                write!(formatter, "{field} is outside SQLite's integer range")
            }
            Self::DamagedRow(column) => {
                write!(
                    formatter,
                    "stored source materialization {column} is damaged"
                )
            }
            Self::ConflictingMaterialization { path, revision } => write!(
                formatter,
                "{path} at revision {revision} already materialized to different bytes"
            ),
            Self::Persistence(message) => formatter.write_str(message),
        }
    }
}

impl Error for SqliteSourceMaterializationStoreError {
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

    use super::*;
    use mineral_core::source::SourceKind;

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mineral-publisher-source-materializations-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn database(&self) -> PathBuf {
            self.0.join("source-materializations.sqlite3")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn source() -> SourceIdentity {
        SourceIdentity::of(
            SourceKind::ObjectStore,
            "https://example.invalid/bucket/vault/",
        )
        .unwrap()
    }

    fn revision(payload: &str) -> SourceRevision {
        SourceRevision::versioned(payload).unwrap()
    }

    fn materialization(
        path: &str,
        payload: &str,
        content: &[u8],
        size: u64,
    ) -> SourceMaterialization {
        SourceMaterialization::new(
            source(),
            ContentPath::new(path).unwrap(),
            revision(payload),
            Sha256::digest(content),
            size,
        )
    }

    #[test]
    fn a_binding_survives_a_restart() {
        let directory = TestDirectory::new();
        let binding = materialization("a.md", "etag=r1", b"hello", 5);
        {
            let store = SqliteSourceMaterializationStore::open(directory.database()).unwrap();
            store.save(&binding).unwrap();
        }

        let reopened = SqliteSourceMaterializationStore::open(directory.database()).unwrap();

        assert_eq!(
            reopened
                .get(source(), binding.path(), binding.revision())
                .unwrap(),
            Some(binding)
        );
    }

    #[test]
    fn saving_the_same_fact_twice_is_idempotent_but_a_different_answer_fails_closed() {
        let directory = TestDirectory::new();
        let store = SqliteSourceMaterializationStore::open(directory.database()).unwrap();
        let binding = materialization("a.md", "etag=r1", b"hello", 5);
        store.save(&binding).unwrap();
        store.save(&binding).unwrap();

        let other_hash = materialization("a.md", "etag=r1", b"other", 5);
        let other_size = materialization("a.md", "etag=r1", b"hello", 6);

        assert!(matches!(
            store.save(&other_hash),
            Err(SqliteSourceMaterializationStoreError::ConflictingMaterialization { .. })
        ));
        assert!(matches!(
            store.save(&other_size),
            Err(SqliteSourceMaterializationStoreError::ConflictingMaterialization { .. })
        ));
    }

    #[test]
    fn historical_revisions_of_one_path_are_kept_apart() {
        let directory = TestDirectory::new();
        let store = SqliteSourceMaterializationStore::open(directory.database()).unwrap();
        let first = materialization("a.md", "etag=r1", b"one", 3);
        let second = materialization("a.md", "etag=r2", b"two", 3);
        store.save(&first).unwrap();
        store.save(&second).unwrap();

        assert_eq!(
            store.get(source(), first.path(), first.revision()).unwrap(),
            Some(first)
        );
        assert_eq!(
            store
                .get(source(), second.path(), second.revision())
                .unwrap(),
            Some(second)
        );
        assert_eq!(
            store
                .get(
                    source(),
                    &ContentPath::new("a.md").unwrap(),
                    &revision("etag=r3")
                )
                .unwrap(),
            None
        );
    }

    #[test]
    fn an_unknown_schema_version_fails_closed() {
        let directory = TestDirectory::new();
        let database = directory.database();
        {
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch("PRAGMA user_version = 7;")
                .unwrap();
        }

        assert!(matches!(
            SqliteSourceMaterializationStore::open(&database),
            Err(SqliteSourceMaterializationStoreError::UnsupportedSchemaVersion(7))
        ));
    }

    #[test]
    fn a_damaged_digest_fails_closed_instead_of_authorizing_a_reuse() {
        let directory = TestDirectory::new();
        let database = directory.database();
        let store = SqliteSourceMaterializationStore::open(&database).unwrap();
        let binding = materialization("a.md", "etag=r1", b"hello", 5);
        store.save(&binding).unwrap();

        // The table's own CHECK rejects a short digest, so the damage a reader must
        // still survive is written with the constraint checks off: a row can be
        // corrupted outside this store (a restored backup, a hand-edited file), and
        // the reader is the last line of defence.
        store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE source_materializations SET content_sha256 = ?1",
                params![vec![0_u8; 8]],
            )
            .unwrap();

        assert!(matches!(
            store.get(source(), binding.path(), binding.revision()),
            Err(SqliteSourceMaterializationStoreError::DamagedRow("content"))
        ));
    }

    /// A row written under another revision encoding can never be read as a
    /// binding for the encoding this engine asks with.
    #[test]
    fn a_binding_from_another_revision_encoding_is_not_reinterpreted() {
        let directory = TestDirectory::new();
        let store = SqliteSourceMaterializationStore::open(directory.database()).unwrap();
        let binding = materialization("a.md", "etag=r1", b"hello", 5);
        store.save(&binding).unwrap();

        store
            .connection
            .execute(
                "UPDATE source_materializations SET source_revision = 'v9:etag=r1'",
                [],
            )
            .unwrap();

        assert_eq!(
            store
                .get(source(), binding.path(), binding.revision())
                .unwrap(),
            None
        );
    }
}
