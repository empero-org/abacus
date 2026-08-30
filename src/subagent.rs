use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, atomic::AtomicBool},
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, process::Command, sync::mpsc};
use uuid::Uuid;

use crate::{
    agent::{AgentEvent, AgentMode, TurnOptions, initial_messages, run_turn},
    compaction::CompactionState,
    goal::GoalState,
    model_info::CompactionBudget,
    provider::Provider,
    services::AgentServices,
    task::TaskList,
};

const MAX_SUBAGENTS: usize = 8;
const MAX_TASK_CHARS: usize = 24_000;
const MAX_PATCH_CHARS: usize = 120_000;
const MAX_UNTRACKED_FILES: usize = 10_000;
const MAX_UNTRACKED_BYTES: u64 = 500_000_000;
const MAX_UNTRACKED_FILE_BYTES: u64 = 100_000_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    tasks: Vec<SubagentTask>,
    #[serde(default)]
    apply: bool,
    /// Block the turn until every worker finishes. Off by default: workers run
    /// in the background and report back after a later tool call, so the
    /// orchestrator keeps working instead of idling.
    #[serde(default)]
    wait: bool,
    #[serde(default = "default_concurrency")]
    max_concurrency: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentTask {
    name: String,
    prompt: String,
    #[serde(default)]
    role: SubagentRole,
    /// An optional model slug for this worker, run on the same endpoint as the
    /// orchestrator — so one swarm can fan out across several models (e.g. five
    /// different OpenRouter models). None uses the orchestrator's model.
    #[serde(default)]
    model: Option<String>,
    /// A JSON Schema this worker's answer must satisfy. When set, the worker's
    /// prose report is coerced into the shape and validated, so the parent gets
    /// data instead of something it has to re-read.
    #[serde(default)]
    schema: Option<Value>,
    /// A harness `subagent` entry id supplying this worker's role prompt and
    /// privileges, instead of one of the built-in roles.
    #[serde(default)]
    spec: Option<String>,
    /// Prior conversation for a resumed worker — never part of the tool call,
    /// set only by `message_subagent` so the worker continues instead of
    /// starting over.
    #[serde(skip)]
    resume_from: Option<Vec<Value>>,
}

/// What kind of worker a task gets. The role shapes both the worker's system
/// prompt and its privileges: scouts run read-only (PLAN mode, no mutations),
/// drones and workers may build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum SubagentRole {
    /// Builder: executes a concrete change and verifies it.
    Drone,
    /// Researcher: reads, crawls, searches — never modifies.
    Scout,
    /// Generic: the full standard toolset.
    #[default]
    Worker,
}

impl SubagentRole {
    fn label(self) -> &'static str {
        match self {
            Self::Drone => "drone",
            Self::Scout => "scout",
            Self::Worker => "worker",
        }
    }

    fn system_prompt(self) -> &'static str {
        match self {
            Self::Drone => {
                "You are a DRONE: a builder. Execute exactly the delegated change, run the \
                 narrowest checks that verify it, and report what you changed and what you ran. \
                 Do not investigate beyond what the change requires. Do not spawn subagents, \
                 commit, push, or modify paths outside the workspace."
            }
            Self::Scout => {
                "You are a SCOUT: a researcher. Investigate the delegated question — read code, \
                 crawl the repository, search the web where allowed — and report findings with \
                 exact file paths and evidence. You must NOT modify anything; your value is the \
                 fidelity of what you bring back. Do not spawn subagents."
            }
            Self::Worker => {
                "You are an isolated subagent. Complete only the delegated task. You may edit \
                 and test this worktree. Do not spawn more subagents, commit, push, or modify \
                 paths outside the workspace. Finish with a concise summary and exact checks run."
            }
        }
    }
}

#[derive(Debug)]
struct SubagentResult {
    name: String,
    response: String,
    patch: String,
    /// The schema-conforming answer, when the task declared a schema.
    structured: Option<Value>,
    error: Option<String>,
}

#[derive(Clone)]
pub struct SubagentRuntime {
    workspace: PathBuf,
    provider: Provider,
    services: Arc<AgentServices>,
    max_steps: usize,
    tool_output_limit: usize,
    web_search: crate::web::WebConfig,
    hive: crate::hive::HiveHandle,
    /// Supplies authored delegation specs; inert stores fall back to built-ins.
    harness: crate::harness::HarnessStore,
    /// Where a background swarm delivers its report when it finishes.
    injections: crate::agent::InjectionQueue,
}

/// A worker's resolved role: what to call it, what to tell it, and whether it
/// is allowed to change anything.
struct ResolvedRole {
    label: String,
    system_prompt: String,
    read_only: bool,
}

impl SubagentRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace: PathBuf,
        provider: Provider,
        services: Arc<AgentServices>,
        max_steps: usize,
        tool_output_limit: usize,
        web_search: crate::web::WebConfig,
        hive: crate::hive::HiveHandle,
        harness: crate::harness::HarnessStore,
        injections: crate::agent::InjectionQueue,
    ) -> Self {
        Self {
            workspace,
            provider,
            services,
            max_steps,
            tool_output_limit,
            web_search,
            hive,
            harness,
            injections,
        }
    }

    fn resolve_role(&self, task: &SubagentTask) -> ResolvedRole {
        resolve_role(&self.harness, task)
    }
}

