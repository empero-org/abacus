//! Papercuts: lessons Abacus learns from its own snags.
//!
//! When the agent hits an error, a repeated tool failure, or a dead end and
//! then works out the fix, it records the lesson — a title, what went wrong,
//! the fix that worked, optional references, and **tripwires**: distinctive
//! strings from the failure that identify the same snag when it happens again.
//!
//! Recall is trigger-driven and frequency-adaptive. Every tool call's
//! arguments and output are scanned against the tripwires; a match counts as
//! an *encounter* and strengthens the papercut, and — subject to a cooldown
//! that shrinks as strength grows — injects the lesson into the tool result
//! right where the model is looking. Repeated failures and blocked loops
//! force-recall the strongest lessons regardless of cooldown. Strength decays
//! with a two-week half-life, so a papercut that stops being encountered
//! fades to an occasional reminder instead of permanent noise.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

/// Strength decays by half every two weeks without an encounter.
const HALF_LIFE_DAYS: f64 = 14.0;
/// A papercut never fully disappears; it can always be tripped back awake.
const MIN_STRENGTH: f64 = 0.2;
/// Cooldown between reminders: 4 hours at strength 1, shrinking with
/// strength to a 5-minute floor for lessons that keep being needed.
const BASE_COOLDOWN_MINUTES: f64 = 240.0;
const MIN_COOLDOWN_MINUTES: f64 = 5.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Papercut {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub fix: String,
    #[serde(default)]
    pub references: Vec<String>,
    pub tripwires: Vec<String>,
    /// `None` applies everywhere; otherwise the canonical workspace path the
    /// lesson belongs to.
    #[serde(default)]
    pub workspace: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default = "default_strength")]
    pub strength: f64,
    #[serde(default)]
    pub trip_count: u32,
    #[serde(default)]
    pub recall_count: u32,
    #[serde(default)]
    pub last_tripped_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_recalled_at: Option<DateTime<Utc>>,
}

fn default_strength() -> f64 {
    1.0
}

impl Papercut {
    /// Strength with time decay applied: halves per `HALF_LIFE_DAYS` since the
    /// last encounter, floored so old lessons stay recallable.
    pub fn decayed_strength(&self, now: DateTime<Utc>) -> f64 {
        let since = self.last_tripped_at.unwrap_or(self.created_at);
        let days = (now - since).num_seconds().max(0) as f64 / 86_400.0;
        (self.strength * 0.5_f64.powf(days / HALF_LIFE_DAYS)).max(MIN_STRENGTH)
    }

    fn cooldown(&self, now: DateTime<Utc>) -> Duration {
        let minutes =
            (BASE_COOLDOWN_MINUTES / self.decayed_strength(now)).max(MIN_COOLDOWN_MINUTES);
        Duration::seconds((minutes * 60.0) as i64)
    }

    fn off_cooldown(&self, now: DateTime<Utc>) -> bool {
        self.last_recalled_at
            .is_none_or(|last| now - last >= self.cooldown(now))
    }

    fn matches(&self, haystack_lower: &str) -> bool {
        self.tripwires
            .iter()
            .any(|tripwire| haystack_lower.contains(&tripwire.to_ascii_lowercase()))
    }

    fn in_scope(&self, workspace: &str) -> bool {
        self.workspace
            .as_deref()
            .is_none_or(|scope| scope == workspace)
    }

    /// The reminder as the model sees it, inline in a tool result.
    fn reminder(&self) -> String {
        let mut text = format!("[papercut] {} — fix: {}", self.title, self.fix);
        if !self.references.is_empty() {
            text.push_str(&format!(" (see {})", self.references.join(", ")));
        }
        text
    }
}

#[derive(Default)]
struct Inner {
    papercuts: Vec<Papercut>,
    /// `None` disables persistence — the inert store subagents get.
    file: Option<PathBuf>,
}

/// Shared, cloneable handle, following the `GoalState` pattern so it can ride
/// in `TurnOptions` across tasks. `Default` yields an inert in-memory store.
#[derive(Clone, Default)]
pub struct PapercutStore {
    inner: Arc<RwLock<Inner>>,
    workspace: String,
}

