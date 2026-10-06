//! `abacus app-server`: a persistent, machine-facing session server.
//!
//! The TUI owns a terminal; a desktop front end cannot. This speaks JSON-RPC
//! 2.0 over stdin/stdout so an external UI can drive a real Abacus session —
//! streaming deltas, live tool calls, and the two points where the agent
//! *blocks on a human*: approvals and questions. Headless mode cannot serve a
//! GUI: it is one-shot, and it auto-rejects every approval.
//!
//! The method and notification vocabulary — `initialize`, `thread/*`,
//! `turn/*`, `item/*` — is the conventional one for agent app servers, so a
//! client written against that shape can drive Abacus with the dispatch table
//! it already has. Methods exist only where Abacus has the concept: there is
//! no sandbox, PTY-process or thread-forking surface here, and pretending
//! otherwise would be worse than their absence.
//!
//! The unit of streaming is an **item**: every user message, agent message,
//! reasoning block and tool call is announced with `item/started`, streamed
//! through `item/*/delta`, and closed with `item/completed`, which is
//! authoritative. A client that only renders `item/completed` is correct but
//! not live; one that renders deltas too is live. That split is what lets the
//! GUI stay simple.
//!
//! One process serves one workspace and one active thread. A second window is
//! a second process — the state a turn needs (harness, tether, handles,
//! injections) is per-session anyway, so multiplexing it inside one process
//! would buy nothing but a lifetime puzzle.
//!
//! **stdout is the protocol.** Every diagnostic goes to stderr, or the frame
//! stream is corrupt and the front end desynchronises.

use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::{
    agent::{AgentEvent, AgentMode, ApprovalDecision, DoneReason, TurnOptions, UserAnswer, initial_messages, run_turn},
    config::{Config, Settings},
    provider::Provider,
    services::AgentServices,
    session::{Session, SessionState, SessionStore},
    tools::ToolCall,
    usage::UsageReporter,
};

/// Bumped when a frame's shape changes in a way a front end must notice.
/// `initialize` reports it so an out-of-date GUI can say so rather than
/// misrender a session.
pub const PROTOCOL_VERSION: u32 = 1;

const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const SERVER_ERROR: i32 = -32000;

/// How long shutting down waits on the last upload and usage report. The front
/// end has already gone; this only keeps the exit from hanging on a dead
/// network.
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Tools whose call is a shell command, and so maps onto the client's
/// command-execution rendering rather than a generic function call.
const COMMAND_TOOLS: &[&str] = &["run_command"];
/// Tools that change files. Split out because a front end shows a diff for
/// these and a log for everything else.
const FILE_CHANGE_TOOLS: &[&str] =
    &["edit_file", "write_file", "apply_patch", "delete_file", "move_file", "append_file", "create_directory"];

fn item_type(tool: &str) -> &'static str {
    if COMMAND_TOOLS.contains(&tool) {
        "commandExecution"
    } else if FILE_CHANGE_TOOLS.contains(&tool) {
        "fileChange"
    } else {
        "functionCall"
    }
}

/// A frame arriving from the front end.
#[derive(Debug)]
enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// The client answering a request *we* sent — an approval or a question.
    Response {
        id: Value,
        result: Value,
    },
    Notification {
        method: String,
    },
}

/// Everything one served thread needs. Rebuilt whenever the front end starts
/// or resumes one, because all of it is session-scoped.
struct ThreadState {
    session: Session,
    /// Whether `session` has ever been written to disk. A thread is created in
    /// memory so it has an id to talk about immediately, but an untouched one
    /// is never persisted — opening the app should not litter the store.
    persisted: bool,
    messages: Vec<Value>,
    state: SessionState,
    trace: Option<crate::sft::TraceWriter>,
}

impl ThreadState {
    fn new(config: &Config, session: Option<Session>) -> Self {
        let persisted = session.is_some();
        let session = session.unwrap_or_else(|| {
            Session::new(
                config.workspace.clone(),
                config.profile.clone(),
                config.model.clone(),
                initial_messages(&config.workspace),
            )
        });
        let state = SessionState::open(config, Some(&session), session.id.to_string());
        let trace = config
            .trace_enabled
            .then(|| crate::sft::TraceWriter::open(&config.paths.traces_dir, &session.id.to_string()).ok())
            .flatten();
        Self { messages: session.messages.clone(), state, trace, persisted, session }
    }

    fn id(&self) -> String {
        self.session.id.to_string()
    }
}

/// One in-flight item, so a `tool.finished` can close the item its
/// `tool.started` opened.
struct OpenItem {
    id: String,
    kind: &'static str,
    summary: String,
}

impl OpenItem {
    /// The item as a client sees a tool call at `status`.
    fn tool(&self, name: &str, status: &str) -> Value {
        json!({
            "id": self.id, "type": self.kind, "name": name,
            "command": self.summary, "status": status,
        })
    }
}

struct App {
    config: Config,
    settings: Settings,
    services: Arc<AgentServices>,
    store: Option<SessionStore>,
    thread: ThreadState,
    provider: Provider,
    tokens: Arc<crate::provider::TokenLedger>,
    mode: AgentMode,
    /// Whether tools may mutate without asking. `acceptForSession` latches it,
    /// the same way the TUI's `/yes` does.
    allow: Arc<AtomicBool>,
    /// Raised to ask the running turn to wind down. Replaced per turn.
    cancel: Arc<AtomicBool>,
    turn_id: Option<String>,
    /// Tool name -> the item its call opened.
    open_items: HashMap<String, OpenItem>,
    /// Server-initiated requests waiting on the client, by request id.
    pending_approvals: HashMap<String, oneshot::Sender<ApprovalDecision>>,
    pending_questions: HashMap<String, oneshot::Sender<UserAnswer>>,
    counter: u64,
    /// Account-tied usage, present only while signed in to Abacus Sync.
    usage: Option<UsageReporter>,
    /// Sync problems already told to the front end.
    sync_warned: Arc<std::sync::Mutex<HashSet<String>>>,
}