/// Resolve a task's role, preferring an authored spec over the built-in.
///
/// An unknown spec id falls back to the built-in role rather than failing the
/// worker: the roster is advisory context in the prompt, and a stale id should
/// cost fidelity, not the task.
fn resolve_role(harness: &crate::harness::HarnessStore, task: &SubagentTask) -> ResolvedRole {
    {
        if let Some(id) = task.spec.as_deref().filter(|id| !id.trim().is_empty())
            && let Some(entry) = harness
                .snapshot_of(crate::harness::EntryKind::Subagent)
                .into_iter()
                .find(|entry| entry.id == id)
        {
            return ResolvedRole {
                label: entry.id.clone(),
                system_prompt: entry.content.clone(),
                read_only: entry
                    .metadata
                    .get("read_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
        }
        ResolvedRole {
            label: task.role.label().to_owned(),
            system_prompt: task.role.system_prompt().to_owned(),
            read_only: task.role == SubagentRole::Scout,
        }
    }
}

impl SubagentRuntime {
    pub fn tool_spec() -> Value {
        json!({
            "type":"function",
            "function":{
                "name":"spawn_subagents",
                "description":"Delegate tasks to parallel agents in isolated git worktrees. Prefer this when the request splits into two or more genuinely separable units — independent files, modules, fixes, or research questions that need no shared intermediate state — and run them in one call; it is the efficient way to parallelize. Use scout roles to parallelize investigation (repo crawls, research, web searches); a quick single lookup is still faster done directly. Do NOT use it for a single indivisible task or for tightly-coupled sequential edits. Each worker starts from the current workspace state and cannot spawn its own subagents.\n\nWorkers run in the BACKGROUND by default: this returns immediately with the roster, and each swarm's report is delivered to you automatically after a later tool call. Keep working while they run — do not idle, and never invent or predict a pending worker's findings; wait for the delivered report. Pass wait=true only when you genuinely cannot proceed without the results. Set apply=true to apply non-conflicting patches to the parent workspace.",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "tasks":{
                            "type":"array",
                            "minItems":1,
                            "maxItems":MAX_SUBAGENTS,
                            "items":{
                                "type":"object",
                                "properties":{
                                    "name":{"type":"string","description":"Short unique worker name"},
                                    "prompt":{"type":"string","description":"Self-contained coding assignment with expected verification"},
                                    "role":{"type":"string","enum":["drone","scout","worker"],"description":"drone = builder executing a concrete change; scout = read-only research/repo-crawl/web (cannot modify anything); worker = generic (default)"},
                                    "spec":{"type":"string","description":"Optional id of a delegation spec from the harness roster, used instead of `role`. The roster is listed in your context; an unknown id quietly falls back to `role`."},
                                    "model":{"type":"string","description":"Optional model slug for this worker on the same endpoint. Omit to use your own (the main) model — that is the default and the right choice unless the user explicitly asked for specific models per worker (e.g. fanning a swarm across several OpenRouter models)."},
                                    "schema":{"type":"object","description":"Optional JSON Schema for this worker's answer. Set it when you want data you can act on rather than a report you have to re-read — e.g. {\"type\":\"object\",\"required\":[\"verdict\"],\"properties\":{\"verdict\":{\"type\":\"string\",\"enum\":[\"pass\",\"fail\"]}}}. With wait=true and every task carrying a schema, the result is a JSON array instead of prose. A worker whose answer cannot be made to fit is reported as failed."}
                                },
                                "required":["name","prompt"],
                                "additionalProperties":false
                            }
                        },
                        "apply":{"type":"boolean","description":"Apply each clean patch to the parent workspace after workers finish (default false)"},
                        "wait":{"type":"boolean","description":"Block until every worker finishes instead of getting the report later (default false). Use only when you cannot continue without the results."},
                        "max_concurrency":{"type":"integer","minimum":1,"maximum":MAX_SUBAGENTS}
                    },
                    "required":["tasks"],
                    "additionalProperties":false
                }
            }
        })
    }

    /// The tool for addressing one worker by name after it was spawned.
    pub fn message_tool_spec() -> Value {
        json!({
            "type":"function",
            "function":{
                "name":"message_subagent",
                "description":"Send a message to one worker you spawned, by name. If it is still running the message reaches it mid-task — use this to correct or extend a worker without killing it. If it already finished, it picks up where it left off with its conversation intact, in a fresh worktree seeded from the current workspace; its reply is delivered to you after a later tool call, like any background worker. Prefer this over re-spawning a worker for the same thread of work, because the worker keeps everything it already learned.",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "name":{"type":"string","description":"The worker's name, exactly as given to spawn_subagents"},
                        "message":{"type":"string","description":"What to tell it — a correction, extra context, or a follow-up task"}
                    },
                    "required":["name","message"],
                    "additionalProperties":false
                }
            }
        })
    }

    pub async fn message(&self, arguments: &str) -> String {
        match self.message_inner(arguments).await {
            Ok(output) => output,
            Err(error) => format!("Error: {error:#}"),
        }
    }

    async fn message_inner(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct MessageArgs {
            name: String,
            message: String,
        }
        let args: MessageArgs =
            serde_json::from_str(arguments).context("invalid message_subagent arguments")?;
        let name = args.name.trim();
        if args.message.trim().is_empty() {
            bail!("message must not be empty");
        }
        let Some(channel) = self.hive.workers.channel(name) else {
            let roster = self.hive.workers.roster();
            if roster.is_empty() {
                bail!("no worker named `{name}` — none have been spawned yet");
            }
            let known = roster
                .iter()
                .map(|(worker, running)| {
                    format!(
                        "{worker} ({})",
                        if *running { "running" } else { "finished" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            bail!("no worker named `{name}`. Known workers: {known}");
        };

        match channel {
            // Running: steer it, exactly as the user steers the main turn.
            crate::hive::WorkerChannel::Live(injections) => {
                injections.push(crate::agent::Injection::UserMessage(args.message));
                Ok(format!(
                    "Delivered to `{name}`, which is still running — it picks the message up \
                     after its current step. Its report will reach you as usual."
                ))
            }
            // Finished: continue the same thread of work with its context.
            crate::hive::WorkerChannel::Finished(transcript) if !transcript.is_empty() => {
                self.resume_worker(name.to_owned(), transcript, args.message)
                    .await
            }
            crate::hive::WorkerChannel::Finished(_) => bail!(
                "`{name}` finished without a usable conversation (it failed early); \
                 spawn a fresh worker instead"
            ),
        }
    }

    /// Continue a finished worker: its own conversation plus the new message,
    /// in a fresh worktree seeded from the workspace as it is now. Runs in the
    /// background and reports back like any other worker.
    async fn resume_worker(
        &self,
        name: String,
        transcript: Vec<Value>,
        message: String,
    ) -> Result<String> {
        let context = Arc::new(WorktreeContext::capture(&self.workspace).await?);
        let runtime = self.clone();
        let injections = self.injections.clone();
        let reported = name.clone();
        tokio::spawn(async move {
            let report = match runtime
                .run_resumed(context, &name, transcript, message)
                .await
            {
                Ok(response) => format!("worker `{name}` (resumed): {response}"),
                Err(error) => format!("worker `{name}` (resumed) failed: {error:#}"),
            };
            injections.push(crate::agent::Injection::SubagentReport(report));
        });
        Ok(format!(
            "`{reported}` finished earlier; it has been restarted with its previous \
             conversation and your message. Keep working — its reply will be delivered \
             after a later tool call."
        ))
    }

    pub fn approval_details(arguments: &str) -> String {
        match parse_args(arguments) {
            Ok(args) => {
                let names = args
                    .tasks
                    .iter()
                    .map(
                        |task| match task.model.as_deref().filter(|m| !m.trim().is_empty()) {
                            Some(model) => {
                                format!("{} ({}, {model})", task.name, task.role.label())
                            }
                            None => format!("{} ({})", task.name, task.role.label()),
                        },
                    )
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "Run {} isolated worker(s): {names}\nApply patches to this workspace: {}",
                    args.tasks.len(),
                    args.apply
                )
            }
            Err(error) => format!("Invalid subagent request: {error:#}"),
        }
    }

    pub async fn execute(&self, arguments: &str) -> String {
        match self.execute_inner(arguments).await {
            Ok(output) => output,
            Err(error) => format!("Error: {error:#}"),
        }
    }

    async fn execute_inner(&self, arguments: &str) -> Result<String> {
        let args = parse_args(arguments)?;
        // The parent snapshot is taken now, in the foreground, so background
        // workers start from the workspace as it was when they were spawned
        // rather than from whatever the main agent edits next.
        let context = Arc::new(WorktreeContext::capture(&self.workspace).await?);
        let concurrency = args.max_concurrency.clamp(1, MAX_SUBAGENTS);
        let apply = args.apply;
        let roster = args
            .tasks
            .iter()
            .map(
                |task| match task.model.as_deref().filter(|m| !m.trim().is_empty()) {
                    Some(model) => format!("{} ({}, {model})", task.name, task.role.label()),
                    None => format!("{} ({})", task.name, task.role.label()),
                },
            )
            .collect::<Vec<_>>()
            .join(", ");
        let count = args.tasks.len();
        let runtime = self.clone();
        let tasks = args.tasks;

        if args.wait {
            return Ok(runtime.run_swarm(context, tasks, concurrency, apply).await);
        }
        // Background: hand the swarm to a detached task and return at once.
        // The report is pushed onto the injection queue, which the running
        // turn drains after its next tool call — the model keeps working in
        // the meantime instead of blocking on workers.
        let injections = self.injections.clone();
        tokio::spawn(async move {
            let report = runtime.run_swarm(context, tasks, concurrency, apply).await;
            injections.push(crate::agent::Injection::SubagentReport(report));
        });
        Ok(format!(
            "Started {count} background worker(s): {roster}.\nThey are running now; their \
             report will be delivered to you automatically after a later tool call. Continue \
             with other work in the meantime — do not wait, and do not guess what they will \
             find. Pass wait=true if you ever need results before continuing."
        ))
    }

    /// Run a swarm to completion and format its report. Shared by the blocking
    /// and background paths so both record the same delegation stats.
    async fn run_swarm(
        &self,
        context: Arc<WorktreeContext>,
        tasks: Vec<SubagentTask>,
        concurrency: usize,
        apply: bool,
    ) -> String {
        // Decided before the tasks are consumed: a swarm where every worker was
        // asked for a shape reports as data.
        let all_typed = !tasks.is_empty() && tasks.iter().all(|task| task.schema.is_some());
        let runtime = self.clone();
        let mut results = stream::iter(tasks.into_iter().map(|task| {
            let runtime = runtime.clone();
            let context = context.clone();
            async move { runtime.run_one(context, task).await }
        }))
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
        results.sort_by(|left, right| left.name.cmp(&right.name));

        if apply {
            for result in &mut results {
                if result.error.is_none()
                    && !result.patch.trim().is_empty()
                    && let Err(error) = apply_patch(&context.repo_root, &result.patch).await
                {
                    result.error = Some(format!("patch was not applied: {error:#}"));
                }
            }
        }

        // The delegation record grows with every swarm, and the model sees
        // its own track record in the result — confidence is earned, in
        // writing.
        let failures = results
            .iter()
            .filter(|result| result.error.is_some())
            .count();
        let record = self.hive.record_run(results.len() as u32, failures as u32);
        let body = if all_typed {
            format_results_json(&results)
        } else {
            format_results(&results, apply)
        };
        format!("{body}\n\n{record}")
    }

    /// Run a resumed worker to completion in its own worktree, replaying its
    /// prior conversation so it continues rather than restarts.
    async fn run_resumed(
        &self,
        context: Arc<WorktreeContext>,
        name: &str,
        transcript: Vec<Value>,
        message: String,
    ) -> Result<String> {
        let (worker_provider, worker_tokens) = self.provider.with_detached_counter();
        let board_id = self
            .hive
            .board
            .begin(name, "resumed", worker_tokens.clone());
        let injections = self.hive.workers.open(name);
        let task = SubagentTask {
            name: name.to_owned(),
            prompt: message,
            role: SubagentRole::Worker,
            model: None,
            // A follow-up is conversational; the parent asks for a shape by
            // spawning, not by messaging.
            schema: None,
            spec: None,
            resume_from: Some(transcript.clone()),
        };
        let outcome = self
            .run_one_inner(&context, &task, board_id, worker_provider, injections)
            .await;
        self.provider
            .add_tokens(worker_tokens.load(std::sync::atomic::Ordering::Relaxed));
        match outcome {
            Ok((response, _patch, new_transcript)) => {
                self.hive.board.finish(board_id, true);
                // The continued conversation replaces the old one, so a second
                // follow-up builds on this round too.
                self.hive.workers.close(name, new_transcript);
                Ok(response)
            }
            Err(error) => {
                self.hive.board.finish(board_id, false);
                self.hive.workers.close(name, transcript);
                Err(error)
            }
        }
    }

    async fn run_one(&self, context: Arc<WorktreeContext>, task: SubagentTask) -> SubagentResult {
        let name = task.name.clone();
        let (mut worker_provider, worker_tokens) = self.provider.with_detached_counter();
        if let Some(model) = task
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
        {
            worker_provider = worker_provider.with_model(model);
        }
        let role_label = self.resolve_role(&task).label;
        let board_id = self
            .hive
            .board
            .begin(&name, &role_label, worker_tokens.clone());
        // Register before starting so a message addressed to this worker mid-run
        // reaches the turn that is about to begin.
        let worker_injections = self.hive.workers.open(&name);
        let outcome = self
            .run_one_inner(
                &context,
                &task,
                board_id,
                worker_provider,
                worker_injections,
            )
            .await;
        // Fold the worker's usage into the session total — the per-worker
        // counter exists for the board, not to hide cost.
        self.provider
            .add_tokens(worker_tokens.load(std::sync::atomic::Ordering::Relaxed));
        match outcome {
            Ok((response, patch, transcript)) => {
                // Coercion happens after the work, not during it: a worker told
                // to fill in a shape while it is still investigating bends its
                // reasoning toward the shape. This way the schema constrains the
                // report and nothing else.
                let structured = match &task.schema {
                    Some(schema) => {
                        match coerce_to_schema(&self.provider, &response, schema).await {
                            Ok(value) => Some(value),
                            Err(error) => {
                                self.hive.board.finish(board_id, false);
                                self.hive.workers.close(&name, transcript);
                                return SubagentResult {
                                    name,
                                    response,
                                    patch,
                                    structured: None,
                                    error: Some(format!(
                                        "result did not match the schema: {error:#}"
                                    )),
                                };
                            }
                        }
                    }
                    None => None,
                };
                self.hive.board.finish(board_id, true);
                // Keep the conversation so the orchestrator can follow up with
                // this worker and have it continue where it left off.
                self.hive.workers.close(&name, transcript);
                SubagentResult {
                    name,
                    response,
                    patch,
                    structured,
                    error: None,
                }
            }
            Err(error) => {
                self.hive.board.finish(board_id, false);
                self.hive.workers.close(&name, Vec::new());
                SubagentResult {
                    name,
                    response: String::new(),
                    patch: String::new(),
                    structured: None,
                    error: Some(format!("{error:#}")),
                }
            }
        }
    }

    async fn run_one_inner(
        &self,
        context: &WorktreeContext,
        task: &SubagentTask,
        board_id: u64,
        provider: Provider,
        injections: crate::agent::InjectionQueue,
    ) -> Result<(String, String, Vec<Value>)> {
        let worker_root = std::env::temp_dir().join("abacus-worktrees").join(format!(
            "{}-{}",
            safe_name(&task.name),
            Uuid::new_v4()
        ));
        if let Some(parent) = worker_root.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let setup = context.create(&worker_root).await;
        if let Err(error) = setup {
            let _ = context.remove(&worker_root).await;
            return Err(error);
        }

        let mut guard = WorktreeGuard::new(context.repo_root.clone(), worker_root.clone());
        let result = self
            .run_in_worktree(context, &worker_root, task, board_id, provider, injections)
            .await;
        let cleanup = context.remove(&worker_root).await;
        if cleanup.is_ok() {
            guard.disarm();
        }
        match (result, cleanup) {
            (Ok(result), Ok(())) => Ok(result),
            (Ok(_), Err(error)) => Err(error.context("worker succeeded but cleanup failed")),
            (Err(error), _) => Err(error),
        }
    }

    async fn run_in_worktree(
        &self,
        context: &WorktreeContext,
        worker_root: &Path,
        task: &SubagentTask,
        board_id: u64,
        provider: Provider,
        injections: crate::agent::InjectionQueue,
    ) -> Result<(String, String, Vec<Value>)> {
        let role = self.resolve_role(task);
        let worker_workspace = worker_root.join(&context.workspace_relative);
        // A resumed worker keeps its own conversation; a fresh one starts from
        // the standard preamble. Either way the new prompt is the last word.
        let mut messages = match &task.resume_from {
            Some(prior) if !prior.is_empty() => {
                let mut messages = prior.clone();
                messages.push(json!({
                    "role":"system",
                    "content":"Continuing your earlier task in a fresh worktree seeded from the \
                               current workspace. Your own file changes from before are not here \
                               unless they were applied; re-read what you need. Everything you \
                               learned above still stands."
                }));
                messages
            }
            _ => {
                let mut messages = initial_messages(&worker_workspace);
                messages.push(json!({
                    "role":"system",
                    "content": role.system_prompt
                }));
                messages
            }
        };
        messages.push(json!({"role":"user","content":task.prompt}));
        let (events, mut receiver) = mpsc::unbounded_channel();
        let services = Arc::new(self.services.for_workspace(worker_workspace.clone()));
        let turn = run_turn(
            provider,
            messages,
            TurnOptions {
                safety: crate::safety::SafetyCache::default(),
                safety_uses_main: false,
                // Subagent work is a different task shape from the main loop;
                // mixing it into the same trace would blur the samples.
                trace: None,
                cancel: Arc::new(AtomicBool::new(false)),
                workspace: worker_workspace,
                max_steps: self.max_steps,
                tool_output_limit: self.tool_output_limit,
                // Scouts are mechanically read-only, not just instructed so:
                // PLAN mode plus a mutation lock that nothing in the worker
                // can flip.
                mode: if role.read_only {
                    AgentMode::Plan
                } else {
                    AgentMode::Build
                },
                allow_mutations: Arc::new(AtomicBool::new(!role.read_only)),
                services,
                session_id: None,
                goal: GoalState::default(),
                tasks: TaskList::default(),
                compaction: CompactionState::default(),
                compaction_budget: CompactionBudget::default(),
                token_compression: false,
                allow_subagents: false,
                web_search: self.web_search.clone(),
                // Inert: a subagent's snags belong to its delegated task, and
                // its worktree paths would pollute workspace scoping.
                papercuts: crate::papercuts::PapercutStore::default(),
                harness: crate::harness::HarnessStore::default(),
                handles: crate::handles::HandleStore::default(),
                tether: crate::tether::TetherState::default(),
                hive: crate::hive::HiveHandle::default(),
                aux_model: None,
                injections,
                modes: crate::modes::ModeCoach::default(),
            },
            events,
        );
        let mut turn = turn;
        let mut final_messages = None;
        let mut failure = None;
        loop {
            tokio::select! {
                () = &mut turn => break,
                event = receiver.recv() => {
                    if let Some(event) = event {
                        self.note_activity(board_id, &event);
                        capture_event(event, &mut final_messages, &mut failure);
                    }
                }
            }
        }
        while let Ok(event) = receiver.try_recv() {
            capture_event(event, &mut final_messages, &mut failure);
        }
        if let Some(error) = failure {
            bail!("subagent stopped: {error}");
        }
        let transcript = final_messages.unwrap_or_default();
        let response = final_assistant_text(&transcript);
        let patch = context.diff(worker_root).await?;
        Ok((response, patch, transcript))
    }
}

impl SubagentRuntime {
    /// Mirror a worker's visible activity onto the live board.
    fn note_activity(&self, board_id: u64, event: &AgentEvent) {
        match event {
            AgentEvent::ToolStarted { name, summary } => {
                self.hive
                    .board
                    .activity(board_id, &format!("{name} {summary}"));
            }
            AgentEvent::Delta(text) => self.hive.board.activity(board_id, text),
            _ => {}
        }
    }
}

/// Attempts at producing a schema-conforming value before giving up.
const COERCE_ATTEMPTS: usize = 3;

/// Turn a worker's prose report into a value matching `schema`.
///
/// The validation error is fed back verbatim on a retry, because it names the
/// offending path — a worker told "$.findings: expected array, found string"
/// can fix that, where "invalid output" leaves it guessing. Exhausting the
/// attempts is a failure, not a silent fallback to prose: a parent that asked
/// for data and got a paragraph would have no way to notice.
async fn coerce_to_schema(provider: &Provider, report: &str, schema: &Value) -> Result<Value> {
    let system = format!(
        "Convert a worker agent's report into JSON matching this schema. Return only the JSON \
         object — no prose, no code fence. Use only what the report actually says; if it does \
         not establish a required field, use the most conservative value the schema allows \
         rather than inventing one.\n\nSchema:\n{}",
        serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
    );
    let mut user = format!("<report>\n{report}\n</report>");
    let mut last_error = String::new();

    for attempt in 0..COERCE_ATTEMPTS {
        if attempt > 0 {
            user = format!(
                "<report>\n{report}\n</report>\n\nYour previous answer was rejected: {last_error}\n\
                 Return corrected JSON only."
            );
        }
        let conversation = vec![
            json!({"role":"system","content":system}),
            json!({"role":"user","content":user}),
        ];
        let (deltas, _sink) = mpsc::unbounded_channel();
        let cancel = AtomicBool::new(false);
        let completion = match provider.complete(&conversation, &[], deltas, &cancel).await {
            Ok(completion) => completion,
            Err(error) => {
                last_error = format!("{error:#}");
                continue;
            }
        };
        if completion.cancelled {
            bail!("cancelled while coercing the result");
        }
        match crate::refine::extract_json(&completion.content) {
            Err(error) => last_error = format!("{error:#}"),
            Ok(value) => match crate::schema::validate(&value, schema) {
                Ok(()) => return Ok(value),
                Err(error) => last_error = format!("{error:#}"),
            },
        }
    }
    bail!("after {COERCE_ATTEMPTS} attempts: {last_error}")
}

fn capture_event(
    event: AgentEvent,
    final_messages: &mut Option<Vec<Value>>,
    failure: &mut Option<String>,
) {
    match event {
        AgentEvent::Approval(request) => {
            let _ = request.respond.send(crate::agent::ApprovalDecision::Once);
        }
        AgentEvent::Done { messages, .. } => *final_messages = Some(messages),
        AgentEvent::Failed { error, messages } => {
            *failure = Some(error);
            *final_messages = Some(messages);
        }
        _ => {}
    }
}

struct WorktreeGuard {
    repo_root: PathBuf,
    worker_root: PathBuf,
    active: bool,
}

impl WorktreeGuard {
    fn new(repo_root: PathBuf, worker_root: PathBuf) -> Self {
        Self {
            repo_root,
            worker_root,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        if self.active && self.worker_root.exists() {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo_root)
                .args(["worktree", "remove", "--force"])
                .arg(&self.worker_root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[derive(Debug)]
struct WorktreeContext {
    repo_root: PathBuf,
    workspace_relative: PathBuf,
    baseline_patch: Vec<u8>,
    untracked: Vec<PathBuf>,
}

impl WorktreeContext {
    async fn capture(workspace: &Path) -> Result<Self> {
        let root = git_output(workspace, &["rev-parse", "--show-toplevel"])
            .await
            .context("subagents require a git workspace")?;
        let repo_root = PathBuf::from(root.trim()).canonicalize()?;
        let workspace = workspace.canonicalize()?;
        let workspace_relative = workspace
            .strip_prefix(&repo_root)
            .context("workspace is outside its git repository")?
            .to_owned();
        let scope = git_scope(&workspace_relative);
        let baseline_patch = git_output_bytes(
            &repo_root,
            &["diff", "--binary", "HEAD", "--", scope.as_str()],
            None,
        )
        .await?;
        let untracked_raw = git_output_bytes(
            &repo_root,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                scope.as_str(),
            ],
            None,
        )
        .await?;
        let untracked = untracked_raw
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                String::from_utf8(path.to_vec())
                    .map(PathBuf::from)
                    .context("subagent worktrees do not support non-UTF-8 untracked paths")
            })
            .collect::<Result<Vec<_>>>()?;
        if untracked.len() > MAX_UNTRACKED_FILES {
            bail!("workspace has more than {MAX_UNTRACKED_FILES} untracked files to seed");
        }
        let mut untracked_bytes = 0_u64;
        for relative in &untracked {
            let metadata = fs::symlink_metadata(repo_root.join(relative))?;
            if metadata.len() > MAX_UNTRACKED_FILE_BYTES {
                bail!("untracked file {} exceeds 100 MB", relative.display());
            }
            untracked_bytes = untracked_bytes.saturating_add(metadata.len());
            if untracked_bytes > MAX_UNTRACKED_BYTES {
                bail!("untracked workspace data exceeds 500 MB");
            }
        }
        Ok(Self {
            repo_root,
            workspace_relative,
            baseline_patch,
            untracked,
        })
    }

    async fn create(&self, worker_root: &Path) -> Result<()> {
        run_git(
            &self.repo_root,
            &[
                "worktree",
                "add",
                "--detach",
                path_text(worker_root)?,
                "HEAD",
            ],
            None,
        )
        .await
        .context("could not create isolated git worktree")?;
        if !self.baseline_patch.is_empty() {
            run_git(
                worker_root,
                &["apply", "--binary", "-"],
                Some(&self.baseline_patch),
            )
            .await
            .context("could not seed worker with current tracked changes")?;
        }
        for relative in &self.untracked {
            let source = self.repo_root.join(relative);
            let destination = worker_root.join(relative);
            copy_entry(&source, &destination)?;
        }
        let scope = git_scope(&self.workspace_relative);
        run_git(worker_root, &["add", "-A", "--", scope.as_str()], None).await?;
        let status = Command::new("git")
            .args(["-C", path_text(worker_root)?, "diff", "--cached", "--quiet"])
            .status()
            .await?;
        if !status.success() {
            let mut command = Command::new("git");
            command
                .args([
                    "-C",
                    path_text(worker_root)?,
                    "commit",
                    "-m",
                    "abacus worker baseline",
                ])
                .env("GIT_AUTHOR_NAME", "Abacus")
                .env("GIT_AUTHOR_EMAIL", "abacus@localhost")
                .env("GIT_COMMITTER_NAME", "Abacus")
                .env("GIT_COMMITTER_EMAIL", "abacus@localhost")
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            let output = command.output().await?;
            if !output.status.success() {
                bail!(
                    "could not snapshot worker baseline: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        Ok(())
    }

    async fn diff(&self, worker_root: &Path) -> Result<String> {
        let scope = git_scope(&self.workspace_relative);
        run_git(worker_root, &["add", "-N", "--", scope.as_str()], None).await?;
        let bytes = git_output_bytes(
            worker_root,
            &["diff", "--binary", "HEAD", "--", scope.as_str()],
            None,
        )
        .await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn remove(&self, worker_root: &Path) -> Result<()> {
        if worker_root.exists() {
            run_git(
                &self.repo_root,
                &["worktree", "remove", "--force", path_text(worker_root)?],
                None,
            )
            .await?;
        }
        Ok(())
    }
}

fn parse_args(arguments: &str) -> Result<SpawnArgs> {
    let args: SpawnArgs = serde_json::from_str(arguments).context("invalid subagent arguments")?;
    if args.tasks.is_empty() || args.tasks.len() > MAX_SUBAGENTS {
        bail!("tasks must contain 1 to {MAX_SUBAGENTS} entries");
    }
    let mut names = std::collections::HashSet::new();
    for task in &args.tasks {
        if task.name.trim().is_empty() || task.name.len() > 64 {
            bail!("worker names must contain 1 to 64 characters");
        }
        if !names.insert(task.name.to_ascii_lowercase()) {
            bail!("worker names must be unique");
        }
        if task.prompt.trim().is_empty() || task.prompt.len() > MAX_TASK_CHARS {
            bail!("each worker prompt must contain 1 to {MAX_TASK_CHARS} characters");
        }
    }
    Ok(args)
}

fn default_concurrency() -> usize {
    4
}

fn safe_name(name: &str) -> String {
    let value: String = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(24)
        .collect();
    if value.is_empty() {
        "worker".into()
    } else {
        value
    }
}

fn git_scope(relative: &Path) -> String {
    if relative.as_os_str().is_empty() {
        ".".into()
    } else {
        relative.to_string_lossy().into_owned()
    }
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("git path is not UTF-8"))
}

async fn git_output(directory: &Path, args: &[&str]) -> Result<String> {
    let output = git_output_bytes(directory, args, None).await?;
    String::from_utf8(output).context("git returned non-UTF-8 text")
}

async fn git_output_bytes(
    directory: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let output = run_git_output(directory, args, stdin).await?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

async fn run_git(directory: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<()> {
    let output = run_git_output(directory, args, stdin).await?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn run_git_output(
    directory: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<std::process::Output> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        // Keep seeded/diffed content byte-exact across platforms (Windows git
        // defaults to core.autocrlf=true, which would rewrite line endings).
        .args(["-c", "core.autocrlf=false"])
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("could not start git")?;
    if let (Some(input), Some(mut child_stdin)) = (stdin, child.stdin.take()) {
        child_stdin.write_all(input).await?;
    }
    Ok(child.wait_with_output().await?)
}

async fn apply_patch(repo_root: &Path, patch: &str) -> Result<()> {
    run_git(
        repo_root,
        &["apply", "--check", "--binary", "-"],
        Some(patch.as_bytes()),
    )
    .await?;
    run_git(
        repo_root,
        &["apply", "--binary", "-"],
        Some(patch.as_bytes()),
    )
    .await
}

fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        bail!("refusing to copy untracked symlink {}", source.display());
    }
    if metadata.is_dir() {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, destination)?;
    }
    Ok(())
}

fn final_assistant_text(messages: &[Value]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant" && message["content"].is_string())
        .and_then(|message| message["content"].as_str())
        .unwrap_or("Subagent completed without a textual summary.")
        .to_owned()
}

/// Render results as JSON when every worker was asked for a shape.
///
/// This is the point of a schema: the parent consumes data instead of parsing
/// prose back out of a markdown report. Mixed swarms fall back to markdown,
/// since half an array is worse than none.
fn format_results_json(results: &[SubagentResult]) -> String {
    let payload: Vec<Value> = results
        .iter()
        .map(|result| match (&result.error, &result.structured) {
            (Some(error), _) => json!({"name": result.name, "error": error}),
            (None, Some(value)) => json!({"name": result.name, "result": value}),
            // Unreachable while every task carried a schema, but a silent
            // `null` here would look like a legitimate empty answer.
            (None, None) => json!({
                "name": result.name,
                "error": "worker produced no structured result"
            }),
        })
        .collect();
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "[]".to_owned())
}

fn format_results(results: &[SubagentResult], applied: bool) -> String {
    let mut output = String::new();
    for result in results {
        output.push_str(&format!("## {}\n", result.name));
        if let Some(error) = &result.error {
            output.push_str(&format!("Status: failed\n{error}\n\n"));
            continue;
        }
        output.push_str(if applied {
            "Status: patch applied\n"
        } else {
            "Status: completed\n"
        });
        output.push_str(&result.response);
        output.push('\n');
        if let Some(value) = &result.structured {
            output.push_str("```json\n");
            output.push_str(
                &serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
            );
            output.push_str("\n```\n");
        }
        if !applied && !result.patch.is_empty() {
            let patch: String = result.patch.chars().take(MAX_PATCH_CHARS).collect();
            output.push_str("```diff\n");
            output.push_str(&patch);
            if patch.len() < result.patch.len() {
                output.push_str("\n… patch truncated");
            }
            output.push_str("\n```\n");
        }
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn result(name: &str, structured: Option<Value>, error: Option<&str>) -> SubagentResult {
        SubagentResult {
            name: name.to_owned(),
            response: format!("{name} did the work"),
            patch: String::new(),
            structured,
            error: error.map(str::to_owned),
        }
    }

    fn task_with_spec(spec: Option<&str>, role: SubagentRole) -> SubagentTask {
        SubagentTask {
            name: "w".to_owned(),
            prompt: "do it".to_owned(),
            role,
            model: None,
            schema: None,
            spec: spec.map(str::to_owned),
            resume_from: None,
        }
    }

    #[test]
    fn an_authored_spec_overrides_the_built_in_role() {
        let dir = tempdir().unwrap();
        let harness = crate::harness::HarnessStore::load(dir.path().join("h"), dir.path());
        harness
            .migrate(&dir.path().join("none.json"), dir.path())
            .unwrap();

        // A seeded built-in resolves by id, and its read-only flag is honoured.
        let scout = resolve_role(
            &harness,
            &task_with_spec(Some("scout"), SubagentRole::Drone),
        );
        assert_eq!(scout.label, "scout");
        assert!(scout.read_only, "a scout spec must lock mutations off");
        assert!(scout.system_prompt.contains("SCOUT"));

        // An authored spec supplies its own brief.
        harness.apply(
            &crate::harness::RefinementProposal {
                summary: "s".to_owned(),
                rationale: "r".to_owned(),
                expected_outcome: "e".to_owned(),
                edits: vec![crate::harness::RefinementEdit {
                    action: crate::harness::EditAction::Create,
                    kind: crate::harness::EntryKind::Subagent,
                    id: Some("release_auditor".to_owned()),
                    title: Some("Release auditor".to_owned()),
                    content: Some("You audit a release branch and report blockers.".to_owned()),
                    path: None,
                    metadata: None,
                    reason: None,
                }],
            },
            crate::harness::Lifetime::Durable,
            None,
            None,
        );
        let authored = resolve_role(
            &harness,
            &task_with_spec(Some("release_auditor"), SubagentRole::Worker),
        );
        assert_eq!(authored.label, "release_auditor");
        assert!(authored.system_prompt.contains("audit a release branch"));
        // No read_only metadata means it may build, like a worker.
        assert!(!authored.read_only);
    }

    #[test]
    fn an_unknown_spec_falls_back_to_the_role_instead_of_failing() {
        let dir = tempdir().unwrap();
        let harness = crate::harness::HarnessStore::load(dir.path().join("h"), dir.path());
        // The roster is advisory context, so a stale id should cost fidelity,
        // not the task.
        let resolved = resolve_role(
            &harness,
            &task_with_spec(Some("deleted_spec"), SubagentRole::Scout),
        );
        assert_eq!(resolved.label, "scout");
        assert!(resolved.read_only);

        let blank = resolve_role(&harness, &task_with_spec(Some("   "), SubagentRole::Drone));
        assert_eq!(blank.label, "drone");
    }

    #[test]
    fn a_schema_is_accepted_on_a_task_and_optional() {
        let args = parse_args(
            r#"{"tasks":[{"name":"a","prompt":"x","schema":{"type":"object"}},
                          {"name":"b","prompt":"y"}]}"#,
        )
        .unwrap();
        assert!(args.tasks[0].schema.is_some());
        assert!(args.tasks[1].schema.is_none());
    }

    #[test]
    fn a_fully_typed_swarm_renders_json_the_parent_can_consume() {
        let results = vec![
            result("auditor", Some(json!({"verdict": "pass"})), None),
            result(
                "scanner",
                Some(json!({"verdict": "fail", "count": 2})),
                None,
            ),
        ];
        let rendered = format_results_json(&results);
        let parsed: Value = serde_json::from_str(&rendered).expect("valid JSON");
        let array = parsed.as_array().unwrap();
        assert_eq!(array.len(), 2);
        assert_eq!(array[0]["name"], "auditor");
        assert_eq!(array[0]["result"]["verdict"], "pass");
        assert_eq!(array[1]["result"]["count"], 2);
        // No prose to re-parse.
        assert!(!rendered.contains("did the work"));
    }

    #[test]
    fn a_failed_worker_appears_as_an_error_not_a_null_result() {
        // A null would read as a legitimate empty answer and quietly skew
        // whatever the parent computes from the array.
        let results = vec![
            result("ok", Some(json!({"verdict": "pass"})), None),
            result("broke", None, Some("worker stopped")),
            result("shapeless", None, None),
        ];
        let parsed: Value = serde_json::from_str(&format_results_json(&results)).unwrap();
        let array = parsed.as_array().unwrap();
        assert_eq!(array[1]["error"], "worker stopped");
        assert!(array[1].get("result").is_none());
        assert!(
            array[2]["error"]
                .as_str()
                .unwrap()
                .contains("no structured result")
        );
    }

    #[test]
    fn a_structured_result_is_still_shown_in_the_markdown_report() {
        // Background swarms and mixed swarms deliver prose; the data must not
        // vanish just because the rendering is markdown.
        let rendered = format_results(
            &[result("auditor", Some(json!({"verdict": "pass"})), None)],
            false,
        );
        assert!(rendered.contains("```json"), "{rendered}");
        assert!(rendered.contains("\"verdict\""), "{rendered}");
        assert!(rendered.contains("auditor did the work"));
    }

    #[test]
    fn validates_worker_bounds_and_names() {
        assert!(parse_args(r#"{"tasks":[]}"#).is_err());
        assert!(
            parse_args(r#"{"tasks":[{"name":"a","prompt":"x"},{"name":"A","prompt":"y"}]}"#)
                .is_err()
        );
        assert!(
            parse_args(r#"{"tasks":[{"name":"test","prompt":"verify it"}],"max_concurrency":2}"#)
                .is_ok()
        );
    }

    #[test]
    fn background_is_the_default_and_wait_is_opt_in() {
        let background = parse_args(r#"{"tasks":[{"name":"a","prompt":"x"}]}"#).unwrap();
        assert!(!background.wait, "workers run in the background by default");
        let blocking = parse_args(r#"{"tasks":[{"name":"a","prompt":"x"}],"wait":true}"#).unwrap();
        assert!(blocking.wait, "wait is the explicit opt-in");
    }

    #[test]
    fn per_task_role_and_model_parse_with_main_model_default() {
        let args = parse_args(
            r#"{"tasks":[
                {"name":"a","prompt":"read","role":"scout"},
                {"name":"b","prompt":"build","role":"drone","model":"anthropic/claude-sonnet-4.6"}
            ]}"#,
        )
        .unwrap();
        // Default: no model → the orchestrator's (main) model is used.
        assert_eq!(args.tasks[0].role, SubagentRole::Scout);
        assert_eq!(args.tasks[0].model, None);
        // Explicit slug is carried per worker.
        assert_eq!(args.tasks[1].role, SubagentRole::Drone);
        assert_eq!(
            args.tasks[1].model.as_deref(),
            Some("anthropic/claude-sonnet-4.6")
        );
    }

    #[tokio::test]
    async fn worktree_is_seeded_from_dirty_parent_and_returns_only_worker_changes() {
        if std::process::Command::new("git")
            .arg("--version")
            .status()
            .is_err()
        {
            return;
        }
        let directory = tempdir().unwrap();
        let repo = directory.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init"], None).await.unwrap();
        std::fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        run_git(&repo, &["add", "tracked.txt"], None).await.unwrap();
        let output = Command::new("git")
            .args(["-C", path_text(&repo).unwrap(), "commit", "-m", "base"])
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@localhost")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@localhost")
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        std::fs::write(repo.join("tracked.txt"), "parent state\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "parent new\n").unwrap();

        let context = WorktreeContext::capture(&repo).await.unwrap();
        let worker = directory.path().join("worker");
        context.create(&worker).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(worker.join("tracked.txt")).unwrap(),
            "parent state\n"
        );
        assert_eq!(
            std::fs::read_to_string(worker.join("untracked.txt")).unwrap(),
            "parent new\n"
        );

        std::fs::write(worker.join("tracked.txt"), "worker state\n").unwrap();
        std::fs::write(worker.join("worker.txt"), "created\n").unwrap();
        let patch = context.diff(&worker).await.unwrap();
        assert!(patch.contains("worker state"));
        assert!(patch.contains("worker.txt"));
        apply_patch(&repo, &patch).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "worker state\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("worker.txt")).unwrap(),
            "created\n"
        );
        context.remove(&worker).await.unwrap();

        let cancelled_worker = directory.path().join("cancelled-worker");
        context.create(&cancelled_worker).await.unwrap();
        {
            let _guard = WorktreeGuard::new(context.repo_root.clone(), cancelled_worker.clone());
        }
        assert!(!cancelled_worker.exists());
    }
}
