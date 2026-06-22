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
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Ok(content) = fs::read(entry.path()) else {
                continue;
            };
            let Ok(session) = serde_json::from_slice::<Session>(&content) else {
                continue;
            };
            if session.workspace != self.workspace || session.version > SESSION_VERSION {
                continue;
            }
            sessions.push(SessionSummary {
                id: session.id,
                title: session.title,
                model: session.model,
                updated_at: session.updated_at,
                message_count: session.messages.len().saturating_sub(1),
            });
        }
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Ok(sessions)
    }

    /// Read the lightweight fields used by the local `/usage` dashboard.
    /// Older session files predate persisted token totals, so their transcript
    /// size provides a best-effort estimate instead of leaving the chart empty.
    pub fn usage(&self) -> Result<Vec<SessionUsage>> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        let mut usage = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Ok(content) = fs::read(entry.path()) else {
                continue;
            };
            let Ok(session) = serde_json::from_slice::<Session>(&content) else {
                continue;
            };
            if session.workspace != self.workspace || session.version > SESSION_VERSION {
                continue;
            }
            let tokens_estimated = session.tokens_used == 0 && session.messages.len() > 1;
            let tokens_used = if tokens_estimated {
                serde_json::to_vec(&session.messages)
                    .map(|messages| messages.len() as u64 / 4)
                    .unwrap_or(0)
            } else {
                session.tokens_used
            };
            usage.push(SessionUsage {
                id: session.id,
                model: session.model,
                created_at: session.created_at,
                updated_at: session.updated_at,
                message_count: session.messages.len().saturating_sub(1),
                tokens_used,
                tokens_estimated,
                active_secs: session.active_secs,
            });
        }
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
}
