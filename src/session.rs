use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::compaction::CompactionState;
use crate::config::{AbacusPaths, atomic_write};
use crate::goal::Goal;
use crate::ralph::RalphLoop;
use crate::task::Task;

const SESSION_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub id: Uuid,
    pub workspace: PathBuf,
    pub title: String,
    pub profile: String,
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub goal: Option<Goal>,
    #[serde(default)]
    pub ralph_loop: Option<RalphLoop>,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub compaction: Option<CompactionState>,
    /// Approximate provider-reported token total accumulated across resumes.
    #[serde(default)]
    pub tokens_used: u64,
    /// Time spent with this session open, accumulated across resumes.
    #[serde(default)]
    pub active_secs: u64,
}

impl Session {
    pub fn new(workspace: PathBuf, profile: String, model: String, messages: Vec<Value>) -> Self {
        let now = Utc::now();
        Self {
            version: SESSION_VERSION,
            id: Uuid::new_v4(),
            workspace,
            title: "New session".to_owned(),
            profile,
            model,
            created_at: now,
            updated_at: now,
            messages,
            goal: None,
            ralph_loop: None,
            tasks: Vec::new(),
            compaction: None,
            tokens_used: 0,
            active_secs: 0,
        }
    }

    pub fn update_messages(&mut self, messages: Vec<Value>) {
        self.messages = messages;
        self.updated_at = Utc::now();
        if self.title == "New session"
            && let Some(prompt) = self.messages.iter().find_map(|message| {
                (message["role"] == "user")
                    .then(|| message["content"].as_str())
                    .flatten()
            })
        {
            self.title = title_from_prompt(prompt);
        }
    }
}

/// A session read for listing: everything but the transcript itself. The
/// messages deserialize into `IgnoredAny`, so they are counted without being
/// built.
#[derive(Deserialize)]
struct SessionHeader {
    version: u32,
    id: Uuid,
    workspace: PathBuf,
    title: String,
    model: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    messages: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    tokens_used: u64,
    #[serde(default)]
    active_secs: u64,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: Uuid,
    pub title: String,
    pub model: String,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
}

#[derive(Debug, Clone)]
pub struct SessionUsage {
    pub id: Uuid,
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
    pub tokens_used: u64,
    pub tokens_estimated: bool,
    pub active_secs: u64,
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    directory: PathBuf,
    workspace: PathBuf,
}

impl SessionStore {
    pub fn new(paths: &AbacusPaths, workspace: PathBuf) -> Self {
        let directory = paths.sessions_dir.join(workspace_key(&workspace));
        Self {
            directory,
            workspace,
        }
    }

    pub fn create(&self, profile: String, model: String, messages: Vec<Value>) -> Result<Session> {
        let session = Session::new(self.workspace.clone(), profile, model, messages);
        self.save(&session)?;
        Ok(session)
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        fs::create_dir_all(&self.directory)?;
        let content = serde_json::to_vec_pretty(session).context("could not encode session")?;
        atomic_write(&self.path(session.id), &content, true)
    }

    pub fn load(&self, id_or_prefix: &str) -> Result<Session> {
        let summaries = self.list()?;
        let matches = summaries
            .iter()
            .filter(|session| session.id.to_string().starts_with(id_or_prefix))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("no session matches `{id_or_prefix}`"),
            [session] => self.load_exact(session.id),
            _ => bail!("session prefix `{id_or_prefix}` is ambiguous"),
        }
    }

    pub fn latest(&self) -> Result<Session> {
        let session = self
            .list()?
            .into_iter()
            .max_by_key(|session| session.updated_at)
            .context("no saved session exists for this workspace")?;
        self.load_exact(session.id)
    }

    pub fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut sessions = self
            .headers()?
            .into_iter()
            .map(|(header, _)| SessionSummary {
                id: header.id,
                title: header.title,
                model: header.model,
                updated_at: header.updated_at,
                message_count: header.messages.len().saturating_sub(1),
            })
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Ok(sessions)
    }

    /// Read every session in this workspace as a header only.
    ///
    /// Listing used to deserialize each file in full — every message of every
    /// past session — to show a title and a count. A long-lived workspace makes
    /// that megabytes of `Value` trees built and dropped each time `/sessions`
    /// or `/usage` opens. The messages are counted but never materialised.
    fn headers(&self) -> Result<Vec<(SessionHeader, u64)>> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        let mut headers = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            let Ok(content) = fs::read(entry.path()) else {
                continue;
            };
            let Ok(header) = serde_json::from_slice::<SessionHeader>(&content) else {
                continue;
            };
            if header.workspace != self.workspace || header.version > SESSION_VERSION {
                continue;
            }
            headers.push((header, size));
        }
        Ok(headers)
    }

    /// Read the lightweight fields used by the local `/usage` dashboard.
    /// Older session files predate persisted token totals, so their transcript
    /// size provides a best-effort estimate instead of leaving the chart empty.
    pub fn usage(&self) -> Result<Vec<SessionUsage>> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        let mut usage = self
            .headers()?
            .into_iter()
            .map(|(header, size)| {
                let tokens_estimated = header.tokens_used == 0 && header.messages.len() > 1;
                // Legacy sessions predate persisted totals. The file's size
                // stands in for the transcript's size, which is what the old
                // estimate measured anyway — without re-encoding it to find out.
                let tokens_used = if tokens_estimated {
                    size / 4
                } else {
                    header.tokens_used
                };
                SessionUsage {
                    id: header.id,
                    model: header.model,
                    created_at: header.created_at,
                    updated_at: header.updated_at,
                    message_count: header.messages.len().saturating_sub(1),
                    tokens_used,
                    tokens_estimated,
                    active_secs: header.active_secs,
                }
            })
            .collect::<Vec<_>>();
        usage.sort_by_key(|record| record.created_at);
        Ok(usage)
    }

    pub fn rename(&self, session: &mut Session, title: &str) -> Result<()> {
        let title = title.trim();
        if title.is_empty() {
            bail!("session title cannot be empty");
        }
        session.title = title.chars().take(100).collect();
        session.updated_at = Utc::now();
        self.save(session)
    }

    fn load_exact(&self, id: Uuid) -> Result<Session> {
        let path = self.path(id);
        let content = fs::read(&path)
            .with_context(|| format!("could not read session {}", path.display()))?;
        let mut session: Session =
            serde_json::from_slice(&content).context("invalid session file")?;
        if session.version > SESSION_VERSION {
            bail!("session requires a newer version of Abacus");
        }
        if session.workspace != self.workspace {
            bail!("session belongs to a different workspace");
        }
        session.version = SESSION_VERSION;
        Ok(session)
    }

    fn path(&self, id: Uuid) -> PathBuf {
        self.directory.join(format!("{id}.json"))
    }
}