/// Run the server until stdin closes.
pub async fn run(
    config: Config,
    settings: Settings,
    session: Option<Session>,
    store: Option<SessionStore>,
    services: Arc<AgentServices>,
) -> Result<()> {
    let tokens =
        Arc::new(crate::provider::TokenLedger::new(session.as_ref().map(|value| value.tokens_used).unwrap_or(0)));
    let provider = Provider::with_tokens(&config, tokens.clone())?;
    let allow = Arc::new(AtomicBool::new(config.yes));
    let mode = config.mode.unwrap_or(AgentMode::Auto);
    let credentials = crate::config::Credentials::load(&config.paths).unwrap_or_default();
    let usage = UsageReporter::new(&config.paths, &credentials, "app-server");
    let mut app = App {
        thread: ThreadState::new(&config, session),
        config,
        settings,
        services,
        store,
        provider,
        tokens,
        mode,
        allow,
        cancel: Arc::new(AtomicBool::new(false)),
        turn_id: None,
        open_items: HashMap::new(),
        pending_approvals: HashMap::new(),
        pending_questions: HashMap::new(),
        counter: 0,
        usage,
        sync_warned: Arc::default(),
    };
    app.thread_opened();
    let usage_task = app.usage.as_ref().map(UsageReporter::spawn_periodic);
    if crate::sync::is_configured(&credentials) {
        // Pull what changed on other devices and upload what an earlier exit
        // left behind, without holding up the first request. The open thread
        // is protected from being written over. `sessions/synced` tells the
        // front end to list again when anything arrived or went away.
        let paths = app.config.paths.clone();
        tokio::spawn(async move {
            if let Ok(outcome) = crate::sync::reconcile(&paths).await
                && outcome.pulled + outcome.deleted + outcome.forked > 0
            {
                notify(
                    "sessions/synced",
                    json!({"pulled": outcome.pulled, "deleted": outcome.deleted, "forked": outcome.forked}),
                );
            }
        });
    }

    // One event channel for the process, not per turn: a turn's tail events can
    // land after the front end has already asked for the next one, and a
    // channel that dies with the turn would drop them.
    let (events, mut event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();

    // stdin on its own task: a blocking read must never stall event delivery,
    // or an approval could not be answered while the agent waits on it.
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            line = line_rx.recv() => {
                let Some(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match parse(&line) {
                    Ok(Incoming::Request { id, method, params }) => {
                        let shutdown = method == "shutdown";
                        match app.handle(&method, params, &events).await {
                            Ok(result) => emit(json!({"jsonrpc":"2.0","id":id,"result":result})),
                            Err(error) => {
                                let code = if error.to_string().starts_with("unknown method") {
                                    METHOD_NOT_FOUND
                                } else {
                                    SERVER_ERROR
                                };
                                emit(json!({
                                    "jsonrpc":"2.0","id":id,
                                    "error":{"code":code,"message":format!("{error:#}")}
                                }));
                            }
                        }
                        if shutdown {
                            break;
                        }
                    }
                    Ok(Incoming::Response { id, result }) => app.resolve(&id, &result),
                    Ok(Incoming::Notification { method }) => {
                        if method == "shutdown" {
                            break;
                        }
                    }
                    Err(error) => emit(json!({
                        "jsonrpc":"2.0","id":Value::Null,
                        "error":{"code":INVALID_PARAMS,"message":error.to_string()}
                    })),
                }
            }
            event = event_rx.recv() => {
                let Some(event) = event else { continue };
                app.handle_event(event);
            }
        }
    }
    if let Some(task) = usage_task {
        task.abort();
    }
    app.close().await;
    Ok(())
}

fn parse(line: &str) -> Result<Incoming> {
    let value: Value = serde_json::from_str(line)?;
    // A frame with a result and an id is the client answering us; the same id
    // space is shared, which is why the answer is matched before the method.
    if value.get("result").is_some() || value.get("error").is_some() {
        return Ok(Incoming::Response {
            id: value["id"].clone(),
            result: value.get("result").cloned().unwrap_or(Value::Null),
        });
    }
    let method = value["method"].as_str().ok_or_else(|| anyhow!("frame has no method"))?.to_owned();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match value.get("id") {
        Some(id) if !id.is_null() => Ok(Incoming::Request { id: id.clone(), method, params }),
        _ => Ok(Incoming::Notification { method }),
    }
}

