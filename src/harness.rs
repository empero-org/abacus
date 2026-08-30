//! The continual harness: versioned, revertable state the agent writes for
//! its future self.
//!
//! Abacus already learns — papercuts remember failures, memories remember
//! knowledge, the working-notes block remembers direction. What it could not
//! do was *undo* any of it. A memory recorded on a wrong hunch was live in
//! every future session with no record of why it was written and no way back.
//!
//! This module is the write path with that gap closed. Every change is an
//! explicit edit against a versioned entry, carries the evidence that
//! motivated it, and can be reverted by inverting the `before`/`after`
//! snapshots it recorded. Rollback needs no separate format: a refinement is
//! itself a refinement, so a rollback can be rolled back.
//!
//! Three kinds of entry:
//!
//! - `Prompt` — narrow behavioural addendums. Supplemental only; the base
//!   system prompt is immutable and is never rewritten from here.
//! - `Memory` — durable facts, decisions and their reasons, conventions.
//! - `Subagent` — reusable delegation specs (role prompt plus defaults).
//!
//! Failure lessons deliberately stay in [`crate::papercuts`]. Papercuts are
//! recalled by tripwire match into the tool result that reproduced the snag,
//! which reaches the model far more reliably than a prompt layer; folding them
//! in here would trade a better mechanism for a tidier one.
//!
//! ## Two independent axes
//!
//! [`Lifetime`] is how long an entry survives: `Session` dies with the
//! session, `Durable` persists to disk. [`HarnessEntry::workspace`] is *which
//! project* it applies to, `None` meaning all of them. The two are pointedly
//! not both called "scope": `memory_record` already takes
//! `scope: workspace|global` for the second axis, so reusing the word for
//! lifetime would give it two meanings inside one tool.
//!
//! New entries default to `Session`. That is a deliberate reduction in what
//! carries across sessions, bought for blast radius: a lesson learned from one
//! bad turn no longer silently follows you forever. An entry re-recorded in
//! [`PROMOTE_AFTER_SESSIONS`] distinct sessions promotes itself to `Durable`,
//! on the theory that repetition across sessions is exactly the evidence that
//! a lesson is real — the same principle papercut strength already encodes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Ceiling on each kind's slice of the injected context layer.
const INJECT_BUDGET_CHARS: usize = 1_800;
/// Newest-first cap on how many entries of one kind reach the prompt.
const INJECT_LIMIT: usize = 10;
/// One entry cannot monopolise its kind's budget.
const MAX_CONTENT_CHARS: usize = 1_200;
/// Distinct sessions an entry must recur in before it persists on its own.
pub const PROMOTE_AFTER_SESSIONS: usize = 3;
/// Refinement events kept in the state file; the full log lives in the JSONL.
const MAX_RESIDENT_REFINEMENTS: usize = 50;

const STATE_FILE: &str = "state.json";
const HISTORY_FILE: &str = "refinements.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Supplemental behavioural guidance.
    Prompt,
    /// Durable knowledge.
    Memory,
    /// A reusable delegation spec.
    Subagent,
}

impl EntryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Memory => "memory",
            Self::Subagent => "subagent",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "prompt" => Some(Self::Prompt),
            "memory" => Some(Self::Memory),
            "subagent" => Some(Self::Subagent),
            _ => None,
        }
    }

    pub const ALL: [EntryKind; 3] = [Self::Prompt, Self::Memory, Self::Subagent];
}

/// How long an entry lives. Not to be confused with `HarnessEntry::workspace`,
/// which is *where* it applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifetime {
    /// Dies with the session. The default for anything newly recorded.
    Session,
    /// Persisted to disk and injected into future sessions.
    Durable,
}

impl Lifetime {
    fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Durable => "durable",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessEntry {
    /// Stable slug; survives across versions and is the handle for edits.
    pub id: String,
    pub kind: EntryKind,
    pub title: String,
    pub content: String,
    /// Free-form grouping hint, for rendering and review.
    #[serde(default = "default_path")]
    pub path: String,
    pub lifetime: Lifetime,
    /// `None` applies everywhere; otherwise the canonical workspace path.
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// What wrote this: `refine`, `model`, or `migration`.
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub version: u32,
}

fn default_path() -> String {
    "general".to_owned()
}

impl HarnessEntry {
    /// `session` or `durable`, for display.
    pub fn lifetime_label(&self) -> &'static str {
        self.lifetime.label()
    }

    fn in_scope(&self, workspace: &str) -> bool {
        self.workspace
            .as_deref()
            .is_none_or(|scope| scope == workspace)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinementEvent {
    pub id: String,
    /// One-line summary of what motivated the change.
    pub trigger: String,
    /// Rendered edits, e.g. `update memory:auth_flow`.
    pub changes: Vec<String>,
    pub evidence: String,
    pub outcome: String,
    pub created_at: DateTime<Utc>,
}

/// Per-kind maps, mirroring the on-disk JSON shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entries {
    #[serde(default)]
    pub prompt: BTreeMap<String, HarnessEntry>,
    #[serde(default)]
    pub memory: BTreeMap<String, HarnessEntry>,
    #[serde(default)]
    pub subagent: BTreeMap<String, HarnessEntry>,
}

impl Entries {
    pub fn of(&self, kind: EntryKind) -> &BTreeMap<String, HarnessEntry> {
        match kind {
            EntryKind::Prompt => &self.prompt,
            EntryKind::Memory => &self.memory,
            EntryKind::Subagent => &self.subagent,
        }
    }

