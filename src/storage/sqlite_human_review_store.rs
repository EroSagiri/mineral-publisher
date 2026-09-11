use std::{error::Error, fmt, path::Path};

use rusqlite::{Connection, OptionalExtension, params, types::Type};

use crate::policy::ReviewRunId;
use crate::workflow::{
    AssetReviewRunId, HumanReviewDecision, HumanReviewId, HumanReviewRecord, HumanReviewStore,
    HumanReviewSubject,
};

const SCHEMA_VERSION: i64 = 1;

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
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE human_review_records (
                         id INTEGER PRIMARY KEY CHECK (id > 0),
                         subject_kind TEXT NOT NULL CHECK (subject_kind IN ('document', 'asset')),
                         subject_run_id INTEGER NOT NULL CHECK (subject_run_id > 0),
                         decision TEXT NOT NULL CHECK (decision IN ('approve', 'reject')),
                         created_at_unix_ms INTEGER NOT NULL CHECK (created_at_unix_ms >= 0),
                         reviewer TEXT CHECK (reviewer IS NULL OR length(trim(reviewer)) > 0),
                         note TEXT,
                         UNIQUE(subject_kind, subject_run_id)
                     );
                     CREATE INDEX human_review_records_order
                         ON human_review_records(subject_kind, subject_run_id, id);
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
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
        let (subject_kind, subject_run_id) = subject_columns(record.subject());
        let inserted = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO human_review_records (
                     id, subject_kind, subject_run_id, decision, created_at_unix_ms, reviewer, note
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    to_sqlite_integer("human review id", record.id().get())?,
                    subject_kind,
                    to_sqlite_integer("automatic review run id", subject_run_id)?,
                    decision_text(record.decision()),
                    to_sqlite_integer("human review timestamp", record.created_at_unix_ms())?,
                    record.reviewer(),
                    record.note(),
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
        if self.get_for_subject(record.subject())?.is_some() {
            return Err(SqliteHumanReviewStoreError::SubjectAlreadyResolved(
                record.subject(),
            ));
        }
        Err(SqliteHumanReviewStoreError::Persistence(
            "human review insert was ignored without a conflicting row".to_owned(),
        ))
    }

    fn get(&self, id: HumanReviewId) -> Result<Option<HumanReviewRecord>, Self::Error> {
        self.connection
            .query_row(
                "SELECT id, subject_kind, subject_run_id, decision,
                        created_at_unix_ms, reviewer, note
                 FROM human_review_records WHERE id = ?1",
                [to_sqlite_integer("human review id", id.get())?],
                row_to_record,
            )
            .optional()
            .map_err(SqliteHumanReviewStoreError::Sqlite)
    }

    fn get_for_subject(
        &self,
        subject: HumanReviewSubject,
    ) -> Result<Option<HumanReviewRecord>, Self::Error> {
        let (kind, run_id) = subject_columns(subject);
        self.connection
            .query_row(
                "SELECT id, subject_kind, subject_run_id, decision,
                        created_at_unix_ms, reviewer, note
                 FROM human_review_records
                 WHERE subject_kind = ?1 AND subject_run_id = ?2",
                params![kind, to_sqlite_integer("automatic review run id", run_id)?],
                row_to_record,
            )
            .optional()
            .map_err(SqliteHumanReviewStoreError::Sqlite)
    }

    fn list(&self) -> Result<Vec<HumanReviewRecord>, Self::Error> {
        self.load_many(
            "SELECT id, subject_kind, subject_run_id, decision,
                    created_at_unix_ms, reviewer, note
             FROM human_review_records
             ORDER BY subject_kind COLLATE BINARY ASC, subject_run_id ASC, id ASC",
        )
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<HumanReviewRecord> {
    let id = HumanReviewId::new(positive_u64(row.get(0)?, 0, "human review id")?)
        .map_err(|error| conversion_error(0, Type::Integer, error.to_string()))?;
    let kind: String = row.get(1)?;
    let run_id = positive_u64(row.get(2)?, 2, "automatic review run id")?;
    let subject = match kind.as_str() {
        "document" => HumanReviewSubject::Document(
            ReviewRunId::new(run_id)
                .map_err(|error| conversion_error(2, Type::Integer, error.to_string()))?,
        ),
        "asset" => HumanReviewSubject::Asset(
            AssetReviewRunId::new(run_id)
                .map_err(|error| conversion_error(2, Type::Integer, error.to_string()))?,
        ),
        _ => {
            return Err(conversion_error(
                1,
                Type::Text,
                "invalid human review subject kind",
            ));
        }
    };
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
    HumanReviewRecord::rehydrate(id, subject, decision, created_at_unix_ms, reviewer, note)
        .map_err(|error| conversion_error(5, Type::Text, error.to_string()))
}

fn subject_columns(subject: HumanReviewSubject) -> (&'static str, u64) {
    match subject {
        HumanReviewSubject::Document(id) => ("document", id.get()),
        HumanReviewSubject::Asset(id) => ("asset", id.get()),
    }
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
    SubjectAlreadyResolved(HumanReviewSubject),
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
                    "human review subject is already resolved: {subject:?}"
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
