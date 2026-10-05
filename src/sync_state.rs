//! What this device last exchanged with the sync server
//! (`<home>/sync-state.json`).
//!
//! The server is the source of truth for revisions; this file only remembers
//! enough to answer two questions cheaply: "has the server changed since we
//! last looked?" (the change-feed cursor and each session's revision) and "has
//! this copy changed since we last synced it?" (content hashes, plus a file
//! fingerprint so an unchanged session is not even re-read).
//!
//! Losing the file is always safe. Without it every remote session is compared
//! with its local copy: equal copies are adopted without a transfer, one side
//! extending the other fast-forwards, and anything else goes through the same
//! fork-on-conflict path as any other conflict. Nothing is overwritten.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::{AbacusPaths, Credentials, atomic_write};
use crate::sync::SessionMeta;

const STATE_VERSION: u32 = 1;

/// Session fields that move without the conversation moving: resuming a
/// session and closing it again bumps all of them. They are left out of the
/// local content hash, so merely opening a session on two devices is neither
/// an upload nor a conflict; they still travel with the next real change.
const VOLATILE_FIELDS: [&str; 4] = ["updated_at", "active_secs", "tokens_used", "version"];

/// How much older than the moment a fingerprint was taken a file's mtime must
/// be before the fingerprint alone vouches for it. Coarse filesystem clocks
/// can give two writes in the same tick the same mtime; a file touched that
/// recently is re-hashed instead of trusted (git's "racy clean" rule).
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// Serialises state-file writes within this process: the background sync task
/// and the UI thread both save, and `atomic_write` names its temporary file
/// after the process id.
static SAVE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub email: String,
    /// Position in the account's change feed; 0 reads it from the start.
    #[serde(default)]
    pub cursor: u64,
    #[serde(default)]
    pub last_pull_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_push_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionRecord>,
    #[serde(skip)]
    path: PathBuf,
    /// Records this process changed, so a save merges them into whatever
    /// another process wrote meanwhile instead of replacing its work.
    #[serde(skip)]
    touched: HashSet<String>,
    #[serde(skip)]
    cursor_touched: bool,
}

/// What one session looked like when this device last agreed with the server.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The server revision the local copy corresponds to; `None` for a
    /// session the server has never accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// The server's hashes of that revision.
    #[serde(default)]
    pub session_sha256: String,
    #[serde(default)]
    pub trace_sha256: String,
    /// The local copy at that moment: the canonical document hash without
    /// [`VOLATILE_FIELDS`] (raw file bytes would call every resume a change),
    /// and the trace file's hash. These decide "changed here".
    #[serde(default)]
    pub local_sha256: String,
    #[serde(default)]
    pub local_trace_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Fingerprint>,
    /// An upload was refused because the server moved on. Nothing was
    /// overwritten; the next pull resolves it (forking if both changed), and
    /// `abacus sync push --force` replaces the server's copy instead.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conflict: bool,
    /// A newer server revision that was not installed because the session was
    /// open here. Fetched again on every pull until it lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<u64>,
    /// The server holds a tombstone and the local copy was kept, because it
    /// was open when the delete arrived. Settled on a later pull.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    /// The server refused this exact content for good (too large, invalid);
    /// automatic sync does not upload it again until it changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected: Option<Rejected>,
}

impl SessionRecord {
    /// Whether the server is known to be ahead of this copy.
    pub fn pending(&self) -> bool {
        self.conflict || self.behind.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejected {
    pub local_sha256: String,
    pub local_trace_sha256: String,
    pub reason: String,
}

/// Size and mtime of a session file and its trace, so an untouched session is
/// recognised without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    session: Stamp,
    trace: Stamp,
    taken_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
struct Stamp {
    len: u64,
    modified_ns: u64,
}

impl Fingerprint {
    /// Stat both files now. Taken *before* reading them, so a write that lands
    /// between the stat and the read leaves a fingerprint that no longer
    /// matches, and the next check re-hashes.
    pub fn take(session: &Path, trace: &Path) -> Self {
        Self { session: stamp(session), trace: stamp(trace), taken_ns: nanos(SystemTime::now()) }
    }

    /// Whether `current` proves the files are the ones this fingerprint saw.
    pub fn vouches_for(&self, current: &Fingerprint) -> bool {
        let newest = self.session.modified_ns.max(self.trace.modified_ns);
        self.session == current.session
            && self.trace == current.trace
            && newest.saturating_add(RACY_WINDOW.as_nanos() as u64) < self.taken_ns
    }
}

fn stamp(path: &Path) -> Stamp {
    std::fs::metadata(path)
        .map(|meta| Stamp { len: meta.len(), modified_ns: meta.modified().map(nanos).unwrap_or(0) })
        .unwrap_or_default()
}

fn nanos(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_nanos() as u64).unwrap_or(0)
}

impl SyncState {
    /// The state for the signed-in account, or an empty one when signed out.
    pub fn load(paths: &AbacusPaths) -> Self {
        let credentials = Credentials::load(paths).unwrap_or_default();
        match &credentials.sync {
            Some(sync) => Self::load_for(paths, &sync.server, &sync.email),
            None => Self::fresh(paths.sync_state_file(), "", ""),
        }
    }

