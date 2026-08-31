//! Eval harness: run a fixed task suite against a pinned model and score it
//! objectively.
//!
//! Abacus carries five self-improvement loops — papercuts, memories, the
//! refine pass, the hive tier, mode discipline — and until now no way to tell
//! whether any of them helps. This module is the measurement side: a task
//! suite, an isolated workspace per run, and a verdict from a script rather
//! than from a model.
//!
//! Scoring is deliberately objective — `check.sh` exits 0 or it doesn't. A
//! model judge would contribute exactly the variance the harness exists to
//! measure.
//!
//! The mode that earns the module is `--state both`: every task runs twice,
//! once against the real `~/.abacus` (papercuts, memories, hive tier all live)
//! and once against an empty one. The delta between those columns is the only
//! direct evidence that accumulated state pays for itself.
//!
//! Models are stochastic, so a single run proves nothing. Results are always
//! reported as a pass *rate* with its repetition count attached, never as a
//! bare pass/fail, and `--repeat` exists to make that count more than one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;

use crate::agent::{
    AgentEvent, AgentMode, ApprovalDecision, TurnOptions, UserAnswer, compression_budget,
    initial_messages, run_turn,
};
use crate::compaction::CompactionState;
use crate::config::{AbacusPaths, Config, EvalState, Settings};
use crate::goal::GoalState;
use crate::model_info::ModelLimits;
use crate::provider::Provider;
use crate::services::AgentServices;
use crate::task::TaskList;

/// Where task definitions live, relative to the workspace.
const TASKS_DIR: &str = "examples/evals";
/// A task that declares no timeout gets this long before it is cancelled.
const DEFAULT_TIMEOUT_SECONDS: u64 = 300;
/// Step ceiling for a task that declares none.
const DEFAULT_MAX_STEPS: usize = 40;
/// `check.sh` is a verifier, not a build — it should be quick.
const CHECK_TIMEOUT_SECONDS: u64 = 120;

/// The on-disk `task.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    prompt: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_max_steps")]
    max_steps: usize,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
    /// `plan`, `build`, or `auto`. Defaults to `build` — most tasks are
    /// scored on a mutation, and AUTO would make the mode-selection step part
    /// of what is being measured.
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    allow_subagents: bool,
}

fn default_max_steps() -> usize {
    DEFAULT_MAX_STEPS
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECONDS
}

#[derive(Debug, Clone)]
pub struct EvalTask {
    name: String,
    root: PathBuf,
    file: TaskFile,
}

impl EvalTask {
    fn mode(&self) -> Result<AgentMode> {
        Ok(match self.file.mode.as_deref() {
            None | Some("build") => AgentMode::Build,
            Some("plan") => AgentMode::Plan,
            Some("auto") => AgentMode::Auto,
            Some(other) => bail!("task `{}`: unknown mode `{other}`", self.name),
        })
    }
}

/// One task, one state setting, one repetition.
#[derive(Debug, Clone, Serialize)]
pub struct RunOutcome {
    pub task: String,
    /// "on" or "off" — whether `~/.abacus` learned state was visible.
    pub state: &'static str,
    pub repetition: usize,
    pub passed: bool,
    pub tool_calls: usize,
    pub assistant_steps: usize,
    pub tokens: u64,
    pub wall_ms: u128,
    /// Times a papercut tripwire fired into a tool result — the observable
    /// proxy for "recalled state actually reached the model".
    pub papercut_recalls: usize,
    pub timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub check_output: String,
}

pub struct EvalOptions {
    /// Substring filter on task names; None runs the whole suite.
    pub filter: Option<String>,
    pub repeat: usize,
    pub state: EvalState,
    pub model: Option<String>,
    pub json: bool,
}

