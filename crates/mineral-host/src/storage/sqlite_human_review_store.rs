use std::{error::Error, fmt, path::Path};

use rusqlite::{Connection, OptionalExtension, params, types::Type};

use crate::domain::{ContentPath, Sha256};
use crate::policy::{PolicyIdentity, ReviewRunId};
use crate::workflow::{
    AssetReviewRunId, HumanReviewAttempt, HumanReviewBinding, HumanReviewDecision, HumanReviewId,
    HumanReviewKind, HumanReviewRecord, HumanReviewStore, HumanReviewSubject,
};

const SCHEMA_VERSION: i64 = 2;

/// The columns every row carries, in the order [`row_to_record`] reads them.
const RECORD_COLUMNS: &str = "id, subject_kind, subject_run_id, decision, created_at_unix_ms,
     reviewer, note, content_path, content_sha256, policy_name, policy_version, policy_hash";

pub struct SqliteHumanReviewStore {
    connection: Connection,
}

impl SqliteHumanReviewStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqliteHumanReviewStoreError> {
        let connection = Connection::open(path).map_err(SqliteHumanReviewStoreError::Sqlite)?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(SqliteHumanReviewStoreError::Sqlite)?;
        match version {
            0 => connection
                .execute_batch(&format!(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE human_review_records (
                         id INTEGER PRIMARY KEY CHECK (id > 0),
                         subject_kind TEXT NOT NULL CHECK (subject_kind IN ('document', 'asset')),
                         subject_run_id INTEGER NOT NULL CHECK (subject_run_id > 0),
                         decision TEXT NOT NULL CHECK (decision IN ('approve', 'reject')),
                         created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
                         reviewer TEXT CHECK (reviewer IS NULL OR length(trim(reviewer)) > 0),
                         note TEXT,
                         content_path TEXT,
                         content_sha256 BLOB,
                         policy_name TEXT,
                         policy_version TEXT,
                         policy_hash BLOB,
                         UNIQUE(subject_kind, subject_run_id),
                         CHECK (
                             (content_path IS NULL AND content_sha256 IS NULL
                              AND policy_name IS NULL AND policy_version IS NULL
                              AND policy_hash IS NULL)
                             OR (content_path IS NOT NULL AND content_sha256 IS NOT NULL
                                 AND policy_name IS NOT NULL AND policy_version IS NOT NULL
                                 AND policy_hash IS NOT NULL)
                         )
                     );
                     CREATE INDEX human_review_records_order
                         ON human_review_records(subject_kind, subject_run_id, id);
                     CREATE UNIQUE INDEX human_review_records_subject
                         ON human_review_records(
                             subject_kind, content_path, content_sha256,
                             policy_name, policy_version, policy_hash
                         )
                         WHERE content_path IS NOT NULL;
                     PRAGMA user_version = {SCHEMA_VERSION};
                     COMMIT;"
                ))
                .map_err(SqliteHumanReviewStoreError::Sqlite)?,
            // Version 1 bound every decision to one automatic attempt and nothing
            // else. Those rows stay exactly as they were written: their author
            // decided about that attempt, so the new columns stay NULL and the
            // binding still says exactly that. Only rows written from now on carry
            // a subject.
            1 => connection
                .execute_batch(&format!(
                    "BEGIN IMMEDIATE;
                     ALTER TABLE human_review_records ADD COLUMN content_path TEXT;
                     ALTER TABLE human_review_records ADD COLUMN content_sha256 BLOB;
                     ALTER TABLE human_review_records ADD COLUMN policy_name TEXT;
                     ALTER TABLE human_review_records ADD COLUMN policy_version TEXT;
                     ALTER TABLE human_review_records ADD COLUMN policy_hash BLOB;
                     CREATE UNIQUE INDEX human_review_records_subject
                         ON human_review_records(
                             subject_kind, content_path, content_sha256,
                             policy_name, policy_version, policy_hash
                         )
                         WHERE content_path IS NOT NULL;
                     PRAGMA user_version = {SCHEMA_VERSION};
                     COMMIT;"
                ))
                .map_err(SqliteHumanReviewStoreError::Sqlite)?,
            SCHEMA_VERSION => {}
            version => {
                return Err(SqliteHumanReviewStoreError::UnsupportedSchemaVersion(
                    version,
                ));
            }
        }
        Ok(Self { connection })
    }

    fn load_many(&self, sql: &str) -> Result<Vec<HumanReviewRecord>, SqliteHumanReviewStoreError> {
        let mut statement = self
            .connection
            .prepare(sql)
            .map_err(SqliteHumanReviewStoreError::Sqlite)?;
        let rows = statement
            .query_map([], row_to_record)
            .map_err(SqliteHumanReviewStoreError::Sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SqliteHumanReviewStoreError::Sqlite)
    }
}

impl HumanReviewStore for SqliteHumanReviewStore {
    type Error = SqliteHumanReviewStoreError;

