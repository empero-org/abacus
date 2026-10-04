//! A fake Abacus sync server for end-to-end tests: the v1 sync contract over
//! plain HTTP/1.1 on a local port, with state a test can inspect and change
//! the way another device would.
#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use abacus_agent::sync_state::{session_sha256, sha256_hex};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

/// One request as the server saw it; header names are lowercase.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Clone)]
struct Row {
    session: Value,
    trace: Vec<u8>,
    revision: u64,
    change_id: u64,
    deleted: bool,
    trace_sha256: String,
}

#[derive(Default)]
struct State {
    rows: BTreeMap<String, Row>,
    change: u64,
    seen: Vec<Seen>,
}

pub struct SyncServer {
    pub address: SocketAddr,
    token: String,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

impl Drop for SyncServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SyncServer {
    /// Serve the sync API to clients presenting `token`.
    pub async fn start(token: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state: Arc<Mutex<State>> = Arc::default();
        let (shared, expected) = (state.clone(), token.to_owned());
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let (state, token) = (shared.clone(), expected.clone());
                tokio::spawn(async move { serve(stream, state, token).await });
            }
        });
        Self { address, token: token.to_owned(), state, task }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Requests whose method matches and whose path starts with `prefix`.
    pub fn seen(&self, method: &str, prefix: &str) -> Vec<Seen> {
        let state = self.state.lock().unwrap();
        state.seen.iter().filter(|seen| seen.method == method && seen.path.starts_with(prefix)).cloned().collect()
    }

    pub fn revision(&self, id: &str) -> Option<u64> {
        self.state.lock().unwrap().rows.get(id).map(|row| row.revision)
    }

    pub fn stored(&self, id: &str) -> Value {
        self.state.lock().unwrap().rows[id].session.clone()
    }

    /// Another device uploads `session` over whatever is there.
    pub fn write(&self, session: Value, trace: &[u8]) {
        let mut state = self.state.lock().unwrap();
        state.change += 1;
        let change_id = state.change;
        let id = session["id"].as_str().unwrap().to_owned();
        let revision = state.rows.get(&id).map_or(1, |row| row.revision + 1);
        let trace_sha256 = sha256_hex(trace);
        state
            .rows
            .insert(id, Row { session, trace: trace.to_vec(), revision, change_id, deleted: false, trace_sha256 });
    }

    /// Another device deletes a session.
    pub fn delete(&self, id: &str) {
        self.bump(id, true);
    }

    /// The revision moves without the content moving (a remote toggle).
    pub fn touch(&self, id: &str) {
        self.bump(id, false);
    }

    fn bump(&self, id: &str, delete: bool) {
        let mut state = self.state.lock().unwrap();
        state.change += 1;
        let change_id = state.change;
        let row = state.rows.get_mut(id).unwrap();
        row.revision += 1;
        row.change_id = change_id;
        row.deleted |= delete;
    }
}

fn meta(id: &str, row: &Row) -> Value {
    json!({
        "id": id,
        "title": row.session["title"],
        "workspace": row.session["workspace"],
        "model": row.session["model"],
        "created_at": row.session["created_at"],
        "updated_at": row.session["updated_at"],
        "revision": row.revision,
        "change_id": row.change_id,
        "deleted": row.deleted,
        "remote_enabled": false,
        "remote_online": false,
        "session_sha256": session_sha256(&row.session),
        "trace_sha256": row.trace_sha256,
        "size_bytes": row.trace.len(),
        "message_count": row.session["messages"].as_array().map(Vec::len),
        "added_in_a_later_version": {"ignored": true},
    })
}

/// The v1.1 error envelope with the legacy `detail` beside it.
fn error(status: u16, code: &str, message: &str, current: Option<Value>) -> (u16, Vec<u8>) {
    let mut error = json!({"code": code, "message": message, "request_id": "test"});
    let mut detail = json!({"code": code});
    if let Some(current) = current {
        error["current"] = current.clone();
        detail["current"] = current;
    }
    (status, json!({"error": error, "detail": detail}).to_string().into_bytes())
}

fn ok(body: Value) -> (u16, Vec<u8>) {
    (200, body.to_string().into_bytes())
}

fn revision_of(header: Option<&String>) -> Option<u64> {
    header?.trim().trim_start_matches("W/").trim_matches('"').parse().ok()
}