    /// The saved state if it belongs to `server` and `email`, else a fresh one:
    /// a different account or server is a different history, so it starts
    /// over with a full resync. A corrupt file is moved aside, not replaced
    /// silently, and also means a full resync — which is safe.
    pub fn load_for(paths: &AbacusPaths, server: &str, email: &str) -> Self {
        let path = paths.sync_state_file();
        let Ok(content) = std::fs::read(&path) else {
            return Self::fresh(path, server, email);
        };
        match serde_json::from_slice::<SyncState>(&content) {
            Ok(state) if state.belongs_to(server, email) => Self { path, ..state },
            Ok(_) => Self::fresh(path, server, email),
            Err(_) => {
                let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                Self::fresh(path, server, email)
            }
        }
    }

    fn fresh(path: PathBuf, server: &str, email: &str) -> Self {
        Self {
            version: STATE_VERSION,
            server: server.trim_end_matches('/').to_owned(),
            email: email.to_owned(),
            path,
            ..Self::default()
        }
    }

    fn belongs_to(&self, server: &str, email: &str) -> bool {
        self.version == STATE_VERSION
            && self.server == server.trim_end_matches('/')
            && self.email.eq_ignore_ascii_case(email)
    }

    pub fn record(&self, id: &Uuid) -> Option<&SessionRecord> {
        self.sessions.get(&id.to_string())
    }

    pub fn set(&mut self, id: &Uuid, record: SessionRecord) {
        let key = id.to_string();
        self.touched.insert(key.clone());
        self.sessions.insert(key, record);
    }

    /// Change one record in place, creating it if needed.
    pub fn update(&mut self, id: &Uuid, change: impl FnOnce(&mut SessionRecord)) {
        let key = id.to_string();
        self.touched.insert(key.clone());
        change(self.sessions.entry(key).or_default());
    }

    /// Drop a session's record: the next sync treats it as never seen.
    pub fn forget(&mut self, id: &Uuid) {
        let key = id.to_string();
        self.touched.insert(key.clone());
        self.sessions.remove(&key);
    }

    /// Record that this device and the server agree on `meta`'s revision,
    /// with the local copy described by `local`.
    pub fn mark_synced(&mut self, id: &Uuid, meta: &SessionMeta, local: &Hashes, fingerprint: Option<Fingerprint>) {
        let or = |remote: &str, local: &str| if remote.is_empty() { local.to_owned() } else { remote.to_owned() };
        self.set(
            id,
            SessionRecord {
                revision: Some(meta.revision),
                session_sha256: or(&meta.session_sha256, &local.session),
                trace_sha256: or(&meta.trace_sha256, &local.trace),
                local_sha256: local.content.clone(),
                local_trace_sha256: local.trace.clone(),
                synced_at: Some(Utc::now()),
                fingerprint,
                ..SessionRecord::default()
            },
        );
    }

    /// Whether a local copy hashing to `local` differs from the last sync.
    pub fn is_dirty(&self, id: &Uuid, local: &Hashes) -> bool {
        self.record(id)
            .is_none_or(|record| record.local_sha256 != local.content || record.local_trace_sha256 != local.trace)
    }

    pub fn set_cursor(&mut self, cursor: u64) {
        self.cursor = cursor;
        self.cursor_touched = true;
    }

