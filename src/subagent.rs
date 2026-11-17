use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, process::Command, sync::mpsc};
use uuid::Uuid;

use crate::{
    agent::{AgentEvent, TurnOptions, initial_messages, run_turn},
    provider::Provider,
    services::AgentServices,
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
    /// agent that started them keeps working instead of idling.
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
    /// agent that started it — so one call can fan out across several models
    /// (e.g. five different OpenRouter models). None uses that agent's model.
    model: Option<String>,
    /// A JSON Schema this worker's answer must satisfy. When set, the worker's
    /// prose report is coerced into the shape and validated, so the parent gets
    /// data instead of something it has to re-read.
    schema: Option<Value>,
    /// Prior conversation for a resumed worker — never part of the tool call,
    /// set only by `send_message` so the worker continues instead of starting
    /// over.
    #[serde(skip)]
    resume_from: Option<Vec<Value>>,
    /// The other workers it can message: those started in the same call, or
    /// for a resumed one, those running when it resumed.
    #[serde(skip)]
    peers: Vec<String>,
}

/// What kind of worker a task gets. The role shapes both the worker's system
/// prompt and its privileges: an explore worker runs read-only, a general one
/// may change things in its worktree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum SubagentRole {
    /// The full standard toolset. The names older versions used still parse.
    #[default]
    #[serde(alias = "worker", alias = "drone")]
    General,
    /// Research: reads, crawls, searches — never modifies.
    #[serde(alias = "scout")]
    Explore,
}

impl SubagentRole {
    fn label(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Explore => "explore",
        }
    }

    /// The role's instructions, and whether it may only read. A read-only
    /// role is enforced mechanically, not just instructed: the worker runs
    /// with every change refused.
    fn built_in(self) -> (&'static str, bool) {
        match self {
            Self::General => (
                "You are a subagent. Complete only the delegated task. You may edit and test this worktree. Do \
                 not commit, push, or modify paths outside the workspace. Finish with a concise summary and the \
                 exact checks you ran.",
                false,
            ),
            Self::Explore => (
                "You are a read-only subagent. Investigate the delegated question — read code, crawl the \
                 repository, search the web where allowed — and report findings with exact file paths and \
                 evidence. You must NOT modify anything; your value is the fidelity of what you bring back.",
                true,
            ),
        }
    }
}

#[derive(Debug, Default)]
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
    shell: crate::tools::Shell,
    workers: crate::workers::Workers,
    /// Where a background batch delivers its report when it finishes, and
    /// where its workers' messages to `main` go.
    injections: crate::agent::InjectionQueue,
    /// The session's mode, shared: workers are reviewed in AUTO just as the
    /// session is, and a switch to YOLO reaches them too.
    yolo: Arc<AtomicBool>,
    /// The reviewer AUTO puts a worker's actions to.
    review: Option<crate::config::Target>,
}

/// What a worker is told about whom it can message.
fn reach_briefing(name: &str, peers: &[String]) -> String {
    let main = crate::workers::MAIN;
    let alongside = match peers {
        [] => String::new(),
        peers => {
            let names: Vec<String> = peers.iter().map(|peer| format!("`{peer}`")).collect();
            format!(" Working alongside you: {}.", names.join(", "))
        }
    };
    format!(
        "You are subagent `{name}`.{alongside} send_message reaches the others while they run, and `{main}`, the \
         agent that started you, at any time: share what another needs, or ask. Nothing waits for an answer; \
         replies arrive as messages. Your final answer is your report to `{main}`."
    )
}

/// The workers of a request as one line: name, role, and model where one was chosen.
fn roster(tasks: &[SubagentTask]) -> String {
    let worker = |task: &SubagentTask| match task.model.as_deref().filter(|m| !m.trim().is_empty()) {
        Some(model) => format!("{} ({}, {model})", task.name, task.role.label()),
        None => format!("{} ({})", task.name, task.role.label()),
    };
    tasks.iter().map(worker).collect::<Vec<_>>().join(", ")
}

impl SubagentRuntime {
    /// A runtime whose workers run on `provider` under the turn's limits, services and stores.
    pub fn for_turn(provider: Provider, options: &TurnOptions) -> Self {
        Self {
            workspace: options.workspace.clone(),
            provider,
            services: options.services.clone(),
            max_steps: options.max_steps,
            tool_output_limit: options.tool_output_limit,
            web_search: options.web_search.clone(),
            shell: options.shell,
            workers: options.workers.clone(),
            injections: options.injections.clone(),
            yolo: options.yolo.clone(),
            review: options.review.clone().or_else(|| options.aux.clone()),
        }
    }

