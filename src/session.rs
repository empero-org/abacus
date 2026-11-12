use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::compaction::CompactionState;
use crate::config::{AbacusPaths, atomic_write};
use crate::goal::Goal;
use crate::task::Task;

const SESSION_VERSION: u32 = 3;

/// What a conversation carries between turns: the state the model reads and
/// writes as it works. Restored from a saved session, handed to every turn,
/// and written back to the session when the turn ends. Every front end — the
/// TUI, a headless run, the app server — holds one of these.
#[derive(Clone, Default)]
pub struct SessionState {
    pub goal: crate::goal::GoalState,
    pub tasks: crate::task::TaskList,
    pub compaction: CompactionState,
    pub memories: crate::memories::MemoryStore,
    /// Large tool outputs bound as inspectable variables.
    pub handles: crate::handles::HandleStore,
    /// Mid-turn arrivals: user steering and finished background subagents.
    pub injections: crate::agent::InjectionQueue,
    /// How the session came to be open, until its next turn has told the
    /// model so (see [`opening_note`]).
    pub opened: Option<Opened>,
}

/// How a session came to be open, for the note its next turn starts with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Opened {
    /// Started here, with nothing said yet.
    New,
    /// Opened again from disk, after being left at this time.
    Resumed(DateTime<Utc>),
}

impl Opened {
    /// How a front end comes to have `session` open: resumed when there is
    /// one, new when there is not.
    pub fn of(session: Option<&Session>) -> Self {
        session.map_or(Self::New, |session| Self::Resumed(session.updated_at))
    }
}

impl SessionState {
    /// The state `session` left off with, or a fresh one.
    pub fn open(config: &crate::config::Config, session: Option<&Session>) -> Self {
        let memories = crate::memories::MemoryStore::load(
            config.paths.memories_file.clone(),
            &config.paths.harness_dir,
            &config.workspace,
        );
        Self {
            goal: crate::goal::GoalState::new(session.and_then(|session| session.goal.clone())),
            tasks: crate::task::TaskList::new(session.map(|session| session.tasks.clone()).unwrap_or_default()),
            compaction: session
                .and_then(|session| session.compaction.as_ref())
                .map(CompactionState::detached)
                .unwrap_or_default(),
            memories,
            handles: Default::default(),
            injections: Default::default(),
            // A session opened from disk says how only where it was resumed;
            // a fork, say, goes on as if never left.
            opened: session.is_none().then_some(Opened::New),
        }
    }

    /// The note the first turn after the session was started or resumed
    /// opens with, for one working in `workspace`; None after that.
    pub fn take_opening_note(&mut self, workspace: &Path) -> Option<Value> {
        self.opened.take().map(|opened| opening_note(opened, workspace))
    }

    /// Write this state into `session`, ready to be saved.
    pub fn save(&self, session: &mut Session) {
        session.goal = self.goal.snapshot();
        session.tasks = self.tasks.snapshot();
        session.compaction = Some(self.compaction.detached());
    }