fn title_from_prompt(prompt: &str) -> String {
    let one_line = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = one_line.chars().take(72).collect::<String>();
    if title.is_empty() {
        "New session".to_owned()
    } else if one_line.chars().count() > 72 {
        format!("{title}…")
    } else {
        title
    }
}

/// Fix message-history corruption left behind by interrupted or failed turns,
/// in place. Strict providers reject the whole request over a single bad entry
/// (a tool call whose streamed JSON arguments were cut mid-write, a call with
/// no recorded result, a result whose call is gone), which presents to the
/// user as every subsequent turn failing. Returns one line per fix applied;
/// empty means the history was already clean.
pub fn repair_messages(messages: &mut Vec<Value>) -> Vec<String> {
    let mut fixes = Vec::new();

    // Pass 1: drop tool calls whose arguments are not valid JSON. These are
    // always truncation artifacts — a model never legitimately emits half an
    // argument object — so nothing of value is lost, and the prose (if any)
    // on the same message is kept with a note marking the interruption.
    for message in messages.iter_mut() {
        let (dropped, now_empty) = {
            let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
                continue;
            };
            let before = calls.len();
            calls.retain(|call| {
                call.pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .is_some_and(|arguments| serde_json::from_str::<Value>(arguments).is_ok())
            });
            (before - calls.len(), calls.is_empty())
        };
        if dropped == 0 {
            continue;
        }
        fixes.push(format!(
            "removed {dropped} tool call{} with truncated arguments",
            if dropped == 1 { "" } else { "s" }
        ));
        let note = "[a tool call was interrupted before it ran; no file or command was executed]";
        let content = match message.get("content").and_then(Value::as_str) {
            Some(text) if !text.is_empty() => format!("{text}\n{note}"),
            _ => note.to_owned(),
        };
        message["content"] = Value::String(content);
        if now_empty && let Some(object) = message.as_object_mut() {
            object.remove("tool_calls");
        }
    }

    // Pass 2: give every surviving call a result. A call the agent never
    // answered (interrupt between tools) makes strict providers reject the
    // pairing, so a synthetic "interrupted" result is inserted directly after
    // the assistant message that made the call.
    let answered: std::collections::HashSet<String> = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|message| message.get("tool_call_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let mut index = 0;
    while index < messages.len() {
        let orphan_ids: Vec<String> = messages[index]
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| call.get("id").and_then(Value::as_str))
                    .filter(|id| !answered.contains(*id))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        for (offset, id) in orphan_ids.iter().enumerate() {
            messages.insert(
                index + 1 + offset,
                serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": "Error: interrupted before a result was recorded.",
                }),
            );
            fixes.push("added a missing result for an unanswered tool call".to_owned());
        }
        index += 1 + orphan_ids.len();
    }

    // Pass 3: drop tool results whose call no longer exists (e.g. the call
    // was removed by pass 1, or by an earlier compaction bug).
    let known_calls: std::collections::HashSet<String> = messages
        .iter()
        .filter_map(|message| message.get("tool_calls").and_then(Value::as_array))
        .flatten()
        .filter_map(|call| call.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let before = messages.len();
    messages.retain(|message| {
        if message.get("role").and_then(Value::as_str) != Some("tool") {
            return true;
        }
        match message.get("tool_call_id").and_then(Value::as_str) {
            Some(id) => known_calls.contains(id),
            None => false,
        }
    });
    let dropped = before - messages.len();
    if dropped > 0 {
        fixes.push(format!(
            "removed {dropped} tool result{} whose call no longer exists",
            if dropped == 1 { "" } else { "s" }
        ));
    }

    fixes
}

