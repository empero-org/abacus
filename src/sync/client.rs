//! The Abacus server's REST API as the CLI uses it: typed errors, timeouts,
//! bounded retries.
//!
//! Every request carries `Abacus-Protocol: 1`, and every response must echo
//! it — a proxy page or a wrong URL fails loudly as a protocol error instead of
//! being parsed as data.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::Compression;
use flate2::write::GzEncoder;
use reqwest::{Client, Method, RequestBuilder, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{AbacusPaths, SyncCredentials};
use crate::session::Session;
use crate::sync_state::{Hashes, SyncState, sha256_hex};

pub(crate) const PROTOCOL: &str = "1";
const USER_AGENT: &str = concat!("abacus-agent/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// For metadata calls: listing, changes, a session document.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The slowest link a transfer is still expected to finish on; a trace's
/// timeout grows with its size at this rate instead of cutting off large
/// uploads on ordinary connections.
const MIN_TRANSFER_RATE: u64 = 128 * 1024;
const MAX_TRANSFER_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A trace download whose size is unknown may still be up to the server's
/// 100 MiB limit.
const UNKNOWN_TRACE_SIZE: u64 = 100 * 1024 * 1024;
/// Upload bodies at least this large are sent gzip-compressed: smaller ones
/// cost more in CPU and a header than they save on the wire.
const GZIP_MIN_BYTES: usize = 64 * 1024;

/// Servers that refused a compressed upload (415) during this process, which
/// then get plain bodies. Process-wide because every sync pass builds its own
/// client.
static GZIP_REFUSED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// Writes whose outcome is unknown, by account and session: what each was
/// first sent as. Process-wide for the same reason as [`GZIP_REFUSED`].
static UNSETTLED_WRITES: LazyLock<Mutex<HashMap<String, UnsettledWrite>>> = LazyLock::new(Mutex::default);

struct UnsettledWrite {
    /// What the server fingerprints a write by: method, precondition, content.
    identity: String,
    key: String,
}

/// Why a request failed, in the terms a caller acts on.
#[derive(Debug, Clone)]
pub enum SyncError {
    /// The saved token is missing, expired or revoked.
    Unauthorized,
    /// The account may not do this (disabled, or the feature is off).
    Forbidden(String),
    /// A conditional write lost: the server holds a different revision. The
    /// current metadata is included when the server sent it.
    Conflict(Option<Box<SessionMeta>>),
    /// A write to a session that was deleted on another device (409
    /// `deleted`). Not a revision conflict: no revision of it can be written
    /// again, so the caller keeps the local work under a new session id.
    Deleted(Option<Box<SessionMeta>>),
    /// The session does not exist on the server, or was deleted.
    Gone,
    /// The server refused this request for good (too large, invalid).
    Rejected { status: u16, detail: String },
    /// Network trouble, a timeout, or a busy server; worth retrying later.
    Transient(String),
    /// Not an Abacus server, or one speaking a different protocol.
    Protocol(String),
}

impl SyncError {
    /// Errors that make every further request pointless.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Unauthorized | Self::Forbidden(_) | Self::Protocol(_))
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => {
                write!(formatter, "the sync server no longer accepts this device's sign-in; run `abacus sync login`")
            }
            Self::Forbidden(detail) => write!(formatter, "the sync server refused access: {detail}"),
            Self::Conflict(_) => write!(formatter, "the session changed on the server since this device last synced"),
            Self::Deleted(_) => write!(formatter, "the session was deleted on another device"),
            Self::Gone => write!(formatter, "not found on the server"),
            Self::Rejected { status, detail } => {
                write!(formatter, "the sync server rejected the request ({status}): {detail}")
            }
            Self::Transient(detail) => write!(formatter, "sync server unavailable: {detail}"),
            Self::Protocol(detail) => write!(formatter, "{detail}"),
        }
    }
}

impl std::error::Error for SyncError {}

/// A synced session as the server describes it. Unknown fields are ignored,
/// so the server can grow this without breaking older clients.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    pub revision: u64,
    #[serde(default)]
    pub change_id: u64,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub remote_enabled: bool,
    #[serde(default)]
    pub remote_online: bool,
    #[serde(default)]
    pub session_sha256: String,
    #[serde(default)]
    pub trace_sha256: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub message_count: Option<u64>,
}

