//! Durable memories the agent writes for its future self.
//!
//! Papercuts capture failures with tripwires; memories capture everything
//! else worth keeping — architecture facts, decisions and their reasons,
//! conventions, roadmap changes, things figured out the hard way. They are
//! injected as a context layer at the start of every turn, and the model is
//! encouraged (via the system prompt and the rethink pass) to record, update,
//! and forget them itself: memory curation is part of the job, not a side
//! effect.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

/// Ceiling on the injected context layer — memories must inform a turn, not
/// crowd it out.
const INJECT_BUDGET_CHARS: usize = 3_000;
/// Newest-first cap on how many memories the layer holds.
const INJECT_LIMIT: usize = 12;
/// A single memory body is bounded so one verbose entry cannot monopolise the
/// whole injection budget.
const MAX_BODY_CHARS: usize = 1_200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: Uuid,
    pub title: String,
    pub body: String,
    /// `None` applies everywhere; otherwise the canonical workspace path.
    #[serde(default)]
    pub workspace: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Default)]
struct Inner {
    memories: Vec<Memory>,
    /// `None` disables persistence — the inert store subagents get.
    file: Option<PathBuf>,
}

/// Shared, cloneable handle following the `PapercutStore` pattern.
/// `Default` yields an inert in-memory store.
#[derive(Clone, Default)]
pub struct MemoryStore {
    inner: Arc<RwLock<Inner>>,
    workspace: String,
}

impl MemoryStore {
    pub fn load(file: PathBuf, workspace: &std::path::Path) -> Self {
        let memories = std::fs::read_to_string(&file)
            .ok()
            .and_then(|content| serde_json::from_str::<Vec<Memory>>(&content).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(RwLock::new(Inner {
                memories,
                file: Some(file),
            })),
            workspace: workspace.to_string_lossy().into_owned(),
        }
    }

    fn save_locked(inner: &Inner) {
        if let Some(file) = &inner.file
            && let Ok(serialized) = serde_json::to_vec_pretty(&inner.memories)
        {
            let _ = crate::config::atomic_write(file, &serialized, false);
        }
    }

    pub fn tool_specs() -> Vec<Value> {
        vec![
            json!({
                "type": "function",
                "function": {
                    "name": "memory_record",
                    "description": "Save a durable memory for future sessions: an architecture fact, a decision and its reason, a convention, a roadmap change, or something figured out the hard way. Re-recording an existing title replaces its body — use that to keep memories current. Not for failure lessons; those go to papercut_record.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string", "description": "Short stable name, e.g. 'auth flow uses one-shot tokens'"},
                            "body": {"type": "string", "description": "The memory itself, concise and self-contained (max ~1200 chars)"},
                            "scope": {"type": "string", "enum": ["workspace", "global"], "description": "workspace (default) or global"}
                        },
                        "required": ["title", "body"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "memory_list",
                    "description": "List the stored memories for this workspace, with their titles as handles for memory_record (update) and memory_forget.",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "memory_forget",
                    "description": "Delete a memory that is stale or wrong, by its exact title.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string", "description": "Exact title of the memory to delete"}
                        },
                        "required": ["title"]
                    }
                }
            }),
        ]
    }

    /// Virtual-tool dispatch, mirroring `GoalState::execute`.
    pub fn execute(&self, name: &str, arguments: &str) -> Option<String> {
        match name {
            "memory_record" => Some(match self.record_from_arguments(arguments) {
                Ok(message) => message,
                Err(error) => format!("Error: {error:#}"),
            }),
            "memory_list" => Some(self.list_for_model()),
            "memory_forget" => Some(match self.forget_from_arguments(arguments) {
                Ok(message) => message,
                Err(error) => format!("Error: {error:#}"),
            }),
            _ => None,
        }
    }

    fn record_from_arguments(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Arguments {
            title: String,
            body: String,
            #[serde(default)]
            scope: Option<String>,
        }
        let arguments: Arguments =
            serde_json::from_str(arguments).context("invalid memory_record arguments")?;
        let title = arguments.title.trim().to_owned();
        if title.is_empty() || title.chars().count() > 120 {
            bail!("title must be 1-120 characters");
        }
        let mut body = arguments.body.trim().to_owned();
        if body.is_empty() {
            bail!("body must not be empty");
        }
        if body.chars().count() > MAX_BODY_CHARS {
            let mut boundary = body
                .char_indices()
                .nth(MAX_BODY_CHARS)
                .map(|(index, _)| index)
                .unwrap_or(body.len());
            while !body.is_char_boundary(boundary) {
                boundary -= 1;
            }
            body.truncate(boundary);
            body.push('…');
        }
        let workspace = match arguments.scope.as_deref() {
            Some("global") => None,
            _ => Some(self.workspace.clone()),
        };

        let mut inner = self.inner.write().expect("memory lock");
        if let Some(existing) = inner.memories.iter_mut().find(|memory| {
            memory.title.eq_ignore_ascii_case(&title) && memory.workspace == workspace
        }) {
            existing.body = body;
            existing.updated_at = Utc::now();
            let title = existing.title.clone();
            Self::save_locked(&inner);
            return Ok(format!("Memory \"{title}\" updated."));
        }
        let now = Utc::now();
        inner.memories.push(Memory {
            id: Uuid::new_v4(),
            title: title.clone(),
            body,
            workspace,
            created_at: now,
            updated_at: now,
        });
        Self::save_locked(&inner);
        Ok(format!("Memory \"{title}\" recorded."))
    }

    fn forget_from_arguments(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Arguments {
            title: String,
        }
        let arguments: Arguments =
            serde_json::from_str(arguments).context("invalid memory_forget arguments")?;
        let title = arguments.title.trim();
        let workspace = self.workspace.clone();
        let mut inner = self.inner.write().expect("memory lock");
        let before = inner.memories.len();
        inner.memories.retain(|memory| {
            !(memory.title.eq_ignore_ascii_case(title) && memory.in_scope(&workspace))
        });
        if inner.memories.len() == before {
            bail!("no memory titled `{title}` in this workspace");
        }
        Self::save_locked(&inner);
        Ok(format!("Memory \"{title}\" forgotten."))
    }

    fn list_for_model(&self) -> String {
        let inner = self.inner.read().expect("memory lock");
        let mut lines: Vec<String> = inner
            .memories
            .iter()
            .filter(|memory| memory.in_scope(&self.workspace))
            .map(|memory| format!("- {} — {}", memory.title, memory.body))
            .collect();
        if lines.is_empty() {
            return "No memories stored for this workspace yet.".to_owned();
        }
        lines.insert(0, "Stored memories:".to_owned());
        lines.join("\n")
    }

    /// The context layer injected at the start of every turn: newest first,
    /// bounded by count and characters so memories inform without crowding.
    pub fn prompt_context(&self) -> String {
        let inner = self.inner.read().expect("memory lock");
        let mut in_scope: Vec<&Memory> = inner
            .memories
            .iter()
            .filter(|memory| memory.in_scope(&self.workspace))
            .collect();
        if in_scope.is_empty() {
            return String::new();
        }
        in_scope.sort_by_key(|memory| std::cmp::Reverse(memory.updated_at));
        let mut context = String::from(
            "Memories from earlier sessions (keep them current with memory_record / memory_forget):",
        );
        let mut used = 0_usize;
        for memory in in_scope.into_iter().take(INJECT_LIMIT) {
            let line = format!("\n- {}: {}", memory.title, memory.body);
            if used + line.len() > INJECT_BUDGET_CHARS {
                break;
            }
            used += line.len();
            context.push_str(&line);
        }
        context
    }

    /// Everything in scope, for `/memories`.
    pub fn snapshot(&self) -> Vec<Memory> {
        let inner = self.inner.read().expect("memory lock");
        inner
            .memories
            .iter()
            .filter(|memory| memory.in_scope(&self.workspace))
            .cloned()
            .collect()
    }

    /// Delete by id, for `/memories delete`.
    pub fn remove(&self, id: Uuid) -> bool {
        let mut inner = self.inner.write().expect("memory lock");
        let before = inner.memories.len();
        inner.memories.retain(|memory| memory.id != id);
        let removed = inner.memories.len() != before;
        if removed {
            Self::save_locked(&inner);
        }
        removed
    }
}

