//! Best-effort, anonymous activity reporting. Abacus pings the Empero activity
//! API when a session opens, periodically while it remains active, and when it
//! closes (with the session's approximate token total and duration), so the
//! dashboard can show how many users and sessions are active. It never sends
//! prompts, code, or transcripts.
//!
//! It is strictly best-effort: every request has a short timeout and failures
//! are ignored, so the agent works identically offline or with the API down.
//! Disable it with `[activity] enabled = false` or `ABACUS_NO_ACTIVITY=1`.

use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::AbacusPaths;

pub const DEFAULT_ACTIVITY_ENDPOINT: &str = "https://abacus.empero.org/v1/activity";

/// How often an open session pings the API. Must stay well under the server's
/// active-session window so a live session is never mistaken for a stale one.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 45;

#[derive(Clone)]
pub struct ActivityReporter {
    client: reqwest::Client,
    base: String,
    install_id: String,
    ingest_token: Option<String>,
}

impl ActivityReporter {
    /// Build a reporter, or `None` when disabled, opted out, or the endpoint is
    /// unusable. A `None` reporter makes all reporting a no-op at the call site.
    pub fn new(enabled: bool, endpoint: &str, paths: &AbacusPaths) -> Option<Self> {
        if !enabled || std::env::var_os("ABACUS_NO_ACTIVITY").is_some() {
            return None;
        }
        let endpoint = endpoint.trim();
        let url = reqwest::Url::parse(endpoint).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .user_agent(concat!("abacus-agent/", env!("CARGO_PKG_VERSION")))
            .build()
            .ok()?;
        Some(Self {
            client,
            base: endpoint.trim_end_matches('/').to_owned(),
            install_id: install_id(paths),
            ingest_token: std::env::var("ABACUS_INGEST_TOKEN")
                .ok()
                .filter(|token| !token.trim().is_empty()),
        })
    }

    pub async fn report_start(&self, session_id: &str, model: &str) {
        self.post(
            "start",
            json!({
                "install_id": self.install_id,
                "session_id": session_id,
                "model": model,
                "app_version": env!("CARGO_PKG_VERSION"),
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            }),
        )
        .await;
    }

    /// Periodic liveness + running token count while a session is open, so the
    /// dashboard can show live tokens and drop sessions that were killed (and so
    /// never sent `end`) instead of leaving them "active".
    pub async fn report_heartbeat(&self, session_id: &str, tokens: u64) {
        self.post(
            "heartbeat",
            json!({
                "install_id": self.install_id,
                "session_id": session_id,
                "tokens": tokens,
            }),
        )
        .await;
    }

    pub async fn report_end(&self, session_id: &str, tokens: u64, duration_secs: u64) {
        self.post(
            "end",
            json!({
                "install_id": self.install_id,
                "session_id": session_id,
                "tokens": tokens,
                "duration_secs": duration_secs,
            }),
        )
        .await;
    }

    async fn post(&self, path: &str, body: Value) {
        let mut request = self
            .client
            .post(format!("{}/{path}", self.base))
            .json(&body);
        if let Some(token) = &self.ingest_token {
            request = request.header("x-abacus-token", token);
        }
        // Best-effort: a failed ping must never affect the agent.
        let _ = request.send().await;
    }
}

/// A stable, anonymous per-install identifier, generated once and persisted
/// under the Abacus home so "unique users" can be counted without identifying
/// anyone.
fn install_id(paths: &AbacusPaths) -> String {
    let path = paths.root.join("install_id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    let id = Uuid::new_v4().to_string();
    let _ = std::fs::create_dir_all(&paths.root);
    let _ = std::fs::write(&path, &id);
    id
}
