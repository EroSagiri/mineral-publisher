use super::{WebApiError, WebState, off_runtime};
use crate::service::{Schedule, ServiceStore, now_ms};
use axum::{
    Json,
    extract::{Path, State},
};
use serde_json::{Value, json};
use std::sync::Arc;
fn store(s: &WebState) -> Result<Arc<ServiceStore>, WebApiError> {
    s.store
        .clone()
        .ok_or_else(|| WebApiError::internal("persistent service is not enabled"))
}
fn failure(_: impl std::fmt::Display) -> WebApiError {
    WebApiError::internal("service storage unavailable")
}
pub async fn schedules(State(s): State<Arc<WebState>>) -> Result<Json<Value>, WebApiError> {
    let db = store(&s)?;
    let enabled = s.scheduler_enabled;
    off_runtime(move || {
        let plans = db.schedules().map_err(failure)?;
        let schedules = plans
            .iter()
            .map(|plan| {
                json!({
                    "kind": plan.kind,
                    "at": plan.at,
                    "enabled": plan.enabled,
                    "utc_offset_minutes": plan.utc_offset_minutes,
                    "next_at_ms": plan.next_at(now_ms()),
                })
            })
            .collect::<Vec<_>>();
        Ok(Json(json!({
            "enabled": enabled,
            "schedules": schedules,
            "dispatches": db.slots().map_err(failure)?,
        })))
    })
    .await
}
pub async fn save(
    State(s): State<Arc<WebState>>,
    Json(plan): Json<Schedule>,
) -> Result<Json<Value>, WebApiError> {
    plan.validate().map_err(WebApiError::invalid_path)?;
    if plan.kind == "backup" && plan.enabled && !s.runtime().backup_enabled() {
        return Err(WebApiError::invalid_path(
            "configure backup before enabling its schedule",
        ));
    }
    let db = store(&s)?;
    off_runtime(move || {
        db.save_schedule(&plan).map_err(failure)?;
        Ok(Json(json!({"saved":true})))
    })
    .await
}
pub async fn history(State(s): State<Arc<WebState>>) -> Result<Json<Value>, WebApiError> {
    let db = store(&s)?;
    off_runtime(move || db.history().map(Json).map_err(failure)).await
}
pub async fn steps(
    State(s): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, WebApiError> {
    let db = store(&s)?;
    off_runtime(move || db.steps(&id).map(Json).map_err(failure)).await
}