/// Discover every task under `<workspace>/examples/evals`.
pub fn discover(workspace: &Path, filter: Option<&str>) -> Result<Vec<EvalTask>> {
    let root = workspace.join(TASKS_DIR);
    if !root.is_dir() {
        // Tasks are workspace-relative, so running this from the wrong
        // directory is the likely mistake — say so rather than just naming a
        // path that does not exist.
        bail!(
            "no eval tasks in {}\n\
             Tasks live in {TASKS_DIR} relative to the project directory. Run this from a \
             project that has them, or name the project: `abacus <path> eval`.",
            root.display()
        );
    }
    let mut tasks = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&root)
        .with_context(|| format!("read {}", root.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    entries.sort();
    for path in entries {
        let manifest = path.join("task.toml");
        if !manifest.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(filter) = filter
            && !name.contains(filter)
        {
            continue;
        }
        let raw = std::fs::read_to_string(&manifest)
            .with_context(|| format!("read {}", manifest.display()))?;
        let file: TaskFile =
            toml::from_str(&raw).with_context(|| format!("parse {}", manifest.display()))?;
        tasks.push(EvalTask {
            name,
            root: path,
            file,
        });
    }
    if tasks.is_empty() {
        bail!("no eval tasks matched");
    }
    Ok(tasks)
}

/// Run the suite and report.
pub async fn run(config: Config, settings: Settings, options: EvalOptions) -> Result<()> {
    if options.repeat == 0 {
        bail!("--repeat must be at least 1");
    }
    let tasks = discover(&config.workspace, options.filter.as_deref())?;
    let states: Vec<bool> = match options.state {
        EvalState::On => vec![true],
        EvalState::Off => vec![false],
        EvalState::Both => vec![true, false],
    };

    let run_id = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S"),
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let run_root = config.paths.root.join("evals").join(&run_id);
    std::fs::create_dir_all(&run_root).with_context(|| format!("create {}", run_root.display()))?;

    if !options.json {
        println!(
            "abacus eval — {} task(s), {} repetition(s), state {}\nworkspaces under {}\n",
            tasks.len(),
            options.repeat,
            match options.state {
                EvalState::On => "on",
                EvalState::Off => "off",
                EvalState::Both => "on+off",
            },
            run_root.display()
        );
        for task in &tasks {
            match task.file.description.as_deref() {
                Some(description) => println!("  {:<28} {description}", task.name),
                None => println!("  {}", task.name),
            }
        }
        println!();
    }

    let mut outcomes: Vec<RunOutcome> = Vec::new();
    for task in &tasks {
        for &state_on in &states {
            for repetition in 1..=options.repeat {
                let outcome = run_once(
                    &config,
                    &settings,
                    task,
                    state_on,
                    repetition,
                    &run_root,
                    options.model.as_deref(),
                )
                .await
                .unwrap_or_else(|error| failed_outcome(task, state_on, repetition, error));
                if options.json {
                    println!("{}", serde_json::to_string(&outcome)?);
                } else {
                    println!(
                        "  {:<28} state={:<3} #{} {} {:>4} calls {:>7} tok {:>6}ms{}",
                        outcome.task,
                        outcome.state,
                        outcome.repetition,
                        if outcome.passed { "PASS" } else { "FAIL" },
                        outcome.tool_calls,
                        outcome.tokens,
                        outcome.wall_ms,
                        if outcome.timed_out { " (timeout)" } else { "" }
                    );
                }
                outcomes.push(outcome);
            }
        }
    }

    let results_path = run_root.join("results.json");
    std::fs::write(&results_path, serde_json::to_vec_pretty(&outcomes)?)
        .with_context(|| format!("write {}", results_path.display()))?;

    if !options.json {
        print_summary(&outcomes, options.repeat, options.state);
        println!("\nfull results: {}", results_path.display());
    }
    Ok(())
}

fn failed_outcome(
    task: &EvalTask,
    state_on: bool,
    repetition: usize,
    error: anyhow::Error,
) -> RunOutcome {
    RunOutcome {
        task: task.name.clone(),
        state: if state_on { "on" } else { "off" },
        repetition,
        passed: false,
        tool_calls: 0,
        assistant_steps: 0,
        tokens: 0,
        wall_ms: 0,
        papercut_recalls: 0,
        timed_out: false,
        error: Some(format!("{error:#}")),
        check_output: String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_once(
    config: &Config,
    settings: &Settings,
    task: &EvalTask,
    state_on: bool,
    repetition: usize,
    run_root: &Path,
    model_override: Option<&str>,
) -> Result<RunOutcome> {
    let label = format!(
        "{}-{}-{repetition}",
        task.name,
        if state_on { "on" } else { "off" }
    );
    let case_root = run_root.join(&label);
    let workspace = case_root.join("workspace");
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("create {}", workspace.display()))?;

    let fixture = task.root.join("fixture");
    if fixture.is_dir() {
        copy_tree(&fixture, &workspace)?;
    }
    init_git(&workspace).await?;

    // A run's config is the real one with the workspace, limits, and learned
    // state swapped. Provider credentials are resolved fields on `Config`, so
    // repointing `paths` cannot break authentication — it only moves where
    // papercuts, memories, the hive tier, and mode counts are read from.
    let mut config = config.clone();
    config.workspace = workspace.clone();
    config.max_steps = task.file.max_steps;
    config.mode = Some(task.mode()?);
    config.no_session = true;
    config.trace_enabled = false;
    // Mutations are the point: the score comes from what the run changed in a
    // throwaway copy.
    config.yes = true;
    if let Some(model) = model_override {
        config.model = model.to_owned();
        config.model_limits = ModelLimits::resolve_from_name(model, None, None);
    }
    if !state_on {
        // An empty ABACUS_HOME is the control arm. Skills live here too, and
        // that is deliberate: once the agent can author its own skills they
        // are learned state, not configuration.
        let isolated = AbacusPaths::under(case_root.join("state"));
        isolated.ensure()?;
        config.paths = isolated;
    }

    let services = Arc::new(
        AgentServices::discover(&config.workspace, &config.paths, settings)
            .await
            .context("discover extensions")?,
    );
    let tokens = Arc::new(AtomicU64::new(0));
    let provider = Provider::with_tokens(&config, tokens.clone())?;

    let mut messages = initial_messages(&config.workspace);
    messages.push(json!({"role": "user", "content": task.file.prompt}));

    let cancel = Arc::new(AtomicBool::new(false));
    let (events, mut receiver) = mpsc::unbounded_channel();
    let turn = TurnOptions {
        workspace: config.workspace.clone(),
        max_steps: config.max_steps,
        tool_output_limit: config.tool_output_limit,
        mode: config.mode.unwrap_or(AgentMode::Build),
        allow_mutations: Arc::new(AtomicBool::new(true)),
        services: services.clone(),
        session_id: None,
        goal: GoalState::default(),
        tasks: TaskList::default(),
        compaction: CompactionState::new(None),
        compaction_budget: compression_budget(
            config.model_limits.compaction_budget(),
            config.token_compression,
        ),
        token_compression: config.token_compression,
        allow_subagents: task.file.allow_subagents,
        web_search: config.web_search.clone(),
        papercuts: crate::papercuts::PapercutStore::load(
            config.paths.papercuts_file.clone(),
            &config.workspace,
        ),
        harness: crate::harness::HarnessStore::load_migrated(
            config.paths.harness_dir.clone(),
            &config.workspace,
            &config.paths.memories_file,
        ),
        handles: crate::handles::HandleStore::default(),
        tether: crate::tether::TetherState::default(),
        hive: crate::hive::HiveHandle::load(config.paths.hive_file.clone()),
        aux_model: config.aux_model.clone(),
        injections: crate::agent::InjectionQueue::default(),
        modes: crate::modes::ModeCoach::load(config.paths.modes_file.clone()),
        safety: crate::safety::SafetyCache::default(),
        safety_uses_main: false,
        trace: None,
        cancel: cancel.clone(),
    };

    let started = Instant::now();
    let handle = tokio::spawn(run_turn(provider, messages, turn, events));

    let mut tool_calls = 0_usize;
    let mut papercut_recalls = 0_usize;
    let mut assistant_steps = 0_usize;
    let mut error: Option<String> = None;
    let mut timed_out = false;
    let deadline = Duration::from_secs(task.file.timeout_seconds);

    loop {
        let next =
            tokio::time::timeout(deadline.saturating_sub(started.elapsed()), receiver.recv()).await;
        let event = match next {
            // A timed-out run is data, not an error: ask the turn to stop and
            // score whatever it managed to produce.
            Err(_) => {
                timed_out = true;
                cancel.store(true, Ordering::Relaxed);
                break;
            }
            Ok(None) => break,
            Ok(Some(event)) => event,
        };
        match event {
            AgentEvent::ToolStarted { .. } => tool_calls += 1,
            AgentEvent::ToolFinished { output, .. } => {
                if output.contains("[papercut]") {
                    papercut_recalls += 1;
                }
            }
            // Nothing is watching, so an unanswered request would hang the run
            // until its timeout. Approve, since the workspace is disposable.
            AgentEvent::Approval(request) => {
                let _ = request.respond.send(ApprovalDecision::Always);
            }
            AgentEvent::UserQuestion(request) => {
                let first = request.options.first().cloned();
                let _ = request.respond.send(UserAnswer {
                    selected_labels: first.clone().into_iter().collect(),
                    custom_text: first
                        .is_none()
                        .then(|| "No preference — proceed with your best judgement.".to_owned()),
                });
            }
            AgentEvent::Done { messages, .. } => {
                assistant_steps = messages
                    .iter()
                    .filter(|message| message["role"] == "assistant")
                    .count();
                break;
            }
            AgentEvent::Failed {
                error: failure,
                messages,
            } => {
                assistant_steps = messages
                    .iter()
                    .filter(|message| message["role"] == "assistant")
                    .count();
                error = Some(failure);
                break;
            }
            _ => {}
        }
    }
    let wall_ms = started.elapsed().as_millis();
    // The turn owns the cancel flag; give it a moment to unwind before scoring.
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;

    let (passed, check_output) = run_check(task, &workspace).await?;

    Ok(RunOutcome {
        task: task.name.clone(),
        state: if state_on { "on" } else { "off" },
        repetition,
        passed,
        tool_calls,
        assistant_steps,
        tokens: tokens.load(Ordering::Relaxed),
        wall_ms,
        papercut_recalls,
        timed_out,
        error,
        check_output,
    })
}

/// Score a finished run. Exit 0 is a pass; everything else is a failure whose
/// output is kept for the report.
async fn run_check(task: &EvalTask, workspace: &Path) -> Result<(bool, String)> {
    let script = task.root.join("check.sh");
    if !script.is_file() {
        bail!("task `{}` has no check.sh", task.name);
    }
    // Invoked through `bash` rather than executed directly, so a fixture that
    // lost its executable bit in git still scores.
    let output = tokio::time::timeout(
        Duration::from_secs(CHECK_TIMEOUT_SECONDS),
        tokio::process::Command::new("bash")
            .arg(&script)
            .current_dir(workspace)
            .env("ABACUS_EVAL_WORKSPACE", workspace)
            .output(),
    )
    .await;
    let output = match output {
        Err(_) => return Ok((false, format!("check.sh exceeded {CHECK_TIMEOUT_SECONDS}s"))),
        Ok(result) => result.with_context(|| format!("run {}", script.display()))?,
    };
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    let passed = output.status.success();
    Ok((
        passed,
        if passed {
            String::new()
        } else {
            truncate(&combined, 2_000)
        },
    ))
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.trim().to_owned();
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

/// A fresh git repository, so `git_diff` and `git_status` work and the model
/// sees a clean tree rather than a pile of untracked files.
async fn init_git(workspace: &Path) -> Result<()> {
    let run = |args: Vec<&str>| {
        let mut command = tokio::process::Command::new("git");
        command.args(args).current_dir(workspace);
        command
    };
    for args in [
        vec!["init", "-q"],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.email=eval@abacus.local",
            "-c",
            "user.name=abacus eval",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    ] {
        let status = run(args).output().await.context("run git")?;
        if !status.status.success() {
            bail!(
                "git setup failed in {}: {}",
                workspace.display(),
                String::from_utf8_lossy(&status.stderr).trim()
            );
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copy into {}", target.display()))?;
        }
    }
    Ok(())
}

fn median(mut values: Vec<u64>) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

fn print_summary(outcomes: &[RunOutcome], repeat: usize, state: EvalState) {
    let mut names: Vec<&str> = Vec::new();
    for outcome in outcomes {
        if !names.contains(&outcome.task.as_str()) {
            names.push(&outcome.task);
        }
    }

    println!("\n{:-<72}", "");
    println!(
        "{:<28} {:>10} {:>10} {:>10} {:>10}",
        "task", "state", "pass", "calls", "tokens"
    );
    println!("{:-<72}", "");
    for name in &names {
        for arm in ["on", "off"] {
            let runs: Vec<&RunOutcome> = outcomes
                .iter()
                .filter(|outcome| outcome.task == *name && outcome.state == arm)
                .collect();
            if runs.is_empty() {
                continue;
            }
            let passed = runs.iter().filter(|outcome| outcome.passed).count();
            println!(
                "{:<28} {:>10} {:>10} {:>10} {:>10}",
                name,
                arm,
                format!("{passed}/{}", runs.len()),
                median(runs.iter().map(|run| run.tool_calls as u64).collect()),
                median(runs.iter().map(|run| run.tokens).collect()),
            );
        }
    }
    println!("{:-<72}", "");

    let rate = |arm: &str| -> (usize, usize) {
        let runs: Vec<&RunOutcome> = outcomes
            .iter()
            .filter(|outcome| outcome.state == arm)
            .collect();
        (
            runs.iter().filter(|outcome| outcome.passed).count(),
            runs.len(),
        )
    };
    if state == EvalState::Both {
        let (on_passed, on_total) = rate("on");
        let (off_passed, off_total) = rate("off");
        println!("state on:  {on_passed}/{on_total}    state off: {off_passed}/{off_total}");
        println!(
            "\nThe on/off delta is the evidence that accumulated state earns its keep.\n\
             At {repeat} repetition(s) per arm this is a weak signal — treat it as a\n\
             regression tripwire, not a benchmark, and do not tune against it."
        );
    } else {
        let arm = if state == EvalState::On { "on" } else { "off" };
        let (passed, total) = rate(arm);
        println!("state {arm}: {passed}/{total} over {repeat} repetition(s)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task(root: &Path, name: &str, body: &str, check: &str) {
        let dir = root.join(TASKS_DIR).join(name);
        std::fs::create_dir_all(dir.join("fixture")).unwrap();
        std::fs::write(dir.join("task.toml"), body).unwrap();
        std::fs::write(dir.join("check.sh"), check).unwrap();
    }

    #[test]
    fn discovers_and_parses_tasks_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        write_task(
            dir.path(),
            "fix-importer",
            "prompt = \"fix it\"\n",
            "exit 0\n",
        );
        let tasks = discover(dir.path(), None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "fix-importer");
        assert_eq!(tasks[0].file.max_steps, DEFAULT_MAX_STEPS);
        assert_eq!(tasks[0].file.timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
        // Build is the default: a task scored on a mutation should not spend
        // steps deciding whether it is allowed to mutate.
        assert_eq!(tasks[0].mode().unwrap(), AgentMode::Build);
    }

    #[test]
    fn filter_selects_a_subset_and_an_empty_match_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_task(dir.path(), "alpha", "prompt = \"a\"\n", "exit 0\n");
        write_task(dir.path(), "beta", "prompt = \"b\"\n", "exit 0\n");
        assert_eq!(discover(dir.path(), None).unwrap().len(), 2);
        let filtered = discover(dir.path(), Some("alph")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "alpha");
        assert!(discover(dir.path(), Some("gamma")).is_err());
    }

    #[test]
    fn an_unknown_key_in_task_toml_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // deny_unknown_fields: a typo'd key must fail loudly rather than
        // silently score a task that is not the one that was written.
        write_task(
            dir.path(),
            "typo",
            "prompt = \"x\"\nmax_step = 3\n",
            "exit 0\n",
        );
        assert!(discover(dir.path(), None).is_err());
    }

    #[test]
    fn unknown_mode_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_task(
            dir.path(),
            "weird",
            "prompt = \"x\"\nmode = \"turbo\"\n",
            "exit 0\n",
        );
        let tasks = discover(dir.path(), None).unwrap();
        assert!(tasks[0].mode().is_err());
    }

    #[test]
    fn copy_tree_reproduces_nested_fixtures() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src");
        std::fs::create_dir_all(source.join("nested/deep")).unwrap();
        std::fs::write(source.join("top.txt"), "top").unwrap();
        std::fs::write(source.join("nested/deep/leaf.txt"), "leaf").unwrap();
        let destination = dir.path().join("dst");
        std::fs::create_dir_all(&destination).unwrap();
        copy_tree(&source, &destination).unwrap();
        assert_eq!(
            std::fs::read_to_string(destination.join("top.txt")).unwrap(),
            "top"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("nested/deep/leaf.txt")).unwrap(),
            "leaf"
        );
    }

    #[tokio::test]
    async fn check_script_decides_pass_and_keeps_output_only_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        write_task(
            dir.path(),
            "passing",
            "prompt = \"x\"\n",
            "echo looks-good\nexit 0\n",
        );
        write_task(
            dir.path(),
            "failing",
            "prompt = \"x\"\n",
            "echo what-went-wrong >&2\nexit 1\n",
        );
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tasks = discover(dir.path(), None).unwrap();

        let failing = tasks.iter().find(|task| task.name == "failing").unwrap();
        let (passed, output) = run_check(failing, &workspace).await.unwrap();
        assert!(!passed);
        assert!(output.contains("what-went-wrong"), "{output}");

        let passing = tasks.iter().find(|task| task.name == "passing").unwrap();
        let (passed, output) = run_check(passing, &workspace).await.unwrap();
        assert!(passed);
        // A passing check contributes no noise to the report.
        assert!(output.is_empty(), "{output}");
    }

    #[tokio::test]
    async fn a_check_script_reads_the_mutated_workspace() {
        let dir = tempfile::tempdir().unwrap();
        write_task(
            dir.path(),
            "reads-workspace",
            "prompt = \"x\"\n",
            "test -f done.txt\n",
        );
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let task = &discover(dir.path(), None).unwrap()[0];
        assert!(!run_check(task, &workspace).await.unwrap().0);
        std::fs::write(workspace.join("done.txt"), "").unwrap();
        assert!(run_check(task, &workspace).await.unwrap().0);
    }

    #[tokio::test]
    async fn git_init_leaves_a_clean_tree() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("a.txt"), "hello").unwrap();
        init_git(&workspace).await.unwrap();
        let status = tokio::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&workspace)
            .output()
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "fixture must be committed so the agent sees a clean tree"
        );
    }

    #[test]
    fn the_shipped_suite_parses_and_is_scoreable() {
        // A malformed task.toml or a task missing its checker would otherwise
        // only surface as a mysterious failure mid-run, after paying for a
        // model call.
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let tasks = discover(repo, None).expect("shipped eval suite must parse");
        assert!(!tasks.is_empty());
        for task in &tasks {
            assert!(
                task.root.join("check.sh").is_file(),
                "task `{}` has no check.sh",
                task.name
            );
            assert!(
                !task.file.prompt.trim().is_empty(),
                "task `{}` has an empty prompt",
                task.name
            );
            task.mode()
                .unwrap_or_else(|error| panic!("task `{}`: {error}", task.name));
        }
    }

    #[test]
    fn median_is_stable_for_even_and_odd_counts() {
        assert_eq!(median(vec![]), 0);
        assert_eq!(median(vec![5]), 5);
        assert_eq!(median(vec![9, 1, 5]), 5);
        assert_eq!(median(vec![4, 1]), 4);
    }

    #[test]
    fn truncate_keeps_a_char_boundary() {
        let text = "é".repeat(50);
        let cut = truncate(&text, 10);
        assert!(cut.chars().count() <= 11);
        assert!(cut.ends_with('…'));
        assert_eq!(truncate("short", 100), "short");
    }
}