fn emit(value: Value) {
    // A front end that has gone away closes the pipe; there is nothing useful
    // to do about that here, and panicking on a broken pipe would be worse.
    if let Ok(text) = serde_json::to_string(&value) {
        println!("{text}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

fn notify(method: &str, params: Value) {
    emit(json!({"jsonrpc": "2.0", "method": method, "params": params}));
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl App {
    fn next(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}_{}", self.counter)
    }

    fn thread_id(&self) -> String {
        self.thread.id()
    }

    fn turn_id(&self) -> String {
        self.turn_id.clone().unwrap_or_default()
    }

    async fn handle(
        &mut self,
        method: &str,
        params: Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<Value> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {"name": "abacus", "version": env!("CARGO_PKG_VERSION")},
                "workspace": self.config.workspace,
                "capabilities": {
                    "threads": true,
                    "approvals": true,
                    "userInput": true,
                    "diff": true,
                    "modelList": true,
                    "configWrite": true,
                    // `turn/start` takes `image` (data URL) and `localImage`
                    // (path) input parts alongside text.
                    "imageInput": true,
                },
                "thread": self.thread_snapshot(),
            })),
            "shutdown" => Ok(json!({})),

            // ---- threads ----------------------------------------------------
            "thread/list" => {
                let store = self.store.as_ref().ok_or_else(|| anyhow!("sessions off"))?;
                let threads: Vec<Value> = store
                    .list()?
                    .into_iter()
                    .map(|summary| {
                        json!({
                            "threadId": summary.id.to_string(),
                            "name": summary.title,
                            "model": summary.model,
                            "itemCount": summary.message_count,
                            "updatedAt": summary.updated_at.to_rfc3339(),
                        })
                    })
                    .collect();
                Ok(json!({"threads": threads}))
            }
            "thread/start" => {
                self.require_idle()?;
                self.thread_closed();
                self.thread = ThreadState::new(&self.config, None);
                self.tokens.store_total(0);
                self.thread_opened();
                notify("thread/started", json!({"threadId": self.thread_id()}));
                Ok(self.thread_snapshot())
            }
            "thread/resume" | "thread/read" => {
                self.require_idle()?;
                let id = params["threadId"].as_str().ok_or_else(|| anyhow!("threadId required"))?;
                let store = self.store.as_ref().ok_or_else(|| anyhow!("sessions off"))?;
                let session = store.load(id)?;
                self.thread_closed();
                self.tokens.store_total(session.tokens_used);
                self.thread = ThreadState::new(&self.config, Some(session));
                self.thread_opened();
                Ok(self.thread_snapshot())
            }
            "thread/name/set" => {
                let name = params["name"].as_str().unwrap_or_default();
                let store = self.store.as_ref().ok_or_else(|| anyhow!("sessions off"))?;
                store.rename(&mut self.thread.session, name)?;
                self.thread.persisted = true;
                notify("thread/name/updated", json!({"threadId": self.thread_id(), "name": self.thread.session.title}));
                Ok(json!({"name": self.thread.session.title}))
            }

            // ---- turns ------------------------------------------------------
            "turn/start" | "turn/steer" => {
                let TurnInput { text, images } = turn_input(&params)?;
                // Mid-turn input steers rather than queueing a second turn —
                // the same rule the TUI follows, so the model can change course
                // at the next tool boundary instead of after everything it has
                // already planned. `turn/steer` is the explicit spelling of it.
                if self.turn_id.is_some() {
                    let content = self.user_content(&text, images);
                    self.thread.state.injections.push(match content {
                        Value::String(_) => crate::agent::Injection::UserMessage(text),
                        content => crate::agent::Injection::UserContent { text, content },
                    });
                    return Ok(json!({"steered": true, "turnId": self.turn_id()}));
                }
                if method == "turn/steer" {
                    return Err(anyhow!("no turn is running to steer"));
                }
                let text = crate::context::expand_file_references(&self.config.workspace, &text).unwrap_or(text);
                let content = self.user_content(&text, images);
                self.start_turn(content, events);
                Ok(json!({"turnId": self.turn_id(), "threadId": self.thread_id()}))
            }
            "turn/interrupt" => {
                self.cancel.store(true, Ordering::Relaxed);
                Ok(json!({"interrupting": self.turn_id.is_some()}))
            }

            // ---- models and config -----------------------------------------
            "model/list" => {
                let models =
                    crate::setup::discover_models(&self.config.base_url, self.config.api_key.as_deref()).await?;
                let models: Vec<Value> =
                    models.into_iter().map(|id| json!({"id": id, "selected": id == self.config.model})).collect();
                Ok(json!({"models": models}))
            }
            "config/read" => Ok(json!({
                "settings": serde_json::to_value(&self.settings)?,
                // The choices behind the enum-valued settings, so a client can
                // offer a real picker instead of a free-text box it cannot
                // validate. Keyed by dotted path into `settings`.
                "choices": settings_choices(),
                "profile": self.config.profile,
                "model": self.config.model,
                "baseUrl": self.config.base_url,
                "workspace": self.config.workspace,
                "mode": self.mode.label().to_ascii_lowercase(),
                "autoApprove": self.allow.load(Ordering::Relaxed),
                "reasoningEffort": self.config.reasoning_effort.map(|effort| effort.label()),
                "contextWindow": self.config.model_limits.context_window,
            })),
            // One key at a time: the client does not have to round-trip the
            // whole settings tree to flip a switch, and a malformed edit
            // cannot take the rest of the settings with it.
            "config/value/write" => {
                let key = params["key"].as_str().unwrap_or_default();
                let value = params.get("value").cloned().unwrap_or(Value::Null);
                self.write_config_value(key, value)
            }
            "config/write" => {
                let settings: Settings = serde_json::from_value(params["settings"].clone())?;
                settings.save(&self.config.paths)?;
                self.settings = settings;
                Ok(json!({"saved": true}))
            }

            // ---- extensions -------------------------------------------------
            "skills/list" => {
                let skills: Vec<Value> = self
                    .services
                    .skills
                    .read()
                    .map(|registry| {
                        registry
                            .list()
                            .map(|skill| {
                                json!({
                                    "name": skill.name,
                                    "description": skill.description,
                                    "source": skill.source,
                                    "path": skill.root,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(json!({"skills": skills}))
            }
            "hooks/list" => {
                let plugins: Vec<Value> = self
                    .services
                    .plugins
                    .list()
                    .map(|plugin| {
                        json!({
                            "name": plugin.name,
                            "version": plugin.version,
                            "description": plugin.description,
                            "source": plugin.source,
                            "commands": plugin.commands.len(),
                            "hooks": plugin.hooks.len(),
                        })
                    })
                    .collect();
                Ok(json!({
                    "plugins": plugins,
                    "trusted": self.services.project_trusted(),
                    "diagnostics": self.services.diagnostics(),
                }))
            }
            "mcpServerStatus/list" => {
                let tools: Vec<Value> = self
                    .services
                    .mcp
                    .tools()
                    .map(|tool| {
                        json!({
                            "name": tool.exposed_name,
                            "server": tool.server,
                            "description": tool.description,
                        })
                    })
                    .collect();
                Ok(json!({"tools": tools, "diagnostics": self.services.mcp.diagnostics()}))
            }
            "cron/list" => {
                let jobs = crate::cron::CronStore::new(&self.config.paths).list()?;
                Ok(json!({"jobs": serde_json::to_value(jobs)?}))
            }

            // ---- workspace --------------------------------------------------
            "thread/diff/read" => Ok(json!({
                "diff": self.git(&["diff"]).await?,
                "status": self.git(&["status", "--porcelain=v1", "-b"]).await?,
            })),

            other => Err(anyhow!("unknown method `{other}`")),
        }
    }

    fn write_config_value(&mut self, key: &str, value: Value) -> Result<Value> {
        match key {
            "model" => {
                self.require_idle()?;
                let model = value.as_str().ok_or_else(|| anyhow!("model must be a string"))?;
                // What the old model spent so far is credited to it before the
                // ledger starts counting for the new one.
                if let Some(usage) = &self.usage {
                    usage.note_model(model);
                }
                self.config.model = model.to_owned();
                self.provider = Provider::with_tokens(&self.config, self.tokens.clone())?;
                let model = model.to_owned();
                self.update_profile(|profile| profile.model = model)?;
            }
            // Reasoning effort rides the profile, the way `/effort` writes it
            // in the TUI: the two front ends are one install, and a level set
            // in the window would be baffling if the terminal ignored it.
            // `null` means "let the endpoint decide".
            "reasoning_effort" => {
                let effort = match value {
                    Value::Null => None,
                    Value::String(ref text) if text.is_empty() || text == "auto" => None,
                    Value::String(ref text) => Some(
                        crate::config::ReasoningEffort::parse(text)
                            .ok_or_else(|| anyhow!("effort must be minimal, low, medium, high, xhigh, max, or auto"))?,
                    ),
                    _ => return Err(anyhow!("effort must be a string or null")),
                };
                self.config.reasoning_effort = effort;
                self.update_profile(|profile| profile.reasoning_effort = effort)?;
            }
            "mode" => {
                let named = value.as_str().and_then(AgentMode::parse);
                self.mode = named.unwrap_or(AgentMode::Auto);
            }
            "autoApprove" => self.allow.store(value.as_bool().unwrap_or(false), Ordering::Relaxed),
            other => return Err(anyhow!("`{other}` is not a writable key")),
        }
        Ok(json!({
            "model": self.config.model,
            "mode": self.mode.label().to_ascii_lowercase(),
            "autoApprove": self.allow.load(Ordering::Relaxed),
            "reasoningEffort": self.config.reasoning_effort.map(|effort| effort.label()),
        }))
    }

    /// Edit the active profile and write the settings file, so a change made
    /// here is the same change `/config` would have made.
    fn update_profile(&mut self, edit: impl FnOnce(&mut crate::config::ProviderProfile)) -> Result<()> {
        let Some(profile) = self.settings.profiles.get_mut(&self.config.profile) else {
            // No stored profile (an env-only or one-off configuration): the
            // change still applies to this session, there is just nowhere to
            // persist it, and failing here would be worse than not saving.
            return Ok(());
        };
        edit(profile);
        self.settings.save(&self.config.paths)
    }

    /// Refuse state swaps while a turn is in flight. Replacing the history
    /// under a running turn would have it finish into a thread that no longer
    /// exists — better a clear error than a silently lost turn.
    fn require_idle(&self) -> Result<()> {
        if self.turn_id.is_some() {
            return Err(anyhow!("a turn is running — interrupt it first"));
        }
        Ok(())
    }

    fn thread_snapshot(&self) -> Value {
        json!({
            "threadId": self.thread_id(),
            "name": self.thread.session.title,
            "model": self.config.model,
            "profile": self.config.profile,
            "mode": self.mode.label().to_ascii_lowercase(),
            "autoApprove": self.allow.load(Ordering::Relaxed),
            "reasoningEffort": self.config.reasoning_effort.map(|effort| effort.label()),
            "workspace": self.config.workspace,
            "contextWindow": self.config.model_limits.context_window,
            "tokensUsed": self.tokens.total(),
            "usage": token_usage(&self.tokens.snapshot()),
            "items": history_items(&self.thread.messages),
        })
    }

    async fn git(&self, args: &[&str]) -> Result<String> {
        let output =
            tokio::process::Command::new("git").args(args).current_dir(&self.config.workspace).output().await?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// A prompt as the model receives it: `[image:…]` tokens and `@shot.png`
    /// references resolved, and the images the client attached appended.
    fn user_content(&self, text: &str, images: Vec<String>) -> Value {
        let content = crate::context::user_content(&self.config.workspace, &self.config.paths.attachments_dir, text);
        crate::context::with_images(content, images)
    }

    fn start_turn(&mut self, content: Value, events: &mpsc::UnboundedSender<AgentEvent>) {
        let item = user_item(&content);
        self.thread.messages.push(json!({"role": "user", "content": content}));
        let turn = self.next("turn");
        self.turn_id = Some(turn.clone());
        self.cancel = Arc::new(AtomicBool::new(false));

        let item_id = self.next("item");
        notify("turn/started", json!({"threadId": self.thread_id(), "turnId": turn}));
        // The user's own message is an item too, so a client that replays
        // `item/*` alone reconstructs the whole transcript.
        self.item("completed", json!({"id": item_id, "type": "userMessage", "text": item.0, "images": item.1}));

        let options = TurnOptions {
            mode: self.mode,
            allow_mutations: self.allow.clone(),
            session_id: Some(self.thread_id()),
            trace: self.thread.trace.clone(),
            cancel: self.cancel.clone(),
            ..self.thread.state.turn(&self.config, self.services.clone()).with_workspace_stores(&self.config)
        };
        tokio::spawn(run_turn(self.provider.clone(), self.thread.messages.clone(), options, events.clone()));
    }

    /// Announce an item of the running turn opening (`started`) or closing
    /// (`completed`).
    fn item(&self, phase: &str, item: Value) {
        let stamp = if phase == "started" { "startedAtMs" } else { "completedAtMs" };
        notify(
            &format!("item/{phase}"),
            json!({
                "threadId": self.thread_id(),
                "turnId": self.turn_id(),
                stamp: now_ms(),
                "item": item,
            }),
        );
    }

    /// Append streamed text to the open item of `kind`, opening one if needed.
    fn stream(&mut self, kind: &'static str, method: &str, text: String) {
        if !self.open_items.contains_key(kind) {
            let id = self.next("item");
            self.item("started", json!({"id": id, "type": kind, "text": ""}));
            self.open_items.insert(kind.to_owned(), OpenItem { id, kind, summary: String::new() });
        }
        let open = self.open_items.get_mut(kind).expect("opened above");
        open.summary.push_str(&text);
        let id = open.id.clone();
        notify(
            method,
            json!({
                "threadId": self.thread_id(),
                "turnId": self.turn_id(),
                "itemId": id,
                "delta": text,
            }),
        );
    }

    /// Open the item for a tool call and announce it at `status`.
    fn open_tool(&mut self, name: &str, summary: &str, status: &str) -> String {
        let open = OpenItem { id: self.next("item"), kind: item_type(name), summary: summary.to_owned() };
        self.item("started", open.tool(name, status));
        let id = open.id.clone();
        self.open_items.insert(name.to_owned(), open);
        id
    }

    fn handle_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Delta(text) => self.stream("agentMessage", "item/agentMessage/delta", text),
            AgentEvent::Reasoning(text) => self.stream("reasoning", "item/reasoning/textDelta", text),
            AgentEvent::ToolStarted { name, summary } => {
                // A tool call ends whatever text was streaming: the model has
                // stopped talking and started doing.
                self.flush_streaming();
                // An approval for this tool already opened the item, so that
                // the prompt could name the call it was about. Reuse it rather
                // than announcing the same call twice.
                match self.open_items.get_mut(&name) {
                    Some(open) => open.summary = summary,
                    None => drop(self.open_tool(&name, &summary, "inProgress")),
                }
            }
            AgentEvent::ToolFinished { name, output } => {
                if let Some(open) = self.open_items.remove(&name) {
                    let mut item = open.tool(&name, "completed");
                    item["output"] = json!(output);
                    self.item("completed", item);
                }
            }
            AgentEvent::ModeChanged { mode, reason } => notify(
                "thread/mode/updated",
                json!({
                    "threadId": self.thread_id(),
                    "mode": mode.label().to_ascii_lowercase(),
                    "reason": reason,
                }),
            ),
            AgentEvent::Notice(text) => notify("warning", json!({"message": text})),
            AgentEvent::TraceFailed { error } => {
                notify("warning", json!({"message": format!("training trace off — {error}")}))
            }
            AgentEvent::Approval(request) => {
                let id = self.next("srv");
                // The approval lands *before* the call starts, so open the item
                // here and let `ToolStarted` adopt it. That way the prompt can
                // name the call it is about instead of floating loose in the
                // transcript, and a rejected call still leaves a visible item.
                let item_id = match self.open_items.get(&request.tool) {
                    Some(open) => open.id.clone(),
                    None => self.open_tool(&request.tool, &request.summary, "awaitingApproval"),
                };
                emit(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": if COMMAND_TOOLS.contains(&request.tool.as_str()) {
                        "item/commandExecution/requestApproval"
                    } else {
                        "item/fileChange/requestApproval"
                    },
                    "params": {
                        "threadId": self.thread_id(),
                        "turnId": self.turn_id(),
                        "itemId": item_id,
                        "startedAtMs": now_ms(),
                        "tool": request.tool,
                        "command": request.summary,
                        "cwd": self.config.workspace,
                        "reason": request.details,
                        "availableDecisions": ["accept", "acceptForSession", "decline", "cancel"],
                    }
                }));
                self.pending_approvals.insert(id, request.respond);
            }
            AgentEvent::UserQuestion(request) => {
                let id = self.next("srv");
                emit(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "tool/requestUserInput",
                    "params": {
                        "threadId": self.thread_id(),
                        "turnId": self.turn_id(),
                        "header": request.header,
                        "question": request.question,
                        "options": request.options,
                        "multiSelect": request.multi_select,
                    }
                }));
                self.pending_questions.insert(id, request.respond);
            }
            AgentEvent::Done { messages, reason } => {
                self.thread.messages = messages;
                self.finish_turn(match reason {
                    DoneReason::Complete => "completed",
                    DoneReason::StepLimit => "stepLimit",
                    DoneReason::Interrupted => "interrupted",
                });
            }
            AgentEvent::Failed { error, messages } => {
                self.thread.messages = messages;
                notify("warning", json!({"message": error}));
                self.finish_turn("failed");
            }
        }
    }

    /// Close any open text items. Their accumulated text is the authoritative
    /// content of `item/completed`, which is what a client that ignored the
    /// deltas renders.
    fn flush_streaming(&mut self) {
        for kind in ["agentMessage", "reasoning"] {
            if let Some(open) = self.open_items.remove(kind) {
                self.item("completed", json!({"id": open.id, "type": open.kind, "text": open.summary}));
            }
        }
    }

    fn finish_turn(&mut self, status: &str) {
        self.flush_streaming();
        // Any tool still open lost its result to an interrupt; say so rather
        // than leaving a spinner running in the client forever.
        for (name, open) in std::mem::take(&mut self.open_items) {
            self.item("completed", open.tool(&name, "aborted"));
        }
        let turn = self.turn_id.take().unwrap_or_default();
        self.persist();
        self.report_diff(&turn);
        if let Some(usage) = &self.usage {
            usage.spawn_report(false);
        }
        notify(
            "thread/tokenUsage/updated",
            json!({
                "threadId": self.thread_id(),
                "tokensUsed": self.tokens.total(),
                "contextWindow": self.config.model_limits.context_window,
                // The split behind the total: input and output are priced
                // differently, and cached input an order of magnitude below
                // uncached, so a client that only renders `tokensUsed` cannot
                // tell an expensive session from a cheap one.
                "usage": token_usage(&self.tokens.snapshot()),
            }),
        );
        notify(
            "turn/completed",
            json!({
                "threadId": self.thread_id(),
                "turnId": turn,
                "status": status,
                "name": self.thread.session.title,
            }),
        );
    }

    /// Publish what the working tree looks like now, per file. A client shows
    /// this as the turn's changed-files list; computing it here means the diff
    /// the UI displays is the one git actually reports, not something
    /// reconstructed from tool arguments.
    ///
    /// Spawned rather than awaited: `finish_turn` runs on the event path, and
    /// a slow `git` in a large repository must not delay `turn/completed`.
    fn report_diff(&self, turn: &str) {
        let workspace = self.config.workspace.clone();
        let thread = self.thread_id();
        let turn = turn.to_owned();
        tokio::spawn(async move {
            let Ok(output) = tokio::process::Command::new("git")
                .args(["diff", "--numstat", "HEAD"])
                .current_dir(&workspace)
                .output()
                .await
            else {
                return;
            };
            let files: Vec<Value> = String::from_utf8_lossy(&output.stdout).lines().filter_map(parse_numstat).collect();
            if files.is_empty() {
                return;
            }
            notify("turn/diff/updated", json!({"threadId": thread, "turnId": turn, "files": files}));
        });
    }

    /// Match a client's answer to the server request that is waiting on it.
    fn resolve(&mut self, id: &Value, result: &Value) {
        let Some(id) = id.as_str() else { return };
        if let Some(responder) = self.pending_approvals.remove(id) {
            let decision = match result["decision"].as_str().unwrap_or("decline") {
                "accept" => ApprovalDecision::Once,
                "acceptForSession" => {
                    self.allow.store(true, Ordering::Relaxed);
                    ApprovalDecision::Always
                }
                "cancel" => {
                    self.cancel.store(true, Ordering::Relaxed);
                    ApprovalDecision::Reject
                }
                _ => ApprovalDecision::Reject,
            };
            let _ = responder.send(decision);
            return;
        }
        if let Some(responder) = self.pending_questions.remove(id) {
            let selected = result["selected"]
                .as_array()
                .map(|values| values.iter().filter_map(|value| value.as_str().map(str::to_owned)).collect())
                .unwrap_or_default();
            let _ = responder.send(UserAnswer {
                selected_labels: selected,
                custom_text: result["custom"].as_str().map(str::to_owned),
            });
        }
    }

    /// Save after every turn rather than at exit: a GUI is closed by killing
    /// its window, and a thread that only persisted on a clean shutdown would
    /// routinely be lost.
    fn persist(&mut self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let session = &mut self.thread.session;
        session.update_messages(self.thread.messages.clone());
        self.thread.state.save(session);
        session.tokens_used = self.tokens.total();
        session.model = self.config.model.clone();
        if let Err(error) = store.save(session) {
            eprintln!("app-server: session save failed — {error:#}");
            return;
        }
        self.thread.persisted = true;
        self.sync_after_turn();
    }

    /// Upload the thread in the background. A conflict or a sign-in that no
    /// longer works is told to the front end, since nothing else will: the
    /// upload runs after the reply it belongs to, with no one waiting on it.
    /// Other failures (offline, a busy server) retry on the next turn without
    /// comment.
    fn sync_after_turn(&self) {
        let session = self.thread.session.clone();
        let paths = self.config.paths.clone();
        let warned = self.sync_warned.clone();
        tokio::spawn(async move {
            let problems = match crate::sync::sync_session(&paths, &session).await {
                Ok(outcome) => outcome
                    .conflicts
                    .iter()
                    .map(|conflict| {
                        format!(
                            "{conflict} also changed on another device; both copies are kept the next time Abacus opens"
                        )
                    })
                    .collect(),
                Err(error) if matches!(error.downcast_ref(), Some(crate::sync::SyncError::Unauthorized)) => {
                    vec![
                        "Session sync stopped: this device is signed out. Run `abacus sync login` to resume."
                            .to_owned(),
                    ]
                }
                Err(_) => Vec::new(),
            };
            for message in problems {
                // Once per message: the same conflict would otherwise come back after every turn.
                if warned.lock().is_ok_and(|mut seen| seen.insert(message.clone())) {
                    notify("warning", json!({"message": message}));
                }
            }
        });
    }

    /// Record the thread as open: sync must not write over it, and its
    /// tokens are reported as a run of their own.
    fn thread_opened(&self) {
        crate::sync::session_opened(self.thread.session.id);
        if let Some(usage) = &self.usage {
            usage.open_session(&self.thread_id(), &self.config.model, self.tokens.clone());
        }
    }

    fn thread_closed(&self) {
        crate::sync::session_closed(self.thread.session.id);
    }

    /// Flush what the front end can no longer wait for: the thread's last
    /// upload and the closing usage report, together and each bounded. The
    /// thread is closed first, so what sync left alone while it was open is
    /// settled before the upload (see [`crate::sync::settle_closed`]).
    async fn close(&self) {
        let upload = async {
            let sync = async {
                let _ = crate::sync::settle_closed(&self.config.paths, self.thread.session.id).await;
                if self.thread.persisted {
                    let _ = crate::sync::push_session(&self.config.paths, &self.thread.session).await;
                }
            };
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, sync).await;
        };
        let report = async {
            if let Some(usage) = &self.usage {
                usage.finish(SHUTDOWN_TIMEOUT).await;
            }
        };
        tokio::join!(upload, report);
        self.thread_closed();
    }
}