impl PapercutStore {
    /// Load the store, applying time decay to every entry.
    pub fn load(file: PathBuf, workspace: &std::path::Path) -> Self {
        let papercuts = std::fs::read_to_string(&file)
            .ok()
            .and_then(|content| serde_json::from_str::<Vec<Papercut>>(&content).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(RwLock::new(Inner {
                papercuts,
                file: Some(file),
            })),
            workspace: workspace.to_string_lossy().into_owned(),
        }
    }

    fn save_locked(inner: &Inner) {
        if let Some(file) = &inner.file
            && let Ok(serialized) = serde_json::to_vec_pretty(&inner.papercuts)
        {
            let _ = crate::config::atomic_write(file, &serialized, false);
        }
    }

    pub fn tool_specs() -> Vec<Value> {
        vec![
            json!({
                "type": "function",
                "function": {
                    "name": "papercut_record",
                    "description": "Record a lesson learned from a snag you just worked through: what went wrong, the fix that worked, and tripwires — distinctive strings from the error output that will identify the same snag next time. Call this after recovering from repeated failures or a non-obvious error so future sessions get the fix immediately.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string", "description": "Short name for the lesson, e.g. 'sqlx tests need DATABASE_URL'"},
                            "description": {"type": "string", "description": "What went wrong and why"},
                            "fix": {"type": "string", "description": "The fix that actually worked, as an instruction"},
                            "references": {"type": "array", "items": {"type": "string"}, "description": "Optional file paths, URLs, or commands worth consulting"},
                            "tripwires": {"type": "array", "items": {"type": "string"}, "description": "1-6 distinctive substrings of the failure (error text, flag names) that should trigger this reminder. Each at least 6 characters; avoid generic words."},
                            "scope": {"type": "string", "enum": ["workspace", "global"], "description": "workspace (default) limits recall to this project; global recalls everywhere"}
                        },
                        "required": ["title", "description", "fix", "tripwires"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "papercut_list",
                    "description": "List the recorded papercuts (lessons from past snags) relevant to this workspace.",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
        ]
    }

    /// Virtual-tool dispatch, mirroring `GoalState::execute`.
    pub fn execute(&self, name: &str, arguments: &str) -> Option<String> {
        match name {
            "papercut_record" => Some(match self.record_from_arguments(arguments) {
                Ok(message) => message,
                Err(error) => format!("Error: {error:#}"),
            }),
            "papercut_list" => Some(self.list_for_model()),
            _ => None,
        }
    }

    fn record_from_arguments(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Arguments {
            title: String,
            description: String,
            fix: String,
            #[serde(default)]
            references: Vec<String>,
            tripwires: Vec<String>,
            #[serde(default)]
            scope: Option<String>,
        }
        let arguments: Arguments =
            serde_json::from_str(arguments).context("invalid papercut_record arguments")?;
        let title = arguments.title.trim().to_owned();
        if title.is_empty() || title.chars().count() > 120 {
            bail!("title must be 1-120 characters");
        }
        if arguments.description.trim().is_empty() || arguments.fix.trim().is_empty() {
            bail!("description and fix must not be empty");
        }
        let tripwires: Vec<String> = arguments
            .tripwires
            .iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect();
        if tripwires.is_empty() || tripwires.len() > 6 {
            bail!("provide 1-6 tripwires");
        }
        // A too-short tripwire matches everything and turns the lesson into
        // spam; the floor forces something distinctive.
        if let Some(short) = tripwires.iter().find(|value| value.chars().count() < 6) {
            bail!("tripwire `{short}` is too short — use a distinctive string of 6+ characters");
        }
        let workspace = match arguments.scope.as_deref() {
            Some("global") => None,
            _ => Some(self.workspace.clone()),
        };

        let mut inner = self.inner.write().expect("papercut lock");
        // Re-recording the same lesson strengthens it and merges tripwires
        // instead of duplicating the entry.
        if let Some(existing) = inner.papercuts.iter_mut().find(|papercut| {
            papercut.title.eq_ignore_ascii_case(&title) && papercut.workspace == workspace
        }) {
            let now = Utc::now();
            existing.strength = existing.decayed_strength(now) + 1.0;
            existing.trip_count += 1;
            existing.last_tripped_at = Some(now);
            existing.fix = arguments.fix.trim().to_owned();
            for tripwire in tripwires {
                if !existing
                    .tripwires
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(&tripwire))
                {
                    existing.tripwires.push(tripwire);
                }
            }
            let title = existing.title.clone();
            Self::save_locked(&inner);
            return Ok(format!("Papercut \"{title}\" reinforced."));
        }
        inner.papercuts.push(Papercut {
            id: Uuid::new_v4(),
            title: title.clone(),
            description: arguments.description.trim().to_owned(),
            fix: arguments.fix.trim().to_owned(),
            references: arguments.references,
            tripwires,
            workspace,
            created_at: Utc::now(),
            strength: 1.0,
            trip_count: 0,
            recall_count: 0,
            last_tripped_at: None,
            last_recalled_at: None,
        });
        Self::save_locked(&inner);
        Ok(format!(
            "Papercut \"{title}\" recorded. It will be recalled when a tripwire matches."
        ))
    }

    fn list_for_model(&self) -> String {
        let inner = self.inner.read().expect("papercut lock");
        let now = Utc::now();
        let mut lines: Vec<String> = inner
            .papercuts
            .iter()
            .filter(|papercut| papercut.in_scope(&self.workspace))
            .map(|papercut| {
                format!(
                    "- {} — {} Fix: {} (tripwires: {}; tripped {}x, strength {:.1})",
                    papercut.title,
                    papercut.description,
                    papercut.fix,
                    papercut.tripwires.join(", "),
                    papercut.trip_count,
                    papercut.decayed_strength(now),
                )
            })
            .collect();
        if lines.is_empty() {
            return "No papercuts recorded for this workspace yet.".to_owned();
        }
        lines.insert(0, "Recorded papercuts:".to_owned());
        lines.join("\n")
    }

    /// Scan `haystack` (a tool call's arguments and output) against every
    /// in-scope tripwire. Every match is an encounter and strengthens the
    /// papercut; the reminder text is returned only for matches that are off
    /// cooldown, which is what makes recall frequency track encounter rate.
    pub fn touch_and_recall(&self, haystack: &str) -> Vec<String> {
        let mut inner = self.inner.write().expect("papercut lock");
        let haystack_lower = haystack.to_ascii_lowercase();
        let now = Utc::now();
        let workspace = self.workspace.clone();
        let mut reminders = Vec::new();
        let mut changed = false;
        for papercut in &mut inner.papercuts {
            if !papercut.in_scope(&workspace) || !papercut.matches(&haystack_lower) {
                continue;
            }
            papercut.strength = papercut.decayed_strength(now) + 1.0;
            papercut.trip_count += 1;
            papercut.last_tripped_at = Some(now);
            changed = true;
            if papercut.off_cooldown(now) {
                papercut.recall_count += 1;
                papercut.last_recalled_at = Some(now);
                reminders.push(papercut.reminder());
            }
        }
        if changed {
            Self::save_locked(&inner);
        }
        reminders
    }

    /// The strongest in-scope lessons, recalled regardless of cooldown — for
    /// repeated-failure bursts and blocked loops, where a reminder is worth
    /// repeating even if it was shown recently.
    pub fn force_recall_top(&self, limit: usize) -> Vec<String> {
        let mut inner = self.inner.write().expect("papercut lock");
        let now = Utc::now();
        let workspace = self.workspace.clone();
        let mut ranked: Vec<usize> = (0..inner.papercuts.len())
            .filter(|&index| inner.papercuts[index].in_scope(&workspace))
            .collect();
        ranked.sort_by(|&a, &b| {
            let strength_a = inner.papercuts[a].decayed_strength(now);
            let strength_b = inner.papercuts[b].decayed_strength(now);
            strength_b
                .partial_cmp(&strength_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut reminders = Vec::new();
        for index in ranked.into_iter().take(limit) {
            let papercut = &mut inner.papercuts[index];
            papercut.recall_count += 1;
            papercut.last_recalled_at = Some(now);
            reminders.push(papercut.reminder());
        }
        if !reminders.is_empty() {
            Self::save_locked(&inner);
        }
        reminders
    }

    /// Everything in scope for this workspace, for `/papercuts`.
    pub fn snapshot(&self) -> Vec<Papercut> {
        let inner = self.inner.read().expect("papercut lock");
        inner
            .papercuts
            .iter()
            .filter(|papercut| papercut.in_scope(&self.workspace))
            .cloned()
            .collect()
    }

    /// Delete by id. Returns whether anything was removed.
    pub fn remove(&self, id: Uuid) -> bool {
        let mut inner = self.inner.write().expect("papercut lock");
        let before = inner.papercuts.len();
        inner.papercuts.retain(|papercut| papercut.id != id);
        let removed = inner.papercuts.len() != before;
        if removed {
            Self::save_locked(&inner);
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &std::path::Path) -> PapercutStore {
        PapercutStore::load(dir.join("papercuts.json"), dir)
    }

    fn record(store: &PapercutStore, title: &str, tripwire: &str) {
        let output = store
            .execute(
                "papercut_record",
                &json!({
                    "title": title,
                    "description": "tests failed",
                    "fix": "export DATABASE_URL first",
                    "tripwires": [tripwire],
                })
                .to_string(),
            )
            .expect("papercut_record handled");
        assert!(
            output.contains("recorded") || output.contains("reinforced"),
            "{output}"
        );
    }

    #[test]
    fn records_recalls_on_tripwire_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        record(
            &store,
            "sqlx needs DATABASE_URL",
            "error: DATABASE_URL must be set",
        );

        let reminders =
            store.touch_and_recall("exit: 1\nstderr: error: DATABASE_URL must be set to compile");
        assert_eq!(reminders.len(), 1);
        assert!(
            reminders[0].contains("export DATABASE_URL first"),
            "{}",
            reminders[0]
        );
        // Unrelated output trips nothing.
        assert!(
            store
                .touch_and_recall("exit: 0\nall tests passed")
                .is_empty()
        );

        // A fresh store from the same file sees the persisted lesson.
        let reloaded = store2(dir.path());
        assert_eq!(reloaded.snapshot().len(), 1);
        assert_eq!(reloaded.snapshot()[0].trip_count, 1);
    }

    fn store2(dir: &std::path::Path) -> PapercutStore {
        PapercutStore::load(dir.join("papercuts.json"), dir)
    }

    #[test]
    fn cooldown_suppresses_immediate_repeats_but_encounters_still_strengthen() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        record(&store, "lesson", "distinctive-marker");

        assert_eq!(store.touch_and_recall("distinctive-marker").len(), 1);
        // Immediately again: suppressed by cooldown…
        assert!(store.touch_and_recall("distinctive-marker").is_empty());
        // …but both matches counted as encounters.
        assert_eq!(store.snapshot()[0].trip_count, 2);
        assert!(store.snapshot()[0].strength > 2.0);
        // Force recall ignores the cooldown.
        assert_eq!(store.force_recall_top(1).len(), 1);
    }

    #[test]
    fn re_recording_reinforces_instead_of_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        record(&store, "lesson", "first-marker");
        record(&store, "LESSON", "second-marker");
        let snapshot = store.snapshot();
        assert_eq!(snapshot.len(), 1, "same title merges");
        assert_eq!(snapshot[0].tripwires.len(), 2, "tripwires merged");
    }

    #[test]
    fn rejects_generic_tripwires() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let output = store
            .execute(
                "papercut_record",
                &json!({"title": "x", "description": "d", "fix": "f", "tripwires": ["error"]})
                    .to_string(),
            )
            .unwrap();
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[test]
    fn workspace_scoping_hides_other_projects_lessons() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        record(&store, "lesson", "scoped-marker");
        // Same file, different workspace: not in scope.
        let other = PapercutStore::load(
            dir.path().join("papercuts.json"),
            std::path::Path::new("/somewhere/else"),
        );
        assert!(other.snapshot().is_empty());
        assert!(other.touch_and_recall("scoped-marker").is_empty());
    }

    #[test]
    fn strength_decays_with_a_half_life() {
        let now = Utc::now();
        let papercut = Papercut {
            id: Uuid::new_v4(),
            title: "old".into(),
            description: "d".into(),
            fix: "f".into(),
            references: vec![],
            tripwires: vec!["marker-string".into()],
            workspace: None,
            created_at: now - Duration::days(28),
            strength: 4.0,
            trip_count: 4,
            recall_count: 0,
            last_tripped_at: Some(now - Duration::days(28)),
            last_recalled_at: None,
        };
        let decayed = papercut.decayed_strength(now);
        assert!(
            (decayed - 1.0).abs() < 0.05,
            "two half-lives: 4.0 -> ~1.0, got {decayed}"
        );
        // And the cooldown grows correspondingly (weaker => rarer reminders).
        assert!(papercut.cooldown(now) > Duration::minutes(180));
    }
}