    fn of_mut(&mut self, kind: EntryKind) -> &mut BTreeMap<String, HarnessEntry> {
        match kind {
            EntryKind::Prompt => &mut self.prompt,
            EntryKind::Memory => &mut self.memory,
            EntryKind::Subagent => &mut self.subagent,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessState {
    pub schema: u32,
    #[serde(default)]
    pub entries: Entries,
    #[serde(default)]
    pub refinements: Vec<RefinementEvent>,
    /// `kind:id` → the distinct sessions that recorded it, driving promotion.
    ///
    /// This cannot live on the entry: a session-lifetime entry dies with its
    /// session, so a counter stored there would reset every time and never
    /// reach the threshold. Recurrence is durable even when the entry it
    /// describes is not.
    #[serde(default)]
    pub recurrence: BTreeMap<String, BTreeSet<String>>,
}

impl Default for HarnessState {
    fn default() -> Self {
        Self {
            schema: 1,
            entries: Entries::default(),
            refinements: Vec::new(),
            recurrence: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditAction {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinementEdit {
    pub action: EditAction,
    pub kind: EntryKind,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub metadata: Option<Map<String, Value>>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RefinementProposal {
    pub summary: String,
    pub rationale: String,
    pub expected_outcome: String,
    pub edits: Vec<RefinementEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedEdit {
    pub action: EditAction,
    pub kind: EntryKind,
    pub id: String,
    #[serde(default)]
    pub before: Option<HarnessEntry>,
    #[serde(default)]
    pub after: Option<HarnessEntry>,
    pub applied: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinementResult {
    pub id: String,
    pub summary: String,
    pub rationale: String,
    pub expected_outcome: String,
    pub applied_edits: Vec<AppliedEdit>,
    #[serde(default)]
    pub rollback_of: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl RefinementResult {
    pub fn applied_count(&self) -> usize {
        self.applied_edits
            .iter()
            .filter(|edit| edit.applied)
            .count()
    }

    pub fn changes(&self) -> Vec<String> {
        self.applied_edits
            .iter()
            .filter(|edit| edit.applied)
            .map(|edit| {
                format!(
                    "{} {}:{}",
                    match edit.action {
                        EditAction::Create => "create",
                        EditAction::Update => "update",
                        EditAction::Delete => "delete",
                    },
                    edit.kind.label(),
                    edit.id
                )
            })
            .collect()
    }
}

/// Mint an id in the canonical `refine_<compact utc>_<nonce>` form.
///
/// The nonce is not decoration. Ids are the handle `rollback` resolves
/// against, and two refinements applied in the same millisecond — a rollback
/// immediately after the edit it reverts, say — would otherwise collide and
/// send the lookup to the wrong one.
pub fn generate_refinement_id() -> String {
    format!(
        "refine_{}_{}",
        Utc::now().format("%Y%m%d%H%M%S"),
        &uuid::Uuid::new_v4().to_string()[..6]
    )
}

/// Normalise arbitrary text into a stable entry id.
pub fn slug(raw: &str, fallback: &str) -> String {
    let mut out = String::new();
    let mut last_underscore = false;
    for character in raw.trim().to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            out.push(character);
            last_underscore = false;
        } else if !last_underscore && !out.is_empty() {
            out.push('_');
            last_underscore = true;
        }
        if out.chars().count() >= 80 {
            break;
        }
    }
    let trimmed = out.trim_matches('_').to_owned();
    if trimmed.is_empty() {
        fallback.to_owned()
    } else {
        trimmed
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut boundary = text
        .char_indices()
        .nth(limit)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &text[..boundary])
}

fn compact(text: &str, limit: usize) -> String {
    truncate(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        limit,
    )
}

fn validate_edit(edit: &RefinementEdit, id: &str) -> Option<String> {
    if id.is_empty() {
        return Some("edit needs an id or a title to derive one from".to_owned());
    }
    if edit.action != EditAction::Delete {
        if edit
            .title
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            return Some(format!("{:?} needs a title", edit.action));
        }
        if edit
            .content
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            return Some(format!("{:?} needs content", edit.action));
        }
    }
    None
}

/// Apply a proposal to `state`, recording before/after for every edit.
///
/// `baseline`, when supplied, is the state as it looked when the proposal was
/// planned. The planning call takes seconds, and device sync or a second
/// session can write the shared file in that window, so an entry that moved
/// underneath the plan is refused rather than clobbered. A failed edit never
/// aborts the batch: partial application is normal and visible.
pub fn apply_proposal(
    state: &mut HarnessState,
    proposal: &RefinementProposal,
    id: &str,
    rollback_of: Option<&str>,
    lifetime: Lifetime,
    workspace: Option<&str>,
    baseline: Option<&HarnessState>,
) -> RefinementResult {
    let mut applied_edits: Vec<AppliedEdit> = Vec::new();
    let mut touched: BTreeSet<(EntryKind, String)> = BTreeSet::new();

    for edit in &proposal.edits {
        let entry_id = edit
            .id
            .clone()
            .map(|id| slug(&id, ""))
            .filter(|id| !id.is_empty())
            .or_else(|| {
                (edit.action == EditAction::Create)
                    .then(|| slug(edit.title.as_deref().unwrap_or_default(), edit.kind.label()))
            })
            .unwrap_or_default();

        let mut push = |applied: bool, error: Option<String>, before, after| {
            applied_edits.push(AppliedEdit {
                action: edit.action,
                kind: edit.kind,
                id: entry_id.clone(),
                before,
                after,
                applied,
                error,
            });
        };

        if let Some(error) = validate_edit(edit, &entry_id) {
            push(false, Some(error), None, None);
            continue;
        }

        let before = state.entries.of(edit.kind).get(&entry_id).cloned();
        let key = (edit.kind, entry_id.clone());
        if let Some(baseline) = baseline
            && !touched.contains(&key)
        {
            let expected = baseline.entries.of(edit.kind).get(&entry_id);
            let same = match (&before, expected) {
                (None, None) => true,
                (Some(current), Some(expected)) => {
                    current.version == expected.version && current.updated_at == expected.updated_at
                }
                _ => false,
            };
            if !same {
                push(
                    false,
                    Some("entry changed during refinement planning".to_owned()),
                    before,
                    None,
                );
                continue;
            }
        }

        match edit.action {
            EditAction::Delete => {
                if before.is_none() {
                    push(false, Some("entry not found".to_owned()), None, None);
                    continue;
                }
                state.entries.of_mut(edit.kind).remove(&entry_id);
                touched.insert(key);
                push(true, None, before, None);
            }
            EditAction::Create if before.is_some() => {
                push(false, Some("entry already exists".to_owned()), before, None);
            }
            EditAction::Update if before.is_none() => {
                push(false, Some("entry not found".to_owned()), None, None);
            }
            EditAction::Create | EditAction::Update => {
                let now = Utc::now();
                let after = HarnessEntry {
                    id: entry_id.clone(),
                    kind: edit.kind,
                    title: edit
                        .title
                        .clone()
                        .or_else(|| before.as_ref().map(|entry| entry.title.clone()))
                        .unwrap_or_else(|| entry_id.clone()),
                    content: truncate(
                        edit.content
                            .as_deref()
                            .or(before.as_ref().map(|entry| entry.content.as_str()))
                            .unwrap_or_default()
                            .trim(),
                        MAX_CONTENT_CHARS,
                    ),
                    path: edit
                        .path
                        .clone()
                        .or_else(|| before.as_ref().map(|entry| entry.path.clone()))
                        .unwrap_or_else(default_path),
                    // An existing entry keeps its lifetime: a refinement edits
                    // content, it does not silently change blast radius.
                    lifetime: before
                        .as_ref()
                        .map(|entry| entry.lifetime)
                        .unwrap_or(lifetime),
                    workspace: before
                        .as_ref()
                        .map(|entry| entry.workspace.clone())
                        .unwrap_or_else(|| workspace.map(str::to_owned)),
                    metadata: edit
                        .metadata
                        .clone()
                        .or_else(|| before.as_ref().map(|entry| entry.metadata.clone()))
                        .unwrap_or_default(),
                    source: "refine".to_owned(),
                    created_at: before.as_ref().map(|entry| entry.created_at).unwrap_or(now),
                    updated_at: now,
                    version: before.as_ref().map(|entry| entry.version + 1).unwrap_or(1),
                };
                state
                    .entries
                    .of_mut(edit.kind)
                    .insert(entry_id.clone(), after.clone());
                touched.insert(key);
                push(true, None, before, Some(after));
            }
        }
    }

    let result = RefinementResult {
        id: id.to_owned(),
        summary: proposal.summary.clone(),
        rationale: proposal.rationale.clone(),
        expected_outcome: proposal.expected_outcome.clone(),
        applied_edits,
        rollback_of: rollback_of.map(str::to_owned),
        created_at: Utc::now(),
    };
    state.refinements.push(RefinementEvent {
        id: result.id.clone(),
        trigger: result.summary.clone(),
        changes: result.changes(),
        evidence: result.rationale.clone(),
        outcome: result.expected_outcome.clone(),
        created_at: result.created_at,
    });
    if state.refinements.len() > MAX_RESIDENT_REFINEMENTS {
        let excess = state.refinements.len() - MAX_RESIDENT_REFINEMENTS;
        state.refinements.drain(..excess);
    }
    result
}

/// Invert an applied refinement. Edits are undone in reverse so that a batch
/// touching the same entry twice unwinds to where it started.
pub fn rollback_proposal(target: &RefinementResult) -> RefinementProposal {
    let mut edits = Vec::new();
    for edit in target.applied_edits.iter().rev() {
        if !edit.applied {
            continue;
        }
        match (&edit.before, &edit.after) {
            (Some(before), _) => edits.push(RefinementEdit {
                action: if edit.after.is_some() {
                    EditAction::Update
                } else {
                    EditAction::Create
                },
                kind: edit.kind,
                id: Some(edit.id.clone()),
                title: Some(before.title.clone()),
                content: Some(before.content.clone()),
                path: Some(before.path.clone()),
                metadata: Some(before.metadata.clone()),
                reason: Some(format!("rollback of {}", target.id)),
            }),
            (None, Some(_)) => edits.push(RefinementEdit {
                action: EditAction::Delete,
                kind: edit.kind,
                id: Some(edit.id.clone()),
                title: None,
                content: None,
                path: None,
                metadata: None,
                reason: Some(format!("rollback of {}", target.id)),
            }),
            (None, None) => {}
        }
    }
    RefinementProposal {
        summary: format!("Roll back refinement {}", target.id),
        rationale: format!(
            "Restores the harness entries changed by {} from its recorded snapshots.",
            target.id
        ),
        expected_outcome: "The reverted entries are back to their previous content.".to_owned(),
        edits,
    }
}

#[derive(Default)]
struct Inner {
    /// Persisted across sessions.
    durable: HarnessState,
    /// Lives and dies with this session; rides the session file.
    session: HarnessState,
    /// `None` disables persistence — the inert store subagents get.
    dir: Option<PathBuf>,
}

/// Shared, cloneable handle, following the `MemoryStore`/`PapercutStore`
/// pattern. `Default` yields an inert in-memory store.
#[derive(Clone, Default)]
pub struct HarnessStore {
    inner: Arc<RwLock<Inner>>,
    workspace: String,
    session: Option<String>,
}

impl HarnessStore {
    /// Load durable state from `dir`. A corrupt or unreadable file degrades to
    /// empty rather than failing: this is read on every prompt build, so a bad
    /// parse must never brick a session. The next save rewrites it cleanly.
    pub fn load(dir: PathBuf, workspace: &Path) -> Self {
        let durable = std::fs::read_to_string(dir.join(STATE_FILE))
            .ok()
            .and_then(|content| serde_json::from_str::<HarnessState>(&content).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(RwLock::new(Inner {
                durable,
                session: HarnessState::default(),
                dir: Some(dir),
            })),
            workspace: workspace.to_string_lossy().into_owned(),
            session: None,
        }
    }

    /// Attach the session id used for promotion accounting.
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    /// Load, importing the pre-harness stores on first use. The entry points
    /// call this; [`Self::load`] stays side-effect-free for tests and for the
    /// isolated stores an eval run builds.
    pub fn load_migrated(dir: PathBuf, workspace: &Path, legacy_memories: &Path) -> Self {
        let store = Self::load(dir, workspace);
        let _ = store.migrate(legacy_memories, workspace);
        store
    }

    fn save_locked(inner: &Inner) {
        if let Some(dir) = &inner.dir
            && let Ok(serialized) = serde_json::to_vec_pretty(&inner.durable)
        {
            let _ = crate::config::atomic_write(&dir.join(STATE_FILE), &serialized, false);
        }
    }

    fn append_history(inner: &Inner, result: &RefinementResult) {
        let Some(dir) = &inner.dir else { return };
        let Ok(line) = serde_json::to_string(result) else {
            return;
        };
        let path = dir.join(HISTORY_FILE);
        let mut existing = std::fs::read(&path).unwrap_or_default();
        existing.extend_from_slice(line.as_bytes());
        existing.push(b'\n');
        let _ = crate::config::atomic_write(&path, &existing, false);
    }

    /// The full refinement log, oldest first. Read from the JSONL so a
    /// refinement stays revertable from any later session. A malformed line is
    /// skipped rather than failing the read.
    pub fn history(&self) -> Vec<RefinementResult> {
        let inner = self.inner.read().expect("harness lock");
        let Some(dir) = &inner.dir else {
            return Vec::new();
        };
        let Ok(content) = std::fs::read_to_string(dir.join(HISTORY_FILE)) else {
            return Vec::new();
        };
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<RefinementResult>(line).ok())
            .collect()
    }

    /// Durable and session entries in one view, session winning on a tie.
    pub fn merged(&self) -> HarnessState {
        let inner = self.inner.read().expect("harness lock");
        let mut merged = inner.durable.clone();
        for kind in EntryKind::ALL {
            for (id, entry) in inner.session.entries.of(kind) {
                merged
                    .entries
                    .of_mut(kind)
                    .insert(id.clone(), entry.clone());
            }
        }
        merged
    }

    /// Session-lifetime state, for the session file to persist.
    pub fn session_snapshot(&self) -> HarnessState {
        self.inner.read().expect("harness lock").session.clone()
    }

    /// Restore session-lifetime state when a session is resumed.
    pub fn restore_session(&self, state: HarnessState) {
        self.inner.write().expect("harness lock").session = state;
    }

    /// Everything applying to this workspace, for `/harness`.
    pub fn snapshot(&self) -> Vec<HarnessEntry> {
        let merged = self.merged();
        let mut entries: Vec<HarnessEntry> = EntryKind::ALL
            .iter()
            .flat_map(|kind| merged.entries.of(*kind).values().cloned())
            .filter(|entry| entry.in_scope(&self.workspace))
            .collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
        entries
    }

    /// Render the durable prompt entries into the workspace `AGENTS.md` block.
    ///
    /// Only durable entries: a session-lifetime note has not earned a place in
    /// a file the user commits. A no-op when nothing durable applies here,
    /// rather than clearing a block the user can see — an empty render would
    /// look like the agent forgot something.
    pub fn render_notes(&self, workspace: &Path) -> Result<()> {
        let entries = self.snapshot_of(EntryKind::Prompt);
        let notes: Vec<String> = entries
            .iter()
            .filter(|entry| entry.lifetime == Lifetime::Durable)
            .map(|entry| format!("- **{}** — {}", entry.title, entry.content))
            .collect();
        if notes.is_empty() {
            return Ok(());
        }
        write_notes_block(workspace, &truncate(&notes.join("\n"), MAX_NOTES_CHARS))
    }

    /// Entries of one kind applying to this workspace, newest first.
    pub fn snapshot_of(&self, kind: EntryKind) -> Vec<HarnessEntry> {
        let merged = self.merged();
        let mut entries: Vec<HarnessEntry> = merged
            .entries
            .of(kind)
            .values()
            .filter(|entry| entry.in_scope(&self.workspace))
            .cloned()
            .collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
        entries
    }

    /// Delete by id, for the `/harness` and `/memories` commands. Removes from
    /// whichever side holds it.
    pub fn remove(&self, kind: EntryKind, id: &str) -> bool {
        let mut inner = self.inner.write().expect("harness lock");
        let from_session = inner.session.entries.of_mut(kind).remove(id).is_some();
        let from_durable = inner.durable.entries.of_mut(kind).remove(id).is_some();
        if from_durable {
            Self::save_locked(&inner);
        }
        from_session || from_durable
    }

    /// Apply a proposal and persist. Durable edits land in the state file and
    /// the history log; session edits stay in memory.
    pub fn apply(
        &self,
        proposal: &RefinementProposal,
        lifetime: Lifetime,
        rollback_of: Option<&str>,
        baseline: Option<&HarnessState>,
    ) -> RefinementResult {
        let id = generate_refinement_id();
        let mut inner = self.inner.write().expect("harness lock");
        let workspace = Some(self.workspace.clone());
        let state = match lifetime {
            Lifetime::Durable => &mut inner.durable,
            Lifetime::Session => &mut inner.session,
        };
        let result = apply_proposal(
            state,
            proposal,
            &id,
            rollback_of,
            lifetime,
            workspace.as_deref(),
            baseline,
        );
        if lifetime == Lifetime::Durable {
            Self::save_locked(&inner);
            Self::append_history(&inner, &result);
        }
        result
    }

    /// Revert a previous refinement by inverting its recorded snapshots.
    pub fn rollback(&self, refinement_id: &str) -> Result<RefinementResult> {
        let target = self
            .history()
            .into_iter()
            .find(|result| result.id == refinement_id)
            .with_context(|| format!("no refinement `{refinement_id}` in the history"))?;
        if target.applied_count() == 0 {
            bail!("refinement `{refinement_id}` applied no edits; nothing to roll back");
        }
        let proposal = rollback_proposal(&target);
        Ok(self.apply(&proposal, Lifetime::Durable, Some(&target.id), None))
    }

    /// A copy of the state a proposal is planned against, so [`Self::apply`]
    /// can detect a concurrent write.
    pub fn baseline(&self, lifetime: Lifetime) -> HarnessState {
        let inner = self.inner.read().expect("harness lock");
        match lifetime {
            Lifetime::Durable => inner.durable.clone(),
            Lifetime::Session => inner.session.clone(),
        }
    }

    // ---- model-facing tools -------------------------------------------------

    /// The `memory_*` names are kept verbatim from `MemoryStore`: the base
    /// system prompt already teaches them, and renaming a tool the model is
    /// trained on costs adherence for no benefit. What changed is underneath.
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
                            "scope": {"type": "string", "enum": ["workspace", "global"], "description": "Which projects it applies to: workspace (default) or global"}
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

    pub fn execute(&self, name: &str, arguments: &str) -> Option<String> {
        match name {
            "memory_record" => Some(match self.record(arguments) {
                Ok(message) => message,
                Err(error) => format!("Error: {error:#}"),
            }),
            "memory_list" => Some(self.list_for_model()),
            "memory_forget" => Some(match self.forget(arguments) {
                Ok(message) => message,
                Err(error) => format!("Error: {error:#}"),
            }),
            _ => None,
        }
    }

    fn record(&self, arguments: &str) -> Result<String> {
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
        let body = arguments.body.trim();
        if body.is_empty() {
            bail!("body must not be empty");
        }
        let workspace = match arguments.scope.as_deref() {
            Some("global") => None,
            _ => Some(self.workspace.clone()),
        };
        let id = slug(&title, "memory");
        let now = Utc::now();

        let mut inner = self.inner.write().expect("harness lock");
        // An existing entry is updated where it already lives, so re-recording
        // never silently downgrades a durable memory to session-only.
        let durable_hit = inner.durable.entries.memory.contains_key(&id);

        // Recurrence across distinct sessions is the evidence that a lesson is
        // real, so an entry promotes itself rather than waiting to be adopted.
        // Tracked on the durable side because the session entry it describes
        // will not survive to be counted again.
        let recurrence_key = format!("{}:{id}", EntryKind::Memory.label());
        let seen = if let Some(session) = &self.session {
            let seen = inner.durable.recurrence.entry(recurrence_key).or_default();
            seen.insert(session.clone());
            seen.len()
        } else {
            0
        };
        let promoted = !durable_hit && seen >= PROMOTE_AFTER_SESSIONS;

        let existing = if durable_hit {
            inner.durable.entries.memory.get(&id).cloned()
        } else {
            inner.session.entries.memory.get(&id).cloned()
        };
        let entry = HarnessEntry {
            id: id.clone(),
            kind: EntryKind::Memory,
            title: title.clone(),
            content: truncate(body, MAX_CONTENT_CHARS),
            path: existing
                .as_ref()
                .map(|entry| entry.path.clone())
                .unwrap_or_else(default_path),
            lifetime: if durable_hit || promoted {
                Lifetime::Durable
            } else {
                Lifetime::Session
            },
            workspace,
            metadata: existing
                .as_ref()
                .map(|entry| entry.metadata.clone())
                .unwrap_or_default(),
            source: "model".to_owned(),
            created_at: existing
                .as_ref()
                .map(|entry| entry.created_at)
                .unwrap_or(now),
            updated_at: now,
            version: existing
                .as_ref()
                .map(|entry| entry.version + 1)
                .unwrap_or(1),
        };
        let updated = existing.is_some();

        if promoted {
            inner.session.entries.memory.remove(&id);
            inner.durable.entries.memory.insert(id, entry);
            Self::save_locked(&inner);
            return Ok(format!(
                "Memory \"{title}\" recorded, and promoted to durable after recurring in {seen} sessions."
            ));
        }
        if durable_hit {
            inner.durable.entries.memory.insert(id, entry);
        } else {
            inner.session.entries.memory.insert(id, entry);
        }
        // The recurrence map changed even when the entry itself is
        // session-only, so the durable side is always written back.
        Self::save_locked(&inner);
        Ok(if updated {
            format!("Memory \"{title}\" updated.")
        } else {
            format!("Memory \"{title}\" recorded for this session.")
        })
    }

    fn forget(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Arguments {
            title: String,
        }
        let arguments: Arguments =
            serde_json::from_str(arguments).context("invalid memory_forget arguments")?;
        let title = arguments.title.trim();
        let id = slug(title, "");
        let mut inner = self.inner.write().expect("harness lock");
        let from_session = inner.session.entries.memory.remove(&id).is_some();
        let from_durable = inner.durable.entries.memory.remove(&id).is_some();
        if from_durable {
            Self::save_locked(&inner);
        }
        if !from_session && !from_durable {
            bail!("no memory titled `{title}` in this workspace");
        }
        Ok(format!("Memory \"{title}\" forgotten."))
    }

    fn list_for_model(&self) -> String {
        let merged = self.merged();
        let mut lines: Vec<String> = merged
            .entries
            .memory
            .values()
            .filter(|entry| entry.in_scope(&self.workspace))
            .map(|entry| {
                format!(
                    "- {} [{}] — {}",
                    entry.title,
                    entry.lifetime.label(),
                    entry.content
                )
            })
            .collect();
        if lines.is_empty() {
            return "No memories stored for this workspace yet.".to_owned();
        }
        lines.insert(0, "Stored memories:".to_owned());
        lines.join("\n")
    }

    /// The context layer injected at the start of every turn.
    pub fn prompt_context(&self) -> String {
        let merged = self.merged();
        let mut sections: Vec<String> = Vec::new();
        for kind in EntryKind::ALL {
            let mut entries: Vec<&HarnessEntry> = merged
                .entries
                .of(kind)
                .values()
                .filter(|entry| entry.in_scope(&self.workspace))
                .collect();
            if entries.is_empty() {
                continue;
            }
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
            let mut section = String::from(match kind {
                EntryKind::Prompt => {
                    "Standing guidance recorded in earlier sessions (supplemental to your instructions, not a replacement):"
                }
                EntryKind::Memory => {
                    "Memories from earlier sessions (keep them current with memory_record / memory_forget):"
                }
                EntryKind::Subagent => {
                    "Delegation specs available as subagent roles (reference one by its id):"
                }
            });
            let mut used = 0_usize;
            for entry in entries.into_iter().take(INJECT_LIMIT) {
                let line = format!("\n- [{}] {}: {}", entry.id, entry.title, entry.content);
                if used + line.len() > INJECT_BUDGET_CHARS {
                    break;
                }
                used += line.len();
                section.push_str(&line);
            }
            sections.push(section);
        }
        sections.join("\n\n")
    }

    /// A compact overview for the refiner's prompt: ids, titles, versions, and
    /// clipped content, plus recent refinement history.
    pub fn overview(&self) -> String {
        let merged = self.merged();
        let mut lines: Vec<String> = Vec::new();
        for kind in EntryKind::ALL {
            let entries: Vec<&HarnessEntry> = merged
                .entries
                .of(kind)
                .values()
                .filter(|entry| entry.in_scope(&self.workspace))
                .collect();
            lines.push(format!("{}: {}", kind.label(), entries.len()));
            for entry in entries {
                lines.push(format!(
                    "- [{}] {} ({}, v{}, {}): {}",
                    entry.id,
                    entry.title,
                    entry.path,
                    entry.version,
                    entry.lifetime.label(),
                    compact(&entry.content, 180)
                ));
            }
        }
        lines.push(String::new());
        let refinements = &merged.refinements;
        lines.push(format!("recent refinements: {}", refinements.len()));
        for event in refinements.iter().rev().take(5).collect::<Vec<_>>() {
            let changes = if event.changes.is_empty() {
                "no applied edits".to_owned()
            } else {
                event.changes.join(", ")
            };
            lines.push(format!(
                "- [{}] {}: {}",
                event.id,
                compact(&event.trigger, 120),
                changes
            ));
        }
        lines.join("\n")
    }

    /// One-shot import of the pre-harness stores, plus seeding of the built-in
    /// delegation roles. Importing runs only when the durable state is still
    /// empty, so it cannot re-import after a user has pruned. The old files are
    /// left untouched.
    ///
    /// Returns how many *legacy* entries were imported. Seeded roles are not
    /// counted: they are not the user's data arriving, and reporting "3
    /// migrated" on a fresh install would be a lie.
    pub fn migrate(&self, memories_file: &Path, workspace_dir: &Path) -> Result<usize> {
        let mut inner = self.inner.write().expect("harness lock");
        // Seeded independently of the legacy import: the built-in delegation
        // roles used to be a closed enum in the source, and turning them into
        // entries is what makes them editable. A fresh install has nothing to
        // import but still needs its roster.
        let seeded = seed_subagents(&mut inner.durable);
        let mut imported = 0_usize;
        if !inner.durable.entries.memory.is_empty() || !inner.durable.entries.prompt.is_empty() {
            if seeded > 0 {
                Self::save_locked(&inner);
            }
            return Ok(0);
        }

        #[derive(Deserialize)]
        struct LegacyMemory {
            title: String,
            body: String,
            #[serde(default)]
            workspace: Option<String>,
            created_at: DateTime<Utc>,
            updated_at: DateTime<Utc>,
        }
        if let Ok(content) = std::fs::read_to_string(memories_file)
            && let Ok(legacy) = serde_json::from_str::<Vec<LegacyMemory>>(&content)
        {
            for memory in legacy {
                let id = slug(&memory.title, "memory");
                inner.durable.entries.memory.insert(
                    id.clone(),
                    HarnessEntry {
                        id,
                        kind: EntryKind::Memory,
                        title: memory.title,
                        content: truncate(memory.body.trim(), MAX_CONTENT_CHARS),
                        path: default_path(),
                        lifetime: Lifetime::Durable,
                        workspace: memory.workspace,
                        metadata: Map::new(),
                        source: "migration".to_owned(),
                        created_at: memory.created_at,
                        updated_at: memory.updated_at,
                        version: 1,
                    },
                );
                imported += 1;
            }
        }

        if let Some(notes) = read_notes_block(workspace_dir) {
            let now = Utc::now();
            inner.durable.entries.prompt.insert(
                "working_notes".to_owned(),
                HarnessEntry {
                    id: "working_notes".to_owned(),
                    kind: EntryKind::Prompt,
                    title: "Working notes".to_owned(),
                    content: truncate(&notes, MAX_CONTENT_CHARS),
                    path: default_path(),
                    lifetime: Lifetime::Durable,
                    workspace: Some(self.workspace.clone()),
                    metadata: Map::new(),
                    source: "migration".to_owned(),
                    created_at: now,
                    updated_at: now,
                    version: 1,
                },
            );
            imported += 1;
        }

        if imported > 0 || seeded > 0 {
            Self::save_locked(&inner);
        }
        Ok(imported)
    }
}

/// The delegation roles Abacus ships with, as ordinary entries.
///
/// `read_only` in the metadata is enforced mechanically, not just instructed:
/// a spec carrying it runs its worker in PLAN with mutations locked off.
const BUILT_IN_SUBAGENTS: [(&str, &str, bool, &str); 3] = [
    (
        "drone",
        "Drone — builder",
        false,
        "You are a DRONE: a builder. Execute exactly the delegated change, run the narrowest \
         checks that verify it, and report what you changed and what you ran. Do not investigate \
         beyond what the change requires. Do not spawn subagents, commit, push, or modify paths \
         outside the workspace.",
    ),
    (
        "scout",
        "Scout — read-only researcher",
        true,
        "You are a SCOUT: a researcher. Investigate the delegated question — read code, crawl the \
         repository, search the web where allowed — and report findings with exact file paths and \
         evidence. You must NOT modify anything; your value is the fidelity of what you bring \
         back. Do not spawn subagents.",
    ),
    (
        "worker",
        "Worker — generic",
        false,
        "You are an isolated subagent. Complete only the delegated task. You may edit and test \
         this worktree. Do not spawn more subagents, commit, push, or modify paths outside the \
         workspace. Finish with a concise summary and exact checks run.",
    ),
];

/// Add any built-in spec the store is missing. Returns how many were added, so
/// a user who deleted one keeps it deleted only until the next seed — which is
/// the right trade for roles the tool schema still names.
fn seed_subagents(state: &mut HarnessState) -> usize {
    let now = Utc::now();
    let mut added = 0;
    for (id, title, read_only, prompt) in BUILT_IN_SUBAGENTS {
        if state.entries.subagent.contains_key(id) {
            continue;
        }
        let mut metadata = Map::new();
        metadata.insert("read_only".to_owned(), Value::Bool(read_only));
        metadata.insert("built_in".to_owned(), Value::Bool(true));
        state.entries.subagent.insert(
            id.to_owned(),
            HarnessEntry {
                id: id.to_owned(),
                kind: EntryKind::Subagent,
                title: title.to_owned(),
                content: prompt.to_owned(),
                path: "roles".to_owned(),
                lifetime: Lifetime::Durable,
                workspace: None,
                metadata,
                source: "built-in".to_owned(),
                created_at: now,
                updated_at: now,
                version: 1,
            },
        );
        added += 1;
    }
    added
}

const NOTES_START: &str = "<!-- abacus:notes:start -->";
const NOTES_END: &str = "<!-- abacus:notes:end -->";
/// Ceiling on the managed block, so notes stay notes.
const MAX_NOTES_CHARS: usize = 4_000;

/// Rewrite only the abacus-managed block of `AGENTS.md`, creating the file (or
/// appending the block) when absent. Everything outside the markers is
/// preserved byte for byte.
///
/// The block used to be written directly by the model and was the whole of its
/// standing guidance — one 4,000-char slot, overwritten wholesale, with no
/// history. It is now a *view*: the harness store is the source of truth and
/// this renders the durable prompt entries into the workspace so the guidance
/// stays visible and diffable in git.
pub fn write_notes_block(workspace: &Path, notes: &str) -> Result<()> {
    let notes = notes.trim();
    if notes.chars().count() > MAX_NOTES_CHARS {
        bail!("working notes exceed {MAX_NOTES_CHARS} characters — keep them summary-sized");
    }
    let path = workspace.join("AGENTS.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    let block = if notes.is_empty() {
        format!("{NOTES_START}\n{NOTES_END}")
    } else {
        format!("{NOTES_START}\n## Working notes (maintained by Abacus)\n\n{notes}\n{NOTES_END}")
    };

    let updated = match (existing.find(NOTES_START), existing.find(NOTES_END)) {
        (Some(start), Some(end)) if end >= start => {
            let after = end + NOTES_END.len();
            format!("{}{}{}", &existing[..start], block, &existing[after..])
        }
        // Markers missing or mangled: append a fresh block rather than guess
        // at intent inside the user's content.
        _ => {
            if existing.is_empty() {
                format!("{block}\n")
            } else {
                let separator = if existing.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                format!("{existing}{separator}{block}\n")
            }
        }
    };
    crate::config::atomic_write(&path, updated.as_bytes(), false)
        .context("write AGENTS.md working notes")
}

/// Read the abacus-managed block out of a workspace `AGENTS.md`, stripped of
/// its heading. Returns `None` when the block is absent or empty.
fn read_notes_block(workspace: &Path) -> Option<String> {
    let content = std::fs::read_to_string(workspace.join("AGENTS.md")).ok()?;
    let start = content.find(NOTES_START)? + NOTES_START.len();
    let end = content.find(NOTES_END)?;
    if end < start {
        return None;
    }
    let block = content[start..end]
        .lines()
        .filter(|line| !line.trim_start().starts_with("## Working notes"))
        .collect::<Vec<_>>()
        .join("\n");
    let block = block.trim();
    (!block.is_empty()).then(|| block.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> HarnessStore {
        HarnessStore::load(dir.join("harness"), dir).with_session("session-1")
    }

    fn edit(action: EditAction, kind: EntryKind, id: &str, content: &str) -> RefinementEdit {
        RefinementEdit {
            action,
            kind,
            id: Some(id.to_owned()),
            title: Some(id.replace('_', " ")),
            content: Some(content.to_owned()),
            path: None,
            metadata: None,
            reason: None,
        }
    }

    fn proposal(edits: Vec<RefinementEdit>) -> RefinementProposal {
        RefinementProposal {
            summary: "a change".to_owned(),
            rationale: "because the trajectory showed it".to_owned(),
            expected_outcome: "better next time".to_owned(),
            edits,
        }
    }

    #[test]
    fn slugs_are_stable_and_bounded() {
        assert_eq!(
            slug("Auth flow uses one-shot tokens", "x"),
            "auth_flow_uses_one_shot_tokens"
        );
        assert_eq!(slug("  ", "fallback"), "fallback");
        assert_eq!(slug("!!!", "fallback"), "fallback");
        assert!(slug(&"a b ".repeat(200), "x").chars().count() <= 80);
        // Case and punctuation differences must land on the same handle.
        assert_eq!(slug("Auth Flow!", "x"), slug("auth   flow", "x"));
    }

    #[test]
    fn an_edit_is_versioned_and_revertable() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());

        let created = store.apply(
            &proposal(vec![edit(
                EditAction::Create,
                EntryKind::Memory,
                "auth_flow",
                "tokens are one-shot",
            )]),
            Lifetime::Durable,
            None,
            None,
        );
        assert_eq!(created.applied_count(), 1);
        assert_eq!(created.changes(), vec!["create memory:auth_flow"]);

        let updated = store.apply(
            &proposal(vec![edit(
                EditAction::Update,
                EntryKind::Memory,
                "auth_flow",
                "tokens are one-shot and expire in 60s",
            )]),
            Lifetime::Durable,
            None,
            None,
        );
        assert_eq!(updated.applied_edits[0].after.as_ref().unwrap().version, 2);

        let rolled = store.rollback(&updated.id).unwrap();
        assert_eq!(rolled.applied_count(), 1);
        let entry = store.merged().entries.memory["auth_flow"].clone();
        assert_eq!(entry.content, "tokens are one-shot");
        // A rollback is itself a refinement, so it can be rolled back too.
        let unrolled = store.rollback(&rolled.id).unwrap();
        assert_eq!(unrolled.applied_count(), 1);
        assert!(
            store.merged().entries.memory["auth_flow"]
                .content
                .contains("60s")
        );
    }

    #[test]
    fn rolling_back_a_create_deletes_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let created = store.apply(
            &proposal(vec![edit(
                EditAction::Create,
                EntryKind::Prompt,
                "always_fmt",
                "run cargo fmt before finishing",
            )]),
            Lifetime::Durable,
            None,
            None,
        );
        assert!(store.merged().entries.prompt.contains_key("always_fmt"));
        store.rollback(&created.id).unwrap();
        assert!(!store.merged().entries.prompt.contains_key("always_fmt"));
    }

    #[test]
    fn a_concurrent_write_rejects_a_stale_edit() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store.apply(
            &proposal(vec![edit(
                EditAction::Create,
                EntryKind::Memory,
                "shared",
                "v1",
            )]),
            Lifetime::Durable,
            None,
            None,
        );
        // What the refiner planned against.
        let baseline = store.baseline(Lifetime::Durable);
        // Another session writes while the planning call is in flight.
        store.apply(
            &proposal(vec![edit(
                EditAction::Update,
                EntryKind::Memory,
                "shared",
                "v2 from the other session",
            )]),
            Lifetime::Durable,
            None,
            None,
        );

        let stale = store.apply(
            &proposal(vec![edit(
                EditAction::Update,
                EntryKind::Memory,
                "shared",
                "v2 from the stale plan",
            )]),
            Lifetime::Durable,
            None,
            Some(&baseline),
        );
        assert_eq!(stale.applied_count(), 0);
        assert_eq!(
            stale.applied_edits[0].error.as_deref(),
            Some("entry changed during refinement planning")
        );
        assert!(
            store.merged().entries.memory["shared"]
                .content
                .contains("other session"),
            "the concurrent write must survive"
        );
    }

    #[test]
    fn a_failed_edit_does_not_abort_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let result = store.apply(
            &proposal(vec![
                edit(EditAction::Update, EntryKind::Memory, "missing", "nope"),
                edit(EditAction::Create, EntryKind::Memory, "present", "yes"),
            ]),
            Lifetime::Durable,
            None,
            None,
        );
        assert_eq!(result.applied_count(), 1);
        assert_eq!(
            result.applied_edits[0].error.as_deref(),
            Some("entry not found")
        );
        assert!(store.merged().entries.memory.contains_key("present"));
    }

    #[test]
    fn edits_missing_a_title_or_content_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut bad = edit(EditAction::Create, EntryKind::Memory, "x", "body");
        bad.title = Some("  ".to_owned());
        let result = store.apply(&proposal(vec![bad]), Lifetime::Durable, None, None);
        assert_eq!(result.applied_count(), 0);
        assert!(result.applied_edits[0].error.is_some());
    }

    #[test]
    fn corrupt_state_degrades_to_empty_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("harness");
        std::fs::create_dir_all(&harness).unwrap();
        std::fs::write(harness.join(STATE_FILE), "{ not json at all").unwrap();
        // Read on every prompt build: a bad parse must not brick the session.
        let store = HarnessStore::load(harness.clone(), dir.path());
        assert!(store.snapshot().is_empty());
        // And the next write repairs the file.
        store.apply(
            &proposal(vec![edit(
                EditAction::Create,
                EntryKind::Memory,
                "fresh",
                "body",
            )]),
            Lifetime::Durable,
            None,
            None,
        );
        let reloaded = HarnessStore::load(harness, dir.path());
        assert_eq!(reloaded.snapshot().len(), 1);
    }