/// The allowed values of every enum-valued setting, keyed by its dotted path.
///
/// This lives here rather than in the client because the client cannot know
/// them: a settings file is free-form JSON on the wire, and a front end that
/// guessed would drift the moment a variant is added.
fn settings_choices() -> Value {
    json!({
        // Closed sets: exactly these values parse, so a client should present
        // them as the only options. `null` where the setting is optional.
        "closed": {
            "ui.permission_mode": ["ask", "always-approve"],
            "ui.glyphs": ["auto", "unicode", "nerd", "ascii"],
            "agent.tool_format": [
                null, "auto", "none", "hermes", "qwen", "llama3_json",
                "mistral", "glm", "kimi", "deepseek", "json",
            ],
            "search.backend": ["auto", "bing", "searxng", "brave"],
            "reasoning_effort": [
                null, "minimal", "low", "medium", "high", "xhigh", "max",
            ],
        },
        // Open sets: these values are known-good, but any other string is
        // valid too — a theme name, for instance, refers to a file the user
        // may have written. A client should suggest, not constrain.
        "open": {
            "ui.theme": ["auto", "dark", "light"],
        },
    })
}

/// One `git diff --numstat` row: added, removed, path. A binary file reports
/// `-` for both counts, which becomes `null` rather than a misleading zero.
fn parse_numstat(line: &str) -> Option<Value> {
    let mut fields = line.split('\t');
    let added = fields.next()?;
    let removed = fields.next()?;
    let path = fields.next()?;
    let count = |value: &str| value.parse::<u64>().ok();
    Some(json!({
        "path": path,
        "added": count(added),
        "removed": count(removed),
        "binary": added == "-",
    }))
}