impl Memory {
    fn in_scope(&self, workspace: &str) -> bool {
        self.workspace
            .as_deref()
            .is_none_or(|scope| scope == workspace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &std::path::Path) -> MemoryStore {
        MemoryStore::load(dir.join("memories.json"), dir)
    }

    fn record(store: &MemoryStore, title: &str, body: &str) -> String {
        store
            .execute(
                "memory_record",
                &json!({"title": title, "body": body}).to_string(),
            )
            .expect("memory_record handled")
    }

    #[test]
    fn records_injects_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        assert!(record(&store, "auth flow", "tokens are one-shot").contains("recorded"));
        let context = store.prompt_context();
        assert!(
            context.contains("auth flow: tokens are one-shot"),
            "{context}"
        );

        let reloaded = MemoryStore::load(dir.path().join("memories.json"), dir.path());
        assert_eq!(reloaded.snapshot().len(), 1);
    }

    #[test]
    fn re_recording_updates_and_forget_removes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        record(&store, "auth flow", "v1");
        assert!(record(&store, "AUTH FLOW", "v2").contains("updated"));
        let snapshot = store.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].body, "v2");

        let output = store
            .execute("memory_forget", &json!({"title": "auth flow"}).to_string())
            .unwrap();
        assert!(output.contains("forgotten"), "{output}");
        assert!(store.snapshot().is_empty());
        // Forgetting again reports the miss instead of silently succeeding.
        let output = store
            .execute("memory_forget", &json!({"title": "auth flow"}).to_string())
            .unwrap();
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[test]
    fn injection_respects_budget_and_recency_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        for index in 0..30 {
            record(&store, &format!("memory {index}"), &"x".repeat(400));
        }
        let context = store.prompt_context();
        assert!(context.len() <= INJECT_BUDGET_CHARS + 200);
        // Newest first: the last recorded memory leads the layer.
        assert!(
            context.contains("memory 29"),
            "newest memory must be present"
        );
        assert!(!context.contains("memory 0:"), "oldest must be dropped");
    }

    #[test]
    fn workspace_scoping_hides_other_projects() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        record(&store, "local fact", "only here");
        let other = MemoryStore::load(
            dir.path().join("memories.json"),
            std::path::Path::new("/somewhere/else"),
        );
        assert!(other.snapshot().is_empty());
        assert!(other.prompt_context().is_empty());
    }

    #[test]
    fn oversized_bodies_are_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        record(&store, "big", &"y".repeat(5_000));
        let body = &store.snapshot()[0].body;
        assert!(body.chars().count() <= MAX_BODY_CHARS + 1);
        assert!(body.ends_with('…'));
    }
}