    #[test]
    fn durable_entries_persist_and_session_entries_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store.apply(
            &proposal(vec![edit(
                EditAction::Create,
                EntryKind::Memory,
                "kept",
                "durable",
            )]),
            Lifetime::Durable,
            None,
            None,
        );
        store.apply(
            &proposal(vec![edit(
                EditAction::Create,
                EntryKind::Memory,
                "transient",
                "session only",
            )]),
            Lifetime::Session,
            None,
            None,
        );
        assert_eq!(store.snapshot().len(), 2);

        let next_session = HarnessStore::load(dir.path().join("harness"), dir.path());
        let ids: Vec<String> = next_session
            .snapshot()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(ids, vec!["kept".to_owned()]);
    }

    #[test]
    fn a_memory_is_session_scoped_until_it_recurs_in_three_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("harness");
        let arguments = json!({"title": "build uses just", "body": "run `just check`"}).to_string();

        // Sessions one and two: recorded, but nothing carries across.
        for session in ["s1", "s2"] {
            let store = HarnessStore::load(harness.clone(), dir.path()).with_session(session);
            let reply = store.execute("memory_record", &arguments).unwrap();
            assert!(reply.contains("for this session"), "{reply}");
            assert_eq!(store.snapshot().len(), 1, "visible within its own session");
        }
        assert!(
            HarnessStore::load(harness.clone(), dir.path())
                .snapshot()
                .is_empty(),
            "a twice-seen lesson must not yet persist"
        );