    fn save(&self, record: &HumanReviewRecord) -> Result<(), Self::Error> {
        let attempt = attempt_of(record.binding());
        let identity = record.subject().map(|subject| subject.identity());
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO human_review_records (
                     id, subject_kind, subject_run_id, decision, created_at_unix_ms, reviewer, note,
                     content_path, content_sha256, policy_name, policy_version, policy_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    to_sqlite_integer("human review id", record.id().get())?,
                    kind_text(attempt.kind()),
                    to_sqlite_integer("automatic review run id", attempt.run_id())?,
                    decision_text(record.decision()),
                    to_sqlite_integer("human review timestamp", record.created_at_unix_ms())?,
                    record.reviewer(),
                    record.note(),
                    identity.map(|identity| identity.content_path().as_str()),
                    identity.map(|identity| identity.content_sha256().as_bytes().to_vec()),
                    identity.map(|identity| identity.policy().name()),
                    identity.map(|identity| identity.policy().version()),
                    identity.map(|identity| identity.policy().hash().as_bytes().to_vec()),
                ],
            )
            .map_err(SqliteHumanReviewStoreError::Sqlite)?;
        if inserted == 1 {
            return Ok(());
        }
        if let Some(stored) = self.get(record.id())? {
            return if stored == *record {
                Ok(())
            } else {
                Err(SqliteHumanReviewStoreError::ConflictingHumanReviewId(
                    record.id(),
                ))
            };
        }
        if let Some(subject) = record.subject()
            && self.get_for_subject(subject)?.is_some()
        {
            return Err(SqliteHumanReviewStoreError::SubjectAlreadyResolved(
                Box::new(subject.clone()),
            ));
        }
        Err(SqliteHumanReviewStoreError::Persistence(
            "human review insert was ignored without a conflicting row".to_owned(),
        ))
    }

    fn get(&self, id: HumanReviewId) -> Result<Option<HumanReviewRecord>, Self::Error> {
        self.connection
            .query_row(
                &format!("SELECT {RECORD_COLUMNS} FROM human_review_records WHERE id = ?1"),
                [to_sqlite_integer("human review id", id.get())?],
                row_to_record,
            )
            .optional()
            .map_err(SqliteHumanReviewStoreError::Sqlite)
    }

    fn get_for_subject(
        &self,
        subject: &HumanReviewSubject,
    ) -> Result<Option<HumanReviewRecord>, Self::Error> {
        let identity = subject.identity();
        self.connection
            .query_row(
                &format!(
                    "SELECT {RECORD_COLUMNS} FROM human_review_records
                     WHERE subject_kind = ?1 AND content_path = ?2 AND content_sha256 = ?3
                       AND policy_name = ?4 AND policy_version = ?5 AND policy_hash = ?6"
                ),
                params![
                    kind_text(subject.kind()),
                    identity.content_path().as_str(),
                    identity.content_sha256().as_bytes().to_vec(),
                    identity.policy().name(),
                    identity.policy().version(),
                    identity.policy().hash().as_bytes().to_vec(),
                ],
                row_to_record,
            )
            .optional()
            .map_err(SqliteHumanReviewStoreError::Sqlite)
    }

    fn get_for_attempt(
        &self,
        attempt: HumanReviewAttempt,
    ) -> Result<Option<HumanReviewRecord>, Self::Error> {
        // Only rows written before subjects were bound carry no identity: a decision
        // taken today is found through its subject, never through the attempt that
        // happened to prompt it.
        self.connection
            .query_row(
                &format!(
                    "SELECT {RECORD_COLUMNS} FROM human_review_records
                     WHERE content_path IS NULL AND subject_kind = ?1 AND subject_run_id = ?2"
                ),
                params![
                    kind_text(attempt.kind()),
                    to_sqlite_integer("automatic review run id", attempt.run_id())?,
                ],
                row_to_record,
            )
            .optional()
            .map_err(SqliteHumanReviewStoreError::Sqlite)
    }

    fn list(&self) -> Result<Vec<HumanReviewRecord>, Self::Error> {
        self.load_many(&format!(
            "SELECT {RECORD_COLUMNS} FROM human_review_records
             ORDER BY subject_kind COLLATE BINARY ASC, subject_run_id ASC, id ASC"
        ))
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<HumanReviewRecord> {
    let id = HumanReviewId::new(positive_u64(row.get(0)?, 0, "human review id")?)
        .map_err(|error| conversion_error(0, Type::Integer, error.to_string()))?;
    let kind = kind_from_text(&row.get::<_, String>(1)?)?
        .ok_or_else(|| conversion_error(1, Type::Text, "invalid human review subject kind"))?;
    let run_id = positive_u64(row.get(2)?, 2, "automatic review run id")?;
    let attempt = attempt_from(kind, run_id)
        .map_err(|error| conversion_error(2, Type::Integer, error.to_string()))?;
    let decision = match row.get::<_, String>(3)?.as_str() {
        "approve" => HumanReviewDecision::Approve,
        "reject" => HumanReviewDecision::Reject,
        _ => {
            return Err(conversion_error(
                3,
                Type::Text,
                "invalid human review decision",
            ));
        }
    };
    let created_at_unix_ms = nonnegative_u64(row.get(4)?, 4, "human review timestamp")?;
    let reviewer = row.get(5)?;
    let note = row.get(6)?;

    let content_path: Option<String> = row.get(7)?;
    let binding = match content_path {
        None => HumanReviewBinding::AttemptOnly(attempt),
        Some(content_path) => {
            let content_path = ContentPath::new(content_path)
                .map_err(|error| conversion_error(7, Type::Text, error.to_string()))?;
            let content_sha256 = sha256_column(row, 8, "reviewed content identity")?;
            let policy_name: String = row.get(9)?;
            let policy_version: String = row.get(10)?;
            let policy_hash = sha256_column(row, 11, "review policy identity")?;
            let policy = PolicyIdentity::new(policy_name, policy_version, policy_hash)
                .map_err(|error| conversion_error(9, Type::Text, error.to_string()))?;
            HumanReviewBinding::Subject {
                subject: HumanReviewSubject::for_path(kind, content_path, content_sha256, policy),
                attempt,
            }
        }
    };
    HumanReviewRecord::rehydrate(id, binding, decision, created_at_unix_ms, reviewer, note)
        .map_err(|error| conversion_error(5, Type::Text, error.to_string()))
}

fn attempt_of(binding: &HumanReviewBinding) -> HumanReviewAttempt {
    match binding {
        HumanReviewBinding::Subject { attempt, .. } => *attempt,
        HumanReviewBinding::AttemptOnly(attempt) => *attempt,
    }
}

fn attempt_from(kind: HumanReviewKind, run_id: u64) -> Result<HumanReviewAttempt, String> {
    match kind {
        HumanReviewKind::Document => ReviewRunId::new(run_id)
            .map(HumanReviewAttempt::Document)
            .map_err(|error| error.to_string()),
        HumanReviewKind::Asset => AssetReviewRunId::new(run_id)
            .map(HumanReviewAttempt::Asset)
            .map_err(|error| error.to_string()),
    }
}

fn kind_text(kind: HumanReviewKind) -> &'static str {
    match kind {
        HumanReviewKind::Document => "document",
        HumanReviewKind::Asset => "asset",
    }
}