    /// Write the state, merged record by record with what is on disk now: a
    /// second Abacus process syncing at the same time keeps the records it
    /// wrote, and this process's records win for the sessions it handled.
    pub fn save(&mut self) -> Result<()> {
        let _guard = SAVE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut merged = std::fs::read(&self.path)
            .ok()
            .and_then(|content| serde_json::from_slice::<SyncState>(&content).ok())
            .filter(|disk| disk.belongs_to(&self.server, &self.email))
            .unwrap_or_default();
        for key in &self.touched {
            match self.sessions.get(key) {
                Some(record) => merged.sessions.insert(key.clone(), record.clone()),
                None => merged.sessions.remove(key),
            };
        }
        // Records this process never touched come from disk, so the in-memory
        // copy catches up with other processes too.
        for (key, record) in &merged.sessions {
            if !self.touched.contains(key) {
                self.sessions.insert(key.clone(), record.clone());
            }
        }
        if !self.cursor_touched {
            self.cursor = self.cursor.max(merged.cursor);
        }
        self.last_pull_at = self.last_pull_at.max(merged.last_pull_at);
        self.last_push_at = self.last_push_at.max(merged.last_push_at);
        let output = SyncState {
            version: STATE_VERSION,
            server: self.server.clone(),
            email: self.email.clone(),
            cursor: self.cursor,
            last_pull_at: self.last_pull_at,
            last_push_at: self.last_push_at,
            sessions: merged.sessions,
            ..SyncState::default()
        };
        let content = serde_json::to_vec_pretty(&output).context("could not encode sync state")?;
        atomic_write(&self.path, &content, true)
    }
}

/// The hashes that identify one copy of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashes {
    /// The whole canonical document — comparable with the server's
    /// `session_sha256`.
    pub session: String,
    /// The canonical document without its volatile fields.
    pub content: String,
    pub trace: String,
}

impl Hashes {
    pub fn of(document: &Value, trace_sha256: String) -> Self {
        Self { session: session_sha256(document), content: content_sha256(document), trace: trace_sha256 }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// SHA-256 of the canonical document, as the server computes `session_sha256`.
pub fn session_sha256(document: &Value) -> String {
    sha256_hex(canonical_json(document).as_bytes())
}

/// SHA-256 of the canonical document without its volatile fields.
pub fn content_sha256(document: &Value) -> String {
    let mut out = String::new();
    match document {
        Value::Object(map) => write_object(map, &VOLATILE_FIELDS, &mut out),
        other => write_value(other, &mut out),
    }
    sha256_hex(out.as_bytes())
}

/// Python's `json.dumps(value, sort_keys=True, separators=(",", ":"),
/// ensure_ascii=False)`, byte for byte — the server's canonical form. Floats
/// follow `repr(float)`, which differs from serde_json's own formatting
/// (`1e+16` vs `1e16`), so the document is printed here rather than by serde.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => write_number(number, out),
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => write_object(map, &[], out),
    }
}

fn write_object(map: &Map<String, Value>, skip: &[&str], out: &mut String) {
    // Byte order of UTF-8 is code point order, which is how Python sorts str.
    // Sorted explicitly: serde_json keeps insertion order if any crate in the
    // build enables its `preserve_order` feature.
    let mut keys = map.keys().filter(|key| !skip.contains(&key.as_str())).collect::<Vec<_>>();
    keys.sort();
    out.push('{');
    for (index, key) in keys.into_iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_string(key, out);
        out.push(':');
        write_value(&map[key], out);
    }
    out.push('}');
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn write_number(number: &Number, out: &mut String) {
    if let Some(integer) = number.as_i64() {
        let _ = write!(out, "{integer}");
    } else if let Some(integer) = number.as_u64() {
        let _ = write!(out, "{integer}");
    } else if let Some(float) = number.as_f64() {
        out.push_str(&python_float(float));
    }
}

/// `repr(float)`: the shortest round-tripping digits, positional between
/// 1e-4 and 1e16, otherwise scientific with a signed two-digit exponent.
fn python_float(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() { "-0.0" } else { "0.0" }.to_owned();
    }
    // Rust's `{:e}` is also shortest-round-trip, e.g. `-1.5e-7`.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let (sign, mantissa) = mantissa.strip_prefix('-').map_or(("", mantissa), |rest| ("-", rest));
    let digits = mantissa.replace('.', "");
    // Where the decimal point falls relative to the digit string.
    let point = exponent + 1;
    if !(-4 < point && point <= 16) {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() { String::new() } else { format!(".{rest}") };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        return format!("{sign}{first}{fraction}e{exponent_sign}{:02}", exponent.unsigned_abs());
    }
    if point <= 0 {
        return format!("{sign}0.{}{digits}", "0".repeat(point.unsigned_abs() as usize));
    }
    let point = point as usize;
    if point >= digits.len() {
        format!("{sign}{digits}{}.0", "0".repeat(point - digits.len()))
    } else {
        format!("{sign}{}.{}", &digits[..point], &digits[point..])
    }
}