/// One page of the account's change feed.
#[derive(Debug, Deserialize)]
pub struct ChangesPage {
    #[serde(default)]
    pub items: Vec<SessionMeta>,
    #[serde(default)]
    pub next_cursor: u64,
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    #[serde(default)]
    pub id: String,
    pub email: String,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub is_admin: bool,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// A one-use ticket for the remote-control WebSocket.
#[derive(Debug, Clone, Deserialize)]
pub struct TicketResponse {
    pub ticket: String,
    #[serde(default)]
    pub expires_in: u64,
    /// The socket's path, e.g. `/v1/remote/agent/<id>`; filled in for servers
    /// that predate the field.
    #[serde(default)]
    pub ws_url: String,
}

/// A single-use link that signs a phone into this account.
#[derive(Debug, Clone, Deserialize)]
pub struct Pairing {
    pub pairing_url: String,
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RemoteDocument {
    pub meta: SessionMeta,
    pub session: Value,
}

#[derive(Debug, Deserialize)]
struct SessionList {
    items: Vec<SessionMeta>,
}

#[derive(Debug, Deserialize)]
struct MetaReply {
    meta: SessionMeta,
}

#[derive(Serialize)]
struct PutBody<'a> {
    session: &'a Value,
    trace_base64: String,
    trace_sha256: &'a str,
    device_id: &'a str,
}

/// The precondition of a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precondition {
    /// `If-None-Match: *` — the session must not exist yet.
    Create,
    /// `If-Match: "<revision>"` — the server must still hold this revision.
    Revision(u64),
}

/// How hard to try before reporting a transient failure.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Retry {
    pub attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Retry {
    /// Background sync: a few quick tries, then wait for the next occasion.
    pub const AUTO: Retry = Retry { attempts: 3, base: Duration::from_millis(400), cap: Duration::from_secs(4) };
    /// A command someone is watching: a little more patience.
    pub const MANUAL: Retry = Retry { attempts: 4, base: Duration::from_millis(500), cap: Duration::from_secs(8) };

    /// Exponential backoff with jitter, so many devices recovering from the
    /// same outage do not retry in lockstep.
    fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(wait) = retry_after {
            return wait.min(self.cap);
        }
        let ceiling = self.base.saturating_mul(1_u32 << attempt.min(16)).min(self.cap);
        let half = ceiling / 2;
        let spread = half.as_millis().max(1) as u64;
        half + Duration::from_millis(jitter() % spread)
    }
}

fn jitter() -> u64 {
    uuid::Uuid::new_v4().as_u128() as u64
}

#[derive(Clone)]
pub struct SyncClient {
    http: Client,
    pub(crate) server: String,
    token: String,
    pub(crate) email: String,
    /// This install's id: the `device_id` of uploads and the owner of remote
    /// sessions.
    pub(crate) install_id: String,
    /// When set, uploads made through [`SyncClient::push`] are recorded in the
    /// sync state, so the next automatic sync does not upload them again.
    home: Option<AbacusPaths>,
    retry: Retry,
    /// Set once an operation ran out of retries; later operations try once,
    /// so an unreachable server costs one timeout per call, not three.
    offline: Arc<AtomicBool>,
}