fn kind_from_text(value: &str) -> rusqlite::Result<Option<HumanReviewKind>> {
    Ok(match value {
        "document" => Some(HumanReviewKind::Document),
        "asset" => Some(HumanReviewKind::Asset),
        _ => None,
    })
}

fn sha256_column(row: &rusqlite::Row<'_>, column: usize, field: &str) -> rusqlite::Result<Sha256> {
    let bytes: Vec<u8> = row.get(column)?;
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| conversion_error(column, Type::Blob, format!("{field} must be 32 bytes")))?;
    Ok(Sha256::new(bytes))
}

fn decision_text(decision: HumanReviewDecision) -> &'static str {
    match decision {
        HumanReviewDecision::Approve => "approve",
        HumanReviewDecision::Reject => "reject",
    }
}

fn to_sqlite_integer(field: &'static str, value: u64) -> Result<i64, SqliteHumanReviewStoreError> {
    value
        .try_into()
        .map_err(|_| SqliteHumanReviewStoreError::ValueOutOfRange(field))
}

fn positive_u64(value: i64, column: usize, field: &str) -> rusqlite::Result<u64> {
    let value = nonnegative_u64(value, column, field)?;
    if value == 0 {
        return Err(conversion_error(
            column,
            Type::Integer,
            format!("{field} must be positive"),
        ));
    }
    Ok(value)
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

fn conversion_error(column: usize, data_type: Type, message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        data_type,
        Box::new(CorruptHumanReview(message.into())),
    )
}

#[derive(Debug)]
struct CorruptHumanReview(String);

impl fmt::Display for CorruptHumanReview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CorruptHumanReview {}

#[derive(Debug)]
pub enum SqliteHumanReviewStoreError {
    Sqlite(rusqlite::Error),
    UnsupportedSchemaVersion(i64),
    ConflictingHumanReviewId(HumanReviewId),
    /// Boxed: the subject is the largest fact here and every store call returns this
    /// error type.
    SubjectAlreadyResolved(Box<HumanReviewSubject>),
    ValueOutOfRange(&'static str),
    Persistence(String),
}

impl fmt::Display for SqliteHumanReviewStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => {
                write!(formatter, "SQLite human-review persistence failed: {error}")
            }
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "unsupported human-review schema version: {version}"
                )
            }
            Self::ConflictingHumanReviewId(id) => write!(
                formatter,
                "human review id {} already stores a different fact",
                id.get()
            ),
            Self::SubjectAlreadyResolved(subject) => {
                write!(
                    formatter,
                    "human review subject is already resolved: {} ({:?})",
                    subject.content_path(),
                    subject.kind()
                )
            }
            Self::ValueOutOfRange(field) => {
                write!(formatter, "{field} is outside SQLite's integer range")
            }
            Self::Persistence(message) => formatter.write_str(message),
        }
    }
}

impl Error for SqliteHumanReviewStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}