    pub fn tool_spec() -> Value {
        json!({
            "type":"function",
            "function":{
                "name":"spawn_subagents",
                "description":"Delegate tasks to subagents that run in parallel, each in its own git worktree. Prefer this when the request splits into two or more genuinely separable units — independent files, modules, fixes, or research questions that need no shared intermediate state — and start them in one call; it is the efficient way to parallelize. Use the explore role to parallelize investigation (repo crawls, research, web searches); a quick single lookup is still faster done directly. Do NOT use it for a single indivisible task or for tightly-coupled sequential edits. Each worker starts from the current workspace state and cannot start subagents of its own.\n\nWorkers run in the BACKGROUND by default: this returns immediately with the roster, and their report is delivered to you automatically after a later tool call. Keep working while they run — do not idle, and never invent or predict a pending worker's findings; wait for the delivered report. Workers can message each other, and you, with send_message while they run; what they send reaches you after a later tool call. Pass wait=true only when you genuinely cannot proceed without the results. Set apply=true to apply non-conflicting patches to the parent workspace.",
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
                                    "role":{"type":"string","enum":["general","explore"],"description":"general (default) = edits and runs checks in its worktree; explore = read-only research, repo crawls and web searches (cannot modify anything)"},
                                    "model":{"type":"string","description":"Optional model slug for this worker on the same endpoint. Omit to use your own (the main) model — that is the default and the right choice unless the user explicitly asked for specific models per worker (e.g. fanning the work across several OpenRouter models)."},
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

    /// `send_message`, as the agent that starts workers sees it, or as one of
    /// those workers does.
    pub fn message_tool_spec(from_worker: bool) -> Value {
        let (description, to) = if from_worker {
            (
                "Send a message to another agent by name: one of the subagents working alongside you, or `main`, \
                 the agent that started you. A running subagent reads it after its current step. Use it to share \
                 something another needs, or to ask; nothing waits for an answer, and replies arrive as messages.",
                "A subagent's name, or main",
            )
        } else {
            (
                "Send a message to a subagent you started, by name. If it is still running it reads the message \
                 after its current step — use this to correct or extend it without stopping it. If it already \
                 finished, it picks up where it left off with its conversation intact, in a fresh worktree seeded \
                 from the current workspace; its reply is delivered to you after a later tool call, like any \
                 background report. Prefer this to starting a new subagent for the same thread of work, because \
                 it keeps everything it already learned.",
                "The subagent's name, exactly as given to spawn_subagents",
            )
        };
        json!({
            "type":"function",
            "function":{
                "name":"send_message",
                "description":description,
                "parameters":{
                    "type":"object",
                    "properties":{
                        "to":{"type":"string","description":to},
                        "message":{"type":"string","description":"What to say: a correction, a finding, extra context, or a follow-up task"}
                    },
                    "required":["to","message"],
                    "additionalProperties":false
                }
            }
        })
    }

    /// `send_message` from the agent that started the workers.
    pub async fn message(&self, arguments: &str) -> Result<String> {
        let (name, message) = crate::workers::parse_message(arguments)?;
        if name == crate::workers::MAIN {
            bail!("`main` is you; message one of your subagents by name");
        }
        match self.workers.registry.channel(&name) {
            Some(crate::workers::WorkerChannel::Live(injections)) => {
                let from = crate::workers::MAIN.to_owned();
                injections.push(crate::agent::Injection::AgentMessage { from, text: message });
                Ok(format!(
                    "Delivered to `{name}`, which is still running — it picks the message up after its current \
                     step. Its report will reach you as usual."
                ))
            }
            // Finished: continue the same thread of work with its context.
            Some(crate::workers::WorkerChannel::Finished(transcript)) if !transcript.is_empty() => {
                self.resume_worker(name, transcript, message).await
            }
            Some(crate::workers::WorkerChannel::Finished(_)) => bail!(
                "`{name}` finished without a usable conversation (it failed early); start a fresh subagent instead"
            ),
            None => bail!("{}", self.workers.registry.unknown(&name)),
        }
    }

    /// Continue a finished worker: its own conversation plus the new message,
    /// in a fresh worktree seeded from the workspace as it is now. Runs in the
    /// background and reports back like any other worker.
    async fn resume_worker(&self, name: String, transcript: Vec<Value>, message: String) -> Result<String> {
        let context = Arc::new(WorktreeContext::capture(&self.workspace).await?);
        let runtime = self.clone();
        let injections = self.injections.clone();
        let reported = name.clone();
        tokio::spawn(async move {
            let report = match runtime.run_resumed(context, &name, transcript, message).await {
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
            Ok(args) => format!(
                "Run {} isolated worker(s): {}\nApply patches to this workspace: {}",
                args.tasks.len(),
                roster(&args.tasks),
                args.apply
            ),
            Err(error) => format!("Invalid subagent request: {error:#}"),
        }
    }

    pub async fn execute(&self, arguments: &str) -> Result<String> {
        let args = parse_args(arguments)?;
        // The parent snapshot is taken now, in the foreground, so background
        // workers start from the workspace as it was when they were spawned
        // rather than from whatever the main agent edits next.
        let context = Arc::new(WorktreeContext::capture(&self.workspace).await?);
        let concurrency = args.max_concurrency.clamp(1, MAX_SUBAGENTS);
        let apply = args.apply;
        let roster = roster(&args.tasks);
        let count = args.tasks.len();
        let runtime = self.clone();
        let mut tasks = args.tasks;
        // Each worker is told whom else it can reach: the rest of this call.
        let names: Vec<String> = tasks.iter().map(|task| task.name.clone()).collect();
        for task in &mut tasks {
            task.peers = names.iter().filter(|name| **name != task.name).cloned().collect();
        }

        if args.wait {
            return Ok(runtime.run_batch(context, tasks, concurrency, apply).await);
        }
        // Background: hand the batch to a detached task and return at once.
        // The report is pushed onto the injection queue, which the running
        // turn drains after its next tool call — the model keeps working in
        // the meantime instead of blocking on workers.
        let injections = self.injections.clone();
        tokio::spawn(async move {
            let report = runtime.run_batch(context, tasks, concurrency, apply).await;
            injections.push(crate::agent::Injection::SubagentReport(report));
        });
        Ok(format!(
            "Started {count} background worker(s): {roster}.\nThey are running now; their \
             report will be delivered to you automatically after a later tool call. Continue \
             with other work in the meantime — do not wait, and do not guess what they will \
             find. Pass wait=true if you ever need results before continuing."
        ))
    }

    /// Run a batch of workers to completion and format its report. Shared by
    /// the blocking and background paths so both report alike.
    async fn run_batch(
        &self,
        context: Arc<WorktreeContext>,
        tasks: Vec<SubagentTask>,
        concurrency: usize,
        apply: bool,
    ) -> String {
        // Decided before the tasks are consumed: a batch where every worker was
        // asked for a shape reports as data.
        let all_typed = !tasks.is_empty() && tasks.iter().all(|task| task.schema.is_some());
        // Every worker is reachable from the start, so a message to one still
        // waiting for a slot is there when it begins.
        let queued: Vec<_> = tasks
            .into_iter()
            .map(|task| {
                let injections = self.workers.registry.open(&task.name);
                (task, injections)
            })
            .collect();
        let runtime = self.clone();
        let mut results = stream::iter(queued.into_iter().map(|(task, injections)| {
            let runtime = runtime.clone();
            let context = context.clone();
            async move { runtime.run_one(context, task, injections).await }
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

        if all_typed { format_results_json(&results) } else { format_results(&results, apply) }
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
        let board_id = self.workers.board.begin(name, "resumed", worker_tokens.clone());
        self.workers.board.describe(board_id, worker_provider.model(), &message);
        let injections = self.workers.registry.open(name);
        let peers = self
            .workers
            .registry
            .roster()
            .into_iter()
            .filter(|(peer, running)| *running && peer != name)
            .map(|(peer, _)| peer)
            .collect();
        let task = SubagentTask {
            name: name.to_owned(),
            prompt: message,
            role: SubagentRole::General,
            model: None,
            // A follow-up is conversational; the parent asks for a shape by
            // spawning, not by messaging.
            schema: None,
            resume_from: Some(transcript.clone()),
            peers,
        };
        let outcome = self.run_one_inner(&context, &task, board_id, worker_provider, injections).await;
        self.provider.add_tokens(&worker_tokens);
        match outcome {
            Ok((response, _patch, new_transcript)) => {
                self.workers.board.finish(board_id, true);
                // The continued conversation replaces the old one, so a second
                // follow-up builds on this round too.
                self.workers.registry.close(name, new_transcript);
                Ok(response)
            }
            Err(error) => {
                self.workers.board.finish(board_id, false);
                self.workers.registry.close(name, transcript);
                Err(error)
            }
        }
    }

    async fn run_one(
        &self,
        context: Arc<WorktreeContext>,
        task: SubagentTask,
        injections: crate::agent::InjectionQueue,
    ) -> SubagentResult {
        let name = task.name.clone();
        let (mut worker_provider, worker_tokens) = self.provider.with_detached_counter();
        if let Some(model) = task.model.as_deref().filter(|model| !model.trim().is_empty()) {
            worker_provider = worker_provider.with_model(model);
        }
        let board_id = self.workers.board.begin(&name, task.role.label(), worker_tokens.clone());
        self.workers.board.describe(board_id, worker_provider.model(), &task.prompt);
        let outcome = self.run_one_inner(&context, &task, board_id, worker_provider, injections).await;
        // Fold the worker's usage into the session total — the per-worker
        // counter exists for the board, not to hide cost.
        self.provider.add_tokens(&worker_tokens);
        match outcome {
            Ok((response, patch, transcript)) => {
                // Coercion happens after the work, not during it: a worker told
                // to fill in a shape while it is still investigating bends its
                // reasoning toward the shape. This way the schema constrains the
                // report and nothing else.
                let coerced = match &task.schema {
                    Some(schema) => coerce_to_schema(&self.provider, &response, schema).await.map(Some),
                    None => Ok(None),
                };
                self.workers.board.finish(board_id, coerced.is_ok());
                // Keep the conversation so the agent that started it can follow
                // up with this worker and have it continue where it left off.
                self.workers.registry.close(&name, transcript);
                let (structured, error) = match coerced {
                    // A cut-short answer must not read as a finished one.
                    _ if self.workers.board.was_stopped(board_id) => {
                        (None, Some("stopped by the user before it finished".to_owned()))
                    }
                    Ok(value) => (value, None),
                    Err(error) => (None, Some(format!("result did not match the schema: {error:#}"))),
                };
                SubagentResult { name, response, patch, structured, error }
            }
            Err(error) => {
                self.workers.board.finish(board_id, false);
                self.workers.registry.close(&name, Vec::new());
                SubagentResult { name, error: Some(format!("{error:#}")), ..Default::default() }
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
        let worker_root =
            std::env::temp_dir().join("abacus-worktrees").join(format!("{}-{}", safe_name(&task.name), Uuid::new_v4()));
        if let Some(parent) = worker_root.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let setup = context.create(&worker_root).await;
        if let Err(error) = setup {
            let _ = context.remove(&worker_root).await;
            return Err(error);
        }

        let mut guard = WorktreeGuard::new(context.repo_root.clone(), worker_root.clone());
        let result = self.run_in_worktree(context, &worker_root, task, board_id, provider, injections).await;
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
        let (role_prompt, read_only) = task.role.built_in();
        let worker_workspace = worker_root.join(&context.workspace_relative);
        // A resumed worker keeps its own conversation; a fresh one starts from
        // the standard preamble. Either way the new prompt is the last word.
        let (mut messages, briefing) = match &task.resume_from {
            Some(prior) if !prior.is_empty() => (
                prior.clone(),
                "Continuing your earlier task in a fresh worktree seeded from the current \
                 workspace. Your own file changes from before are not here unless they were \
                 applied; re-read what you need. Everything you learned above still stands."
                    .to_owned(),
            ),
            _ => (initial_messages(&worker_workspace), role_prompt.to_owned()),
        };
        // Whom it can reach is said every time it starts: a resumed worker's
        // company has moved on since its first round.
        let reach = reach_briefing(&task.name, &task.peers);
        messages.push(json!({"role":"system","content":format!("{briefing}\n\n{reach}")}));
        messages.push(json!({"role":"user","content":task.prompt}));
        let (events, mut receiver) = mpsc::unbounded_channel();
        let services = Arc::new(self.services.for_workspace(worker_workspace.clone()));
        let mut turn = run_turn(
            provider,
            messages,
            TurnOptions {
                max_steps: self.max_steps,
                tool_output_limit: self.tool_output_limit,
                // An explore worker is mechanically read-only, not just
                // instructed so: a lock that nothing in the worker can flip.
                // Everyone else works under the session's own mode.
                read_only,
                yolo: self.yolo.clone(),
                review: self.review.clone(),
                web_search: self.web_search.clone(),
                shell: self.shell,
                // A worker's questions go to `main` with send_message; nothing
                // would show it a question for the person.
                can_ask: false,
                injections,
                // What it sends is signed with its name.
                mailbox: Some(crate::workers::Mailbox {
                    name: task.name.clone(),
                    registry: self.workers.registry.clone(),
                    parent: self.injections.clone(),
                }),
                // The board's stop button reaches the worker through this.
                cancel: self.workers.board.cancel_flag(board_id),
                // Everything else stays inert. No trace: subagent work is a
                // different task shape and would blur the samples. No
                // memories: what a worker records belongs to its delegated
                // task, and its worktree paths would pollute workspace scoping.
                ..TurnOptions::bare(worker_workspace, services)
            },
            events,
        );
        let mut final_messages = None;
        let mut failure = None;
        // A stop from the board ends the worker now, mid-tool if need be:
        // its turn would otherwise only notice after the running tool call,
        // and a long command can take minutes. Dropping the turn kills the
        // command (kill_on_drop) and the worktree is removed below.
        let stop = self.workers.board.cancel_flag(board_id);
        let mut check = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                () = &mut turn => break,
                event = receiver.recv() => {
                    if let Some(event) = event {
                        self.note_activity(board_id, &event);
                        capture_event(event, &mut final_messages, &mut failure);
                    }
                }
                _ = check.tick() => {
                    if stop.load(Ordering::Relaxed) {
                        return Ok((String::new(), String::new(), Vec::new()));
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

    /// Mirror a worker's visible activity and transcript onto the live board.
    fn note_activity(&self, board_id: u64, event: &AgentEvent) {
        self.workers.board.record(board_id, event);
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
        let conversation = vec![json!({"role":"system","content":system}), json!({"role":"user","content":user})];
        let completion = match provider.ask(&conversation, &[], &AtomicBool::new(false)).await {
            Ok(completion) => completion,
            Err(error) => {
                last_error = format!("{error:#}");
                continue;
            }
        };
        if completion.cancelled {
            bail!("cancelled while coercing the result");
        }
        match crate::schema::extract_json(&completion.content) {
            Err(error) => last_error = format!("{error:#}"),
            Ok(value) => match crate::schema::validate(&value, schema) {
                Ok(()) => return Ok(value),
                Err(error) => last_error = format!("{error:#}"),
            },
        }
    }
    bail!("after {COERCE_ATTEMPTS} attempts: {last_error}")
}

fn capture_event(event: AgentEvent, final_messages: &mut Option<Vec<Value>>, failure: &mut Option<String>) {
    match event {
        // A worker only asks when AUTO's reviewer could not decide, and nobody
        // is there to answer: that is a no, not a yes.
        AgentEvent::Approval(request) => {
            let _ = request.respond.send(crate::agent::ApprovalDecision::Reject);
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
        Self { repo_root, worker_root, active: true }
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
        let root = git(workspace, &["rev-parse", "--show-toplevel"], None)
            .await
            .context("subagents require a git workspace")?;
        let root = String::from_utf8(root).context("git returned non-UTF-8 text")?;
        let repo_root = PathBuf::from(root.trim()).canonicalize()?;
        let workspace = workspace.canonicalize()?;
        let workspace_relative =
            workspace.strip_prefix(&repo_root).context("workspace is outside its git repository")?.to_owned();
        let scope = git_scope(&workspace_relative);
        let baseline_patch = git(&repo_root, &["diff", "--binary", "HEAD", "--", scope.as_str()], None).await?;
        let untracked_raw =
            git(&repo_root, &["ls-files", "--others", "--exclude-standard", "-z", "--", scope.as_str()], None).await?;
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
        Ok(Self { repo_root, workspace_relative, baseline_patch, untracked })
    }

    async fn create(&self, worker_root: &Path) -> Result<()> {
        git(&self.repo_root, &["worktree", "add", "--detach", path_text(worker_root)?, "HEAD"], None)
            .await
            .context("could not create isolated git worktree")?;
        if !self.baseline_patch.is_empty() {
            git(worker_root, &["apply", "--binary", "-"], Some(&self.baseline_patch))
                .await
                .context("could not seed worker with current tracked changes")?;
        }
        for relative in &self.untracked {
            let source = self.repo_root.join(relative);
            let destination = worker_root.join(relative);
            copy_entry(&source, &destination)?;
        }
        let scope = git_scope(&self.workspace_relative);
        git(worker_root, &["add", "-A", "--", scope.as_str()], None).await?;
        let status =
            Command::new("git").args(["-C", path_text(worker_root)?, "diff", "--cached", "--quiet"]).status().await?;
        if !status.success() {
            let mut command = Command::new("git");
            command
                .args(["-C", path_text(worker_root)?, "commit", "-m", "abacus worker baseline"])
                .env("GIT_AUTHOR_NAME", "Abacus")
                .env("GIT_AUTHOR_EMAIL", "abacus@localhost")
                .env("GIT_COMMITTER_NAME", "Abacus")
                .env("GIT_COMMITTER_EMAIL", "abacus@localhost")
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            let output = command.output().await?;
            if !output.status.success() {
                bail!("could not snapshot worker baseline: {}", String::from_utf8_lossy(&output.stderr).trim());
            }
        }
        Ok(())
    }

    async fn diff(&self, worker_root: &Path) -> Result<String> {
        let scope = git_scope(&self.workspace_relative);
        git(worker_root, &["add", "-N", "--", scope.as_str()], None).await?;
        let bytes = git(worker_root, &["diff", "--binary", "HEAD", "--", scope.as_str()], None).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn remove(&self, worker_root: &Path) -> Result<()> {
        if worker_root.exists() {
            let remove = ["worktree", "remove", "--force", path_text(worker_root)?];
            git(&self.repo_root, &remove, None).await?;
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
        if task.name.trim().eq_ignore_ascii_case(crate::workers::MAIN) {
            bail!("`{}` is what workers call you; give the worker another name", crate::workers::MAIN);
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
    let value: String =
        name.chars().filter(|character| character.is_ascii_alphanumeric() || *character == '-').take(24).collect();
    if value.is_empty() { "worker".into() } else { value }
}

fn git_scope(relative: &Path) -> String {
    if relative.as_os_str().is_empty() { ".".into() } else { relative.to_string_lossy().into_owned() }
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| anyhow!("git path is not UTF-8"))
}

/// Runs git in `directory`, feeding it `stdin`, and returns what it printed.
async fn git(directory: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        // Keep seeded/diffed content byte-exact across platforms (Windows git
        // defaults to core.autocrlf=true, which would rewrite line endings).
        .args(["-c", "core.autocrlf=false"])
        .args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("could not start git")?;
    if let (Some(input), Some(mut child_stdin)) = (stdin, child.stdin.take()) {
        child_stdin.write_all(input).await?;
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.stdout)
}

async fn apply_patch(repo_root: &Path, patch: &str) -> Result<()> {
    git(repo_root, &["apply", "--check", "--binary", "-"], Some(patch.as_bytes())).await?;
    git(repo_root, &["apply", "--binary", "-"], Some(patch.as_bytes())).await.map(drop)
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
/// prose back out of a markdown report. A mixed batch falls back to markdown,
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

/// The workers a background report covers, by name: each `## name` that
/// opens a section of [`format_results`], or the one a resumed worker's
/// report names.
pub fn report_workers(report: &str) -> Vec<String> {
    if let Some(rest) = report.strip_prefix("worker `")
        && let Some((name, _)) = rest.split_once('`')
    {
        return vec![name.to_owned()];
    }
    let lines: Vec<&str> = report.lines().collect();
    lines
        .windows(2)
        .filter(|pair| pair[1].starts_with("Status: "))
        .filter_map(|pair| pair[0].strip_prefix("## "))
        .map(str::to_owned)
        .collect()
}

fn format_results(results: &[SubagentResult], applied: bool) -> String {
    let mut output = String::new();
    for result in results {
        output.push_str(&format!("## {}\n", result.name));
        if let Some(error) = &result.error {
            output.push_str(&format!("Status: failed\n\n{error}\n\n"));
            continue;
        }
        output.push_str(if applied { "Status: patch applied\n\n" } else { "Status: completed\n\n" });
        output.push_str(&result.response);
        output.push('\n');
        if let Some(value) = &result.structured {
            output.push_str("```json\n");
            output.push_str(&serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()));
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

    #[test]
    fn a_report_names_the_workers_it_covers() {
        let report = format_results(&[result("callers", None, None), result("perf", None, Some("timed out"))], false);
        assert_eq!(report_workers(&report), ["callers", "perf"]);
        // A heading in a worker's own answer is not another worker.
        assert_eq!(report_workers("## a\nStatus: completed\n## Findings\nnone\n"), ["a"]);
        assert_eq!(report_workers("worker `perf` (resumed): done"), ["perf"]);
        assert!(report_workers("all good").is_empty());
    }

    #[test]
    fn an_explore_worker_may_only_read_and_a_general_one_may_change_things() {
        let (prompt, read_only) = SubagentRole::Explore.built_in();
        assert!(read_only, "an explore worker must have mutations locked off");
        assert!(prompt.contains("read-only"));
        assert!(!SubagentRole::General.built_in().1);
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
    fn a_fully_typed_batch_renders_json_the_parent_can_consume() {
        let results = vec![
            result("auditor", Some(json!({"verdict": "pass"})), None),
            result("scanner", Some(json!({"verdict": "fail", "count": 2})), None),
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
        assert!(array[2]["error"].as_str().unwrap().contains("no structured result"));
    }

    #[test]
    fn a_structured_result_is_still_shown_in_the_markdown_report() {
        // Background and mixed batches deliver prose; the data must not
        // vanish just because the rendering is markdown.
        let rendered = format_results(&[result("auditor", Some(json!({"verdict": "pass"})), None)], false);
        assert!(rendered.contains("```json"), "{rendered}");
        assert!(rendered.contains("\"verdict\""), "{rendered}");
        assert!(rendered.contains("auditor did the work"));
    }

    #[test]
    fn validates_worker_bounds_and_names() {
        assert!(parse_args(r#"{"tasks":[]}"#).is_err());
        assert!(parse_args(r#"{"tasks":[{"name":"a","prompt":"x"},{"name":"A","prompt":"y"}]}"#).is_err());
        assert!(parse_args(r#"{"tasks":[{"name":"test","prompt":"verify it"}],"max_concurrency":2}"#).is_ok());
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
                {"name":"a","prompt":"read","role":"explore"},
                {"name":"b","prompt":"build","role":"general","model":"anthropic/claude-sonnet-4.6"},
                {"name":"c","prompt":"look","role":"scout"}
            ]}"#,
        )
        .unwrap();
        // Default: no model → the main model is used.
        assert_eq!(args.tasks[0].role, SubagentRole::Explore);
        assert_eq!(args.tasks[0].model, None);
        // Explicit slug is carried per worker.
        assert_eq!(args.tasks[1].role, SubagentRole::General);
        // The names older versions used still parse.
        assert_eq!(args.tasks[2].role, SubagentRole::Explore);
        assert_eq!(args.tasks[1].model.as_deref(), Some("anthropic/claude-sonnet-4.6"));
    }

    #[tokio::test]
    async fn worktree_is_seeded_from_dirty_parent_and_returns_only_worker_changes() {
        if std::process::Command::new("git").arg("--version").status().is_err() {
            return;
        }
        let directory = tempdir().unwrap();
        let repo = directory.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init"], None).await.unwrap();
        std::fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        git(&repo, &["add", "tracked.txt"], None).await.unwrap();
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
        assert_eq!(std::fs::read_to_string(worker.join("tracked.txt")).unwrap(), "parent state\n");
        assert_eq!(std::fs::read_to_string(worker.join("untracked.txt")).unwrap(), "parent new\n");

        std::fs::write(worker.join("tracked.txt"), "worker state\n").unwrap();
        std::fs::write(worker.join("worker.txt"), "created\n").unwrap();
        let patch = context.diff(&worker).await.unwrap();
        assert!(patch.contains("worker state"));
        assert!(patch.contains("worker.txt"));
        apply_patch(&repo, &patch).await.unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("tracked.txt")).unwrap(), "worker state\n");
        assert_eq!(std::fs::read_to_string(repo.join("worker.txt")).unwrap(), "created\n");
        context.remove(&worker).await.unwrap();

        let cancelled_worker = directory.path().join("cancelled-worker");
        context.create(&cancelled_worker).await.unwrap();
        {
            let _guard = WorktreeGuard::new(context.repo_root.clone(), cancelled_worker.clone());
        }
        assert!(!cancelled_worker.exists());
    }
}