impl SyncClient {
    pub fn new(credentials: &SyncCredentials) -> Result<Self> {
        let server = credentials.server.trim_end_matches('/').to_owned();
        let url = reqwest::Url::parse(&server).context("sync server is not a valid URL")?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("sync server must use HTTP or HTTPS");
        }
        Ok(Self {
            http: Client::builder().user_agent(USER_AGENT).connect_timeout(CONNECT_TIMEOUT).build()?,
            server,
            token: credentials.token.clone(),
            email: credentials.email.clone(),
            // Set by `with_home`; constructing a client touches no files.
            install_id: String::new(),
            home: None,
            retry: Retry::AUTO,
            offline: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Bind the client to an Abacus home: its install id names the device, and
    /// its sync state learns about uploads made outside the sync engine.
    pub(crate) fn with_home(mut self, paths: &AbacusPaths) -> Self {
        self.install_id = paths.install_id();
        self.home = Some(paths.clone());
        self
    }

    pub(crate) fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.server, path))
            .bearer_auth(&self.token)
            .header("Abacus-Protocol", PROTOCOL)
            .header(header::ACCEPT, "application/json")
            .timeout(REQUEST_TIMEOUT)
    }

    /// Send `request`, retrying transient failures, and return the body of a
    /// successful response.
    async fn send(&self, request: RequestBuilder) -> Result<Vec<u8>, SyncError> {
        let attempts = if self.offline.load(Ordering::Relaxed) { 1 } else { self.retry.attempts.max(1) };
        self.send_with(request, attempts).await
    }

    async fn send_with(&self, request: RequestBuilder, attempts: u32) -> Result<Vec<u8>, SyncError> {
        let mut attempt = 0;
        loop {
            let attempt_request =
                request.try_clone().ok_or_else(|| SyncError::Protocol("request body cannot be retried".into()))?;
            let (error, retry_after) = match exchange(attempt_request).await {
                Ok(body) => {
                    self.offline.store(false, Ordering::Relaxed);
                    return Ok(body);
                }
                Err((SyncError::Transient(detail), retry_after)) => (SyncError::Transient(detail), retry_after),
                Err((other, _)) => return Err(other),
            };
            attempt += 1;
            if attempt >= attempts {
                self.offline.store(true, Ordering::Relaxed);
                return Err(error);
            }
            tokio::time::sleep(self.retry.delay(attempt, retry_after)).await;
        }
    }

    async fn json<T: for<'de> Deserialize<'de>>(&self, request: RequestBuilder) -> Result<T, SyncError> {
        let body = self.send(request).await?;
        parse(&body)
    }

    /// `GET /v1/auth/me`.
    pub async fn account(&self) -> Result<Account, SyncError> {
        self.json(self.request(Method::GET, "/v1/auth/me")).await
    }

    /// `GET /v1/sync/sessions`: every live session's metadata.
    pub async fn sessions(&self) -> Result<Vec<SessionMeta>, SyncError> {
        Ok(self.json::<SessionList>(self.request(Method::GET, "/v1/sync/sessions")).await?.items)
    }

    /// `GET /v1/sync/changes`: sessions changed after `cursor`, tombstones
    /// included, oldest change first.
    pub async fn changes(&self, cursor: u64, limit: u32) -> Result<ChangesPage, SyncError> {
        let query = [("cursor", cursor.to_string()), ("limit", limit.to_string())];
        self.json(self.request(Method::GET, "/v1/sync/changes").query(&query)).await
    }

    /// `GET /v1/sync/sessions/{id}`, parsed into a [`Session`].
    pub async fn session(&self, id: &str) -> Result<(Session, SessionMeta), SyncError> {
        let RemoteDocument { meta, session } = self.document(id).await?;
        let session = serde_json::from_value(session).map_err(|error| {
            SyncError::Protocol(format!("the server sent a session this version cannot read: {error}"))
        })?;
        Ok((session, meta))
    }

    /// The session document exactly as stored, unknown fields included.
    pub(crate) async fn document(&self, id: &str) -> Result<RemoteDocument, SyncError> {
        self.json(self.request(Method::GET, &format!("/v1/sync/sessions/{id}"))).await
    }

    /// `GET /v1/sync/sessions/{id}/trace`: the trace JSONL.
    pub async fn trace(&self, id: &str) -> Result<Vec<u8>, SyncError> {
        self.trace_sized(id, UNKNOWN_TRACE_SIZE).await
    }

    pub(crate) async fn trace_sized(&self, id: &str, size_hint: u64) -> Result<Vec<u8>, SyncError> {
        let request = self
            .request(Method::GET, &format!("/v1/sync/sessions/{id}/trace"))
            .header(header::ACCEPT, "application/x-ndjson")
            .timeout(transfer_timeout(size_hint));
        self.send(request).await
    }

    /// `PUT /v1/sync/sessions/{id}`: upload `session` with its trace.
    pub async fn put(
        &self,
        session: &Session,
        trace: &[u8],
        precondition: Precondition,
    ) -> Result<SessionMeta, SyncError> {
        let document = serde_json::to_value(session).map_err(|error| SyncError::Protocol(error.to_string()))?;
        let hashes = Hashes::of(&document, sha256_hex(trace));
        self.put_document(&session.id.to_string(), &document, trace, &hashes, precondition).await
    }

    /// Upload one revision, gzip-compressed once the body is large. A 409
    /// whose current revision already holds exactly this content is a success:
    /// an earlier attempt landed and only its reply was lost (the server's
    /// `Idempotency-Key` replay normally answers that case first).
    pub(crate) async fn put_document(
        &self,
        id: &str,
        document: &Value,
        trace: &[u8],
        hashes: &Hashes,
        precondition: Precondition,
    ) -> Result<SessionMeta, SyncError> {
        let device = if self.install_id.is_empty() { crate::config::device_name() } else { self.install_id.clone() };
        let body = PutBody {
            session: document,
            trace_base64: STANDARD.encode(trace),
            trace_sha256: &hashes.trace,
            device_id: &device,
        };
        let body = serde_json::to_vec(&body).map_err(|error| SyncError::Protocol(error.to_string()))?;
        // The server answers a repeat of this exact write with the stored
        // reply, but only when it carries the key the first attempt did.
        let key = self.write_key(id, &format!("PUT {precondition:?} {} {} {device}", hashes.session, hashes.trace));
        let packed = if body.len() >= GZIP_MIN_BYTES && !self.refused_gzip() { gzip(&body) } else { None };
        let sent = match packed {
            Some(packed) => match self.send_put(id, packed, true, precondition, &key).await {
                // A server (or a proxy in front of it) that cannot read
                // compressed bodies: the same write goes again as plain JSON,
                // and later ones skip the attempt.
                Err(SyncError::Rejected { status: 415, .. }) => {
                    GZIP_REFUSED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(self.server.clone());
                    self.send_put(id, body, false, precondition, &key).await
                }
                other => other,
            },
            None => self.send_put(id, body, false, precondition, &key).await,
        };
        self.write_settled(id, &sent);
        match sent {
            Ok(meta) => Ok(meta),
            Err(SyncError::Conflict(Some(current)))
                if !current.deleted
                    && current.session_sha256 == hashes.session
                    && current.trace_sha256 == hashes.trace =>
            {
                Ok(*current)
            }
            Err(error) => Err(error),
        }
    }

    async fn send_put(
        &self,
        id: &str,
        body: Vec<u8>,
        compressed: bool,
        precondition: Precondition,
        key: &str,
    ) -> Result<SessionMeta, SyncError> {
        let mut request = self
            .request(Method::PUT, &format!("/v1/sync/sessions/{id}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", key)
            .timeout(transfer_timeout(body.len() as u64))
            .body(body);
        if compressed {
            request = request.header(header::CONTENT_ENCODING, "gzip");
        }
        request = match precondition {
            Precondition::Create => request.header(header::IF_NONE_MATCH, "*"),
            Precondition::Revision(revision) => request.header(header::IF_MATCH, format!("\"{revision}\"")),
        };
        Ok(self.json::<MetaReply>(request).await?.meta)
    }

    /// `DELETE /v1/sync/sessions/{id}`: leave a tombstone.
    pub async fn delete(&self, id: &str, revision: u64) -> Result<SessionMeta, SyncError> {
        let key = self.write_key(id, &format!("DELETE {revision}"));
        let request = self
            .request(Method::DELETE, &format!("/v1/sync/sessions/{id}"))
            .header("Idempotency-Key", key)
            .header(header::IF_MATCH, format!("\"{revision}\""));
        let result = self.json::<MetaReply>(request).await.map(|reply| reply.meta);
        self.write_settled(id, &result);
        result
    }

    fn refused_gzip(&self) -> bool {
        GZIP_REFUSED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(&self.server)
    }

    /// Where this account's pending write to `session` is remembered.
    fn write_slot(&self, session: &str) -> String {
        format!("{}\n{}\n{session}", self.server, self.email)
    }

    /// The `Idempotency-Key` for a write. One logical write keeps one key
    /// through every attempt: the backoff loop resends the request it was
    /// given, and a write whose outcome stayed unknown (the reply was lost, or
    /// the server was unavailable) is sent again with the key it first had, so
    /// a server that did apply it replays its answer instead of refusing the
    /// retry as a conflict with itself. Any other write gets a new key; the
    /// server rejects a key reused for a different request.
    fn write_key(&self, session: &str, identity: &str) -> String {
        let mut writes = UNSETTLED_WRITES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let slot = self.write_slot(session);
        match writes.get(&slot) {
            Some(write) if write.identity == identity => write.key.clone(),
            _ => {
                let key = uuid::Uuid::new_v4().to_string();
                writes.insert(slot, UnsettledWrite { identity: identity.to_owned(), key: key.clone() });
                key
            }
        }
    }

    /// Forget a write's key once the server has answered it either way; only
    /// an unknown outcome keeps it for the retry.
    fn write_settled<T>(&self, session: &str, result: &Result<T, SyncError>) {
        if !matches!(result, Err(SyncError::Transient(_))) {
            UNSETTLED_WRITES.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&self.write_slot(session));
        }
    }

    /// Make a session controllable from the account's browsers, owned by this
    /// install.
    pub async fn enable_remote(&self, id: &str) -> Result<(), SyncError> {
        let body = if self.install_id.is_empty() { json!({}) } else { json!({"install_id": self.install_id}) };
        let request = self.request(Method::POST, &format!("/v1/remote/sessions/{id}/enable")).json(&body);
        self.json::<Value>(request).await.map(drop)
    }

    pub async fn disable_remote(&self, id: &str) -> Result<(), SyncError> {
        let request = self.request(Method::POST, &format!("/v1/remote/sessions/{id}/disable"));
        self.json::<Value>(request).await.map(drop)
    }

    /// A one-use ticket for this session's agent socket.
    pub async fn agent_ticket(&self, id: &str) -> Result<TicketResponse, SyncError> {
        let request =
            self.request(Method::POST, "/v1/remote/tickets").json(&json!({"session_id": id, "role": "agent"}));
        let mut ticket: TicketResponse = self.json(request).await?;
        if ticket.ws_url.is_empty() {
            ticket.ws_url = format!("/v1/remote/agent/{id}");
        }
        Ok(ticket)
    }

    /// The server's address with a WebSocket scheme, followed by
    /// `path_and_query`.
    pub fn ws_url(&self, path_and_query: &str) -> Result<String> {
        let rest = if let Some(rest) = self.server.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.server.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            bail!("sync server must use HTTP or HTTPS");
        };
        Ok(format!("{rest}{path_and_query}"))
    }

    /// The full agent socket URL for `ticket`, with the ticket encoded into the
    /// query.
    pub fn agent_socket_url(&self, ticket: &TicketResponse) -> Result<String> {
        let mut url = reqwest::Url::parse(&self.ws_url(&ticket.ws_url)?).context("invalid remote socket URL")?;
        url.query_pairs_mut().append_pair("ticket", &ticket.ticket);
        Ok(url.to_string())
    }

    /// `POST /v1/auth/pairing`: a single-use link that signs a phone into this
    /// account, opening `session` when one is named.
    pub async fn pairing_url(&self, session: Option<&str>) -> Result<Pairing, SyncError> {
        let body = match session {
            Some(id) => json!({"session_id": id}),
            None => json!({}),
        };
        self.json(self.request(Method::POST, "/v1/auth/pairing").json(&body)).await
    }

    /// `POST /v1/usage/report`. One attempt: reports are cumulative, so the
    /// next periodic report supersedes a lost one.
    pub async fn report_usage(&self, body: &Value) -> Result<Value, SyncError> {
        let request = self.request(Method::POST, "/v1/usage/report").json(body);
        parse(&self.send_with(request, 1).await?)
    }

    /// Upload `session` as-is, outside the sync engine (the remote bridge
    /// shares a session this way). Without `force` an existing remote copy is
    /// left alone and reported; with it, the current remote revision is
    /// replaced.
    pub async fn push(&self, session: &Session, trace: &[u8], force: bool) -> Result<u64> {
        let id = session.id.to_string();
        let document = serde_json::to_value(session).context("could not encode session")?;
        let hashes = Hashes::of(&document, sha256_hex(trace));
        let mut state = self.home.as_ref().map(|paths| SyncState::load_for(paths, &self.server, &self.email));
        let known = state.as_ref().and_then(|state| state.record(&session.id)).and_then(|record| record.revision);
        let precondition = known.map_or(Precondition::Create, Precondition::Revision);
        let meta = match self.put_document(&id, &document, trace, &hashes, precondition).await {
            Ok(meta) => meta,
            Err(SyncError::Conflict(Some(current))) if force => {
                self.put_document(&id, &document, trace, &hashes, Precondition::Revision(current.revision)).await?
            }
            Err(SyncError::Conflict(Some(current))) => {
                bail!("remote session {id} already exists; use --force to replace revision {}", current.revision)
            }
            // The record names a revision the server no longer has.
            Err(SyncError::Conflict(None)) if precondition != Precondition::Create => {
                self.put_document(&id, &document, trace, &hashes, Precondition::Create).await?
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(state) = &mut state {
            state.mark_synced(&session.id, &meta, &hashes, None);
            let _ = state.save();
        }
        Ok(meta.revision)
    }
}

fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, SyncError> {
    serde_json::from_slice(body).map_err(|error| SyncError::Protocol(format!("sync server sent invalid JSON: {error}")))
}

/// One request and its classified outcome. A transient failure carries the
/// server's `Retry-After`, when it sent one.
async fn exchange(request: RequestBuilder) -> Result<Vec<u8>, (SyncError, Option<Duration>)> {
    let response = request.send().await.map_err(|error| (transport_error(&error), None))?;
    let status = response.status();
    if is_transient_status(status) {
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return Err((SyncError::Transient(format!("server returned {status}")), retry_after));
    }
    let confirmed = response.headers().get("Abacus-Protocol").and_then(|value| value.to_str().ok()) == Some(PROTOCOL);
    if !confirmed {
        let origin = response.url().origin().ascii_serialization();
        return Err((
            SyncError::Protocol(format!(
                "{origin} did not confirm Abacus protocol {PROTOCOL} (HTTP {status}); check the sync server address"
            )),
            None,
        ));
    }
    let body = response.bytes().await.map_err(|error| (transport_error(&error), None))?;
    if status.is_success() {
        return Ok(body.into());
    }
    Err((error_for_status(status, &body), None))
}

fn is_transient_status(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(status, StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT)
        || status.as_u16() == 425
}

fn transport_error(error: &reqwest::Error) -> SyncError {
    if error.is_builder() || error.is_redirect() {
        return SyncError::Protocol(format!("sync request failed: {error}"));
    }
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "connection failed"
    };
    SyncError::Transient(kind.to_owned())
}

