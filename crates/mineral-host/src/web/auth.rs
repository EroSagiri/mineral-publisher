//! Single administrator sessions and Bearer access for automation.
use super::WebState;
use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub struct Auth {
    digest: [u8; 32],
    sessions: Mutex<HashMap<String, Instant>>,
    attempts: Mutex<Vec<Instant>>,
    secure: bool,
}
impl Auth {
    pub fn new(token: &str, secure: bool) -> Result<Self, String> {
        if token.len() < 32 {
            return Err("Web authentication token must contain at least 32 bytes".into());
        }
        Ok(Self {
            digest: Sha256::digest(token.as_bytes()).into(),
            sessions: Mutex::new(HashMap::new()),
            attempts: Mutex::new(vec![]),
            secure,
        })
    }
    fn matches(&self, token: &str) -> bool {
        let supplied = Sha256::digest(token.as_bytes());
        self.digest
            .iter()
            .zip(supplied)
            .fold(0u8, |n, (a, b)| n | (a ^ b))
            == 0
    }
    fn cookie(&self, id: &str, age: u32) -> String {
        format!(
            "mineral_session={id}; Path=/; HttpOnly; SameSite=Strict; Max-Age={age}{}",
            if self.secure { "; Secure" } else { "" }
        )
    }
    fn valid_session(&self, headers: &HeaderMap) -> bool {
        let Some(id) = session(headers) else {
            return false;
        };
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, until| *until > Instant::now());
        sessions.contains_key(id)
    }
}
fn session(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|v| v.trim().strip_prefix("mineral_session="))
}
fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({"code":code,"message":code}))).into_response()
}

pub async fn guard(State(state): State<Arc<WebState>>, request: Request, next: Next) -> Response {
    let Some(auth) = &state.auth else {
        return next.run(request).await;
    };
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| auth.matches(t));
    if !bearer && !auth.valid_session(request.headers()) {
        return error(StatusCode::UNAUTHORIZED, "authentication_required");
    }
    if !bearer
        && request.method() != axum::http::Method::GET
        && request.method() != axum::http::Method::HEAD
        && request
            .headers()
            .get("x-mineral-request")
            .and_then(|v| v.to_str().ok())
            != Some("1")
    {
        return error(StatusCode::FORBIDDEN, "csrf_header_required");
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    token: String,
}
pub async fn login(State(state): State<Arc<WebState>>, Json(input): Json<Login>) -> Response {
    let Some(auth) = &state.auth else {
        return Json(json!({"authenticated":true})).into_response();
    };
    let mut attempts = auth.attempts.lock().unwrap();
    let now = Instant::now();
    attempts.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
    if attempts.len() >= 10 {
        return error(StatusCode::TOO_MANY_REQUESTS, "login_rate_limited");
    }
    if !auth.matches(&input.token) {
        attempts.push(now);
        return error(StatusCode::UNAUTHORIZED, "invalid_credentials");
    }
    drop(attempts);
    let id = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut sessions = auth.sessions.lock().unwrap();
    sessions.retain(|_, until| *until > now);
    if sessions.len() >= 128 {
        return error(StatusCode::TOO_MANY_REQUESTS, "session_limit");
    }
    sessions.insert(id.clone(), now + Duration::from_secs(43200));
    (
        [
            (header::SET_COOKIE, auth.cookie(&id, 43200)),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
        Json(json!({"authenticated":true})),
    )
        .into_response()
}
pub async fn status(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    Json(
        json!({"authenticated":state.auth.as_ref().is_none_or(|a|a.valid_session(&headers)),"required":state.auth.is_some()}),
    )
}
pub async fn logout(State(state): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    if let Some(auth) = &state.auth {
        if let Some(id) = session(&headers) {
            auth.sessions.lock().unwrap().remove(id);
        }
        return (
            [(header::SET_COOKIE, auth.cookie("", 0))],
            Json(json!({"authenticated":false})),
        )
            .into_response();
    }
    Json(json!({"authenticated":false})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_expired_and_restart_sessions_are_rejected() {
        assert!(Auth::new("short", false).is_err());
        let token = "0123456789012345678901234567890123456789";
        let auth = Auth::new(token, true).unwrap();
        assert!(auth.matches(token));
        assert!(!auth.matches("wrong"));
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "mineral_session=expired".parse().unwrap());
        auth.sessions
            .lock()
            .unwrap()
            .insert("expired".into(), Instant::now() - Duration::from_secs(1));
        assert!(!auth.valid_session(&headers));
        auth.sessions
            .lock()
            .unwrap()
            .insert("expired".into(), Instant::now() + Duration::from_secs(1));
        assert!(auth.valid_session(&headers));
        assert!(!Auth::new(token, true).unwrap().valid_session(&headers));
        assert!(auth.cookie("id", 1).contains("; Secure"));
    }
}
