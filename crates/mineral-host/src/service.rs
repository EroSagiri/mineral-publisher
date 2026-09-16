//! Durable daily scheduling and execution history for the single workspace host.
use crate::{
    operations::{
        OperationErrorCode, OperationExecutor, OperationFailure, OperationRequest, OperationResult,
        OperationSupervisor,
    },
    runtime::Progress,
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

type ServiceResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub publish_at: Option<String>,
    pub backup_at: Option<String>,
    pub utc_offset_minutes: i32,
    pub token_env: String,
    pub secure_cookie: bool,
}
impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            publish_at: None,
            backup_at: None,
            utc_offset_minutes: 480,
            token_env: "MINERAL_WEB_TOKEN".into(),
            secure_cookie: false,
        }
    }
}
impl DaemonConfig {
    pub fn validate(&self) -> Result<(), String> {
        for at in [&self.publish_at, &self.backup_at].into_iter().flatten() {
            minute(at)?;
        }
        if !(-720..=840).contains(&self.utc_offset_minutes) {
            return Err("daemon.utc_offset_minutes must be between -720 and 840".into());
        }
        if self.token_env.is_empty() {
            return Err("daemon.token_env must name an environment variable".into());
        }
        Ok(())
    }
}
fn minute(at: &str) -> Result<i64, String> {
    let b = at.as_bytes();
    if b.len() != 5 || b[2] != b':' || ![b[0], b[1], b[3], b[4]].iter().all(u8::is_ascii_digit) {
        return Err("daily time must be HH:MM".into());
    }
    let h: i64 = at[..2].parse().unwrap();
    let m: i64 = at[3..].parse().unwrap();
    if h > 23 || m > 59 {
        return Err("daily time must be 00:00 through 23:59".into());
    }
    Ok(h * 60 + m)
}
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub kind: String,
    pub at: String,
    pub enabled: bool,
    pub utc_offset_minutes: i32,
}
impl Schedule {
    pub fn validate(&self) -> Result<(), String> {
        if self.kind != "publish" && self.kind != "backup" {
            return Err("schedule kind must be publish or backup".into());
        }
        minute(&self.at)?;
        if !(-720..=840).contains(&self.utc_offset_minutes) {
            return Err("UTC offset must be -720..840 minutes".into());
        }
        Ok(())
    }
    pub fn slot(&self, now: i64) -> Option<i64> {
        let local = now / 1000 + self.utc_offset_minutes as i64 * 60;
        let day = local.div_euclid(86400);
        (self.enabled && local.rem_euclid(86400) >= minute(&self.at).ok()? * 60).then_some(day)
    }
    pub fn next_at(&self, now: i64) -> i64 {
        let local = now / 1000 + self.utc_offset_minutes as i64 * 60;
        let mut next = local.div_euclid(86400) * 86400 + minute(&self.at).unwrap_or(0) * 60;
        if next <= local {
            next += 86400;
        }
        (next - self.utc_offset_minutes as i64 * 60) * 1000
    }
}

