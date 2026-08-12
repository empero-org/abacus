//! Hive: the delegation ladder and the live subagent board.
//!
//! Two halves. The **board** is shared mutable state the subagent runtime
//! writes and the TUI reads: every worker's name, state, and latest activity,
//! pinned above the composer while a swarm runs and expandable into a detail
//! overlay. The **stats** are the persistent delegation record — how many
//! swarms have run here and how they went — from which a maturity tier is
//! derived: an inexperienced abacus is told to *probe* subagents on low-risk
//! separable work, a proven one to *swarm* freely, and a veteran to act as a
//! *hive* — clusters of parallel swarms per surface, its own effort spent on
//! validation and coordination. The tier guidance is injected as a context
//! layer, so confidence is earned from recorded outcomes, not assumed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Individual worker lines shown when a swarm is at most this large;
/// bigger swarms cluster into a single summary line.
pub const CLUSTER_THRESHOLD: usize = 3;

/// Clean swarm runs required to leave the probing tier.
const SWARM_AT: u32 = 3;
/// Clean swarm runs required for hive tier…
const HIVE_AT: u32 = 12;
/// …provided the worker failure rate stays under this.
const HIVE_MAX_FAILURE_RATE: f64 = 0.3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub struct WorkerStatus {
    pub id: u64,
    pub name: String,
    /// drone / scout / worker — shown beside the name.
    pub role: &'static str,
    pub state: WorkerState,
    /// The worker's most recent visible activity — a tool call or the tail of
    /// its streamed answer — clipped to one line.
    pub activity: String,
    pub started: Instant,
    /// Live token counter for this worker's own provider clone.
    pub tokens: Arc<AtomicU64>,
}

