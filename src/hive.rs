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
/// Every promotion is gated on this, not just the top one. Counting clean runs
/// alone let a workspace where most workers *fail* still read as "delegation
/// works here" and ask for more of it: 3 clean runs out of 12, with 16 of 30
/// workers failing, promoted to swarm and kept pushing.
const SWARM_MAX_FAILURE_RATE: f64 = 0.4;
/// Below this many workers the rate is too noisy to demote on — one early
/// failure out of two should not pin a workspace to probing forever.
const RATE_MEANINGFUL_AFTER: u32 = 6;

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
    /// The worker's role: a built-in name, or the id of an authored delegation
    /// spec. Owned rather than `&'static str` because an authored spec's id is
    /// only known at runtime.
    pub role: String,
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
    /// Bumped on every visible mutation so the UI can redraw the strip when
    /// workers move even though no turn is running (nothing else marks the
    /// frame dirty while the transcript is idle).
    version: u64,
}

/// Live worker state, written by the subagent runtime and read by the UI.
#[derive(Clone, Default)]
pub struct SubagentBoard {
    inner: Arc<RwLock<BoardInner>>,
}

impl SubagentBoard {
    /// Register a worker; a new batch starting while only finished workers
    /// remain replaces them, so the strip always shows the current swarm.
    pub fn begin(&self, name: &str, role: &str, tokens: Arc<AtomicU64>) -> u64 {
        let mut inner = self.inner.write().expect("board lock");
        if inner
            .workers
            .iter()
            .all(|worker| worker.state != WorkerState::Running)
        {
            inner.workers.clear();
        }
        inner.next_id += 1;
        inner.version = inner.version.wrapping_add(1);
        let id = inner.next_id;
        inner.workers.push(WorkerStatus {
            id,
            name: name.to_owned(),
            role: role.to_owned(),
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
                inner.version = inner.version.wrapping_add(1);
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
            inner.version = inner.version.wrapping_add(1);
        }
    }

    /// Counter of visible board changes; a larger value than the last one the
    /// UI saw means the worker strip should be redrawn.
    pub fn version(&self) -> u64 {
        self.inner.read().expect("board lock").version
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
        let mut inner = self.inner.write().expect("board lock");
        if !inner.workers.is_empty() {
            inner.version = inner.version.wrapping_add(1);
        }
        inner.workers.clear();
    }
}

/// How many finished workers stay resumable. Transcripts are held in memory,
/// so the window is bounded; older workers fall out oldest-first.
const RESUMABLE_WORKERS: usize = 8;

/// What the orchestrator can reach when it addresses a worker by name.
pub enum WorkerChannel {
    /// The worker is still running — a message steers it mid-flight, exactly
    /// like the user steering the main turn.
    Live(crate::agent::InjectionQueue),
    /// The worker finished. Its conversation is kept so a follow-up can
    /// continue with context intact, in a fresh worktree.
    Finished(Vec<serde_json::Value>),
}

struct WorkerEntry {
    running: bool,
    injections: crate::agent::InjectionQueue,
    transcript: Vec<serde_json::Value>,
    /// Monotonic sequence for oldest-first eviction.
    seq: u64,
}

/// Named workers the orchestrator can address after spawning them. Lives on
/// the hive handle so it survives across turns — a worker spawned in one turn
/// is still reachable from the next.
#[derive(Clone, Default)]
pub struct WorkerRegistry {
    inner: Arc<RwLock<std::collections::BTreeMap<String, WorkerEntry>>>,
    next_seq: Arc<AtomicU64>,
}

impl WorkerRegistry {
    /// Register a starting worker and hand back the queue its turn drains.
    /// A repeated name replaces the older entry: the live worker is the one
    /// the orchestrator means.
    pub fn open(&self, name: &str) -> crate::agent::InjectionQueue {
        let injections = crate::agent::InjectionQueue::default();
        let mut inner = self.inner.write().expect("worker registry lock");
        inner.insert(
            name.to_owned(),
            WorkerEntry {
                running: true,
                injections: injections.clone(),
                transcript: Vec::new(),
                seq: self.next_seq.fetch_add(1, Ordering::Relaxed),
            },
        );
        injections
    }