/// Map a refused request to its error.
pub(crate) fn error_for_status(status: StatusCode, body: &[u8]) -> SyncError {
    match status {
        StatusCode::UNAUTHORIZED => SyncError::Unauthorized,
        StatusCode::FORBIDDEN => SyncError::Forbidden(detail(body)),
        StatusCode::NOT_FOUND | StatusCode::GONE => SyncError::Gone,
        StatusCode::CONFLICT => conflict(body),
        _ => SyncError::Rejected { status: status.as_u16(), detail: detail(body) },
    }
}

/// A 409: a lost revision race, or a write to a session deleted elsewhere. The
/// server says the latter with code `deleted`; servers before that code sent a
/// plain conflict whose current revision is the tombstone, which is the same
/// thing.
fn conflict(body: &[u8]) -> SyncError {
    let current = conflict_meta(body).map(Box::new);
    let code = serde_json::from_slice::<Value>(body).ok().and_then(|value| {
        ["/error/code", "/detail/code"].iter().find_map(|path| value.pointer(path)?.as_str().map(str::to_owned))
    });
    if code.as_deref() == Some("deleted") || current.as_ref().is_some_and(|current| current.deleted) {
        SyncError::Deleted(current)
    } else {
        SyncError::Conflict(current)
    }
}

/// The server's current metadata from a 409 body: `error.current` in the v1.1
/// envelope, `detail.current` in the older FastAPI shape.
pub(crate) fn conflict_meta(body: &[u8]) -> Option<SessionMeta> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let current = ["/error/current", "/detail/current", "/current"].iter().find_map(|path| value.pointer(path))?;
    serde_json::from_value(current.clone()).ok()
}