fn workspace_key(workspace: &std::path::Path) -> String {
    // Stable FNV-1a keeps workspace directories short without another runtime dependency.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in workspace.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let name = workspace
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("workspace")
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    format!("{name}-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn session_round_trip_and_prefix_resume() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        let store = SessionStore::new(&paths, workspace.canonicalize().unwrap());
        let mut session = store
            .create(
                "local".into(),
                "model".into(),
                vec![json!({"role":"system","content":"x"})],
            )
            .unwrap();
        session.update_messages(vec![
            json!({"role":"system","content":"x"}),
            json!({"role":"user","content":"Fix the parser without changing its API"}),
        ]);
        session.ralph_loop = Some(
            crate::ralph::RalphLoop::new("Keep fixing".into(), "DONE".into(), Some(5)).unwrap(),
        );
        session.tokens_used = 12_345;
        session.active_secs = 3_661;
        store.save(&session).unwrap();

        let loaded = store.load(&session.id.to_string()[..8]).unwrap();
        assert_eq!(loaded.title, "Fix the parser without changing its API");
        assert_eq!(loaded.tokens_used, 12_345);
        assert_eq!(loaded.active_secs, 3_661);
        assert_eq!(loaded.ralph_loop.unwrap().prompt, "Keep fixing");
        assert_eq!(store.latest().unwrap().id, session.id);
        let usage = store.usage().unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].tokens_used, 12_345);
        assert!(!usage[0].tokens_estimated);
    }

    #[test]
    fn repair_leaves_clean_history_untouched() {
        let mut messages = vec![
            json!({"role":"system","content":"s"}),
            json!({"role":"user","content":"u"}),
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"a","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"x\"}"}}
            ]}),
            json!({"role":"tool","tool_call_id":"a","content":"ok"}),
            json!({"role":"assistant","content":"done"}),
        ];
        let original = messages.clone();
        assert!(repair_messages(&mut messages).is_empty());
        assert_eq!(messages, original);
    }

    #[test]
    fn repair_removes_truncated_call_and_keeps_prose() {
        // The exact corruption an interrupt leaves behind: a call whose
        // streamed arguments were cut mid-string, with no tool result.
        let mut messages = vec![
            json!({"role":"user","content":"write it"}),
            json!({"role":"assistant","content":"Let me rewrite that.","tool_calls":[
                {"id":"cut","type":"function","function":{"name":"write_file","arguments":"{\"content\": \"unterminated"}}
            ]}),
            json!({"role":"user","content":"no, differently"}),
        ];
        let fixes = repair_messages(&mut messages);
        assert_eq!(fixes.len(), 1, "{fixes:?}");
        assert_eq!(messages.len(), 3);
        let repaired = &messages[1];
        assert!(repaired.get("tool_calls").is_none());
        let content = repaired["content"].as_str().unwrap();
        assert!(content.starts_with("Let me rewrite that."));
        assert!(content.contains("interrupted"));
    }

    #[test]
    fn repair_answers_orphan_call_and_drops_orphan_result() {
        let mut messages = vec![
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"unanswered","type":"function","function":{"name":"run_command","arguments":"{\"command\":\"ls\"}"}}
            ]}),
            json!({"role":"tool","tool_call_id":"ghost","content":"result of a vanished call"}),
        ];
        let fixes = repair_messages(&mut messages);
        assert_eq!(fixes.len(), 2, "{fixes:?}");
        assert_eq!(messages.len(), 2);
        // The valid-but-unanswered call gained a synthetic result right after it.
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "unanswered");
        assert!(
            messages[1]["content"]
                .as_str()
                .unwrap()
                .contains("interrupted")
        );
        // The ghost result is gone.
        assert!(!messages.iter().any(|m| m["tool_call_id"] == "ghost"));
    }

    #[test]
    fn repair_fixes_the_real_interrupt_shape_end_to_end() {
        // Truncated call on a message that also has valid calls: only the
        // broken one is removed, and the valid one keeps its pairing.
        let mut messages = vec![
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"good","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}},
                {"id":"bad","type":"function","function":{"name":"write_file","arguments":"{\"trunc"}}
            ]}),
            json!({"role":"tool","tool_call_id":"good","content":"contents"}),
        ];
        let fixes = repair_messages(&mut messages);
        assert!(!fixes.is_empty());
        let calls = messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "good");
        assert_eq!(messages[1]["tool_call_id"], "good");
        // Second run is a no-op: repair is idempotent.
        assert!(repair_messages(&mut messages).is_empty());
    }
}