/// What a client sent as a turn: its text, and its images as data URLs.
#[derive(Debug, PartialEq)]
struct TurnInput {
    text: String,
    images: Vec<String>,
}

fn turn_input(params: &Value) -> Result<TurnInput> {
    // Accept the structured `input: [{type:"text",text}]` form and a bare
    // `text`, so a hand-written client is not forced through the array form.
    // Images come as `{type:"image", url}` (a data URL) or `{type:"localImage",
    // path}` (a file the client's user picked; read here).
    let mut input = TurnInput { text: String::new(), images: Vec::new() };
    if let Some(text) = params["text"].as_str() {
        input.text = text.to_owned();
    } else if let Some(items) = params["input"].as_array() {
        let mut texts = Vec::new();
        for item in items {
            match item["type"].as_str().unwrap_or("text") {
                "text" => texts.extend(item["text"].as_str()),
                "image" => {
                    let url = item["url"].as_str().ok_or_else(|| anyhow!("an image input needs a url"))?;
                    crate::context::check_image_url(url)?;
                    input.images.push(url.to_owned());
                }
                "localImage" => {
                    let path = item["path"].as_str().ok_or_else(|| anyhow!("a localImage input needs a path"))?;
                    input.images.push(crate::context::image_data_url(std::path::Path::new(path))?);
                }
                other => return Err(anyhow!("unsupported input type `{other}`")),
            }
        }
        input.text = texts.join("\n");
    } else {
        return Err(anyhow!("turn input required"));
    }
    if input.text.trim().is_empty() && input.images.is_empty() {
        return Err(anyhow!("turn input is empty"));
    }
    Ok(input)
}