/// A short human-readable reason from an error body.
fn detail(body: &[u8]) -> String {
    let text = match serde_json::from_slice::<Value>(body) {
        Ok(value) => {
            if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
                message.to_owned()
            } else {
                match value.get("detail") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Object(object)) => object
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| Value::Object(object.clone()).to_string()),
                    Some(other) => other.to_string(),
                    None => value.to_string(),
                }
            }
        }
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    };
    crate::text::clip(text.trim(), 300, "…")
}

/// `bytes` as a gzip stream. Fast rather than small: the JSON of a session
/// shrinks several-fold at any level, and this runs while a person waits.
fn gzip(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(bytes.len() / 4), Compression::fast());
    encoder.write_all(bytes).ok()?;
    encoder.finish().ok()
}

fn transfer_timeout(bytes: u64) -> Duration {
    (REQUEST_TIMEOUT + Duration::from_secs(bytes / MIN_TRANSFER_RATE)).min(MAX_TRANSFER_TIMEOUT)
}

/// A client for the unauthenticated sign-in endpoints.
pub(crate) fn anonymous_client() -> Result<Client> {
    Ok(Client::builder().user_agent(USER_AGENT).connect_timeout(CONNECT_TIMEOUT).timeout(REQUEST_TIMEOUT).build()?)
}

