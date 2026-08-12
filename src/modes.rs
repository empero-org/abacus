//! Mode discipline: teaching the workflow modes, and noticing when it fails.
//!
//! Every session's system prompt explains the modes and the ideal flow —
//! scout and plan first, then build and follow the plan. Models still slip,
//! reaching for a mutation before switching to BUILD, and the block that
//! follows costs a whole step. So the slips are counted: past a threshold the
//! reminder is escalated into every request, and it relaxes again once the
//! model stops tripping over it.
//!
//! Same shape as papercuts and the hive tier — teach always, escalate on
//! evidence rather than nagging by default.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// Blocks before the standing reminder is injected into every request.
const ESCALATE_AT: u32 = 2;
/// Blocks before the reminder becomes emphatic.
const INSIST_AT: u32 = 6;

/// The always-present part: what the modes are, what needs BUILD, and the
/// order of work. Included in the first system prompt of every session.
pub const MODE_GUIDE: &str = "\
Workflow modes. A session starts in AUTO and you must call `mode_set` before doing anything that \
changes state. BUILD is required for: writing, editing, patching, or appending to files; creating, \
moving, or deleting paths; git commit/restore/checkout; running any command that changes something \
(installs, migrations, formatters that rewrite files); and delegating with spawn_subagents. \
Everything read-only stays available in every mode — reading, globbing, grepping, git status/diff/log, \
and running builds, linters, and tests. Those never need BUILD.\n\
The ideal flow is two phases, in this order:\n\
1. SCOUT AND PLAN — in PLAN mode. Read the relevant code, gather the facts, and write a concrete \
plan or spec: what changes, in which files, and how it will be verified. Begin a project this way; \
do not begin by editing.\n\
2. BUILD AND FOLLOW — call `mode_set` with mode=build, then execute that plan, following it step by \
step and keeping it current as reality corrects it.\n\
Switch to BUILD *before* the first mutating call, not after one is blocked: a blocked call wastes a \
step and tells you nothing you could not have known.";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModeStats {
    /// Tool calls blocked for acting before switching to BUILD.
    #[serde(default)]
    pub blocked: u32,
    /// Mode switches the model made on its own, for context on the ratio.
    #[serde(default)]
    pub switches: u32,
}

/// Shared, cloneable handle over the persisted counts.
#[derive(Clone, Default)]
pub struct ModeCoach {
    stats: Arc<RwLock<ModeStats>>,
    file: Option<PathBuf>,
}

impl ModeCoach {
    pub fn load(file: PathBuf) -> Self {
        let stats = std::fs::read_to_string(&file)
            .ok()
            .and_then(|content| serde_json::from_str::<ModeStats>(&content).ok())
            .unwrap_or_default();
        Self {
            stats: Arc::new(RwLock::new(stats)),
            file: Some(file),
        }
    }

    pub fn stats(&self) -> ModeStats {
        self.stats.read().expect("mode lock").clone()
    }

    fn save(&self, stats: &ModeStats) {
        if let Some(file) = &self.file
            && let Ok(serialized) = serde_json::to_vec_pretty(stats)
        {
            let _ = crate::config::atomic_write(file, &serialized, false);
        }
    }

    /// A tool call was blocked because the mode did not allow it.
    pub fn record_block(&self) {
        let mut stats = self.stats.write().expect("mode lock");
        stats.blocked = stats.blocked.saturating_add(1);
        let snapshot = stats.clone();
        drop(stats);
        self.save(&snapshot);
    }

    /// The model set a mode itself — the behaviour we want more of.
    ///
    /// A clean stretch pays down the debt: each switch without a block
    /// forgives one earlier slip, so a model that learns stops being nagged
    /// instead of carrying its early mistakes forever.
    pub fn record_switch(&self) {
        let mut stats = self.stats.write().expect("mode lock");
        stats.switches = stats.switches.saturating_add(1);
        if stats.switches.is_multiple_of(3) {
            stats.blocked = stats.blocked.saturating_sub(1);
        }
        let snapshot = stats.clone();
        drop(stats);
        self.save(&snapshot);
    }

    /// The standing reminder for this request, if the record warrants one.
    /// Empty while the model is handling modes correctly.
    pub fn reminder(&self) -> String {
        let blocked = self.stats().blocked;
        if blocked >= INSIST_AT {
            format!(
                "MODE DISCIPLINE: {blocked} of your tool calls here have been blocked for acting \
                 before switching to BUILD. Before every single tool call, ask yourself: does this \
                 change anything on disk or run something that does? If yes, your first action is \
                 `mode_set` with mode=build. If you are starting work rather than finishing it, \
                 stay in PLAN and write the plan first."
            )
        } else if blocked >= ESCALATE_AT {
            format!(
                "Reminder: {blocked} call(s) here were blocked for mutating before switching to \
                 BUILD. Call `mode_set` with mode=build before the first change, and plan before \
                 you build."
            )
        } else {
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_names_the_gated_actions_and_the_two_phases() {
        // The guide has to answer "what needs BUILD?" concretely — a vague
        // instruction is exactly what models slip on.
        for gated in ["editing", "deleting", "git commit", "spawn_subagents"] {
            assert!(MODE_GUIDE.contains(gated), "guide should mention {gated}");
        }
        for allowed in ["grepping", "linters", "tests"] {
            assert!(
                MODE_GUIDE.contains(allowed),
                "guide should permit {allowed}"
            );
        }
        assert!(MODE_GUIDE.contains("SCOUT AND PLAN"));
        assert!(MODE_GUIDE.contains("BUILD AND FOLLOW"));
    }

    #[test]
    fn reminder_is_silent_until_blocks_accumulate_then_escalates() {
        let coach = ModeCoach::default();
        assert!(coach.reminder().is_empty(), "no nagging without evidence");

        coach.record_block();
        assert!(coach.reminder().is_empty(), "one slip is not a pattern");

        coach.record_block();
        let reminder = coach.reminder();
        assert!(reminder.starts_with("Reminder:"), "{reminder}");

        for _ in 0..(INSIST_AT - ESCALATE_AT) {
            coach.record_block();
        }
        assert!(coach.reminder().starts_with("MODE DISCIPLINE:"));
    }

    #[test]
    fn a_clean_stretch_pays_down_the_debt() {
        let coach = ModeCoach::default();
        for _ in 0..ESCALATE_AT {
            coach.record_block();
        }
        assert!(!coach.reminder().is_empty());
        // Three correct switches forgive one earlier slip.
        for _ in 0..3 {
            coach.record_switch();
        }
        assert!(
            coach.reminder().is_empty(),
            "a model that learns should stop being reminded"
        );
    }

    #[test]
    fn counts_persist_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("modes.json");
        let coach = ModeCoach::load(file.clone());
        coach.record_block();
        coach.record_block();
        assert_eq!(ModeCoach::load(file).stats().blocked, 2);
    }
}