/// A user message as a client item: its text, and its images when it has any.
fn user_item(content: &Value) -> (String, Option<Vec<&str>>) {
    let images = crate::text::content_images(content);
    (crate::text::content_text(content).into_owned(), (!images.is_empty()).then_some(images))
}

/// The token ledger as a client sees it. `cacheRate` is omitted rather than
/// zeroed when the endpoint reports no cache figures, so a client can tell "no
/// hits" from "no information".
fn token_usage(usage: &crate::provider::TokenUsage) -> Value {
    json!({
        "input": usage.input,
        "output": usage.output,
        "cacheRead": usage.cache_read,
        "cacheWrite": usage.cache_write,
        "uncachedInput": usage.uncached_input(),
        "cacheRate": usage.cache_rate(),
        "total": usage.total,
    })
}

/// Project a saved message history into the item shape the live stream uses, so
/// a resumed thread and a running one render through exactly one code path.
fn history_items(messages: &[Value]) -> Vec<Value> {
    // The live stream names each call by its summary ("src/lib.rs", "cargo
    // test"). History keeps the arguments on the assistant turn that made the
    // call, so a resumed thread can name it the same way.
    let summaries: HashMap<&str, String> = messages
        .iter()
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        .filter_map(|call| {
            let id = call["id"].as_str()?;
            let name = call["function"]["name"].as_str()?;
            let arguments = call["function"]["arguments"].as_str().unwrap_or_default();
            let call = ToolCall { id: id.to_owned(), name: name.to_owned(), arguments: arguments.to_owned() };
            Some((id, call.summary()))
        })
        .collect();

    let mut items = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let id = format!("history_{index}");
        let role = message["role"].as_str().unwrap_or_default();
        let content = message["content"].as_str().unwrap_or_default();
        match role {
            // The system prompt is scaffolding, not conversation.
            "system" => continue,
            "user" => {
                let (text, images) = user_item(&message["content"]);
                if !text.trim().is_empty() || images.is_some() {
                    items.push(json!({"id": id, "type": "userMessage", "text": text, "images": images}));
                }
            }
            "assistant" => {
                let reasoning = message["reasoning_content"].as_str().unwrap_or_default();
                if !reasoning.trim().is_empty() {
                    items.push(json!({"id": format!("{id}_reasoning"), "type": "reasoning", "text": reasoning}));
                }
                if !content.trim().is_empty() {
                    items.push(json!({"id": id, "type": "agentMessage", "text": content}));
                }
            }
            "tool" => {
                let name = message["name"].as_str().unwrap_or("tool");
                let mut item = json!({
                    "id": id,
                    "type": item_type(name),
                    "name": name,
                    "status": "completed",
                    "output": content,
                });
                if let Some(summary) = message["tool_call_id"].as_str().and_then(|call| summaries.get(call)) {
                    item["command"] = json!(summary);
                }
                items.push(item);
            }
            _ => continue,
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests_notifications_and_responses() {
        let request = parse(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).unwrap();
        assert!(matches!(request, Incoming::Request { .. }));
        let notification = parse(r#"{"jsonrpc":"2.0","method":"shutdown"}"#).unwrap();
        assert!(matches!(notification, Incoming::Notification { .. }));
        // A reply to a server-initiated request carries a result, not a method.
        let response = parse(r#"{"jsonrpc":"2.0","id":"srv_1","result":{"decision":"accept"}}"#).unwrap();
        match response {
            Incoming::Response { id, result } => {
                assert_eq!(id, json!("srv_1"));
                assert_eq!(result["decision"], "accept");
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn turn_input_accepts_both_shapes() {
        let hello = TurnInput { text: "hello".into(), images: Vec::new() };
        assert_eq!(turn_input(&json!({"text": "hello"})).unwrap(), hello);
        assert_eq!(turn_input(&json!({"input": [{"type": "text", "text": "hello"}]})).unwrap(), hello);
        assert!(turn_input(&json!({"text": "  "})).is_err());
        assert!(turn_input(&json!({})).is_err());
    }

    #[test]
    fn turn_input_takes_images_by_url_and_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("shot.png");
        std::fs::write(&shot, b"png bytes").unwrap();
        let url = "data:image/png;base64,QUJD";

        let input = turn_input(&json!({"input": [
            {"type": "text", "text": "what is this?"},
            {"type": "image", "url": url},
            {"type": "localImage", "path": shot},
        ]}))
        .unwrap();
        assert_eq!(input.text, "what is this?");
        assert_eq!(input.images[0], url);
        assert!(input.images[1].starts_with("data:image/png;base64,"));

        // An image alone is a prompt; a bad one is refused rather than dropped.
        assert!(turn_input(&json!({"input": [{"type": "image", "url": url}]})).is_ok());
        assert!(turn_input(&json!({"input": [{"type": "image", "url": "https://example.com/a.png"}]})).is_err());
        assert!(turn_input(&json!({"input": [{"type": "localImage", "path": dir.path().join("x.txt")}]})).is_err());
        assert!(turn_input(&json!({"input": [{"type": "audio"}]})).is_err());
    }

    #[test]
    fn history_shows_image_prompts_with_their_images() {
        let items = history_items(&[json!({"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
        ]})]);
        assert_eq!(items[0]["type"], "userMessage");
        assert_eq!(items[0]["text"], "what is this?");
        assert_eq!(items[0]["images"], json!(["data:image/png;base64,QUJD"]));
    }

    #[test]
    fn history_drops_the_system_prompt_and_keeps_order() {
        let items = history_items(&[
            json!({"role": "system", "content": "scaffolding"}),
            json!({"role": "user", "content": "fix the parser"}),
            json!({"role": "assistant", "content": "on it"}),
            json!({"role": "tool", "name": "run_command", "content": "ok"}),
        ]);
        let types: Vec<&str> = items.iter().map(|item| item["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["userMessage", "agentMessage", "commandExecution"]);
    }

    #[test]
    fn history_names_tool_calls_and_keeps_reasoning() {
        let items = history_items(&[
            json!({"role": "user", "content": "what does it print?"}),
            json!({
                "role": "assistant",
                "content": null,
                "reasoning_content": "Read the file first.",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\": \"fizzbuzz.py\"}"},
                }],
            }),
            json!({"role": "tool", "name": "read_file", "tool_call_id": "call_1", "content": "def fizzbuzz(n): ..."}),
            json!({"role": "tool", "name": "glob", "content": "orphaned result"}),
        ]);
        let types: Vec<&str> = items.iter().map(|item| item["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["userMessage", "reasoning", "functionCall", "functionCall"]);
        assert_eq!(items[1]["text"], "Read the file first.");
        assert_eq!(items[2]["command"], "fizzbuzz.py");
        // A result whose call is gone still shows, just without a summary.
        assert!(items[3].get("command").is_none());
    }

    #[test]
    fn numstat_rows_parse_including_binary_files() {
        let text = parse_numstat("44\t3\tsrc/lib.rs").unwrap();
        assert_eq!(text["path"], "src/lib.rs");
        assert_eq!(text["added"], 44);
        assert_eq!(text["removed"], 3);
        assert_eq!(text["binary"], false);

        // A binary file has no line counts; reporting 0/0 would read as
        // "changed nothing", which is the opposite of the truth.
        let binary = parse_numstat("-\t-\tassets/logo.png").unwrap();
        assert_eq!(binary["binary"], true);
        assert!(binary["added"].is_null());

        assert!(parse_numstat("garbage").is_none());
    }

    #[test]
    fn tools_map_onto_client_item_kinds() {
        assert_eq!(item_type("run_command"), "commandExecution");
        assert_eq!(item_type("apply_patch"), "fileChange");
        assert_eq!(item_type("grep"), "functionCall");
    }
}