/// Decode a response from an unauthenticated endpoint (sign-in), with the
/// same protocol and error handling as everything else.
pub(crate) async fn decode<T: for<'de> Deserialize<'de>>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    let confirmed = response.headers().get("Abacus-Protocol").and_then(|value| value.to_str().ok()) == Some(PROTOCOL);
    let body = response.bytes().await.unwrap_or_default();
    if is_transient_status(status) {
        return Err(SyncError::Transient(format!("server returned {status}")).into());
    }
    if !confirmed {
        bail!("sync server did not confirm Abacus protocol version {PROTOCOL}");
    }
    if !status.is_success() {
        return Err(error_for_status(status, &body).into());
    }
    serde_json::from_slice(&body).context("sync server returned invalid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_bodies_carry_the_current_revision() {
        // The v1.1 envelope, with the legacy `detail` beside it.
        let body = br#"{"error":{"code":"conflict","message":"revision does not match","request_id":"r",
            "current":{"id":"a","revision":7,"session_sha256":"s","trace_sha256":"t","future":{"x":1}}},
            "detail":{"code":"conflict","current":{"id":"a","revision":6}}}"#;
        let SyncError::Conflict(Some(current)) = error_for_status(StatusCode::CONFLICT, body) else {
            panic!("a 409 with metadata is a conflict that carries it");
        };
        assert_eq!((current.id.as_str(), current.revision, current.session_sha256.as_str()), ("a", 7, "s"));

        // The pre-1.1 FastAPI shape.
        let legacy = br#"{"detail":{"code":"conflict","current":{"id":"b","revision":3}}}"#;
        let SyncError::Conflict(Some(current)) = error_for_status(StatusCode::CONFLICT, legacy) else {
            panic!("legacy conflict");
        };
        assert_eq!(current.revision, 3);

        // A create that raced a missing row says so without metadata.
        let bare = br#"{"detail":{"code":"conflict","message":"create requires If-None-Match: *"}}"#;
        assert!(matches!(error_for_status(StatusCode::CONFLICT, bare), SyncError::Conflict(None)));
        assert!(matches!(error_for_status(StatusCode::CONFLICT, b"<html>"), SyncError::Conflict(None)));
    }

    #[test]
    fn a_write_to_a_deleted_session_is_not_a_revision_conflict() {
        let body = br#"{"error":{"code":"deleted","message":"session was deleted","request_id":"r",
            "current":{"id":"a","revision":9,"deleted":true,"title":"Gone"}},
            "detail":{"code":"deleted","current":{"id":"a","revision":9,"deleted":true}}}"#;
        let SyncError::Deleted(Some(current)) = error_for_status(StatusCode::CONFLICT, body) else {
            panic!("409 deleted is its own error");
        };
        assert_eq!((current.revision, current.title.as_str(), current.deleted), (9, "Gone", true));
        assert!(!error_for_status(StatusCode::CONFLICT, body).is_fatal());

        // Only the legacy envelope.
        let legacy = br#"{"detail":{"code":"deleted","current":{"id":"a","revision":4,"deleted":true}}}"#;
        assert!(matches!(error_for_status(StatusCode::CONFLICT, legacy), SyncError::Deleted(Some(_))));
        // A server that predates the code reports the tombstone as the conflict's current revision.
        let older = br#"{"detail":{"code":"conflict","current":{"id":"a","revision":4,"deleted":true}}}"#;
        assert!(matches!(error_for_status(StatusCode::CONFLICT, older), SyncError::Deleted(Some(_))));
        // The code alone is enough.
        let bare = br#"{"error":{"code":"deleted","message":"session was deleted"}}"#;
        assert!(matches!(error_for_status(StatusCode::CONFLICT, bare), SyncError::Deleted(None)));
    }

    #[test]
    fn a_write_keeps_its_key_until_the_server_answers() {
        let client = |email: &str| {
            SyncClient::new(&SyncCredentials {
                server: "http://keys.test".into(),
                token: "t".into(),
                email: email.into(),
            })
            .unwrap()
        };
        let (first, other_account) = (client("a@keys.test"), client("b@keys.test"));
        let put = "PUT Revision(1) s t d";
        let key = first.write_key("session-1", put);
        assert_eq!(first.write_key("session-1", put), key, "a retry of the same write");
        assert_ne!(first.write_key("session-2", put), key, "another session is another write");
        assert_ne!(other_account.write_key("session-1", put), key, "keys are per account");

        // The server could not be reached: the outcome is unknown, so the
        // retry may be a write the server already applied.
        first.write_settled::<()>("session-1", &Err(SyncError::Transient("timed out".into())));
        assert_eq!(first.write_key("session-1", put), key);

        // Different content or a different revision is a different write.
        let changed = first.write_key("session-1", "PUT Revision(1) s2 t d");
        assert_ne!(changed, key);
        assert_ne!(first.write_key("session-1", "PUT Revision(2) s2 t d"), changed);

        // Any answer settles it, a refusal included.
        let key = first.write_key("session-1", put);
        first.write_settled::<()>("session-1", &Err(SyncError::Rejected { status: 422, detail: "x".into() }));
        assert_ne!(first.write_key("session-1", put), key);
        let key = first.write_key("session-1", put);
        first.write_settled("session-1", &Ok(()));
        assert_ne!(first.write_key("session-1", put), key);
    }

    #[test]
    fn large_bodies_shrink_and_survive_a_round_trip() {
        use std::io::Read;
        let body = serde_json::to_vec(&json!({"trace": "ABCD".repeat(100_000)})).unwrap();
        let packed = gzip(&body).unwrap();
        assert!(packed.len() * 20 < body.len(), "{} of {}", packed.len(), body.len());
        let mut unpacked = Vec::new();
        flate2::read::GzDecoder::new(packed.as_slice()).read_to_end(&mut unpacked).unwrap();
        assert_eq!(unpacked, body);
    }

    #[test]
    fn statuses_map_to_what_the_caller_does_next() {
        assert!(matches!(error_for_status(StatusCode::UNAUTHORIZED, b"{}"), SyncError::Unauthorized));
        assert!(error_for_status(StatusCode::UNAUTHORIZED, b"{}").is_fatal());
        assert!(matches!(error_for_status(StatusCode::GONE, b"{}"), SyncError::Gone));
        assert!(matches!(error_for_status(StatusCode::NOT_FOUND, b"{}"), SyncError::Gone));
        let SyncError::Rejected { status, detail } =
            error_for_status(StatusCode::PAYLOAD_TOO_LARGE, br#"{"detail":"trace too large"}"#)
        else {
            panic!("413 is a permanent rejection");
        };
        assert_eq!((status, detail.as_str()), (413, "trace too large"));
        let SyncError::Rejected { detail, .. } = error_for_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            br#"{"error":{"code":"validation_failed","message":"trace is not valid JSONL"},"detail":"x"}"#,
        ) else {
            panic!("422 is a permanent rejection");
        };
        assert_eq!(detail, "trace is not valid JSONL");
        assert!(is_transient_status(StatusCode::BAD_GATEWAY));
        assert!(is_transient_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(!is_transient_status(StatusCode::CONFLICT));
    }

    #[test]
    fn backoff_grows_stays_bounded_and_honours_retry_after() {
        let retry = Retry::AUTO;
        for attempt in 1..10 {
            let delay = retry.delay(attempt, None);
            assert!(delay <= retry.cap, "{delay:?}");
            assert!(delay >= retry.base.saturating_mul(1_u32 << attempt.min(16)).min(retry.cap) / 2);
        }
        assert_eq!(retry.delay(1, Some(Duration::from_secs(1))), Duration::from_secs(1));
        assert_eq!(retry.delay(1, Some(Duration::from_secs(600))), retry.cap);
    }

    #[test]
    fn large_transfers_get_proportionally_longer() {
        assert_eq!(transfer_timeout(0), REQUEST_TIMEOUT);
        assert!(transfer_timeout(100 * 1024 * 1024) > Duration::from_secs(600));
        assert_eq!(transfer_timeout(u64::MAX / 2), MAX_TRANSFER_TIMEOUT);
    }

    #[test]
    fn socket_urls_follow_the_server_scheme() {
        let client = SyncClient::new(&SyncCredentials {
            server: "https://sync.example/".into(),
            token: "t".into(),
            email: "a@b".into(),
        })
        .unwrap();
        assert_eq!(client.ws_url("/v1/remote/agent/x").unwrap(), "wss://sync.example/v1/remote/agent/x");
        let ticket = TicketResponse { ticket: "a b&c".into(), expires_in: 60, ws_url: "/v1/remote/agent/x".into() };
        assert_eq!(client.agent_socket_url(&ticket).unwrap(), "wss://sync.example/v1/remote/agent/x?ticket=a+b%26c");
        let local = SyncClient::new(&SyncCredentials {
            server: "http://127.0.0.1:8765".into(),
            token: "t".into(),
            email: "e".into(),
        })
        .unwrap();
        assert_eq!(local.ws_url("/p?q=1").unwrap(), "ws://127.0.0.1:8765/p?q=1");
    }
}