    /// Mark a worker finished and keep its conversation for a follow-up.
    pub fn close(&self, name: &str, transcript: Vec<serde_json::Value>) {
        let mut inner = self.inner.write().expect("worker registry lock");
        if let Some(entry) = inner.get_mut(name) {
            entry.running = false;
            entry.transcript = transcript;
        }
        // Bound the retained set, dropping the oldest finished workers first.
        while inner.values().filter(|entry| !entry.running).count() > RESUMABLE_WORKERS {
            let Some(oldest) = inner
                .iter()
                .filter(|(_, entry)| !entry.running)
                .min_by_key(|(_, entry)| entry.seq)
                .map(|(name, _)| name.clone())
            else {
                break;
            };
            inner.remove(&oldest);
        }
    }

    /// Reach a worker by name, if it is known.
    pub fn channel(&self, name: &str) -> Option<WorkerChannel> {
        let inner = self.inner.read().expect("worker registry lock");
        let entry = inner.get(name)?;
        Some(if entry.running {
            WorkerChannel::Live(entry.injections.clone())
        } else {
            WorkerChannel::Finished(entry.transcript.clone())
        })
    }

    /// Known worker names with their state, for error messages and listings.
    pub fn roster(&self) -> Vec<(String, bool)> {
        let inner = self.inner.read().expect("worker registry lock");
        inner
            .iter()
            .map(|(name, entry)| (name.clone(), entry.running))
            .collect()
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
        // A rate computed from a handful of workers says little, so it only
        // starts counting once there is enough of a record to mean something.
        let rate_is_meaningful = self.workers >= RATE_MEANINGFUL_AFTER;
        if self.clean_runs >= HIVE_AT && failure_rate < HIVE_MAX_FAILURE_RATE {
            HiveTier::Hive
        } else if self.clean_runs >= SWARM_AT
            && !(rate_is_meaningful && failure_rate >= SWARM_MAX_FAILURE_RATE)
        {
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
    /// Named workers the orchestrator can message after spawning them.
    pub workers: WorkerRegistry,
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
            workers: WorkerRegistry::default(),
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
        // Workers already in flight come first, and they come every time.
        //
        // Spawning does not block: the call returns "started in background" and
        // results arrive after a later tool call. So a model that only sees the
        // encouragement below, and nothing about what it already launched, will
        // launch the same work again — three spawns of three became nine
        // workers for one task. Naming them is what closes that loop.
        let running: Vec<String> = self
            .board
            .snapshot()
            .into_iter()
            .filter(|worker| worker.state == WorkerState::Running)
            .map(|worker| format!("{} ({}, {})", worker.name, worker.role, worker.activity))
            .collect();
        let in_flight = if running.is_empty() {
            String::new()
        } else {
            format!(
                "ALREADY RUNNING — {} worker(s) you have already started, still \
                 working: {}. Their results reach you after your next tool call. Do \
                 NOT spawn more workers for this same work, and do not redo it \
                 yourself; carry on with something else or wait for them.\n\n",
                running.len(),
                running.join("; ")
            )
        };
        let guidance = self.tier_guidance(&stats);
        format!("{in_flight}{guidance}")
    }

    fn tier_guidance(&self, stats: &HiveStats) -> String {
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

    /// The record that caused this: 12 runs, 3 clean, 30 workers, 16 failed.
    /// Counting clean runs alone promoted it to swarm, so the system prompt
    /// told the model "delegation works in this workspace" while more than
    /// half of every worker it started was failing — and asked for more.
    #[test]
    fn a_workspace_where_most_workers_fail_is_not_promoted() {
        let losing = HiveStats {
            runs: 12,
            clean_runs: 3,
            workers: 30,
            worker_failures: 16,
        };
        assert_eq!(losing.tier(), HiveTier::Probing, "53% of workers failed");

        // The same clean-run count with workers that mostly succeed does earn it.
        let winning = HiveStats {
            runs: 12,
            clean_runs: 3,
            workers: 30,
            worker_failures: 4,
        };
        assert_eq!(winning.tier(), HiveTier::Swarm);
    }

    /// A rate from two or three workers is noise, and one early failure should
    /// not pin a workspace to probing forever.
    #[test]
    fn a_thin_record_is_not_judged_on_its_failure_rate() {
        let thin = HiveStats {
            runs: 3,
            clean_runs: 3,
            workers: 4,
            worker_failures: 2,
        };
        assert_eq!(thin.tier(), HiveTier::Swarm, "too few workers to demote on");
    }

    /// Spawning does not block, so without this the model cannot tell that it
    /// already started the work it is about to start again.
    #[test]
    fn guidance_names_the_workers_already_in_flight() {
        let hive = HiveHandle::default();
        assert!(
            !hive.guidance().contains("ALREADY RUNNING"),
            "nothing running, nothing to say"
        );

        let tokens = Arc::new(AtomicU64::new(0));
        let id = hive.board.begin("recon", "scout", tokens.clone());
        hive.board.activity(id, "reading src/agent.rs");
        hive.board.begin("builder", "drone", tokens);

        let guidance = hive.guidance();
        assert!(guidance.contains("ALREADY RUNNING"), "{guidance}");
        assert!(guidance.contains("recon"), "names them: {guidance}");
        assert!(guidance.contains("builder"), "{guidance}");
        assert!(guidance.contains("scout"), "with their roles");
        assert!(
            guidance.contains("Do NOT spawn more workers"),
            "and says what to do about it: {guidance}"
        );
        // The warning leads, ahead of the encouragement to delegate.
        let warn = guidance.find("ALREADY RUNNING").unwrap();
        let tier = guidance.find("Delegation experience").unwrap();
        assert!(warn < tier, "the live state comes first");
    }

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
    fn registry_routes_to_a_live_worker_then_to_its_transcript() {
        let registry = WorkerRegistry::default();
        assert!(registry.channel("nobody").is_none());

        let queue = registry.open("recon");
        // Running: a message steers the live worker.
        match registry.channel("recon") {
            Some(WorkerChannel::Live(live)) => {
                live.push(crate::agent::Injection::UserMessage(
                    "also check tests".into(),
                ));
            }
            other => panic!("expected a live channel, got {}", other.is_some()),
        }
        assert_eq!(queue.drain().len(), 1, "the worker's own turn receives it");

        // Finished: the channel switches to its conversation.
        registry.close(
            "recon",
            vec![serde_json::json!({"role":"assistant","content":"found it"})],
        );
        match registry.channel("recon") {
            Some(WorkerChannel::Finished(transcript)) => {
                assert_eq!(transcript.len(), 1, "context is kept for a follow-up");
            }
            _ => panic!("expected a finished channel"),
        }
        assert_eq!(registry.roster(), vec![("recon".to_owned(), false)]);
    }

    #[test]
    fn registry_bounds_retained_transcripts_oldest_first() {
        let registry = WorkerRegistry::default();
        for index in 0..(RESUMABLE_WORKERS + 3) {
            let name = format!("w{index}");
            registry.open(&name);
            registry.close(&name, vec![serde_json::json!({"role":"assistant"})]);
        }
        let roster = registry.roster();
        assert_eq!(roster.len(), RESUMABLE_WORKERS, "retention is bounded");
        assert!(registry.channel("w0").is_none(), "oldest evicted first");
        assert!(
            registry
                .channel(&format!("w{}", RESUMABLE_WORKERS + 2))
                .is_some(),
            "newest retained"
        );
    }

    #[test]
    fn a_running_worker_is_never_evicted() {
        let registry = WorkerRegistry::default();
        registry.open("long-runner");
        for index in 0..(RESUMABLE_WORKERS + 4) {
            let name = format!("w{index}");
            registry.open(&name);
            registry.close(&name, vec![serde_json::json!({"role":"assistant"})]);
        }
        assert!(
            matches!(
                registry.channel("long-runner"),
                Some(WorkerChannel::Live(_))
            ),
            "eviction only takes finished workers"
        );
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