        // Third distinct session: recurrence is the evidence, so it promotes.
        let third = HarnessStore::load(harness.clone(), dir.path()).with_session("s3");
        let reply = third.execute("memory_record", &arguments).unwrap();
        assert!(reply.contains("promoted to durable"), "{reply}");

        let later = HarnessStore::load(harness.clone(), dir.path()).with_session("s4");
        let snapshot = later.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].lifetime, Lifetime::Durable);
    }

    #[test]
    fn recurrence_counts_distinct_sessions_not_repeats_within_one() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("harness");
        let arguments = json!({"title": "same lesson", "body": "again"}).to_string();
        let store = HarnessStore::load(harness.clone(), dir.path()).with_session("s1");
        for _ in 0..5 {
            store.execute("memory_record", &arguments).unwrap();
        }
        // Five recordings in one session are one piece of evidence, not five.
        assert!(
            HarnessStore::load(harness, dir.path())
                .snapshot()
                .is_empty(),
            "repeating within a session must not fake recurrence"
        );
    }

    #[test]
    fn re_recording_updates_and_forget_removes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let first = json!({"title": "auth flow", "body": "v1"}).to_string();
        let second = json!({"title": "Auth Flow", "body": "v2"}).to_string();
        assert!(
            store
                .execute("memory_record", &first)
                .unwrap()
                .contains("recorded")
        );
        assert!(
            store
                .execute("memory_record", &second)
                .unwrap()
                .contains("updated"),
            "title casing must resolve to the same entry"
        );
        assert_eq!(store.snapshot().len(), 1);
        assert_eq!(store.snapshot()[0].content, "v2");

        let forget = json!({"title": "auth flow"}).to_string();
        assert!(
            store
                .execute("memory_forget", &forget)
                .unwrap()
                .contains("forgotten")
        );
        assert!(store.snapshot().is_empty());
        assert!(
            store
                .execute("memory_forget", &forget)
                .unwrap()
                .starts_with("Error:"),
            "forgetting twice must report the miss"
        );
    }

    #[test]
    fn workspace_scoping_hides_other_projects() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store
            .execute(
                "memory_record",
                &json!({"title": "local fact", "body": "only here"}).to_string(),
            )
            .unwrap();
        store
            .execute(
                "memory_record",
                &json!({"title": "everywhere", "body": "all projects", "scope": "global"})
                    .to_string(),
            )
            .unwrap();

        let elsewhere = HarnessStore {
            inner: store.inner.clone(),
            workspace: "/somewhere/else".to_owned(),
            session: None,
        };
        let visible: Vec<String> = elsewhere
            .snapshot()
            .into_iter()
            .map(|entry| entry.title)
            .collect();
        assert_eq!(visible, vec!["everywhere".to_owned()]);
    }

    #[test]
    fn prompt_context_is_bounded_and_grouped_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut edits = vec![edit(
            EditAction::Create,
            EntryKind::Prompt,
            "style",
            "match surrounding code",
        )];
        for index in 0..40 {
            edits.push(edit(
                EditAction::Create,
                EntryKind::Memory,
                &format!("memory_{index}"),
                &"x".repeat(300),
            ));
        }
        store.apply(&proposal(edits), Lifetime::Durable, None, None);

        let context = store.prompt_context();
        assert!(context.contains("Standing guidance"), "{context}");
        assert!(context.contains("Memories from earlier sessions"));
        // Each kind is bounded independently, so a flood of memories cannot
        // crowd out standing guidance.
        assert!(context.contains("match surrounding code"));
        assert!(context.len() < INJECT_BUDGET_CHARS * 3);
    }

    #[test]
    fn migration_imports_memories_and_the_notes_block_once() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let memories = workspace.join("memories.json");
        std::fs::write(
            &memories,
            json!([{
                "id": "00000000-0000-0000-0000-000000000000",
                "title": "auth flow",
                "body": "tokens are one-shot",
                "workspace": workspace.to_string_lossy(),
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-02T00:00:00Z"
            }])
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            workspace.join("AGENTS.md"),
            format!(
                "# House rules\n\nAlways run fmt.\n\n{NOTES_START}\n## Working notes (maintained by Abacus)\n\nCurrent focus: the importer.\n{NOTES_END}\n"
            ),
        )
        .unwrap();

        let store = store(workspace);
        // Counts imported legacy entries only — the seeded roles are not the
        // user's data arriving.
        assert_eq!(store.migrate(&memories, workspace).unwrap(), 2);
        let merged = store.merged();
        assert_eq!(
            merged.entries.memory["auth_flow"].content,
            "tokens are one-shot"
        );
        assert_eq!(
            merged.entries.prompt["working_notes"].content,
            "Current focus: the importer."
        );
        // Both arrive durable — they were already cross-session state.
        assert_eq!(
            merged.entries.memory["auth_flow"].lifetime,
            Lifetime::Durable
        );
        // The legacy file is left alone.
        assert!(memories.is_file());
        // Idempotent: a second run must not re-import over a pruned store.
        assert_eq!(store.migrate(&memories, workspace).unwrap(), 0);
    }

    #[test]
    fn migration_ignores_an_absent_or_empty_notes_block() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        std::fs::write(
            workspace.join("AGENTS.md"),
            format!("# House rules\n\n{NOTES_START}\n{NOTES_END}\n"),
        )
        .unwrap();
        let store = store(workspace);
        assert_eq!(
            store
                .migrate(&workspace.join("nonexistent.json"), workspace)
                .unwrap(),
            0
        );
    }

    #[test]
    fn notes_render_durable_prompts_without_touching_user_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "# My rules\n\nAlways run fmt.\n").unwrap();
        let store = store(dir.path());

        // A session-lifetime note stays out of a file the user commits.
        store.apply(
            &proposal(vec![edit(
                EditAction::Create,
                EntryKind::Prompt,
                "transient",
                "only for now",
            )]),
            Lifetime::Session,
            None,
            None,
        );
        store.render_notes(dir.path()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("only for now"), "{content}");

        store.apply(
            &proposal(vec![edit(
                EditAction::Create,
                EntryKind::Prompt,
                "style",
                "match the surrounding code",
            )]),
            Lifetime::Durable,
            None,
            None,
        );
        store.render_notes(dir.path()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.starts_with("# My rules\n\nAlways run fmt.\n"),
            "{content}"
        );
        assert!(content.contains("match the surrounding code"));
        assert_eq!(content.matches(NOTES_START).count(), 1);
    }

    #[test]
    fn rendering_replaces_the_block_and_never_empties_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        write_notes_block(dir.path(), "first").unwrap();
        write_notes_block(dir.path(), "second").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("second"));
        assert!(!content.contains("first"), "{content}");
        assert_eq!(content.matches(NOTES_START).count(), 1);

        // Nothing durable to say leaves the visible block alone rather than
        // wiping it, which would read as the agent forgetting.
        let store = store(dir.path());
        store.render_notes(dir.path()).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("second"));
    }

    #[test]
    fn oversized_notes_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_notes_block(dir.path(), &"x".repeat(5_000)).is_err());
    }

    #[test]
    fn history_survives_a_reload_so_rollback_works_from_a_later_session() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("harness");
        let first = HarnessStore::load(harness.clone(), dir.path());
        let created = first.apply(
            &proposal(vec![edit(
                EditAction::Create,
                EntryKind::Memory,
                "regret",
                "a hasty lesson",
            )]),
            Lifetime::Durable,
            None,
            None,
        );

        let later = HarnessStore::load(harness, dir.path());
        assert_eq!(later.history().len(), 1);
        later.rollback(&created.id).unwrap();
        assert!(!later.merged().entries.memory.contains_key("regret"));
    }

    #[test]
    fn built_in_delegation_roles_are_seeded_as_editable_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store
            .migrate(&dir.path().join("nothing.json"), dir.path())
            .unwrap();

        let roles = store.snapshot_of(EntryKind::Subagent);
        let ids: Vec<&str> = roles.iter().map(|entry| entry.id.as_str()).collect();
        for expected in ["drone", "scout", "worker"] {
            assert!(ids.contains(&expected), "{ids:?}");
        }
        // A scout is read-only mechanically, not just by instruction.
        let scout = roles.iter().find(|entry| entry.id == "scout").unwrap();
        assert_eq!(scout.metadata["read_only"], serde_json::json!(true));
        assert_eq!(
            roles
                .iter()
                .find(|entry| entry.id == "drone")
                .unwrap()
                .metadata["read_only"],
            serde_json::json!(false)
        );

        // Seeding is idempotent and does not fight an edit.
        store.apply(
            &proposal(vec![edit(
                EditAction::Update,
                EntryKind::Subagent,
                "scout",
                "A revised scout brief.",
            )]),
            Lifetime::Durable,
            None,
            None,
        );
        store
            .migrate(&dir.path().join("nothing.json"), dir.path())
            .unwrap();
        let scout = store
            .snapshot_of(EntryKind::Subagent)
            .into_iter()
            .find(|entry| entry.id == "scout")
            .unwrap();
        assert_eq!(scout.content, "A revised scout brief.");
    }

    #[test]
    fn session_entries_survive_a_snapshot_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("harness");
        let first = HarnessStore::load(harness.clone(), dir.path());
        first.apply(
            &proposal(vec![edit(
                EditAction::Create,
                EntryKind::Memory,
                "in_progress",
                "mid-task state",
            )]),
            Lifetime::Session,
            None,
            None,
        );
        // What the session file persists.
        let carried = first.session_snapshot();

        // A resumed session restores it; a fresh one does not see it.
        let resumed = HarnessStore::load(harness.clone(), dir.path());
        assert!(resumed.snapshot().is_empty());
        resumed.restore_session(carried);
        assert_eq!(resumed.snapshot().len(), 1);
        assert_eq!(resumed.snapshot()[0].lifetime, Lifetime::Session);

        let unrelated = HarnessStore::load(harness, dir.path());
        assert!(
            unrelated.snapshot().is_empty(),
            "session state must not leak into another session"
        );
    }

    #[test]
    fn refinement_ids_are_unique_within_a_millisecond() {
        // Ids are what rollback resolves against, so a collision would revert
        // the wrong refinement.
        let ids: BTreeSet<String> = (0..500).map(|_| generate_refinement_id()).collect();
        assert_eq!(ids.len(), 500);
    }

    #[test]
    fn rolling_back_an_unknown_refinement_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        assert!(store.rollback("refine_nope").is_err());
    }

    #[test]
    fn overview_lists_ids_versions_and_recent_refinements() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store.apply(
            &proposal(vec![edit(
                EditAction::Create,
                EntryKind::Subagent,
                "release_auditor",
                "audits a release branch",
            )]),
            Lifetime::Durable,
            None,
            None,
        );
        let overview = store.overview();
        assert!(overview.contains("[release_auditor]"), "{overview}");
        assert!(overview.contains("v1"));
        assert!(overview.contains("create subagent:release_auditor"));
    }

    #[test]
    fn an_inert_store_persists_nothing_but_still_answers() {
        let store = HarnessStore::default();
        // Subagents get this: recording must not panic or write anywhere.
        let reply = store
            .execute(
                "memory_record",
                &json!({"title": "t", "body": "b"}).to_string(),
            )
            .unwrap();
        assert!(reply.contains("recorded"), "{reply}");
        assert!(store.history().is_empty());
        assert_eq!(store.snapshot().len(), 1);
    }
}