fn handle(request: &Seen, state: &mut State, token: &str) -> (u16, Vec<u8>) {
    if request.header("authorization") != Some(&format!("Bearer {token}")) {
        return error(401, "unauthorized", "invalid or expired token", None);
    }
    let (path, query) = request.path.split_once('?').unwrap_or((&request.path, ""));
    let query: BTreeMap<&str, &str> = query.split('&').filter_map(|pair| pair.split_once('=')).collect();
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (request.method.as_str(), parts.as_slice()) {
        ("GET", ["v1", "auth", "me"]) => {
            ok(json!({"id": "u1", "email": "me@example.com", "verified": true, "is_admin": false}))
        }
        ("GET", ["v1", "sync", "sessions"]) => ok(json!({"items": state
            .rows
            .iter()
            .filter(|(_, row)| !row.deleted)
            .map(|(id, row)| meta(id, row))
            .collect::<Vec<_>>()})),
        ("GET", ["v1", "sync", "changes"]) => {
            let cursor: u64 = query.get("cursor").and_then(|value| value.parse().ok()).unwrap_or(0);
            let limit: usize = query.get("limit").and_then(|value| value.parse().ok()).unwrap_or(100);
            let mut rows: Vec<_> = state.rows.iter().filter(|(_, row)| row.change_id > cursor).collect();
            rows.sort_by_key(|(_, row)| row.change_id);
            let has_more = rows.len() > limit;
            rows.truncate(limit);
            let next_cursor = rows.last().map_or(cursor, |(_, row)| row.change_id);
            ok(json!({
                "items": rows.iter().map(|(id, row)| meta(id, row)).collect::<Vec<_>>(),
                "next_cursor": next_cursor,
                "has_more": has_more,
            }))
        }
        ("GET", ["v1", "sync", "sessions", id]) => match state.rows.get(*id) {
            None => error(404, "not_found", "session not found", None),
            Some(row) if row.deleted => error(410, "gone", "session deleted", None),
            Some(row) => ok(json!({"meta": meta(id, row), "session": row.session, "trace_sha256": row.trace_sha256})),
        },
        ("GET", ["v1", "sync", "sessions", id, "trace"]) => match state.rows.get(*id) {
            Some(row) if !row.deleted => (200, row.trace.clone()),
            _ => error(404, "not_found", "session not found", None),
        },
        ("PUT", ["v1", "sync", "sessions", id]) => put(request, state, id),
        ("POST", ["v1", "remote", "sessions", id, "enable" | "disable"]) => match state.rows.get(*id) {
            Some(row) => ok(json!({"meta": meta(id, row)})),
            None => error(404, "not_found", "session not found", None),
        },
        // As servers before 1.1 answer: no `ws_url`.
        ("POST", ["v1", "remote", "tickets"]) => ok(json!({"ticket": "one/use+ticket", "expires_in": 60})),
        ("POST", ["v1", "auth", "pairing"]) => (
            201,
            json!({"pairing_url": "http://pair.test/pair#t=secret", "expires_in": 300,
                   "session_id": request.json()["session_id"]})
            .to_string()
            .into_bytes(),
        ),
        ("POST", ["v1", "usage", "report"]) => (
            202,
            json!({"accepted": request.json()["reports"].as_array().map_or(0, Vec::len), "results": []})
                .to_string()
                .into_bytes(),
        ),
        ("DELETE", ["v1", "sync", "sessions", id]) => {
            let expected = revision_of(request.headers.get("if-match"));
            let current = match state.rows.get(*id) {
                None => return error(404, "not_found", "session not found", None),
                Some(row) => (row.revision, meta(id, row)),
            };
            if expected != Some(current.0) {
                return error(409, "conflict", "revision does not match", Some(current.1));
            }
            state.change += 1;
            let change_id = state.change;
            let row = state.rows.get_mut(*id).unwrap();
            row.revision += 1;
            row.change_id = change_id;
            row.deleted = true;
            ok(json!({"meta": meta(id, row)}))
        }
        _ => error(404, "not_found", "no such endpoint", None),
    }
}

fn put(request: &Seen, state: &mut State, id: &str) -> (u16, Vec<u8>) {
    let body = request.json();
    let session = body["session"].clone();
    if session["id"].as_str() != Some(id) {
        return error(422, "validation_failed", "path and document session IDs differ", None);
    }
    let Ok(trace) = STANDARD.decode(body["trace_base64"].as_str().unwrap_or_default()) else {
        return error(422, "validation_failed", "trace_base64 is invalid", None);
    };
    if body["trace_sha256"].as_str() != Some(sha256_hex(&trace).as_str()) {
        return error(422, "validation_failed", "trace SHA-256 does not match", None);
    }
    let expected = revision_of(request.headers.get("if-match"));
    let create = request.header("if-none-match") == Some("*");
    let revision = match state.rows.get(id) {
        Some(row) if create || expected != Some(row.revision) => {
            return error(409, "conflict", "revision does not match", Some(meta(id, row)));
        }
        Some(row) => row.revision + 1,
        None if !create => return error(409, "conflict", "create requires If-None-Match: *", None),
        None => 1,
    };
    state.change += 1;
    let row =
        Row { session, trace_sha256: sha256_hex(&trace), trace, revision, change_id: state.change, deleted: false };
    let reply = json!({"meta": meta(id, &row)});
    state.rows.insert(id.to_owned(), row);
    ok(reply)
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>, token: String) {
    let Some(request) = read(&mut stream).await else { return };
    let (status, body) = {
        let mut state = state.lock().unwrap();
        let reply = handle(&request, &mut state, &token);
        state.seen.push(request.clone());
        reply
    };
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        410 => "Gone",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nabacus-protocol: 1\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&body).await;
    let _ = stream.shutdown().await;
}

async fn read(stream: &mut TcpStream) -> Option<Seen> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.lines();
    let mut start = lines.next()?.split_whitespace();
    let (method, path) = (start.next()?.to_owned(), start.next()?.to_owned());
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers.get("content-length").and_then(|value| value.parse().ok()).unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(Seen { method, path, headers, body })
}