    /// The options for a turn over this state, shaped by `config`.
    pub fn turn(
        &self,
        config: &crate::config::Config,
        services: std::sync::Arc<crate::services::AgentServices>,
    ) -> crate::agent::TurnOptions {
        crate::agent::TurnOptions {
            goal: self.goal.clone(),
            tasks: self.tasks.clone(),
            compaction: self.compaction.clone(),
            memories: self.memories.clone(),
            handles: self.handles.clone(),
            injections: self.injections.clone(),
            ..crate::agent::TurnOptions::for_config(config, services)
        }
    }
}

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
    pub goal: Option<Goal>,
    #[serde(default)]
    pub tasks: Vec<Task>,
    pub compaction: Option<CompactionState>,
    /// Approximate provider-reported token total accumulated across resumes.
    #[serde(default)]
    pub tokens_used: u64,
    /// Time spent with this session open, accumulated across resumes.
    #[serde(default)]
    pub active_secs: u64,
    /// The session this one was forked from, so forks can be found and shown
    /// under it. None for one started fresh, and for forks made before 0.7.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<Uuid>,
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
            tasks: Vec::new(),
            compaction: None,
            tokens_used: 0,
            active_secs: 0,
            forked_from: None,
        }
    }

    pub fn update_messages(&mut self, messages: Vec<Value>) {
        self.messages = messages;
        self.updated_at = Utc::now();
        if self.title == "New session"
            && let Some(prompt) = self
                .messages
                .iter()
                .filter(|message| message["role"] == "user")
                .map(|message| crate::text::content_text(&message["content"]))
                .find(|prompt| !prompt.trim().is_empty())
        {
            self.title = title_from_prompt(&prompt);
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
    #[serde(default)]
    forked_from: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: Uuid,
    pub title: String,
    pub model: String,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
    pub forked_from: Option<Uuid>,
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
        Self { directory, workspace }
    }

    pub fn create(&self, profile: String, model: String, messages: Vec<Value>) -> Result<Session> {
        let session = Session::new(self.workspace.clone(), profile, model, messages);
        self.save(&session)?;
        Ok(session)
    }

    /// Save a fork of `original` holding `messages`, and return it. A fork is
    /// a branch of the work, so what shapes the work — goal, loop, tasks, the
    /// rolling summary — carries over; time and token accounting start fresh.
    /// It remembers where it came from, so it can be found from there and
    /// shown under it.
    pub fn fork(&self, original: &Session, profile: String, model: String, messages: Vec<Value>) -> Result<Session> {
        let mut fork = Session::new(self.workspace.clone(), profile, model, messages);
        fork.title = format!("(fork) {}", original.title).chars().take(100).collect();
        fork.goal = original.goal.clone();
        fork.tasks = original.tasks.clone();
        fork.compaction = original.compaction.clone();
        fork.forked_from = Some(original.id);
        self.save(&fork)?;
        Ok(fork)
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        fs::create_dir_all(&self.directory)?;
        let content = serde_json::to_vec_pretty(session).context("could not encode session")?;
        atomic_write(&self.path(session.id), &content, true)
    }

    pub fn load(&self, id_or_prefix: &str) -> Result<Session> {
        let summaries = self.list()?;
        let matches =
            summaries.iter().filter(|session| session.id.to_string().starts_with(id_or_prefix)).collect::<Vec<_>>();
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
        Ok(summaries(self.headers()?))
    }

    /// Read every session in this workspace as a header only.
    ///
    /// Listing used to deserialize each file in full — every message of every
    /// past session — to show a title and a count. A long-lived workspace makes
    /// that megabytes of `Value` trees built and dropped each time `/sessions`
    /// or `/usage` opens. The messages are counted but never materialised.
    fn headers(&self) -> Result<Vec<(SessionHeader, u64)>> {
        read_headers(&self.directory, Some(&self.workspace))
    }

    /// Read the lightweight fields used by the local `/usage` dashboard.
    /// Older session files predate persisted token totals, so their transcript
    /// size provides a best-effort estimate instead of leaving the chart empty.
    pub fn usage(&self) -> Result<Vec<SessionUsage>> {
        let mut usage = self
            .headers()?
            .into_iter()
            .map(|(header, size)| {
                let tokens_estimated = header.tokens_used == 0 && header.messages.len() > 1;
                // Legacy sessions predate persisted totals. The file's size
                // stands in for the transcript's size, which is what the old
                // estimate measured anyway — without re-encoding it to find out.
                let tokens_used = if tokens_estimated { size / 4 } else { header.tokens_used };
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

    pub fn delete(&self, id: Uuid) -> Result<()> {
        let path = self.path(id);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("could not remove session file {}", path.display()))?;
        }
        Ok(())
    }

    fn load_exact(&self, id: Uuid) -> Result<Session> {
        let path = self.path(id);
        let content = fs::read(&path).with_context(|| format!("could not read session {}", path.display()))?;
        let mut session: Session = serde_json::from_slice(&content).context("invalid session file")?;
        if session.version > SESSION_VERSION {
            bail!("session requires a newer version of Abacus");
        }
        if session.workspace != self.workspace {
            bail!("session belongs to a different workspace");
        }
        session.version = SESSION_VERSION;
        Ok(session)
    }

    pub fn path(&self, id: Uuid) -> PathBuf {
        self.directory.join(format!("{id}.json"))
    }
}

/// One workspace's saved sessions, newest first.
#[derive(Debug, Clone)]
pub struct WorkspaceSessions {
    pub workspace: PathBuf,
    pub sessions: Vec<SessionSummary>,
}

/// Every workspace with saved sessions, the most recently active first. A
/// front end lists these as projects without opening each one.
pub fn list_all(paths: &AbacusPaths) -> Result<Vec<WorkspaceSessions>> {
    if !paths.sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let mut workspaces = Vec::new();
    for entry in fs::read_dir(&paths.sessions_dir)? {
        let entry = entry?;
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        // One unreadable directory must not hide every other workspace.
        let Ok(headers) = read_headers(&entry.path(), None) else {
            continue;
        };
        let Some(workspace) = headers.first().map(|(header, _)| header.workspace.clone()) else {
            continue;
        };
        // The directory is keyed by workspace, so its sessions share one; a
        // stray file from elsewhere is left to that workspace's own listing.
        let headers = headers.into_iter().filter(|(header, _)| header.workspace == workspace).collect();
        workspaces.push(WorkspaceSessions { workspace, sessions: summaries(headers) });
    }
    workspaces.sort_by_key(|entry| std::cmp::Reverse(entry.sessions.first().map(|session| session.updated_at)));
    Ok(workspaces)
}

fn summaries(headers: Vec<(SessionHeader, u64)>) -> Vec<SessionSummary> {
    let mut sessions = headers
        .into_iter()
        .map(|(header, _)| SessionSummary {
            id: header.id,
            title: header.title,
            model: header.model,
            updated_at: header.updated_at,
            message_count: header.messages.len().saturating_sub(1),
            forked_from: header.forked_from,
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    sessions
}

/// Session headers in one workspace directory; `workspace`, when given, skips
/// files that belong to another (a hash collision, or a moved file).
fn read_headers(directory: &Path, workspace: Option<&Path>) -> Result<Vec<(SessionHeader, u64)>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut headers = Vec::new();
    for entry in fs::read_dir(directory)? {
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
        if workspace.is_some_and(|workspace| header.workspace != workspace) || header.version > SESSION_VERSION {
            continue;
        }
        headers.push((header, size));
    }
    Ok(headers)
}

fn title_from_prompt(prompt: &str) -> String {
    let title = crate::text::clip(&crate::text::squeeze(prompt), 72, "…");
    if title.is_empty() { "New session".to_owned() } else { title }
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

/// How the part of an [`opening_note`] about a resume opens.
const RESUME_TAG: &str = "<resume_info>";

/// What the model is told on the first turn after a session was started or
/// resumed: where it runs (see [`crate::environment`]), and for a resume,
/// when it was left, when it goes on, and that what it saw before may no
/// longer hold. It goes in the history as a system message, before the
/// person's message, so later turns still know.
pub fn opening_note(opened: Opened, workspace: &Path) -> Value {
    let environment = crate::environment::block(workspace);
    let content = match opened {
        Opened::New => environment,
        Opened::Resumed(exited) => format!("{}\n{environment}", resume_text(exited, Utc::now(), &chrono::Local)),
    };
    serde_json::json!({"role": "system", "content": content})
}

/// Whether `text` is an [`opening_note`]'s.
pub fn is_opening_note(text: &str) -> bool {
    text.starts_with(RESUME_TAG) || text.starts_with(crate::environment::TAG)
}

fn resume_text<Zone: chrono::TimeZone>(exited: DateTime<Utc>, now: DateTime<Utc>, zone: &Zone) -> String
where
    Zone::Offset: std::fmt::Display,
{
    let local = |at: DateTime<Utc>| at.with_timezone(zone).format("%d.%m.%Y %H:%M").to_string();
    format!(
        "{RESUME_TAG}\n  <session_exit>{}</session_exit>\n  <resume_now>{}</resume_now>\n  \
         <time_away>{}</time_away>\n  <note>This session was closed and has now been resumed. Time has \
         passed since the conversation above, and things may have changed in the meantime: files may have \
         been edited, branches switched or commits made; processes and servers that were running may have \
         stopped, restarted or been replaced; background tasks, services and anything else outside this \
         conversation may be different. Do not assume the earlier state is still exact. Check what matters \
         before relying on it.</note>\n</resume_info>",
        local(exited),
        local(now),
        time_away((now - exited).num_seconds()),
    )
}

/// `seconds` in words, to the two largest units: "5 days, 14 hours".
fn time_away(seconds: i64) -> String {
    let unit = |count: i64, name: &str| format!("{count} {name}{}", if count == 1 { "" } else { "s" });
    let (days, hours, minutes) = (seconds / 86_400, seconds % 86_400 / 3_600, seconds % 3_600 / 60);
    match (days, hours, minutes) {
        _ if seconds < 60 => "less than a minute".to_owned(),
        (0, 0, minutes) => unit(minutes, "minute"),
        (0, hours, 0) => unit(hours, "hour"),
        (0, hours, minutes) => format!("{}, {}", unit(hours, "hour"), unit(minutes, "minute")),
        (days, 0, _) => unit(days, "day"),
        (days, hours, _) => format!("{}, {}", unit(days, "day"), unit(hours, "hour")),
    }
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
        let mut session =
            store.create("local".into(), "model".into(), vec![json!({"role":"system","content":"x"})]).unwrap();
        session.update_messages(vec![
            json!({"role":"system","content":"x"}),
            json!({"role":"user","content":"Fix the parser without changing its API"}),
        ]);
        session.goal = Some(Goal::new("Keep fixing").unwrap());
        session.tokens_used = 12_345;
        session.active_secs = 3_661;
        store.save(&session).unwrap();

        let loaded = store.load(&session.id.to_string()[..8]).unwrap();
        assert_eq!(loaded.title, "Fix the parser without changing its API");
        assert_eq!(loaded.tokens_used, 12_345);
        assert_eq!(loaded.active_secs, 3_661);
        assert_eq!(loaded.goal.unwrap().objective, "Keep fixing");
        assert_eq!(store.latest().unwrap().id, session.id);
        let usage = store.usage().unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].tokens_used, 12_345);
        assert!(!usage[0].tokens_estimated);
    }

    /// A fork carries the work over and remembers where it came from, so
    /// listings can find it from there and front ends can nest it.
    #[test]
    fn a_fork_remembers_its_origin() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        let store = SessionStore::new(&paths, dir.path().join("project"));
        let mut original = store.create("p".into(), "m".into(), vec![json!({"role":"system","content":"x"})]).unwrap();
        original.title = "Fix the parser".into();
        original.tasks = vec![crate::task::Task { text: "keep the parser's API".into(), done: false }];
        store.save(&original).unwrap();

        let fork = store.fork(&original, "p".into(), "m".into(), original.messages.clone()).unwrap();
        assert_eq!(fork.title, "(fork) Fix the parser");
        assert_eq!(fork.tasks.len(), 1, "the work carries over");
        let listed = store.list().unwrap();
        let forked = listed.iter().find(|summary| summary.id == fork.id).unwrap();
        assert_eq!(forked.forked_from, Some(original.id));
        assert_eq!(listed.iter().find(|summary| summary.id == original.id).unwrap().forked_from, None);
        // A session saved before forks were linked reads as unlinked.
        let saved = fs::read_to_string(store.path(original.id)).unwrap();
        assert!(!saved.contains("forked_from"), "an unlinked session writes no field");
    }

    #[test]
    fn list_all_groups_sessions_by_workspace_newest_first() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        assert!(list_all(&paths).unwrap().is_empty());

        let older = dir.path().join("older");
        let newer = dir.path().join("newer");
        let system = vec![json!({"role":"system","content":"x"})];
        let first = SessionStore::new(&paths, older.clone()).create("p".into(), "m".into(), system.clone()).unwrap();
        let mut second = SessionStore::new(&paths, newer.clone()).create("p".into(), "m".into(), system).unwrap();
        second.updated_at = first.updated_at + chrono::Duration::seconds(5);
        SessionStore::new(&paths, newer.clone()).save(&second).unwrap();

        let all = list_all(&paths).unwrap();
        let workspaces: Vec<&Path> = all.iter().map(|entry| entry.workspace.as_path()).collect();
        assert_eq!(workspaces, [newer.as_path(), older.as_path()]);
        assert_eq!(all[0].sessions[0].id, second.id);
        assert_eq!(all[1].sessions[0].id, first.id);
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
        assert!(messages[1]["content"].as_str().unwrap().contains("interrupted"));
        // The ghost result is gone.
        assert!(!messages.iter().any(|m| m["tool_call_id"] == "ghost"));
    }

    #[test]
    fn session_store_delete_removes_file() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        let store = SessionStore::new(&paths, workspace.canonicalize().unwrap());
        let session =
            store.create("local".into(), "model".into(), vec![json!({"role":"user","content":"hi"})]).unwrap();
        assert!(store.load(&session.id.to_string()).is_ok());
        store.delete(session.id).unwrap();
        assert!(store.load(&session.id.to_string()).is_err());
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

    #[test]
    fn a_resumed_session_is_told_when_it_was_left_and_that_things_may_have_changed() {
        let exited = "1972-04-01T13:28:00Z".parse::<DateTime<Utc>>().unwrap();
        let now = "1972-04-07T04:01:30Z".parse::<DateTime<Utc>>().unwrap();
        let text = resume_text(exited, now, &Utc);
        assert!(is_opening_note(&text));
        assert!(text.contains("<session_exit>01.04.1972 13:28</session_exit>"), "{text}");
        assert!(text.contains("<resume_now>07.04.1972 04:01</resume_now>"), "{text}");
        assert!(text.contains("<time_away>5 days, 14 hours</time_away>"), "{text}");
        assert!(text.contains("processes and servers that were running"), "{text}");
        assert!(text.contains("Do not assume the earlier state is still exact"), "{text}");
        assert!(text.ends_with("</resume_info>"));
    }

    #[test]
    fn time_away_reads_in_the_two_largest_units() {
        let cases = [
            (-30, "less than a minute"),
            (59, "less than a minute"),
            (60, "1 minute"),
            (3_599, "59 minutes"),
            (3_600, "1 hour"),
            (2 * 3_600 + 60, "2 hours, 1 minute"),
            (86_400 + 59, "1 day"),
            (3 * 86_400 + 3_600, "3 days, 1 hour"),
        ];
        for (seconds, words) in cases {
            assert_eq!(time_away(seconds), words, "{seconds}s");
        }
    }

    #[test]
    fn a_started_session_says_where_it_runs_and_a_resumed_one_also_when_it_was_left() {
        let workspace = Path::new("/work/project");
        let started = opening_note(Opened::New, workspace);
        let started = started["content"].as_str().unwrap();
        assert!(started.starts_with("<environment>") && !started.contains("<resume_info>"), "{started}");
        assert!(is_opening_note(started));

        let resumed = opening_note(Opened::Resumed(Utc::now() - chrono::Duration::hours(3)), workspace);
        assert_eq!(resumed["role"], "system");
        let resumed = resumed["content"].as_str().unwrap();
        assert!(resumed.starts_with("<resume_info>"), "{resumed}");
        assert!(resumed.contains("<time_away>3 hours</time_away>"), "{resumed}");
        assert!(resumed.contains("</resume_info>\n<environment>"), "{resumed}");
        assert!(resumed.contains("<cwd>/work/project</cwd>"), "{resumed}");
    }

    #[test]
    fn only_the_first_turn_after_a_start_or_resume_carries_the_note() {
        let workspace = Path::new("/work");
        let mut state = SessionState { opened: Some(Opened::Resumed(Utc::now())), ..Default::default() };
        let note = state.take_opening_note(workspace).expect("the first turn's note");
        assert!(is_opening_note(note["content"].as_str().unwrap()));
        assert!(state.take_opening_note(workspace).is_none());
        assert_eq!(Opened::of(None), Opened::New);
    }
}