/// SHA-256 of a file streamed from disk; a missing file hashes as empty,
/// matching the empty trace a session has before its first model call.
pub fn file_sha256(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    match std::fs::File::open(path) {
        Ok(mut file) => {
            let mut buffer = vec![0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer).with_context(|| format!("could not read {}", path.display()))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("could not open {}", path.display())),
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// A session file on this device.
#[derive(Debug, Clone)]
pub struct LocalEntry {
    pub id: Uuid,
    pub path: PathBuf,
    pub trace: PathBuf,
}

/// Every session file under every workspace shard, by id. Sync is global: a
/// session belongs to the account, not to the directory it was started in.
pub fn local_sessions(paths: &AbacusPaths) -> HashMap<Uuid, LocalEntry> {
    let mut found: HashMap<Uuid, (LocalEntry, SystemTime)> = HashMap::new();
    let Ok(shards) = std::fs::read_dir(&paths.sessions_dir) else {
        return HashMap::new();
    };
    for shard in shards.flatten() {
        let Ok(files) = std::fs::read_dir(shard.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()).and_then(|stem| Uuid::parse_str(stem).ok())
            else {
                continue;
            };
            let modified = file.metadata().and_then(|meta| meta.modified()).unwrap_or(UNIX_EPOCH);
            // The same id in two shards means its workspace moved; the newer
            // file is the live one.
            if found.get(&id).is_some_and(|(_, seen)| *seen >= modified) {
                continue;
            }
            let trace = trace_path(paths, &id);
            found.insert(id, (LocalEntry { id, path, trace }, modified));
        }
    }
    found.into_iter().map(|(id, (entry, _))| (id, entry)).collect()
}

pub fn trace_path(paths: &AbacusPaths, id: &Uuid) -> PathBuf {
    paths.traces_dir.join(format!("{id}.jsonl"))
}

/// A local session read from disk, with the hashes of exactly what was read.
#[derive(Debug)]
pub struct Snapshot {
    pub document: Value,
    pub hashes: Hashes,
    pub fingerprint: Fingerprint,
}

impl Snapshot {
    pub fn read(entry: &LocalEntry) -> Result<Self> {
        let fingerprint = Fingerprint::take(&entry.path, &entry.trace);
        let content = std::fs::read(&entry.path).with_context(|| format!("could not read {}", entry.path.display()))?;
        let document: Value = serde_json::from_slice(&content).context("invalid session file")?;
        let hashes = Hashes::of(&document, file_sha256(&entry.trace)?);
        Ok(Self { document, hashes, fingerprint })
    }
}

/// How a local copy relates to the last synced revision.
#[derive(Debug)]
pub enum Local {
    /// Unchanged since the last sync. Carries a fresh fingerprint when the
    /// files had to be re-hashed to find that out, worth saving.
    Clean(Option<Fingerprint>),
    Dirty(Box<Snapshot>),
    /// No record: never synced from this device, or the state was lost.
    Untracked(Box<Snapshot>),
}

pub fn inspect(entry: &LocalEntry, record: Option<&SessionRecord>) -> Result<Local> {
    if let Some(record) = record
        && let Some(saved) = &record.fingerprint
        && saved.vouches_for(&Fingerprint::take(&entry.path, &entry.trace))
    {
        return Ok(Local::Clean(None));
    }
    let snapshot = Snapshot::read(entry)?;
    Ok(match record {
        None => Local::Untracked(Box::new(snapshot)),
        Some(record)
            if record.local_sha256 == snapshot.hashes.content && record.local_trace_sha256 == snapshot.hashes.trace =>
        {
            Local::Clean(Some(snapshot.fingerprint))
        }
        Some(_) => Local::Dirty(Box::new(snapshot)),
    })
}

/// A session that has never received a prompt is not a session yet. The
/// transcript always opens with the system prompt, so "empty" means no user
/// message has ever been added.
pub fn is_placeholder_document(document: &Value) -> bool {
    document["title"] == "New session"
        && !document["messages"].as_array().is_some_and(|messages| messages.iter().any(|m| m["role"] == "user"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    /// Generated with CPython 3.13:
    /// `json.dumps(doc, ensure_ascii=False, sort_keys=True, separators=(",", ":"))`.
    #[test]
    fn canonical_json_matches_python_byte_for_byte() {
        let document: Value = serde_json::from_str(
            r#"{"z": 1, "a": [1.0, 0.1, 1e16, 1.5e-7, 123456789012345.6, 1e15, 0.0001, 0.00001, -0.0, 100, -5, 18446744073709551615, 2.5e-320, 1.7976931348623157e308],
 "é": "naïve – “quotes” \"esc\" back\\slash \n\r\t\b\f \u0001\u001f\u007f   😀 /",
 "B": {"y": null, "x": true, "w": false, "": "empty key"}, "aa": [], "ab": {}, "Z": "upper"}"#,
        )
        .unwrap();
        let expected = "{\"B\":{\"\":\"empty key\",\"w\":false,\"x\":true,\"y\":null},\"Z\":\"upper\",\"a\":[1.0,0.1,1e+16,1.5e-07,123456789012345.6,1000000000000000.0,0.0001,1e-05,-0.0,100,-5,18446744073709551615,2.5e-320,1.7976931348623157e+308],\"aa\":[],\"ab\":{},\"z\":1,\"é\":\"naïve – “quotes” \\\"esc\\\" back\\\\slash \\n\\r\\t\\b\\f \\u0001\\u001f\u{7f} \u{2028} 😀 /\"}";
        assert_eq!(canonical_json(&document), expected);
        assert_eq!(session_sha256(&document), "9a9b0a93a966da41f7fcdad8ffa4c26880b9ca958482de9b44aa6e7a88226afb");
    }

    #[test]
    fn floats_print_like_python_repr() {
        for (value, expected) in [
            (1.0, "1.0"),
            (0.1, "0.1"),
            (1e16, "1e+16"),
            (1.5e-7, "1.5e-07"),
            (1e15, "1000000000000000.0"),
            (1234567890123456.0, "1234567890123456.0"),
            (12345678901234567.0, "1.2345678901234568e+16"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (5e-324, "5e-324"),
            (1e22, "1e+22"),
            (3.25, "3.25"),
            (-2.5, "-2.5"),
        ] {
            assert_eq!(python_float(value), expected, "{value}");
        }
    }

    #[test]
    fn content_hash_ignores_only_volatile_fields() {
        let base = json!({"id": "x", "title": "t", "messages": [{"role": "user", "content": "hi"}],
            "updated_at": "2026-01-01T00:00:00Z", "active_secs": 1, "tokens_used": 2, "version": 3});
        let mut reopened = base.clone();
        reopened["updated_at"] = json!("2026-02-02T00:00:00Z");
        reopened["active_secs"] = json!(99);
        assert_eq!(content_sha256(&base), content_sha256(&reopened));
        assert_ne!(session_sha256(&base), session_sha256(&reopened));
        let mut renamed = base.clone();
        renamed["title"] = json!("renamed");
        assert_ne!(content_sha256(&base), content_sha256(&renamed));
    }

    fn record(revision: u64) -> SessionRecord {
        SessionRecord {
            revision: Some(revision),
            session_sha256: "s".into(),
            local_sha256: "c".into(),
            trace_sha256: "t".into(),
            ..SessionRecord::default()
        }
    }

    #[test]
    fn state_round_trips_and_resets_for_another_account() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().to_path_buf());
        let id = Uuid::new_v4();
        let mut state = SyncState::load_for(&paths, "https://sync.example/", "me@example.com");
        state.set(&id, record(4));
        state.set_cursor(17);
        state.last_pull_at = Some(Utc::now());
        state.save().unwrap();
        assert_eq!(paths.sync_state_file(), paths.root.join("sync-state.json"));

        let loaded = SyncState::load_for(&paths, "https://sync.example", "ME@example.com");
        assert_eq!(loaded.record(&id), Some(&record(4)));
        assert_eq!(loaded.cursor, 17);
        assert!(loaded.last_pull_at.is_some());

        let other_account = SyncState::load_for(&paths, "https://sync.example", "someone@example.com");
        assert!(other_account.sessions.is_empty() && other_account.cursor == 0);
        let other_server = SyncState::load_for(&paths, "https://elsewhere.example", "me@example.com");
        assert!(other_server.sessions.is_empty());
    }

    #[test]
    fn corrupt_state_falls_back_to_a_fresh_one_and_is_kept_aside() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.root).unwrap();
        std::fs::write(paths.sync_state_file(), b"{ not json").unwrap();
        let mut state = SyncState::load_for(&paths, "https://sync.example", "me@example.com");
        assert!(state.sessions.is_empty() && state.cursor == 0);
        assert!(paths.sync_state_file().with_extension("json.corrupt").exists());
        state.set(&Uuid::new_v4(), record(1));
        state.save().unwrap();
        assert_eq!(SyncState::load_for(&paths, "https://sync.example", "me@example.com").sessions.len(), 1);
    }

    #[test]
    fn concurrent_saves_merge_by_record() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().to_path_buf());
        let (first_id, second_id) = (Uuid::new_v4(), Uuid::new_v4());
        let mut first = SyncState::load_for(&paths, "https://s", "a@b");
        let mut second = SyncState::load_for(&paths, "https://s", "a@b");
        first.set(&first_id, record(1));
        first.set_cursor(5);
        first.save().unwrap();
        second.set(&second_id, record(2));
        second.forget(&Uuid::new_v4());
        second.save().unwrap();
        let merged = SyncState::load_for(&paths, "https://s", "a@b");
        assert_eq!(merged.record(&first_id), Some(&record(1)));
        assert_eq!(merged.record(&second_id), Some(&record(2)));
        // The second process never moved the cursor, so it kept the first's.
        assert_eq!(merged.cursor, 5);
    }

    fn write_session(dir: &Path, id: &Uuid, document: &Value) -> LocalEntry {
        let path = dir.join(format!("{id}.json"));
        std::fs::write(&path, serde_json::to_vec_pretty(document).unwrap()).unwrap();
        LocalEntry { id: *id, path, trace: dir.join(format!("{id}.jsonl")) }
    }

    #[test]
    fn dirty_detection_follows_content_not_bookkeeping() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().to_path_buf());
        let id = Uuid::new_v4();
        let document = json!({"id": id.to_string(), "title": "t", "messages": [], "active_secs": 1});
        let entry = write_session(dir.path(), &id, &document);
        assert!(matches!(inspect(&entry, None).unwrap(), Local::Untracked(_)));

        let snapshot = Snapshot::read(&entry).unwrap();
        let mut state = SyncState::load_for(&paths, "https://s", "a@b");
        let meta = SessionMeta { id: id.to_string(), revision: 1, ..SessionMeta::default() };
        state.mark_synced(&id, &meta, &snapshot.hashes, None);
        assert!(!state.is_dirty(&id, &snapshot.hashes));
        let record = state.record(&id).unwrap().clone();
        let Local::Clean(Some(_)) = inspect(&entry, Some(&record)).unwrap() else {
            panic!("an unchanged session is clean, with a fingerprint worth saving");
        };

        // Reopening bumps only bookkeeping: still clean.
        let mut reopened = document.clone();
        reopened["active_secs"] = json!(500);
        write_session(dir.path(), &id, &reopened);
        assert!(matches!(inspect(&entry, Some(&record)).unwrap(), Local::Clean(_)));

        // A new message or a trace write is a real change.
        let mut continued = document.clone();
        continued["messages"] = json!([{"role": "user", "content": "more"}]);
        write_session(dir.path(), &id, &continued);
        assert!(matches!(inspect(&entry, Some(&record)).unwrap(), Local::Dirty(_)));
        write_session(dir.path(), &id, &document);
        std::fs::write(&entry.trace, b"{}\n").unwrap();
        let Local::Dirty(changed) = inspect(&entry, Some(&record)).unwrap() else { panic!("trace changed") };
        assert!(state.is_dirty(&id, &changed.hashes));

        // A fingerprint vouches only for files older than the racy window.
        let fresh = Fingerprint::take(&entry.path, &entry.trace);
        assert!(!fresh.vouches_for(&fresh));
        let settled = Fingerprint { taken_ns: fresh.taken_ns + 10_000_000_000, ..fresh };
        assert!(settled.vouches_for(&fresh));
    }

    #[test]
    fn placeholders_are_sessions_without_a_prompt() {
        assert!(is_placeholder_document(&json!({"title": "New session", "messages": [{"role": "system"}]})));
        assert!(!is_placeholder_document(&json!({"title": "New session", "messages": [{"role": "user"}]})));
        assert!(!is_placeholder_document(&json!({"title": "Renamed", "messages": []})));
    }
}