pub struct ServiceStore {
    db: Mutex<Connection>,
}
impl ServiceStore {
    pub fn open(path: &Path, config: &DaemonConfig) -> ServiceResult<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS schedules(kind TEXT PRIMARY KEY, body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS slots(kind TEXT NOT NULL, day INTEGER NOT NULL, state TEXT NOT NULL, operation TEXT, PRIMARY KEY(kind,day));
            CREATE TABLE IF NOT EXISTS history(id TEXT PRIMARY KEY, kind TEXT NOT NULL, started INTEGER NOT NULL, finished INTEGER, state TEXT NOT NULL, outcome TEXT);
            CREATE TABLE IF NOT EXISTS steps(sequence INTEGER PRIMARY KEY AUTOINCREMENT, run TEXT NOT NULL, at INTEGER NOT NULL, kind TEXT NOT NULL, message TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS steps_run ON steps(run,sequence);")?;
        db.execute(
            "UPDATE history SET state='interrupted',finished=?1 WHERE state='running'",
            [now_ms()],
        )?;
        db.execute(
            "UPDATE slots SET state='interrupted' WHERE state='claimed'",
            [],
        )?;
        for (kind, at) in [
            ("publish", &config.publish_at),
            ("backup", &config.backup_at),
        ] {
            let s = Schedule {
                kind: kind.into(),
                at: at.clone().unwrap_or_else(|| "03:00".into()),
                enabled: at.is_some(),
                utc_offset_minutes: config.utc_offset_minutes,
            };
            db.execute(
                "INSERT OR IGNORE INTO schedules VALUES(?1,?2)",
                params![kind, serde_json::to_string(&s)?],
            )?;
        }
        Ok(Self { db: Mutex::new(db) })
    }
    pub fn schedules(&self) -> ServiceResult<Vec<Schedule>> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT body FROM schedules ORDER BY kind")?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn save_schedule(&self, s: &Schedule) -> ServiceResult<()> {
        s.validate()?;
        self.db.lock().unwrap().execute(
            "INSERT OR REPLACE INTO schedules VALUES(?1,?2)",
            params![s.kind, serde_json::to_string(s)?],
        )?;
        Ok(())
    }
    pub fn claim(&self, kind: &str, day: i64) -> ServiceResult<bool> {
        Ok(self.db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO slots(kind,day,state) VALUES(?1,?2,'claimed')",
            params![kind, day],
        )? == 1)
    }
    pub fn dispatched(&self, kind: &str, day: i64, operation: &str) -> ServiceResult<()> {
        self.db.lock().unwrap().execute(
            "UPDATE slots SET state='dispatched', operation=?3 WHERE kind=?1 AND day=?2",
            params![kind, day, operation],
        )?;
        Ok(())
    }
    pub fn release(&self, kind: &str, day: i64) -> ServiceResult<()> {
        self.db.lock().unwrap().execute(
            "DELETE FROM slots WHERE kind=?1 AND day=?2 AND state='claimed'",
            params![kind, day],
        )?;
        Ok(())
    }
    pub fn slots(&self) -> ServiceResult<Value> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare(
            "SELECT kind,day,state,operation FROM slots ORDER BY day DESC,kind LIMIT 100",
        )?;
        let rows = q.query_map([], |row| {
            Ok(json!({
                "kind": row.get::<_, String>(0)?,
                "day": row.get::<_, i64>(1)?,
                "state": row.get::<_, String>(2)?,
                "operation": row.get::<_, Option<String>>(3)?,
            }))
        })?;
        Ok(Value::Array(rows.collect::<Result<Vec<_>, _>>()?))
    }
    pub fn history(&self) -> ServiceResult<Value> {
        let db = self.db.lock().unwrap();
        let mut q=db.prepare("SELECT id,kind,started,finished,state,outcome FROM history ORDER BY started DESC LIMIT 200")?;
        let rows = q.query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "started_at_ms": row.get::<_, i64>(2)?,
                "finished_at_ms": row.get::<_, Option<i64>>(3)?,
                "state": row.get::<_, String>(4)?,
                "outcome": row.get::<_, Option<String>>(5)?
                    .and_then(|body| serde_json::from_str::<Value>(&body).ok()),
            }))
        })?;
        Ok(Value::Array(rows.collect::<Result<Vec<_>, _>>()?))
    }
    pub fn steps(&self, id: &str) -> ServiceResult<Value> {
        let db = self.db.lock().unwrap();
        let mut q = db
            .prepare("SELECT sequence,at,kind,message FROM steps WHERE run=?1 ORDER BY sequence")?;
        let rows = q.query_map([id], |row| {
            Ok(json!({
                "sequence": row.get::<_, i64>(0)?,
                "at_ms": row.get::<_, i64>(1)?,
                "kind": row.get::<_, String>(2)?,
                "message": row.get::<_, String>(3)?,
            }))
        })?;
        Ok(Value::Array(rows.collect::<Result<Vec<_>, _>>()?))
    }
    fn begin(&self, id: &str, kind: &str) -> ServiceResult<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO history(id,kind,started,state) VALUES(?1,?2,?3,'running')",
            params![id, kind, now_ms()],
        )?;
        Ok(())
    }
    fn step(&self, id: &str, kind: &str, message: &str) -> ServiceResult<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO steps(run,at,kind,message) VALUES(?1,?2,?3,?4)",
            params![id, now_ms(), kind, message],
        )?;
        Ok(())
    }
    fn finish(&self, id: &str, state: &str, outcome: Value) -> ServiceResult<()> {
        self.db.lock().unwrap().execute(
            "UPDATE history SET finished=?2,state=?3,outcome=?4 WHERE id=?1",
            params![id, now_ms(), state, outcome.to_string()],
        )?;
        Ok(())
    }
}