impl WorkerStatus {
    pub fn tokens_used(&self) -> u64 {
        self.tokens.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct BoardInner {
    workers: Vec<WorkerStatus>,
    next_id: u64,
}

/// Live worker state, written by the subagent runtime and read by the UI.
#[derive(Clone, Default)]
pub struct SubagentBoard {
    inner: Arc<RwLock<BoardInner>>,
}

impl SubagentBoard {
    /// Register a worker; a new batch starting while only finished workers
    /// remain replaces them, so the strip always shows the current swarm.
    pub fn begin(&self, name: &str, role: &'static str, tokens: Arc<AtomicU64>) -> u64 {
        let mut inner = self.inner.write().expect("board lock");
        if inner
            .workers
            .iter()
            .all(|worker| worker.state != WorkerState::Running)
        {
            inner.workers.clear();
        }
        inner.next_id += 1;
        let id = inner.next_id;
        inner.workers.push(WorkerStatus {
            id,
            name: name.to_owned(),
            role,
            state: WorkerState::Running,
            activity: "starting".to_owned(),
            started: Instant::now(),
            tokens,
        });
        id
    }

    pub fn activity(&self, id: u64, activity: &str) {
        let mut inner = self.inner.write().expect("board lock");
        if let Some(worker) = inner.workers.iter_mut().find(|worker| worker.id == id) {
            // Trailing punctuation fragments of a streamed answer ("…", ")")
            // are not activity; require something readable.
            let line = activity
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| line.chars().filter(|ch| ch.is_alphanumeric()).count() >= 3)
                .unwrap_or("")
                .to_owned();
            if !line.is_empty() {
                worker.activity = line;
            }
        }
    }

    pub fn finish(&self, id: u64, ok: bool) {
        let mut inner = self.inner.write().expect("board lock");
        if let Some(worker) = inner.workers.iter_mut().find(|worker| worker.id == id) {
            worker.state = if ok {
                WorkerState::Done
            } else {
                WorkerState::Failed
            };
        }
    }

    pub fn snapshot(&self) -> Vec<WorkerStatus> {
        self.inner.read().expect("board lock").workers.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().expect("board lock").workers.is_empty()
    }

    /// Rows the pinned strip needs right now: one per worker up to the
    /// cluster threshold, one summary row past it, zero when idle.
    pub fn strip_rows(&self) -> u16 {
        let count = self.inner.read().expect("board lock").workers.len();
        match count {
            0 => 0,
            n if n <= CLUSTER_THRESHOLD => n as u16,
            _ => 1,
        }
    }

    pub fn clear(&self) {
        self.inner.write().expect("board lock").workers.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiveTier {
    Probing,
    Swarm,
    Hive,
}

impl HiveTier {
    pub fn label(self) -> &'static str {
        match self {
            Self::Probing => "probing",
            Self::Swarm => "swarm",
            Self::Hive => "hive",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HiveStats {
    #[serde(default)]
    pub runs: u32,
    #[serde(default)]
    pub clean_runs: u32,
    #[serde(default)]
    pub workers: u32,
    #[serde(default)]
    pub worker_failures: u32,
}

impl HiveStats {
    pub fn tier(&self) -> HiveTier {
        let failure_rate = if self.workers == 0 {
            0.0
        } else {
            f64::from(self.worker_failures) / f64::from(self.workers)
        };
        if self.clean_runs >= HIVE_AT && failure_rate < HIVE_MAX_FAILURE_RATE {
            HiveTier::Hive
        } else if self.clean_runs >= SWARM_AT {
            HiveTier::Swarm
        } else {
            HiveTier::Probing
        }
    }
}

/// Shared handle: persistent stats plus the live board, riding `TurnOptions`.
/// `Default` yields an inert handle (no persistence) for nested contexts.
#[derive(Clone, Default)]
pub struct HiveHandle {
    stats: Arc<RwLock<HiveStats>>,
    file: Option<PathBuf>,
    pub board: SubagentBoard,
}

impl HiveHandle {
    pub fn load(file: PathBuf) -> Self {
        let stats = std::fs::read_to_string(&file)
            .ok()
            .and_then(|content| serde_json::from_str::<HiveStats>(&content).ok())
            .unwrap_or_default();
        Self {
            stats: Arc::new(RwLock::new(stats)),
            file: Some(file),
            board: SubagentBoard::default(),
        }
    }

    pub fn stats(&self) -> HiveStats {
        self.stats.read().expect("hive lock").clone()
    }

    /// Record a finished swarm and return the updated record line the model
    /// sees appended to the tool result — its own track record, kept honest.
    pub fn record_run(&self, workers: u32, failures: u32) -> String {
        let mut stats = self.stats.write().expect("hive lock");
        stats.runs += 1;
        stats.workers += workers;
        stats.worker_failures += failures;
        if failures == 0 {
            stats.clean_runs += 1;
        }
        if let Some(file) = &self.file
            && let Ok(serialized) = serde_json::to_vec_pretty(&*stats)
        {
            let _ = crate::config::atomic_write(file, &serialized, false);
        }
        format!(
            "Hive record: {} swarm(s) run in total, {} clean; {} worker(s), {} failed. Tier: {}.",
            stats.runs,
            stats.clean_runs,
            stats.workers,
            stats.worker_failures,
            stats.tier().label()
        )
    }

    /// The tier guidance injected as a context layer. Confidence is earned:
    /// the wording changes only as recorded outcomes accumulate.
    pub fn guidance(&self) -> String {
        let stats = self.stats();
        match stats.tier() {
            HiveTier::Probing => format!(
                "Delegation experience here: {} swarm run(s), {} clean. You are still \
                 learning how subagents fit your workflow — PROBE them: when a task \
                 contains low-risk, separable sub-work (independent research, isolated \
                 test runs, self-contained fixes), delegate it with spawn_subagents and \
                 record what you learn about delegating well as memories. Keep the \
                 stakes small until the record grows.",
                stats.runs, stats.clean_runs
            ),
            HiveTier::Swarm => format!(
                "Delegation experience here: {} swarm run(s), {} clean, {} worker(s) with \
                 {} failure(s). Delegation works in this workspace — SWARM: prefer \
                 spawn_subagents for any genuinely separable work — scouts to gather, \
                 drones to build, workers for the general case — keep your own focus \
                 on the parts that need shared context, and keep recording what \
                 delegates well.",
                stats.runs, stats.clean_runs, stats.workers, stats.worker_failures
            ),
            HiveTier::Hive => format!(
                "Delegation experience here: {} swarm run(s), {} clean, {} worker(s) with \
                 {} failure(s). You are a proven swarm operator — act as a HIVE: split \
                 multi-surface work into clusters of parallel swarms (one spawn_subagents \
                 call per surface, sequenced as their dependencies require; scouts ahead \
                 of drones, so building starts informed), validate \
                 every worker outcome yourself before building on it, and spend your own \
                 effort on inter-swarm coordination and integration rather than on work a \
                 swarm can carry.",
                stats.runs, stats.clean_runs, stats.workers, stats.worker_failures
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_are_earned_from_clean_runs_and_failure_rate() {
        let mut stats = HiveStats::default();
        assert_eq!(stats.tier(), HiveTier::Probing);
        stats.clean_runs = 3;
        stats.runs = 3;
        stats.workers = 9;
        assert_eq!(stats.tier(), HiveTier::Swarm);
        stats.clean_runs = 12;
        stats.runs = 14;
        stats.workers = 40;
        stats.worker_failures = 2;
        assert_eq!(stats.tier(), HiveTier::Hive);
        // A high failure rate holds a veteran at swarm tier.
        stats.worker_failures = 20;
        assert_eq!(stats.tier(), HiveTier::Swarm);
    }

    #[test]
    fn record_run_persists_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hive.json");
        let hive = HiveHandle::load(file.clone());
        let line = hive.record_run(4, 0);
        assert!(line.contains("1 swarm(s)"), "{line}");
        assert!(line.contains("probing"), "{line}");
        let reloaded = HiveHandle::load(file);
        assert_eq!(reloaded.stats().runs, 1);
        assert_eq!(reloaded.stats().workers, 4);
    }

    #[test]
    fn guidance_matures_with_the_record() {
        let hive = HiveHandle::default();
        assert!(hive.guidance().contains("PROBE"));
        for _ in 0..3 {
            hive.record_run(2, 0);
        }
        assert!(hive.guidance().contains("SWARM"));
        for _ in 0..9 {
            hive.record_run(2, 0);
        }
        assert!(hive.guidance().contains("HIVE"));
    }

    #[test]
    fn board_tracks_workers_and_clusters_at_scale() {
        let board = SubagentBoard::default();
        assert_eq!(board.strip_rows(), 0);
        let a = board.begin("alpha", "scout", Arc::new(AtomicU64::new(0)));
        let b = board.begin("beta", "drone", Arc::new(AtomicU64::new(0)));
        assert_eq!(board.strip_rows(), 2, "small swarms list individually");
        board.activity(a, "running tests\npass 3 of 9");
        assert_eq!(board.snapshot()[0].activity, "pass 3 of 9");
        board.finish(a, true);
        board.finish(b, false);
        let states: Vec<WorkerState> = board.snapshot().iter().map(|w| w.state).collect();
        assert_eq!(states, vec![WorkerState::Done, WorkerState::Failed]);

        for index in 0..5 {
            board.begin(
                &format!("worker-{index}"),
                "worker",
                Arc::new(AtomicU64::new(0)),
            );
        }
        // The new batch replaced the settled one, and five workers cluster.
        assert_eq!(board.snapshot().len(), 5);
        assert_eq!(board.strip_rows(), 1);
        board.clear();
        assert!(board.is_empty());
    }
}