pub struct JournalExecutor {
    pub inner: Arc<dyn OperationExecutor>,
    pub store: Arc<ServiceStore>,
}
struct JournalProgress {
    store: Arc<ServiceStore>,
    id: String,
    upstream: Arc<dyn Progress>,
}
impl Progress for JournalProgress {
    fn stage(&self, message: &str) {
        self.store
            .step(&self.id, "stage", message)
            .expect("execution journal unavailable");
        self.upstream.stage(message);
    }
    fn detail(&self, message: &str) {
        self.store
            .step(&self.id, "detail", message)
            .expect("execution journal unavailable");
        self.upstream.detail(message);
    }
}
fn journal_failure() -> OperationFailure {
    OperationFailure::Application {
        code: OperationErrorCode::InvalidRequest,
        message: "execution journal unavailable; inspect durable publication state before retrying"
            .into(),
        causes: vec![],
    }
}
impl OperationExecutor for JournalExecutor {
    fn execute(
        &self,
        request: &OperationRequest,
        progress: Arc<dyn Progress>,
    ) -> Result<OperationResult, OperationFailure> {
        let id = uuid::Uuid::new_v4().to_string();
        self.store
            .begin(&id, &request.kind().to_string())
            .map_err(|_| journal_failure())?;
        progress.detail(&format!("Execution history: {id}"));
        let sink = Arc::new(JournalProgress {
            store: self.store.clone(),
            id: id.clone(),
            upstream: progress,
        });
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.inner.execute(request,sink))).unwrap_or_else(|_|Err(OperationFailure::Application{code:OperationErrorCode::OperationPanicked,message:"operation interrupted by an internal error; inspect durable state before retrying".into(),causes:vec![]}));
        let (state, outcome) = match &result {
            Ok(r) => (
                "succeeded",
                json!({"result":crate::web::WebOperationResult::from_result(r)}),
            ),
            Err(e) => (
                "failed",
                json!({"failure":crate::web::WebOperationFailure::from_failure(e)}),
            ),
        };
        self.store
            .finish(&id, state, outcome)
            .map_err(|_| journal_failure())?;
        result
    }
}

/// Claim before dispatch. An uncertain dispatch after a crash is never retried automatically.
/// Busy work is deferred within the current day; missed earlier days are not replayed.
pub fn tick(store: &ServiceStore, supervisor: &OperationSupervisor, now: i64) -> ServiceResult<()> {
    if supervisor.active_mutation().is_some() {
        return Ok(());
    }
    for s in store.schedules()? {
        let Some(day) = s.slot(now) else { continue };
        let request = if s.kind == "publish" {
            OperationRequest::Publish(crate::application::publish::PublishRequest::now())
        } else {
            OperationRequest::Backup(
                crate::application::backup::BackupRequest::now().map_err(|e| e.to_string())?,
            )
        };
        if !store.claim(&s.kind, day)? {
            continue;
        }
        match supervisor.start(request) {
            Ok(id) => {
                store.dispatched(&s.kind, day, &id.to_string())?;
                break;
            }
            Err(_) => {
                store.release(&s.kind, day)?;
                break;
            }
        }
    }
    Ok(())
}

/// A separate SQLite exclusive transaction provides an OS-released process lease.
pub fn process_lease(path: &Path) -> ServiceResult<Connection> {
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::ZERO)?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS lease(id INTEGER); BEGIN EXCLUSIVE;")?;
    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn daily_boundaries_and_offsets() {
        let s = Schedule {
            kind: "publish".into(),
            at: "08:30".into(),
            enabled: true,
            utc_offset_minutes: 480,
        };
        assert_eq!(s.slot(29 * 60 * 1000), None);
        assert_eq!(s.slot(30 * 60 * 1000), Some(0));
        assert_eq!(s.slot(86_400_000 + 30 * 60 * 1000), Some(1));
        assert!(minute("24:00").is_err());
        assert!(minute("8:00").is_err());
        assert!(minute("00:60").is_err());
    }
    #[test]
    fn claims_and_history_survive_restart() {
        let path =
            std::env::temp_dir().join(format!("mineral-service-{}.sqlite", uuid::Uuid::new_v4()));
        {
            let s = ServiceStore::open(&path, &DaemonConfig::default()).unwrap();
            assert!(s.claim("publish", 7).unwrap());
            assert!(!s.claim("publish", 7).unwrap());
            s.begin("run", "publish").unwrap();
            s.step("run", "stage", "snapshot").unwrap();
        }
        {
            let s = ServiceStore::open(&path, &DaemonConfig::default()).unwrap();
            assert!(!s.claim("publish", 7).unwrap());
            assert_eq!(s.history().unwrap()[0]["state"], "interrupted");
            assert_eq!(s.steps("run").unwrap()[0]["message"], "snapshot");
        }
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn lease_excludes_second_process_connection() {
        let path =
            std::env::temp_dir().join(format!("mineral-lease-{}.sqlite", uuid::Uuid::new_v4()));
        let lease = process_lease(&path).unwrap();
        assert!(process_lease(&path).is_err());
        drop(lease);
        assert!(process_lease(&path).is_ok());
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod execution_tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    struct Controlled {
        count: Arc<AtomicUsize>,
        gate: Mutex<mpsc::Receiver<()>>,
    }
    impl OperationExecutor for Controlled {
        fn execute(
            &self,
            _: &OperationRequest,
            progress: Arc<dyn Progress>,
        ) -> Result<OperationResult, OperationFailure> {
            self.count.fetch_add(1, Ordering::SeqCst);
            progress.stage("snapshot");
            progress.detail("one immutable file");
            self.gate.lock().unwrap().recv().unwrap();
            Err(OperationFailure::Application {
                code: OperationErrorCode::PublicationFailed,
                message: "fixture failure".into(),
                causes: vec![],
            })
        }
    }
    #[test]
    fn scheduler_serializes_due_work_and_journals_failure_without_daily_retry() {
        let path = std::env::temp_dir().join(format!(
            "mineral-scheduling-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let store = Arc::new(ServiceStore::open(&path, &DaemonConfig::default()).unwrap());
        for kind in ["publish", "backup"] {
            store
                .save_schedule(&Schedule {
                    kind: kind.into(),
                    at: "00:00".into(),
                    enabled: true,
                    utc_offset_minutes: 0,
                })
                .unwrap();
        }
        let count = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let executor = Arc::new(JournalExecutor {
            store: store.clone(),
            inner: Arc::new(Controlled {
                count: count.clone(),
                gate: Mutex::new(rx),
            }),
        });
        let supervisor = OperationSupervisor::new(executor);
        tick(&store, &supervisor, 1000).unwrap();
        let first = supervisor.active_mutation().unwrap();
        tick(&store, &supervisor, 1000).unwrap();
        assert_eq!(supervisor.snapshots().len(), 1);
        tx.send(()).unwrap();
        assert!(supervisor.wait(first).unwrap().failure().is_some());
        tick(&store, &supervisor, 1000).unwrap();
        let second = supervisor.active_mutation().unwrap();
        tx.send(()).unwrap();
        supervisor.wait(second).unwrap();
        tick(&store, &supervisor, 1000).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let history = store.history().unwrap();
        assert_eq!(history.as_array().unwrap().len(), 2);
        for run in history.as_array().unwrap() {
            assert_eq!(run["state"], "failed");
            assert_eq!(
                store
                    .steps(run["id"].as_str().unwrap())
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
        }
        drop(supervisor);
        drop(store);
        let store = ServiceStore::open(&path, &DaemonConfig::default()).unwrap();
        assert!(!store.claim("publish", 0).unwrap());
        assert!(store.claim("publish", 1).unwrap());
        drop(store);
        let _ = std::fs::remove_file(path);
    }
}
