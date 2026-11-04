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
//! a second process — the state a turn needs (memories, handles, injections)
//! is per-session anyway, so multiplexing it inside one process would buy
//! nothing but a lifetime puzzle.
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
    agent::{AgentEvent, ApprovalDecision, DoneReason, TurnOptions, UserAnswer, initial_messages, run_turn},
    config::{Config, Credentials, Permission, Settings},
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
const COMMAND_TOOLS: &[&str] = &["run_command", "run_python"];
/// Tools that change files. Split out because a front end shows a diff for
/// these and a log for everything else.
const FILE_CHANGE_TOOLS: &[&str] =
    &["edit_file", "write_file", "apply_patch", "delete_file", "move_file", "append_file", "create_directory"];

pub(crate) fn item_type(tool: &str) -> &'static str {
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
    /// The client answering a request *we* sent — an approval, a question,
    /// or a front-end tool call.
    Response {
        id: Value,
        result: Value,
        error: Option<Value>,
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
                config.model_ref(),
                if config.chat { crate::agent::chat_messages() } else { initial_messages(&config.workspace) },
            )
        });
        let state = SessionState::open(config, Some(&session));
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
    /// What the call is for, in the model's words.
    description: Option<String>,
    /// What a `send_message` call said.
    message: Option<String>,
}

impl OpenItem {
    /// The item as a client sees a tool call at `status`.
    fn tool(&self, name: &str, status: &str) -> Value {
        let mut item = json!({
            "id": self.id, "type": self.kind, "name": name,
            "command": self.summary, "status": status,
        });
        if let Some(description) = &self.description {
            item["description"] = json!(description);
        }
        if let Some(message) = &self.message {
            item["message"] = json!(message);
        }
        item
    }
}

struct App {
    config: Config,
    settings: Settings,
    /// Read when a provider or profile changes, written when a key is pasted.
    credentials: Credentials,
    services: Arc<AgentServices>,
    store: Option<SessionStore>,
    thread: ThreadState,
    provider: Provider,
    tokens: Arc<crate::provider::TokenLedger>,
    /// The provider's prompt count at the last usage report, so a reply with
    /// several tool calls reports once.
    reported_context: u64,
    /// The thread's goal and task list as last told to the front end, so
    /// each change is reported once.
    reported_goal: Value,
    reported_tasks: Value,
    /// Whether the running turn has called a tool, which tells an active
    /// goal whether its turns still get anywhere.
    turn_worked: bool,
    /// The prompt of the turn an active goal starts once the last one ended
    /// by itself (see `finish_turn`).
    goal_turn: Option<String>,
    /// YOLO when set, AUTO otherwise. Shared with the running turn, so a
    /// switch applies from its next tool call.
    yolo: Arc<AtomicBool>,
    /// Raised to ask the running turn to wind down. Replaced per turn.
    cancel: Arc<AtomicBool>,
    /// What the running turn's file tools changed. Replaced per turn.
    journal: crate::checkpoint::Journal,
    turn_id: Option<String>,
    /// Tool name -> the item its call opened.
    open_items: HashMap<String, OpenItem>,
    /// Tool name -> what its next call is for, until that call's item opens.
    descriptions: HashMap<String, String>,
    /// Server-initiated requests waiting on the client, by request id.
    pending_approvals: HashMap<String, oneshot::Sender<ApprovalDecision>>,
    pending_questions: HashMap<String, oneshot::Sender<UserAnswer>>,
    /// Front-end tool calls waiting on their answer: the tool, and who waits.
    pending_client_calls: HashMap<String, (String, oneshot::Sender<crate::client_tools::ClientResult>)>,
    client_calls: Option<mpsc::UnboundedSender<crate::client_tools::ClientCall>>,
    counter: u64,
    /// Account-tied usage, present only while signed in to Abacus Sync.
    usage: Option<UsageReporter>,
    /// Sync problems already told to the front end.
    sync_warned: Arc<std::sync::Mutex<HashSet<String>>>,
    /// One set of workers for the process, as the TUI keeps: background workers
    /// outlive the turn that spawned them, and the next turn (and the front
    /// end) must still see them.
    workers: crate::workers::Workers,
    /// When idle compaction is due: set by a turn that finished, cleared by
    /// anything that replaces the thread or starts a turn.
    idle_due: Option<tokio::time::Instant>,
    /// Raised to stop the idle compaction in flight, if there is one.
    idle_compaction: Option<Arc<AtomicBool>>,
    /// Where idle compaction reports back to the serving loop.
    idle_done: mpsc::UnboundedSender<IdleDone>,
    /// Stop once nothing has happened for this long (see [`IDLE_EXIT_ENV`]).
    idle_exit: Option<std::time::Duration>,
    /// When anything last happened: a request, an event of a turn, a summary
    /// written.
    last_activity: tokio::time::Instant,
    /// The open thread's presence and inbox: other sessions see it as open,
    /// and their messages reach it here.
    mailbox: crate::inbox::Mailbox,
}

/// Idle compaction finished, written from `basis`. `job` is the flag it could
/// be stopped by, which tells it apart from one that was.
struct IdleDone {
    job: Arc<AtomicBool>,
    basis: Vec<Value>,
    result: Option<crate::agent::IdleCompaction>,
}

/// Wait for `deadline`, or forever when there is none.
/// Seconds of nothing happening after which the server stops by itself, when
/// set: a desktop that keeps one per thread sets it, and starts the thread's
/// server again when the person comes back to it. An environment variable, so
/// a front end can set it without knowing whether this abacus understands it.
pub const IDLE_EXIT_ENV: &str = "ABACUS_IDLE_EXIT_SECS";

async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
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
    let yolo = Arc::new(AtomicBool::new(config.permission == Permission::Yolo));
    let credentials = Credentials::load(&config.paths).unwrap_or_default();
    let usage = UsageReporter::new(&config.paths, &credentials, "app-server");
    let workers = crate::workers::Workers::default();
    let (idle_done, mut idle_rx) = mpsc::unbounded_channel::<IdleDone>();
    let mailbox = crate::inbox::Mailbox::new(config.paths.clone(), crate::presence::FrontEnd::Desktop);
    let mut app = App {
        thread: ThreadState::new(&config, session),
        config,
        settings,
        credentials: credentials.clone(),
        services,
        store,
        provider,
        tokens,
        reported_context: 0,
        reported_goal: Value::Null,
        reported_tasks: Value::Null,
        turn_worked: false,
        goal_turn: None,
        yolo,
        cancel: Arc::new(AtomicBool::new(false)),
        journal: Default::default(),
        turn_id: None,
        open_items: HashMap::new(),
        descriptions: HashMap::new(),
        pending_approvals: HashMap::new(),
        pending_questions: HashMap::new(),
        pending_client_calls: HashMap::new(),
        client_calls: None,
        counter: 0,
        usage,
        sync_warned: Arc::default(),
        workers,
        idle_due: None,
        idle_compaction: None,
        idle_done,
        idle_exit: std::env::var(IDLE_EXIT_ENV)
            .ok()
            .and_then(|seconds| seconds.trim().parse::<u64>().ok())
            .filter(|&seconds| seconds > 0)
            .map(std::time::Duration::from_secs),
        last_activity: tokio::time::Instant::now(),
        mailbox,
    };
    app.thread_opened();
    let usage_task = app.usage.as_ref().map(UsageReporter::spawn_periodic);
    let tasks_task = watch_tasks(app.workers.board.clone());
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
    // Calls to tools the front end registered come back through here, so the
    // request goes out on the one stdout writer and its answer is matched in
    // `resolve` like an approval's.
    let (client_calls, mut client_call_rx) = mpsc::unbounded_channel::<crate::client_tools::ClientCall>();
    app.client_calls = Some(client_calls);
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    let mut inbox_checks = tokio::time::interval(std::time::Duration::from_secs(1));
    inbox_checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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

    let mut went_idle = false;
    loop {
        tokio::select! {
            line = line_rx.recv() => {
                let Some(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                app.last_activity = tokio::time::Instant::now();
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
                    Ok(Incoming::Response { id, result, error }) => app.resolve(&id, &result, error.as_ref()),
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
                app.last_activity = tokio::time::Instant::now();
                app.handle_event(event);
                app.start_pending_turn(&events);
            }
            call = client_call_rx.recv() => {
                let Some(call) = call else { continue };
                app.forward_client_call(call);
            }
            () = until(app.idle_due) => app.start_idle_compaction(),
            done = idle_rx.recv() => {
                let Some(done) = done else { continue };
                app.last_activity = tokio::time::Instant::now();
                app.idle_compacted(done);
            }
            // Nothing has happened for a while: stop, and give the machine its
            // memory back. The front end starts it again when it is needed.
            () = until(app.idle_exit_due()) => {
                if app.quiet() {
                    notify("server/idle", json!({"threadId": app.thread_id()}));
                    went_idle = true;
                    break;
                }
            }
            // Another session can write, and a background subagent can
            // finish or message `main`, while no turn runs to pick it up.
            _ = inbox_checks.tick() => {
                app.check_inbox();
                app.start_pending_turn(&events);
            }
        }
    }
    if let Some(task) = usage_task {
        task.abort();
    }
    tasks_task.abort();
    app.close().await;
    if went_idle {
        // The front end still holds stdin open, and the task reading it would
        // keep the process waiting on it: leave now that everything is saved.
        std::process::exit(0);
    }
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
            error: value.get("error").cloned(),
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
                // No provider yet: the front end walks the person through
                // `presets/list`, `provider/save` and `profile/save` before
                // the first turn.
                "needsSetup": !self.config.is_configured(),
                "modes": ["auto", "yolo"],
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
                    // ...and `file` (name and text) and `localFile` (path).
                    "fileInput": true,
                    "fileSearch": true,
                    "commands": true,
                    // Background workers: tasks/list, task/read, task/stop,
                    // tasks/clear, and tasks/updated as they change.
                    "tasks": true,
                    // tools/register: the front end lends the agent tools of
                    // its own (the desktop's browser), called back as tool/call.
                    "clientTools": true,
                    "compact": true,
                    // providers/list, provider/save, provider/remove,
                    // provider/models, presets/list.
                    "providers": true,
                    // profiles/list, profile/select, profile/save,
                    // profile/remove; the profile picker replaces the
                    // model picker.
                    "profiles": true,
                    // A long thread left idle is compacted before the
                    // person comes back (`thread/compacted`); the
                    // `idle_compaction` config key turns it off.
                    "idleCompaction": true,
                    // thread/fork, and `forkedFrom` on thread/list.
                    "fork": true,
                    // Sessions message each other: `live` on thread/list, and
                    // a `from` on the userMessage items they send.
                    "sessionMessages": true,
                    // Subagents' messages and reports are userMessage items
                    // `from` an agent (`from.agent`), `report` on a report;
                    // a send_message call's item carries its `message`.
                    "subagentMessages": true,
                    // A tool item, and a command's approval, carry the
                    // model's `description` of what the call is for.
                    "toolDescriptions": true,
                    // command/run answers /memory, /goal, /usage, /repair
                    // and /tools; `max_steps` and `tool_output_limit` are
                    // writable config keys.
                    "commandRun": true,
                    "agentLimits": true,
                    // The thread snapshot carries the `goal` and the
                    // `taskList`; thread/goal/updated and
                    // thread/taskList/updated report each change.
                    "goal": true,
                    "taskList": true,
                    // feedback/submit sends a report where the terminal's
                    // /feedback sends it.
                    "feedback": true,
                    // Profiles, providers and settings save while a turn
                    // runs; the turn keeps what it started with.
                    "settingsMidTurn": true,
                },
                "idleCompaction": self.config.idle_compaction,
                "maxSteps": self.config.max_steps,
                "toolOutputLimit": self.config.tool_output_limit,
                "thread": self.thread_snapshot(),
            })),
            "shutdown" => Ok(json!({})),

            // ---- threads ----------------------------------------------------
            "thread/list" => {
                let store = self.store.as_ref().ok_or_else(|| anyhow!("sessions off"))?;
                let live = crate::presence::live_sessions(&self.config.paths);
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
                            "forkedFrom": summary.forked_from.map(|id| id.to_string()),
                            "live": live.get(&summary.id).map(crate::presence::Live::to_json),
                        })
                    })
                    .collect();
                Ok(json!({"threads": threads}))
            }
            "thread/start" => {
                self.require_idle()?;
                self.stop_idle_compaction();
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
                self.stop_idle_compaction();
                self.thread_closed();
                self.tokens.store_total(session.tokens_used);
                self.thread = ThreadState::new(&self.config, Some(session));
                self.thread_opened();
                Ok(self.thread_snapshot())
            }
            // Continue a thread as a new one that remembers where it came
            // from: the open thread, or the one `threadId` names. The window
            // switches to the fork; the original is left as it was.
            "thread/fork" => {
                self.require_idle()?;
                let store = self.store.as_ref().ok_or_else(|| anyhow!("sessions off"))?;
                let original = match params["threadId"].as_str() {
                    Some(id) if id != self.thread_id() => store.load(id)?,
                    _ if !self.thread.persisted => return Err(anyhow!("nothing to fork yet — send a message first")),
                    _ => {
                        self.persist();
                        self.thread.session.clone()
                    }
                };
                let store = self.store.as_ref().ok_or_else(|| anyhow!("sessions off"))?;
                let fork = store.fork(
                    &original,
                    self.config.profile.clone(),
                    self.config.model_ref(),
                    original.messages.clone(),
                )?;
                self.stop_idle_compaction();
                self.thread_closed();
                self.tokens.store_total(0);
                self.thread = ThreadState::new(&self.config, Some(fork));
                self.thread_opened();
                notify("thread/started", json!({"threadId": self.thread_id(), "forkedFrom": original.id.to_string()}));
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
                if !self.config.is_configured() {
                    return Err(anyhow!("connect a provider and choose a model first"));
                }
                // The person is here: messages from other sessions may start
                // turns again.
                self.mailbox.attended();
                let TurnInput { text, images, files } = turn_input(&params, &self.config.workspace)?;
                let typed = text.clone();
                // A slash command means what it means in the TUI: a skill or
                // plugin command becomes its prompt, `/btw` a side note.
                let (text, command) = match self.slash(&text)? {
                    Slash::Prompt(prompt) => (prompt, true),
                    Slash::SideNote(note) => {
                        self.thread.state.injections.push(crate::agent::Injection::SideNote(note));
                        return Ok(json!({"noted": true, "turnId": self.turn_id()}));
                    }
                    Slash::None => (text, false),
                };
                let text = with_files(text, files);
                // Mid-turn input steers rather than queueing a second turn —
                // the same rule the TUI follows, so the model can change course
                // at the next tool boundary instead of after everything it has
                // already planned. `turn/steer` is the explicit spelling of it.
                if self.turn_id.is_some() {
                    let content = self.user_content(&text, images);
                    self.thread.state.injections.push(match content {
                        Value::String(_) => crate::agent::Injection::UserMessage(text.clone()),
                        content => crate::agent::Injection::UserContent { text: text.clone(), content },
                    });
                    // It joins the transcript when the model reads it, which
                    // `turn/steer/delivered` announces with this same text.
                    return Ok(json!({"steered": true, "turnId": self.turn_id(), "text": text}));
                }
                if method == "turn/steer" {
                    return Err(anyhow!("no turn is running to steer"));
                }
                let text = crate::context::expand_file_references(&self.config.workspace, &text).unwrap_or(text);
                let content = self.user_content(&text, images);
                self.start_turn(content, command.then_some(typed), events);
                Ok(json!({"turnId": self.turn_id(), "threadId": self.thread_id()}))
            }
            // Take back a steering message the model has not read yet.
            "turn/steer/withdraw" => {
                let text = params["text"].as_str().ok_or_else(|| anyhow!("text required"))?;
                Ok(json!({"withdrawn": self.thread.state.injections.withdraw(text)}))
            }
            // A report from the window: what the terminal's /feedback sends,
            // to the same place.
            "feedback/submit" => {
                if !self.settings.feedback.enabled {
                    return Err(anyhow!("feedback is turned off in the settings"));
                }
                let message = params["message"].as_str().unwrap_or_default().trim();
                if message.is_empty() {
                    return Err(anyhow!("describe what happened or what you would like changed"));
                }
                let category = params["category"].as_str().unwrap_or_default().to_ascii_lowercase();
                let include_diagnostics = params["includeDiagnostics"].as_bool().unwrap_or(false);
                let version = env!("CARGO_PKG_VERSION");
                let payload = crate::feedback::FeedbackPayload {
                    category: if FEEDBACK_CATEGORIES.contains(&category.as_str()) {
                        category
                    } else {
                        "general".into()
                    },
                    message: message.to_owned(),
                    include_diagnostics,
                    diagnostics: if include_diagnostics { self.services.diagnostics() } else { Vec::new() },
                    session_id: self.thread.persisted.then(|| self.thread_id()),
                    workspace: self.config.workspace_name().to_owned(),
                    // Which window sent it, when one says.
                    app_version: match params["client"].as_str() {
                        Some(client) => format!("{version} ({client})"),
                        None => version.to_owned(),
                    },
                    os: std::env::consts::OS.to_owned(),
                    arch: std::env::consts::ARCH.to_owned(),
                };
                let receipt =
                    crate::feedback::FeedbackClient::new(&self.settings.feedback.endpoint)?.submit(&payload).await?;
                Ok(json!({"id": receipt.id, "message": receipt.message}))
            }
            "commands/list" => {
                let mut commands = vec![
                    json!({"name": "btw", "description": "Note something for the running turn without derailing it", "kind": "builtin"}),
                    json!({"name": "compact", "description": "Compact the conversation's context", "kind": "builtin"}),
                ];
                if let Ok(skills) = self.services.skills.read() {
                    commands.extend(
                        skills.list().map(
                            |skill| json!({"name": skill.name, "description": skill.description, "kind": "skill"}),
                        ),
                    );
                }
                commands.extend(self.services.plugins.commands().map(
                    |command| json!({"name": command.name, "description": command.description, "kind": "plugin"}),
                ));
                Ok(json!({"commands": commands}))
            }
            // The terminal's commands that answer with text: the window shows
            // what they say. `/goal` may start a turn as well, as it does in
            // the terminal.
            "command/run" => {
                let name = params["name"].as_str().ok_or_else(|| anyhow!("name required"))?;
                let argument = params["argument"].as_str().unwrap_or_default().trim();
                self.run_command(name, argument, events)
            }
            "tools/register" => {
                let calls = self.client_calls.clone().ok_or_else(|| anyhow!("front-end tools are not available"))?;
                // Measured without any earlier front-end tools, which this replaces.
                let without_client = self.services.with_client_tools(None);
                let taken: Vec<String> = without_client
                    .tool_specs()
                    .iter()
                    .filter_map(|spec| spec["function"]["name"].as_str().map(str::to_owned))
                    .collect();
                let tools = crate::client_tools::ClientTools::from_params(&params, &taken, calls)?;
                let names: Vec<String> = tools.names().cloned().collect();
                self.services = Arc::new(without_client.with_client_tools(Some(Arc::new(tools))));
                Ok(json!({"registered": names}))
            }
            "tasks/list" => Ok(json!({"tasks": task_summaries(&self.workers.board)})),
            "task/read" => {
                let id = params["id"].as_u64().ok_or_else(|| anyhow!("task id required"))?;
                let task = self.workers.board.all().into_iter().find(|worker| worker.id == id);
                let (Some(task), Some(items)) = (task, self.workers.board.transcript(id)) else {
                    return Err(anyhow!("no task {id}"));
                };
                Ok(json!({"task": task_summary(&task), "items": items}))
            }
            "task/stop" => {
                let id = params["id"].as_u64().ok_or_else(|| anyhow!("task id required"))?;
                Ok(json!({"stopping": self.workers.board.stop(id)}))
            }
            "tasks/clear" => {
                self.workers.board.clear_finished();
                Ok(json!({"tasks": task_summaries(&self.workers.board)}))
            }
            "files/search" => {
                let query = params["query"].as_str().unwrap_or_default();
                let limit = params["limit"].as_u64().map_or(20, |limit| limit.clamp(1, 200) as usize);
                Ok(json!({"files": crate::context::search_files(&self.config.workspace, query, limit)}))
            }
            "thread/compact" => {
                self.require_idle()?;
                let before = self.thread.messages.len();
                let target = self.config.model_limits.compaction_budget().recent_budget_chars;
                self.thread.messages = crate::agent::compact_messages(&self.thread.messages, target);
                self.persist();
                Ok(json!({"before": before, "after": self.thread.messages.len()}))
            }
            // The person is writing in the window, which nothing else tells
            // the app server, so idle compaction waits, as it does for a
            // keystroke in the terminal.
            "thread/typing" => {
                self.hold_idle_compaction();
                Ok(json!({}))
            }
            "turn/interrupt" => {
                self.cancel.store(true, Ordering::Relaxed);
                Ok(json!({"interrupting": self.turn_id.is_some()}))
            }

            // ---- providers and profiles --------------------------------------
            "presets/list" => Ok(json!({"presets": crate::providers::PRESETS.iter().map(|preset| json!({
                "id": preset.id,
                "name": preset.name,
                "baseUrl": preset.base_url,
                "protocol": preset.protocol,
                "keyVariable": preset.env_key,
                "keyFound": crate::providers::key_in_env(preset),
                "local": crate::providers::is_local_url(preset.base_url),
                "hint": preset.hint,
            })).collect::<Vec<_>>()})),
            "providers/list" => Ok(json!({"providers": self.provider_rows()})),
            "provider/save" => self.save_provider(&params),
            "provider/remove" => {
                let id = params["id"].as_str().ok_or_else(|| anyhow!("id required"))?;
                if !self.settings.providers.contains_key(id) {
                    return Err(anyhow!("no provider named `{id}`"));
                }
                if self.settings.profiles.get(&self.settings.default_profile).is_some_and(|p| p.provider() == id)
                    && self.settings.profiles_on(id).len() == self.settings.profiles.len()
                {
                    return Err(anyhow!("every profile runs on {id}; add another provider before removing this one"));
                }
                let removed = self.settings.remove_provider(id);
                crate::providers::drop_catalog(&self.config.paths, id);
                if self.credentials.keys.remove(id).is_some() {
                    self.credentials.save(&self.config.paths)?;
                }
                self.settings.save(&self.config.paths)?;
                self.reapply_profile()?;
                Ok(json!({"removedProfiles": removed, "providers": self.provider_rows()}))
            }
            "provider/models" => {
                let id = params["id"].as_str().ok_or_else(|| anyhow!("id required"))?;
                let provider = self.settings.providers.get(id).ok_or_else(|| anyhow!("no provider named `{id}`"))?;
                let connection = crate::providers::Connection::resolve(id, provider, &self.credentials);
                let refresh = params["refresh"].as_bool().unwrap_or(false);
                let catalog = crate::providers::catalog(&self.config.paths, &connection, refresh).await?;
                Ok(json!({
                    "models": catalog.models.iter().map(|card| model_card(id, card)).collect::<Vec<_>>(),
                    "fetchedAt": catalog.fetched_at.to_rfc3339(),
                }))
            }
            "profiles/list" => Ok(json!({"profiles": self.profile_rows(), "active": self.config.profile})),
            "profile/select" => {
                let name = params["name"].as_str().ok_or_else(|| anyhow!("name required"))?;
                self.select_profile(name)?;
                Ok(self.config_state())
            }
            "profile/save" => self.save_profile(&params),
            "profile/remove" => {
                let name = params["name"].as_str().ok_or_else(|| anyhow!("name required"))?;
                self.settings.remove_profile(name)?;
                self.settings.save(&self.config.paths)?;
                if self.config.profile == name {
                    self.reapply_profile()?;
                }
                Ok(json!({"profiles": self.profile_rows(), "active": self.config.profile}))
            }

            // ---- models and config -----------------------------------------
            // The active provider's models, as profiles name them. Kept for
            // front ends that pick a model rather than a profile.
            "model/list" => {
                if !self.config.is_configured() {
                    return Ok(json!({"models": []}));
                }
                let catalog = crate::providers::catalog(&self.config.paths, &self.config.connection, false).await?;
                let current = self.config.model_ref();
                let models: Vec<Value> = catalog
                    .models
                    .iter()
                    .map(|card| {
                        let id = crate::providers::join_ref(&self.config.connection.provider, &card.id);
                        json!({"id": id, "selected": id == current})
                    })
                    .collect();
                Ok(json!({"models": models}))
            }
            "config/read" => Ok(json!({
                "settings": serde_json::to_value(&self.settings)?,
                // The choices behind the enum-valued settings, so a client can
                // offer a real picker instead of a free-text box it cannot
                // validate. Keyed by dotted path into `settings`.
                "choices": settings_choices(),
                "profile": self.config.profile,
                "model": self.config.model_ref(),
                "baseUrl": self.config.connection.base_url,
                "workspace": self.config.workspace,
                "mode": self.mode().label().to_ascii_lowercase(),
                "autoApprove": self.yolo.load(Ordering::Relaxed),
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
            // Settings save mid-turn too: the running turn keeps what it
            // started with, and the next one runs what they say.
            "config/write" => {
                let settings: Settings = serde_json::from_value(params["settings"].clone())?;
                settings.save(&self.config.paths)?;
                self.settings = settings;
                self.reapply_profile()?;
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
            // One turn's own edits, or the whole working tree's.
            "thread/diff/read" => match params["checkpoint"].as_str() {
                Some(checkpoint) => Ok(json!({"diff": crate::checkpoint::diff(&self.checkpoints(), checkpoint)?})),
                None => Ok(json!({
                    "diff": self.git(&["diff"]).await?,
                    "status": self.git(&["status", "--porcelain=v1", "-b"]).await?,
                })),
            },
            // The meter's figure, split by what fills it.
            "thread/context/read" => {
                let used = self.context_tokens();
                Ok(json!({
                    "used": used,
                    "window": self.config.model_limits.context_window,
                    "compactsAt": self.config.compaction_marks().summary,
                    "estimated": self.provider.context_tokens() == 0,
                    "parts": self.context_parts(used),
                }))
            }
            "turn/undo" => {
                let checkpoint =
                    params["checkpoint"].as_str().ok_or_else(|| anyhow!("checkpoint required"))?.to_owned();
                self.undo_edits(&checkpoint)
            }

            other => Err(anyhow!("unknown method `{other}`")),
        }
    }

    fn write_config_value(&mut self, key: &str, value: Value) -> Result<Value> {
        match key {
            "profile" => {
                let name = value.as_str().ok_or_else(|| anyhow!("profile must be a name"))?;
                self.select_profile(name)?;
            }
            // A model for the active profile, as `provider/model` or an id on
            // its own provider. The profile picker is the newer way.
            "model" => {
                let model = value.as_str().ok_or_else(|| anyhow!("model must be a string"))?;
                let name = self.config.profile.clone();
                let profile = self.settings.profiles.get(&name).ok_or_else(|| anyhow!("no active profile"))?;
                let reference = self.settings.qualify(model, profile);
                self.settings.target(&reference, &self.credentials)?;
                if let Some(profile) = self.settings.profiles.get_mut(&name) {
                    profile.model = reference;
                }
                self.settings.save(&self.config.paths)?;
                self.reapply_profile()?;
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
                self.provider = Provider::with_tokens(&self.config, self.tokens.clone())?;
                if let Some(profile) = self.settings.profiles.get_mut(&self.config.profile) {
                    profile.reasoning_effort = effort;
                    self.settings.save(&self.config.paths)?;
                }
            }
            // AUTO or YOLO for this session; the older names still parse.
            "mode" => {
                let mode = value.as_str().and_then(Permission::parse).unwrap_or_default();
                self.set_mode(mode);
            }
            "autoApprove" => {
                self.set_mode(if value.as_bool().unwrap_or(false) { Permission::Yolo } else { Permission::Auto });
            }
            // Saved for every session, as `/config` saves it in the terminal.
            "idle_compaction" => {
                let enabled = value.as_bool().ok_or_else(|| anyhow!("idle_compaction must be true or false"))?;
                self.settings.agent.idle_compaction = enabled;
                self.settings.save(&self.config.paths)?;
                self.config.idle_compaction = enabled;
                if !enabled {
                    self.stop_idle_compaction();
                }
            }
            // Saved for every session; the next turn runs with them.
            "max_steps" => {
                let steps = value.as_u64().ok_or_else(|| anyhow!("max_steps must be a number"))? as usize;
                crate::config::check_max_steps(steps)?;
                self.settings.agent.max_steps = steps;
                self.settings.save(&self.config.paths)?;
                self.config.max_steps = steps;
            }
            "tool_output_limit" => {
                let limit = value.as_u64().ok_or_else(|| anyhow!("tool_output_limit must be a number"))? as usize;
                crate::config::check_tool_output_limit(limit)?;
                self.settings.agent.tool_output_limit = limit;
                self.settings.save(&self.config.paths)?;
                self.config.tool_output_limit = limit;
            }
            other => return Err(anyhow!("`{other}` is not a writable key")),
        }
        Ok(self.config_state())
    }

    /// One of the terminal's text-answering commands, for `command/run`.
    fn run_command(&mut self, name: &str, argument: &str, events: &mpsc::UnboundedSender<AgentEvent>) -> Result<Value> {
        let text = match name {
            "memory" | "memories" => crate::commands::memory(&self.thread.state.memories, argument)?,
            "tools" | "skills" | "plugins" | "mcps" => crate::commands::extensions(&self.services),
            "repair" => {
                self.require_idle()?;
                let (said, repaired) = crate::commands::repair(&mut self.thread.messages);
                if repaired && self.thread.persisted {
                    self.persist();
                }
                said
            }
            "usage" => {
                // The open thread counts too, as of now.
                if self.thread.persisted {
                    self.persist();
                }
                let store = self.store.as_ref().ok_or_else(|| anyhow!("sessions off"))?;
                crate::commands::usage_report(
                    &store.usage()?,
                    &self.config.model_ref(),
                    chrono::Local::now().date_naive(),
                )
            }
            "goal" => {
                let reply = crate::commands::goal(&self.thread.state.goal, argument);
                if reply.paused {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                let said = reply.said.map_err(|error| anyhow!("goal error: {error:#}"))?;
                if self.thread.persisted {
                    self.persist();
                }
                self.report_work();
                if let Some((shown, prompt)) = reply.start {
                    // Mid-turn it steers, as anything sent then does.
                    if self.turn_id.is_some() {
                        self.thread.state.injections.push(crate::agent::Injection::UserMessage(prompt));
                    } else {
                        let content = self.user_content(&prompt, Vec::new());
                        self.start_turn(content, Some(shown), events);
                    }
                }
                return Ok(json!({"text": said, "turnId": self.turn_id}));
            }
            other => return Err(anyhow!("`/{other}` is not a command the app server runs")),
        };
        Ok(json!({"text": text}))
    }

    fn mode(&self) -> Permission {
        if self.yolo.load(Ordering::Relaxed) { Permission::Yolo } else { Permission::Auto }
    }

    fn set_mode(&mut self, mode: Permission) {
        self.yolo.store(mode == Permission::Yolo, Ordering::Relaxed);
        notify("thread/mode/updated", json!({"threadId": self.thread_id(), "mode": mode.label().to_ascii_lowercase()}));
    }

    /// What `config/value/write` and the profile methods answer with.
    fn config_state(&self) -> Value {
        json!({
            "profile": self.config.profile,
            "model": self.config.model_ref(),
            "mode": self.mode().label().to_ascii_lowercase(),
            "autoApprove": self.yolo.load(Ordering::Relaxed),
            "reasoningEffort": self.config.reasoning_effort.map(|effort| effort.label()),
            "contextWindow": self.config.model_limits.context_window,
            "compaction": self.config.compaction_marks(),
            "needsSetup": !self.config.is_configured(),
            "idleCompaction": self.config.idle_compaction,
            "maxSteps": self.config.max_steps,
            "toolOutputLimit": self.config.tool_output_limit,
        })
    }

    /// Make `name` the active profile, for this session and as the default.
    fn select_profile(&mut self, name: &str) -> Result<()> {
        if !self.settings.profiles.contains_key(name) {
            return Err(anyhow!("no profile named `{name}`"));
        }
        self.settings.default_profile = name.to_owned();
        self.settings.save(&self.config.paths)?;
        self.reapply_profile()
    }

    /// Point the session at the default profile as it now stands, after a
    /// change to it, its provider, or which profile is the default.
    fn reapply_profile(&mut self) -> Result<()> {
        let name = self.settings.default_profile.clone();
        if !self.settings.profiles.contains_key(&name) {
            return Ok(());
        }
        let previous = self.config.model_ref();
        self.config.use_profile(&self.settings, &self.credentials, &name)?;
        // What the old model spent so far is credited to it before the
        // ledger starts counting for the new one.
        if let Some(usage) = &self.usage
            && previous != self.config.model_ref()
        {
            usage.note_model(&self.config.model);
        }
        // Another model has none of this thread cached.
        if previous != self.config.model_ref() {
            self.stop_idle_compaction();
        }
        let mut provider = Provider::with_tokens(&self.config, self.tokens.clone())?;
        provider.adopt_session(&self.provider);
        self.provider = provider;
        notify("thread/profile/updated", self.config_state());
        Ok(())
    }

    fn provider_rows(&self) -> Vec<Value> {
        self.settings
            .providers
            .iter()
            .map(|(id, provider)| {
                let catalog = crate::providers::cached_catalog(&self.config.paths, id);
                json!({
                    "id": id,
                    "name": provider.display_name(id),
                    "baseUrl": provider.base_url,
                    "protocol": provider.protocol,
                    "apiKeyEnv": provider.api_key_env,
                    "key": provider.key_status(id, &self.credentials).label(),
                    "keyMissing": provider.key_status(id, &self.credentials).is_missing(),
                    "hasStoredKey": self.credentials.keys.get(id).is_some_and(|key| !key.is_empty()),
                    "authHeader": provider.auth_header,
                    "authFormat": provider.auth_format,
                    "tokenCommand": crate::providers::command_line(&provider.token_command),
                    "headers": provider.headers,
                    "body": crate::providers::body_line(&provider.body),
                    "remove": provider.remove,
                    "systemPrefix": provider.system_prefix,
                    "requestUrl": provider.request_url,
                    "modelsUrl": provider.models_url,
                    "models": provider.models,
                    "local": provider.is_local(),
                    "modelCount": catalog.as_ref().map(|catalog| catalog.models.len()),
                    "profiles": self.settings.profiles_on(id),
                })
            })
            .collect()
    }

    fn profile_rows(&self) -> Vec<Value> {
        self.settings
            .profiles
            .iter()
            .map(|(name, profile)| {
                json!({
                    "name": name,
                    "model": profile.model,
                    "provider": profile.provider(),
                    "active": *name == self.config.profile,
                    "contextWindow": profile.context_window,
                    "maxOutputTokens": profile.max_output_tokens,
                    "reasoningEffort": profile.reasoning_effort,
                    "toolFormat": profile.tool_format,
                    "roles": profile.roles,
                })
            })
            .collect()
    }

    /// Add or change a provider. Only the fields present change, so a front
    /// end can send a whole form or a single edit; `apiKey` goes to
    /// `credentials.toml`, never to the settings, and `""` removes it.
    fn save_provider(&mut self, params: &Value) -> Result<Value> {
        let preset = params["preset"].as_str().and_then(crate::providers::preset);
        let id = match params["id"].as_str().filter(|id| !id.is_empty()) {
            Some(id) => crate::providers::validate_provider_id(id)?.to_owned(),
            None => {
                let base = preset.map(|preset| preset.id.to_owned()).unwrap_or_else(|| {
                    Some(crate::providers::slug(params["name"].as_str().unwrap_or_default()))
                        .filter(|slug| !slug.is_empty())
                        .unwrap_or_else(|| "custom".to_owned())
                });
                crate::providers::unused_id(&base, |candidate| self.settings.providers.contains_key(candidate))
            }
        };
        let mut provider = self
            .settings
            .providers
            .get(&id)
            .cloned()
            .unwrap_or_else(|| preset.map(crate::providers::ProviderConfig::from_preset).unwrap_or_default());
        let text = |key: &str| {
            params
                .get(key)
                .filter(|value| !value.is_null())
                .map(|value| value.as_str().unwrap_or_default().trim().to_owned())
        };
        let optional = |value: String| Some(value).filter(|value| !value.is_empty());
        if let Some(name) = text("name") {
            provider.name = name;
        }
        if let Some(url) = text("baseUrl") {
            provider.base_url = crate::providers::normalise_base_url(&url)?;
        }
        if let Some(protocol) = params.get("protocol").filter(|value| !value.is_null()) {
            provider.protocol = serde_json::from_value(protocol.clone())
                .map_err(|_| anyhow!("protocol is chat-completions, responses, or anthropic"))?;
        }
        if let Some(env) = text("apiKeyEnv") {
            provider.api_key_env = optional(env);
        }
        if let Some(header) = text("authHeader") {
            provider.auth_header = optional(header);
        }
        if let Some(format) = text("authFormat") {
            provider.auth_format = optional(format);
        }
        if let Some(command) = text("tokenCommand") {
            provider.token_command = crate::providers::parse_command_line(&command);
        }
        if let Some(headers) = params.get("headers").filter(|value| !value.is_null()) {
            provider.headers = match headers {
                Value::String(line) => crate::providers::parse_headers_line(line)?,
                other => serde_json::from_value(other.clone()).map_err(|_| anyhow!("headers are name: value pairs"))?,
            };
        }
        if let Some(body) = text("body") {
            provider.body = crate::providers::parse_body_line(&body)?;
        }
        if let Some(remove) = params.get("remove").filter(|value| !value.is_null()) {
            provider.remove = string_list(remove);
        }
        if let Some(prefix) = text("systemPrefix") {
            provider.system_prefix = optional(prefix);
        }
        if let Some(url) = text("requestUrl") {
            provider.request_url = optional(url);
        }
        if let Some(url) = text("modelsUrl") {
            provider.models_url = optional(url);
        }
        if let Some(models) = params.get("models").filter(|value| !value.is_null()) {
            provider.models = string_list(models);
        }
        if provider.base_url.is_empty() {
            return Err(anyhow!("a provider needs a base URL"));
        }
        if let Some(key) = text("apiKey") {
            if key.is_empty() {
                self.credentials.keys.remove(&id);
            } else {
                self.credentials.keys.insert(id.clone(), key);
            }
            self.credentials.save(&self.config.paths)?;
        }
        self.settings.providers.insert(id.clone(), provider);
        self.settings.save(&self.config.paths)?;
        crate::providers::drop_catalog(&self.config.paths, &id);
        if self.settings.profiles.get(&self.config.profile).is_some_and(|profile| profile.provider() == id) {
            self.reapply_profile()?;
        }
        let row = self.provider_rows().into_iter().find(|row| row["id"] == id.as_str()).unwrap_or(Value::Null);
        Ok(json!({"id": id, "provider": row}))
    }

    /// Add or change a profile. A new one needs a `model` (`provider/model`)
    /// and is named after it unless a `name` is given; `previousName` renames.
    /// Limits and effort take `null` for "detect / provider default". The
    /// first profile there is, or one saved with `select`, becomes active.
    fn save_profile(&mut self, params: &Value) -> Result<Value> {
        let previous = params["previousName"].as_str().filter(|name| self.settings.profiles.contains_key(*name));
        let mut profile = previous.and_then(|name| self.settings.profiles.get(name).cloned()).unwrap_or_default();
        if let Some(model) = params["model"].as_str().filter(|model| !model.trim().is_empty()) {
            self.settings.target(model.trim(), &self.credentials)?;
            profile.model = model.trim().to_owned();
        }
        if profile.model.is_empty() {
            return Err(anyhow!("a profile needs a model, as provider/model"));
        }
        let tokens = |key: &str| -> Result<Option<Option<usize>>> {
            match params.get(key) {
                None => Ok(None),
                Some(Value::Null) => Ok(Some(None)),
                Some(Value::Number(number)) => Ok(Some(number.as_u64().map(|value| value as usize))),
                Some(Value::String(text)) if text.trim().is_empty() => Ok(Some(None)),
                Some(Value::String(text)) => Ok(Some(Some(crate::model_info::parse_tokens(text)?))),
                Some(_) => Err(anyhow!("{key} must be a number of tokens")),
            }
        };
        if let Some(context) = tokens("contextWindow")? {
            profile.context_window = context;
        }
        if let Some(output) = tokens("maxOutputTokens")? {
            profile.max_output_tokens = output;
        }
        match params.get("reasoningEffort") {
            Some(Value::Null) => profile.reasoning_effort = None,
            Some(Value::String(text)) if text.is_empty() || text == "auto" => profile.reasoning_effort = None,
            Some(Value::String(text)) => {
                profile.reasoning_effort = Some(
                    crate::config::ReasoningEffort::parse(text).ok_or_else(|| anyhow!("unknown effort `{text}`"))?,
                )
            }
            _ => {}
        }
        if let Some(format) = params.get("toolFormat") {
            profile.tool_format = format.as_str().map(str::to_owned).filter(|format| !format.is_empty());
        }
        if let Some(roles) = params.get("roles").and_then(Value::as_object) {
            for (role, model) in roles {
                if crate::roles::role(role).is_none() || role == "default" {
                    return Err(anyhow!("unknown role `{role}`"));
                }
                let model = model.as_str().map(str::trim).filter(|model| !model.is_empty()).map(str::to_owned);
                if let Some(model) = &model {
                    self.settings.target(model, &self.credentials)?;
                }
                profile.set_role_model(role, model);
            }
        }
        let name = match params["name"].as_str().filter(|name| !name.trim().is_empty()) {
            Some(name) => crate::config::validate_profile_name(name)?.to_owned(),
            None => previous.map(str::to_owned).unwrap_or_else(|| self.settings.unused_profile_name(&profile.model)),
        };
        if let Some(previous) = previous
            && previous != name
        {
            self.settings.rename_profile(previous, &name)?;
        } else if previous.is_none() && self.settings.profiles.contains_key(&name) {
            return Err(anyhow!("a profile named `{name}` already exists"));
        }
        self.settings.profiles.insert(name.clone(), profile);
        let select = params["select"].as_bool().unwrap_or(false) || !self.config.is_configured();
        if select {
            self.settings.default_profile = name.clone();
        }
        self.settings.save(&self.config.paths)?;
        if select || self.settings.default_profile == name {
            self.reapply_profile()?;
        }
        Ok(json!({"name": name, "profiles": self.profile_rows(), "state": self.config_state()}))
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

    /// How full the context window is, as the terminal's meter reckons it:
    /// the provider's count of the last prompt when it reported one, else an
    /// estimate from the messages, and the larger of the two, since the
    /// messages can have grown since that prompt was sent. `tokensUsed` is a
    /// different quantity, every token the session has spent, and outgrows
    /// the window on any long session.
    fn context_tokens(&self) -> u64 {
        let estimated = (crate::agent::message_chars(&self.thread.messages) / 4) as u64;
        match self.provider.context_tokens() {
            // Nothing reported yet (a reopened thread): the tools offered
            // are sent with every request too.
            0 => {
                estimated
                    + self
                        .context_parts(0)
                        .iter()
                        .filter(|part| part.count.is_some())
                        .map(|part| part.tokens)
                        .sum::<u64>()
            }
            reported => reported.max(estimated),
        }
    }

    /// What fills the context, by kind, adding up to `total` (0: as estimated).
    fn context_parts(&self, total: u64) -> Vec<crate::context_use::Part> {
        let options = self.turn_options();
        let tools = crate::agent::turn_tool_specs(&options);
        let mcp = options.services.mcp.tools().map(|tool| tool.exposed_name.clone()).collect();
        let app = options.services.client_tools.iter().flat_map(|client| client.names().cloned()).collect();
        crate::context_use::breakdown(&self.thread.messages, &tools, &mcp, &app, total)
    }

    fn report_usage(&mut self) {
        self.reported_context = self.provider.context_tokens();
        notify(
            "thread/tokenUsage/updated",
            json!({
                "threadId": self.thread_id(),
                "tokensUsed": self.tokens.total(),
                "contextTokens": self.context_tokens(),
                "contextWindow": self.config.model_limits.context_window,
                // Where compaction steps in, for the meter to mark.
                "compaction": self.config.compaction_marks(),
                // The split behind the total: input and output are priced
                // differently, and cached input an order of magnitude below
                // uncached, so a client that only renders `tokensUsed` cannot
                // tell an expensive session from a cheap one.
                "usage": token_usage(&self.tokens.snapshot()),
            }),
        );
    }

    fn thread_snapshot(&self) -> Value {
        json!({
            "threadId": self.thread_id(),
            "name": self.thread.session.title,
            "model": self.config.model_ref(),
            "profile": self.config.profile,
            "mode": self.mode().label().to_ascii_lowercase(),
            "autoApprove": self.yolo.load(Ordering::Relaxed),
            "reasoningEffort": self.config.reasoning_effort.map(|effort| effort.label()),
            "workspace": self.config.workspace,
            "contextWindow": self.config.model_limits.context_window,
            "contextTokens": self.context_tokens(),
            "compaction": self.config.compaction_marks(),
            "tokensUsed": self.tokens.total(),
            "usage": token_usage(&self.tokens.snapshot()),
            "goal": goal_item(self.thread.state.goal.snapshot()),
            "taskList": self.thread.state.tasks.snapshot(),
            "items": history_items(&self.thread.messages),
        })
    }

    /// Tell the front end what changed in the work the thread is pinned to:
    /// the goal (the model updated it, a turn counted against it, a command
    /// set it) and the task list. Each once per change.
    fn report_work(&mut self) {
        let thread = self.thread_id();
        let goal = json!({"threadId": thread, "goal": goal_item(self.thread.state.goal.snapshot())});
        if goal != self.reported_goal {
            notify("thread/goal/updated", goal.clone());
            self.reported_goal = goal;
        }
        let tasks = json!({"threadId": thread, "tasks": self.thread.state.tasks.snapshot()});
        if tasks != self.reported_tasks {
            notify("thread/taskList/updated", tasks.clone());
            self.reported_tasks = tasks;
        }
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

    /// What a leading `/name` means here, matching the TUI: a skill or plugin
    /// command expands to its prompt, and `/btw` is a side note for the
    /// running turn. Anything else, a path included, is just text.
    fn slash(&self, text: &str) -> Result<Slash> {
        let Some(rest) = text.trim_start().strip_prefix('/') else {
            return Ok(Slash::None);
        };
        let (name, argument) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let argument = argument.trim();
        if name == "btw" {
            if argument.is_empty() {
                return Err(anyhow!("usage: /btw <side question or remark>"));
            }
            if self.turn_id.is_none() {
                return Err(anyhow!("/btw is for while a turn is running — ask it directly instead"));
            }
            return Ok(Slash::SideNote(argument.to_owned()));
        }
        if let Ok(skills) = self.services.skills.read()
            && skills.get(name).is_some()
        {
            return Ok(Slash::Prompt(skills.invocation(name, argument)?));
        }
        if let Some(command) = self.services.plugins.command(name) {
            return Ok(Slash::Prompt(command.prompt.replace("{{args}}", argument)));
        }
        Ok(Slash::None)
    }

    /// `shown` is what the person typed when it differs from what the model
    /// gets (a slash command's expansion), so the transcript shows the former.
    fn start_turn(&mut self, content: Value, shown: Option<String>, events: &mpsc::UnboundedSender<AgentEvent>) {
        let mut item = user_item(&content);
        if let Some(shown) = shown {
            item["text"] = json!(shown);
        }
        self.begin_turn(content, item, events);
    }

    /// Start the agent on `content`, announcing `item` as the person's message.
    fn begin_turn(&mut self, content: Value, mut item: Value, events: &mpsc::UnboundedSender<AgentEvent>) {
        self.stop_idle_compaction();
        self.thread.messages.push(json!({"role": "user", "content": content}));
        // A new thread is saved as its first turn starts, as the terminal
        // saves a session: other sessions can find it and message it while
        // that turn runs, and what it sends them says who it is from.
        if !self.thread.persisted {
            self.persist();
        }
        self.mailbox.set_working(true);
        self.turn_worked = false;
        self.goal_turn = None;
        let turn = self.next("turn");
        self.turn_id = Some(turn.clone());
        self.cancel = Arc::new(AtomicBool::new(false));
        self.journal = crate::checkpoint::Journal::recording(self.config.workspace.clone());

        notify("turn/started", json!({"threadId": self.thread_id(), "turnId": turn}));
        // The user's own message is an item too, so a client that replays
        // `item/*` alone reconstructs the whole transcript.
        item["id"] = json!(self.next("item"));
        self.item("completed", item);

        let options = self.turn_options();
        tokio::spawn(run_turn(self.provider.clone(), self.thread.messages.clone(), options, events.clone()));
    }

    /// The options the next turn runs with. Idle compaction uses them too, so
    /// its summary request carries the same tools as the turns before it.
    fn turn_options(&self) -> TurnOptions {
        TurnOptions {
            yolo: self.yolo.clone(),
            session_id: Some(self.thread_id()),
            trace: self.thread.trace.clone(),
            cancel: self.cancel.clone(),
            journal: self.journal.clone(),
            workers: self.workers.clone(),
            peers: match &self.store {
                Some(_) => crate::peers::Peers::new(
                    self.config.paths.clone(),
                    self.config.workspace.clone(),
                    Some(self.thread.session.id),
                ),
                None => Default::default(),
            },
            ..self.thread.state.turn(&self.config, self.services.clone())
        }
    }

    /// Summarise a long thread left idle since its last turn, while the
    /// provider still has it cached (see [`crate::compaction::IDLE_AFTER`]).
    fn start_idle_compaction(&mut self) {
        self.idle_due = None;
        if self.turn_id.is_some() || self.idle_compaction.is_some() || !self.config.compacts_when_idle() {
            return;
        }
        let options = self.turn_options();
        if !crate::compaction::idle_worthwhile(&self.thread.messages, &options.compaction, &options.compaction_budget) {
            return;
        }
        let job = Arc::new(AtomicBool::new(false));
        let options = TurnOptions { cancel: job.clone(), ..options };
        self.idle_compaction = Some(job.clone());
        let (provider, basis, done) = (self.provider.clone(), self.thread.messages.clone(), self.idle_done.clone());
        tokio::spawn(async move {
            let result = crate::agent::compact_idle(&provider, &basis, &options).await;
            let _ = done.send(IdleDone { job, basis, result });
        });
    }

    /// Whether stopping now would cut nothing short: no turn, no summary being
    /// written or due, no answer awaited, no background worker running, and
    /// nothing waiting to be delivered.
    fn quiet(&self) -> bool {
        self.turn_id.is_none()
            && self.idle_due.is_none()
            && self.idle_compaction.is_none()
            && self.pending_approvals.is_empty()
            && self.pending_questions.is_empty()
            && self.pending_client_calls.is_empty()
            && self.thread.state.injections.is_empty()
            && !self.workers.board.all().iter().any(|worker| worker.state == crate::workers::WorkerState::Running)
    }

    /// When the server stops by itself, if it is set to and nothing is going on.
    fn idle_exit_due(&self) -> Option<tokio::time::Instant> {
        self.idle_exit.filter(|_| self.quiet()).map(|after| self.last_activity + after)
    }

    /// Start the quiet spell idle compaction waits for over again: the person
    /// is writing. One already writing its summary goes on; sending the
    /// message stops it.
    fn hold_idle_compaction(&mut self) {
        if self.idle_due.is_some() {
            self.idle_due = Some(tokio::time::Instant::now() + crate::compaction::IDLE_AFTER);
        }
    }

    /// Stop idle compaction: a turn is starting, or the thread it was
    /// summarising is being replaced.
    fn stop_idle_compaction(&mut self) {
        if let Some(job) = self.idle_compaction.take() {
            job.store(true, Ordering::Relaxed);
        }
        self.idle_due = None;
    }

    /// Take idle compaction's result, if it is still the summary of the
    /// thread as it stands.
    fn idle_compacted(&mut self, done: IdleDone) {
        // A stopped job reports too. It is not the one in flight, if any is.
        if !self.idle_compaction.as_ref().is_some_and(|current| Arc::ptr_eq(current, &done.job)) {
            return;
        }
        self.idle_compaction = None;
        // `thread/compact` since then makes it the summary of something else.
        let Some(compacted) = done.result.filter(|_| self.turn_id.is_none() && self.thread.messages == done.basis)
        else {
            return;
        };
        self.thread.messages = compacted.messages;
        self.thread.state.compaction.set(compacted.summary);
        self.provider.clear_context_tokens();
        self.persist();
        notify(
            "thread/compacted",
            json!({"threadId": self.thread_id(), "reason": "idle", "savedTokens": compacted.saved_tokens}),
        );
        self.report_usage();
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
            let open = OpenItem { id, kind, summary: String::new(), description: None, message: None };
            self.open_items.insert(kind.to_owned(), open);
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
        let open = OpenItem {
            id: self.next("item"),
            kind: item_type(name),
            summary: summary.to_owned(),
            description: self.descriptions.remove(name),
            message: None,
        };
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
                self.turn_worked = true;
                // It also means a reply just came back, with the size of the
                // prompt it answered: the context meter moves during a long
                // turn instead of only at its end.
                if self.provider.context_tokens() != self.reported_context {
                    self.report_usage();
                }
                // An approval for this tool already opened the item, so that
                // the prompt could name the call it was about. Reuse it rather
                // than announcing the same call twice.
                match self.open_items.get_mut(&name) {
                    Some(open) => open.summary = summary,
                    None => drop(self.open_tool(&name, &summary, "inProgress")),
                }
            }
            AgentEvent::ToolFinished { name, output, refused } => {
                if let Some(open) = self.open_items.remove(&name) {
                    // A call that was stopped or declined must not read as run.
                    let mut item = open.tool(&name, if refused { "declined" } else { "completed" });
                    item["output"] = json!(output);
                    self.item("completed", item);
                }
                if matches!(name.as_str(), "goal_update" | "task_create" | "task_update") {
                    self.report_work();
                }
            }
            AgentEvent::Notice(text) => match text.strip_prefix(crate::agent::STEERING_NOTICE) {
                // The model read a steering message: it joins the transcript
                // here, where it landed.
                Some(steer) => {
                    // What the model said before reading it is finished; what
                    // it says next is a reply of its own, below the message.
                    self.flush_streaming();
                    let item = json!({"id": self.next("item"), "type": "userMessage", "text": steer});
                    self.item("completed", item);
                    self.steer_delivered(steer);
                }
                None => notify("warning", json!({"message": text})),
            },
            // Another session's message reached the model mid-turn: it joins
            // the transcript where it landed, like steering.
            AgentEvent::PeerMessage(message) => {
                self.flush_streaming();
                let mut item = peer_item(&message.from.title, message.from.session, &message.text);
                item["id"] = json!(self.next("item"));
                self.item("completed", item);
            }
            // So does a subagent's message, or their report.
            AgentEvent::AgentMessage { from, text } => {
                self.flush_streaming();
                let mut item = agent_item(&from, &text);
                item["id"] = json!(self.next("item"));
                self.item("completed", item);
            }
            AgentEvent::SubagentReport(report) => {
                self.flush_streaming();
                let mut item = report_item(&report);
                item["id"] = json!(self.next("item"));
                self.item("completed", item);
            }
            AgentEvent::ToolDescribed { name, description } => {
                self.descriptions.insert(name, description);
            }
            AgentEvent::MessageSent { text, .. } => {
                if let Some(open) = self.open_items.get_mut("send_message") {
                    open.message = Some(text);
                }
            }
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
                let description = self.open_items.get(&request.tool).and_then(|open| open.description.clone());
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
                        "reason": request.reason,
                        "details": request.details,
                        "description": description,
                        "availableDecisions": ["accept", "decline", "cancel"],
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
        self.report_edits();
        let turn = self.turn_id.take().unwrap_or_default();
        self.mailbox.set_working(false);
        // The provider has the conversation cached now; a failed turn may
        // never have reached it.
        if status != "failed" {
            self.idle_due = Some(tokio::time::Instant::now() + crate::compaction::IDLE_AFTER);
        }
        // A turn that ended by itself carries an active goal on, unless
        // something waiting starts the next turn anyway: the goal carries on
        // once that one ends. A turn the person stopped, or that failed, ends
        // the run of turns.
        if matches!(status, "completed" | "stepLimit") && !self.thread.state.injections.has_starter() {
            match self.thread.state.goal.after_turn(self.turn_worked) {
                Some(crate::goal::Next::Turn(prompt)) => self.goal_turn = Some(prompt),
                Some(crate::goal::Next::Stalled) => notify(
                    "warning",
                    json!({"message": "Goal paused: two turns in a row ended without doing any work."}),
                ),
                None => {}
            }
        }
        self.persist();
        self.report_diff(&turn);
        if let Some(usage) = &self.usage {
            usage.spawn_report(false);
        }
        self.report_usage();
        // The turn counted against the goal.
        self.report_work();
        notify(
            "turn/completed",
            json!({
                "threadId": self.thread_id(),
                "turnId": turn,
                "status": status,
                "name": self.thread.session.title,
                // A steering message this turn did not reach, a background
                // report, a message or an unfinished goal starts the next one
                // at once; a client holding messages for later waits.
                "continues": self.thread.state.injections.has_starter() || self.goal_turn.is_some(),
            }),
        );
    }

    /// What reached an idle thread was meant for now, not for whenever the
    /// person next types, so it starts a turn, as in the terminal: a steering
    /// message the last turn ended before reaching, a background subagent's
    /// report, a message one sent to `main`, or one from another session.
    fn start_pending_turn(&mut self, events: &mpsc::UnboundedSender<AgentEvent>) {
        use crate::agent::Injection;
        if self.turn_id.is_some() {
            return;
        }
        let (content, steer) = match self.thread.state.injections.take_starter() {
            Some(Injection::UserMessage(text)) => (Value::String(text.clone()), Some(text)),
            Some(Injection::UserContent { text, content }) => (content, Some(text)),
            // A subagent's report or message opens the turn, shown as what it
            // is rather than as the prompt that carries it.
            Some(Injection::SubagentReport(report)) => {
                let content = Value::String(crate::agent::report_prompt(&report));
                self.begin_turn(content, report_item(&report), events);
                return;
            }
            Some(Injection::AgentMessage { from, text }) => {
                let content = Value::String(crate::agent::agent_message(&from, &text));
                self.begin_turn(content, agent_item(&from, &text), events);
                return;
            }
            // Another session wrote while nothing was running: its message is
            // the next turn, shown with who sent it.
            Some(Injection::PeerMessage(message)) => {
                let item = peer_item(&message.from.title, message.from.session, &message.text);
                self.begin_turn(Value::String(message.prompt(&self.config.workspace)), item, events);
                return;
            }
            Some(Injection::SideNote(_) | Injection::ToolImages { .. }) | None => {
                // Nothing else waits: an active goal goes on.
                if let Some(prompt) = self.goal_turn.take()
                    && self.thread.state.goal.is_active()
                {
                    self.begin_turn(Value::String(prompt), carry_on_item(), events);
                }
                return;
            }
        };
        let item = user_item(&content);
        self.begin_turn(content, item, events);
        if let Some(text) = steer {
            self.steer_delivered(&text);
        }
    }

    /// Hand what other sessions sent to the agent, as `start_pending_turn`
    /// and the running turn take it.
    fn check_inbox(&mut self) {
        match self.mailbox.collect(self.turn_id.is_none()) {
            crate::inbox::Collected::Nothing => {}
            crate::inbox::Collected::Messages(messages) => {
                for message in messages {
                    self.thread.state.injections.push(crate::agent::Injection::PeerMessage(message));
                }
            }
            crate::inbox::Collected::Held(count) => {
                notify("warning", json!({"message": crate::inbox::held_notice(count)}))
            }
        }
    }

    /// Tell the client a steering message reached the model, by the text
    /// `turn/start` answered with when it was sent.
    fn steer_delivered(&self, text: &str) {
        notify("turn/steer/delivered", json!({"threadId": self.thread_id(), "text": text}));
    }

    /// Where this thread's checkpoints are kept: what each turn's file tools
    /// changed, both sides.
    fn checkpoints(&self) -> std::path::PathBuf {
        self.config.paths.root.join("checkpoints").join(self.thread_id())
    }

    /// Show what the turn changed through the file tools, under it, and keep
    /// that with the conversation so a reopened thread shows it too.
    fn report_edits(&mut self) {
        match std::mem::take(&mut self.journal).finish(&self.checkpoints()) {
            Ok(Some(edits)) => {
                let id = self.next("item");
                self.item("completed", edits.item(&id));
                self.thread.messages.push(edits.marker());
            }
            Ok(None) => {}
            Err(error) => eprintln!("app-server: could not keep the turn's edits — {error:#}"),
        }
    }

    /// Put back what the turn under `checkpoint` changed, and tell the model.
    fn undo_edits(&mut self, checkpoint: &str) -> Result<Value> {
        self.require_idle()?;
        let files = crate::checkpoint::undo(&self.checkpoints(), checkpoint, &self.config.workspace)?;
        for message in &mut self.thread.messages {
            if let Some(mut edits) = crate::checkpoint::TurnEdits::marked_by(message)
                && edits.checkpoint == checkpoint
            {
                edits.undone = true;
                *message = edits.marker();
            }
        }
        self.thread.messages.push(json!({"role": "user", "content": crate::checkpoint::undo_note(&files)}));
        self.persist();
        self.report_diff("");
        notify("turn/undone", json!({"threadId": self.thread_id(), "checkpoint": checkpoint, "files": files}));
        Ok(json!({"undone": true, "files": files}))
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
    /// Send a front-end tool call out as a `tool/call` request.
    fn forward_client_call(&mut self, call: crate::client_tools::ClientCall) {
        let id = self.next("srv");
        emit(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tool/call",
            "params": {
                "threadId": self.thread_id(),
                "turnId": self.turn_id(),
                "name": call.name,
                "arguments": call.arguments,
            }
        }));
        self.pending_client_calls.insert(id, (call.name, call.respond));
    }

    fn resolve(&mut self, id: &Value, result: &Value, error: Option<&Value>) {
        let Some(id) = id.as_str() else { return };
        if let Some((name, responder)) = self.pending_client_calls.remove(id) {
            let mut answer = crate::client_tools::read_result(result, error);
            // Tool results are text; a screenshot reaches the model as an image
            // in the message that follows the call, which the transcript
            // leaves out: nobody sent it.
            let images = std::mem::take(&mut answer.images);
            if !images.is_empty() {
                self.thread.state.injections.push(crate::agent::Injection::ToolImages { tool: name.clone(), images });
            }
            let _ = responder.send(answer);
            return;
        }
        if let Some(responder) = self.pending_approvals.remove(id) {
            // `acceptForSession` came from the days of asking about every
            // change; a window that still sends it means this one.
            let decision = match result["decision"].as_str().unwrap_or("decline") {
                "accept" | "acceptForSession" => ApprovalDecision::Once,
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
        session.model = self.config.model_ref();
        if let Err(error) = store.save(session) {
            eprintln!("app-server: session save failed — {error:#}");
            return;
        }
        self.thread.persisted = true;
        self.follow_thread();
        self.sync_after_turn();
    }

    /// Point the presence record and the inbox at the open thread, once it is
    /// saved: other sessions can only find a thread that is.
    fn follow_thread(&mut self) {
        let id = self.thread.persisted.then_some(self.thread.session.id);
        self.mailbox.open(id, &self.config.workspace);
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
    fn thread_opened(&mut self) {
        self.follow_thread();
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
            "agent.mode": ["auto", "yolo"],
            "ui.glyphs": ["auto", "unicode", "nerd", "ascii"],
            "profiles.*.tool_format": [
                null, "auto", "none", "hermes", "qwen", "llama3_json",
                "mistral", "glm", "kimi", "deepseek", "json",
            ],
            "providers.*.protocol": ["chat-completions", "responses", "anthropic"],
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

/// A list of strings sent either as an array or as one comma-separated line.
fn string_list(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect(),
        Value::String(line) => {
            line.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_owned).collect()
        }
        _ => Vec::new(),
    }
}

/// A discovered model as a front end lists it.
fn model_card(provider: &str, card: &crate::model_info::ModelCard) -> Value {
    json!({
        "id": crate::providers::join_ref(provider, &card.id),
        "model": card.id,
        "name": card.name,
        "contextLength": card.context_length,
        "maxOutputTokens": card.max_output_tokens,
        "inputCost": card.input_cost,
        "outputCost": card.output_cost,
        "vision": card.vision,
        "reasoning": card.reasoning,
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

/// The thread's goal as a client shows it, or null when none is set.
fn goal_item(goal: Option<crate::goal::Goal>) -> Value {
    goal.map_or(Value::Null, |goal| {
        json!({
            "objective": goal.objective,
            "status": goal.status,
            // Turns run toward it.
            "iterations": goal.iterations,
            // The model's latest word on it.
            "note": goal.note,
        })
    })
}

/// A background worker as a client lists it.
fn task_summary(worker: &crate::workers::WorkerStatus) -> Value {
    json!({
        "id": worker.id,
        "name": worker.name,
        "role": worker.role,
        "model": worker.model,
        "prompt": crate::text::clip(&worker.prompt, 400, "…"),
        "state": worker.state.label(),
        "activity": worker.activity,
        "tool": worker.tool,
        "toolUses": worker.tool_uses,
        "tokens": worker.tokens_used(),
        "startedAt": worker.started_at.to_rfc3339(),
        "finishedAt": worker.finished_at.map(|at| at.to_rfc3339()),
    })
}

fn task_summaries(board: &crate::workers::WorkerBoard) -> Vec<Value> {
    board.all().iter().map(task_summary).collect()
}

/// Tell the front end when background workers change: state, activity, and
/// (while any run) their token counts. Twice a second at most, and silent
/// while nothing moves.
fn watch_tasks(board: crate::workers::WorkerBoard) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = (board.version(), 0_u64);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            tick.tick().await;
            let workers = board.all();
            let running_tokens = workers
                .iter()
                .filter(|worker| worker.state == crate::workers::WorkerState::Running)
                .map(crate::workers::WorkerStatus::tokens_used)
                .sum::<u64>();
            let now = (board.version(), running_tokens);
            if now != last {
                last = now;
                notify("tasks/updated", json!({"tasks": workers.iter().map(task_summary).collect::<Vec<_>>()}));
            }
        }
    })
}

/// What a leading slash in a prompt turned out to be.
enum Slash {
    Prompt(String),
    SideNote(String),
    None,
}

/// What a client sent as a turn: its text, its images as data URLs, and the
/// files it attached as (label, contents).
#[derive(Debug, PartialEq)]
struct TurnInput {
    text: String,
    images: Vec<String>,
    files: Vec<(String, String)>,
}

fn turn_input(params: &Value, workspace: &std::path::Path) -> Result<TurnInput> {
    // Accept the structured `input: [{type:"text",text}]` form and a bare
    // `text`, so a hand-written client is not forced through the array form.
    // Images come as `{type:"image", url}` (a data URL) or `{type:"localImage",
    // path}` (a file the client's user picked; read here). Other files come as
    // `{type:"file", name, text}` or `{type:"localFile", path}`; an image sent
    // as a localFile is taken as an image.
    let mut input = TurnInput { text: String::new(), images: Vec::new(), files: Vec::new() };
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
                "file" => {
                    let name = item["name"].as_str().ok_or_else(|| anyhow!("a file input needs a name"))?;
                    let text = item["text"].as_str().ok_or_else(|| anyhow!("a file input needs its text"))?;
                    crate::context::check_text_attachment(name, text)?;
                    input.files.push((name.to_owned(), text.to_owned()));
                }
                "localFile" => {
                    let path = std::path::Path::new(
                        item["path"].as_str().ok_or_else(|| anyhow!("a localFile input needs a path"))?,
                    );
                    if crate::context::is_image_path(path) {
                        input.images.push(crate::context::image_data_url(path)?);
                    } else {
                        // Named by its place in the workspace when it has one,
                        // so the model can open or edit it by that path.
                        let label = path.strip_prefix(workspace).unwrap_or(path).display().to_string();
                        input.files.push((label, crate::context::read_text_attachment(path)?));
                    }
                }
                other => return Err(anyhow!("unsupported input type `{other}`")),
            }
        }
        input.text = texts.join("\n");
    } else {
        return Err(anyhow!("turn input required"));
    }
    if input.text.trim().is_empty() && input.images.is_empty() && input.files.is_empty() {
        return Err(anyhow!("turn input is empty"));
    }
    let total: usize = input.files.iter().map(|(_, text)| text.len()).sum();
    if total > crate::context::MAX_ATTACHED_TOTAL {
        return Err(anyhow!(
            "the attached files are larger than {} KB together",
            crate::context::MAX_ATTACHED_TOTAL / 1000
        ));
    }
    Ok(input)
}

/// Attached files ride after the prompt in the same `<attached_file>` blocks
/// `@file` references use.
fn with_files(mut text: String, files: Vec<(String, String)>) -> String {
    for (label, content) in files {
        text.push_str(&format!("\n\n<attached_file path=\"{label}\">\n{content}\n</attached_file>"));
    }
    text
}

/// A user message as a client item: what the person wrote, with the files and
/// images that went along listed apart instead of inlined.
fn user_item(content: &Value) -> Value {
    let text = crate::text::content_text(content);
    const MARK: &str = "\n\n<attached_file path=\"";
    let (said, attached) = text.split_once(MARK).map_or((text.as_ref(), ""), |(said, rest)| (said, rest));
    let files: Vec<&str> = if attached.is_empty() {
        Vec::new()
    } else {
        // The first block's path starts `attached`; each later one follows a mark.
        std::iter::once(attached)
            .chain(attached.split(MARK).skip(1))
            .filter_map(|block| block.split_once('"').map(|(path, _)| path))
            .collect()
    };
    let images = crate::text::content_images(content);
    let mut item = json!({"type": "userMessage", "text": said});
    if !images.is_empty() {
        item["images"] = json!(images);
    }
    if !files.is_empty() {
        item["files"] = json!(files);
    }
    item
}

/// Another session's message as a transcript item: a `userMessage`, which is
/// what a client that predates them shows, saying who it is from.
fn peer_item(title: &str, session: Option<uuid::Uuid>, text: &str) -> Value {
    json!({
        "type": "userMessage",
        "text": text,
        "from": {"title": title, "session": session.map(|id| id.to_string())},
    })
}

/// Another agent's message as a client item: a `userMessage` `from` that
/// agent, which a client that predates them shows like another session's.
pub(crate) fn agent_item(from: &str, text: &str) -> Value {
    json!({"type": "userMessage", "text": text, "from": {"title": from, "agent": true}})
}

/// What a report may be about, as the terminal's /feedback offers them.
const FEEDBACK_CATEGORIES: [&str; 4] = ["general", "bug", "feature", "performance"];

/// The turn an unfinished goal started by itself, as a client item: a
/// `userMessage` from the goal, which a client that predates it shows like
/// another agent's message.
fn carry_on_item() -> Value {
    json!({
        "type": "userMessage",
        "text": "The goal is not done yet, so the agent carries on.",
        "goal": true,
        "from": {"title": "Goal", "agent": true},
    })
}

/// Background subagents' report as a client item, from the workers it covers.
fn report_item(report: &str) -> Value {
    let workers = crate::subagent::report_workers(report);
    let title = if workers.is_empty() { "background subagents".to_owned() } else { workers.join(", ") };
    json!({"type": "userMessage", "text": report, "report": true, "from": {"title": title, "agent": true}})
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
    // What each call was for, and what each `send_message` said, as the live
    // items carry them.
    let descriptions: HashMap<&str, String> = messages
        .iter()
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        .filter_map(|call| {
            let arguments = call["function"]["arguments"].as_str().unwrap_or_default();
            let parsed = ToolCall { id: String::new(), name: String::new(), arguments: arguments.to_owned() };
            Some((call["id"].as_str()?, parsed.description()?))
        })
        .collect();
    let sent: HashMap<&str, String> = messages
        .iter()
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        .filter(|call| call["function"]["name"] == "send_message")
        .filter_map(|call| {
            let arguments = call["function"]["arguments"].as_str()?;
            let arguments = serde_json::from_str::<Value>(arguments).ok()?;
            Some((call["id"].as_str()?, arguments["message"].as_str()?.to_owned()))
        })
        .collect();

    let mut items = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let id = format!("history_{index}");
        let role = message["role"].as_str().unwrap_or_default();
        let content = message["content"].as_str().unwrap_or_default();
        match role {
            // Where compaction folded the middle away, which the history no
            // longer holds. Every other system message is scaffolding, not
            // conversation.
            "system" => {
                if let Some(edits) = crate::checkpoint::TurnEdits::marked_by(message) {
                    items.push(edits.item(&id));
                }
                if let Some(folded) = crate::compaction::Folded::marked_by(message) {
                    items.push(json!({
                        "id": id,
                        "type": "contextCompaction",
                        "messages": folded.messages,
                        "tokens": folded.tokens,
                        "summarised": folded.summarised,
                    }));
                }
            }
            "user" => {
                let said = crate::text::content_text(&message["content"]);
                // A screenshot a tool showed the model, or what it was told
                // of an undo, which its turn's edits show: nobody said it.
                if crate::agent::is_tool_images(&said) || crate::checkpoint::is_undo_note(&said) {
                    continue;
                }
                let mut item = if crate::goal::is_carry_on(&said) {
                    carry_on_item()
                } else if let Some((title, text)) = crate::inbox::shown(&said) {
                    peer_item(&title, None, &text)
                } else {
                    match crate::agent::arrival(&said) {
                        Some(crate::agent::Arrival::Report(report)) => report_item(&report),
                        Some(crate::agent::Arrival::Message { from, text }) => agent_item(&from, &text),
                        None => user_item(&message["content"]),
                    }
                };
                let empty = item["text"].as_str().is_none_or(|text| text.trim().is_empty())
                    && item.get("images").is_none()
                    && item.get("files").is_none();
                if !empty {
                    item["id"] = json!(id);
                    items.push(item);
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
                let status = if crate::agent::tool_result_refused(content) { "declined" } else { "completed" };
                let mut item = json!({
                    "id": id,
                    "type": item_type(name),
                    "name": name,
                    "status": status,
                    "output": content,
                });
                if let Some(summary) = message["tool_call_id"].as_str().and_then(|call| summaries.get(call)) {
                    item["command"] = json!(summary);
                }
                if let Some(description) = message["tool_call_id"].as_str().and_then(|call| descriptions.get(call)) {
                    item["description"] = json!(description);
                }
                if let Some(said) = message["tool_call_id"].as_str().and_then(|call| sent.get(call)) {
                    item["message"] = json!(said);
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
            Incoming::Response { id, result, .. } => {
                assert_eq!(id, json!("srv_1"));
                assert_eq!(result["decision"], "accept");
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    fn turn_input_in_tests(params: &Value) -> Result<TurnInput> {
        turn_input(params, std::path::Path::new("/workspace"))
    }

    #[test]
    fn turn_input_takes_files_by_text_and_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes.md");
        std::fs::write(&notes, "# notes").unwrap();
        let shot = dir.path().join("shot.png");
        std::fs::write(&shot, b"png bytes").unwrap();
        let binary = dir.path().join("blob.bin");
        std::fs::write(&binary, [0_u8, 159, 146, 150]).unwrap();

        let input = turn(
            dir.path(),
            json!({"input": [
                {"type": "text", "text": "review these"},
                {"type": "file", "name": "pasted.txt", "text": "hello"},
                {"type": "localFile", "path": notes},
                {"type": "localFile", "path": shot},
            ]}),
        )
        .unwrap();
        // A file inside the workspace is named by its place in it.
        assert_eq!(
            input.files,
            [("pasted.txt".to_owned(), "hello".to_owned()), ("notes.md".to_owned(), "# notes".to_owned())]
        );
        assert_eq!(input.images.len(), 1, "an image sent as a file is an image");

        assert!(turn(dir.path(), json!({"input": [{"type": "localFile", "path": binary}]})).is_err());
        assert!(
            turn(dir.path(), json!({"input": [{"type": "file", "name": "big.txt", "text": "x".repeat(300_000)}]}))
                .is_err()
        );
        // Files alone are a prompt.
        assert!(turn(dir.path(), json!({"input": [{"type": "file", "name": "a.txt", "text": "a"}]})).is_ok());
    }

    fn turn(workspace: &std::path::Path, params: Value) -> Result<TurnInput> {
        turn_input(&params, workspace)
    }

    #[test]
    fn user_items_list_attached_files_apart_from_what_was_said() {
        let text = with_files(
            "compare these".to_owned(),
            vec![("a.rs".to_owned(), "fn a() {}".to_owned()), ("docs/b.md".to_owned(), "# b".to_owned())],
        );
        let item = user_item(&json!(text));
        assert_eq!(item["text"], "compare these");
        assert_eq!(item["files"], json!(["a.rs", "docs/b.md"]));
        assert!(item.get("images").is_none());

        let plain = user_item(&json!("just words"));
        assert_eq!(plain["text"], "just words");
        assert!(plain.get("files").is_none());
    }

    #[test]
    fn turn_input_accepts_both_shapes() {
        let hello = TurnInput { text: "hello".into(), images: Vec::new(), files: Vec::new() };
        assert_eq!(turn_input_in_tests(&json!({"text": "hello"})).unwrap(), hello);
        assert_eq!(turn_input_in_tests(&json!({"input": [{"type": "text", "text": "hello"}]})).unwrap(), hello);
        assert!(turn_input_in_tests(&json!({"text": "  "})).is_err());
        assert!(turn_input_in_tests(&json!({})).is_err());
    }

    #[test]
    fn turn_input_takes_images_by_url_and_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("shot.png");
        std::fs::write(&shot, b"png bytes").unwrap();
        let url = "data:image/png;base64,QUJD";

        let input = turn_input_in_tests(&json!({"input": [
            {"type": "text", "text": "what is this?"},
            {"type": "image", "url": url},
            {"type": "localImage", "path": shot},
        ]}))
        .unwrap();
        assert_eq!(input.text, "what is this?");
        assert_eq!(input.images[0], url);
        assert!(input.images[1].starts_with("data:image/png;base64,"));

        // An image alone is a prompt; a bad one is refused rather than dropped.
        assert!(turn_input_in_tests(&json!({"input": [{"type": "image", "url": url}]})).is_ok());
        assert!(
            turn_input_in_tests(&json!({"input": [{"type": "image", "url": "https://example.com/a.png"}]})).is_err()
        );
        assert!(
            turn_input_in_tests(&json!({"input": [{"type": "localImage", "path": dir.path().join("x.txt")}]})).is_err()
        );
        assert!(turn_input_in_tests(&json!({"input": [{"type": "audio"}]})).is_err());
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

    /// A resumed thread shows another session's message as it arrived, not as
    /// the prompt the model was given.
    #[test]
    fn history_shows_who_sent_a_message_from_another_session() {
        let message = crate::inbox::Message {
            id: uuid::Uuid::new_v4(),
            from: crate::inbox::Sender {
                session: Some(uuid::Uuid::new_v4()),
                title: "Fork A".into(),
                workspace: "/work/api".into(),
            },
            text: "tests pass here".into(),
            sent_at: chrono::Utc::now(),
        };
        let items = history_items(&[
            json!({"role": "user", "content": message.prompt(std::path::Path::new("/work/api"))}),
            json!({"role": "user", "content": "thanks"}),
        ]);
        assert_eq!(items[0]["text"], "tests pass here");
        assert_eq!(items[0]["from"]["title"], "Fork A");
        assert!(items[1].get("from").is_none());
    }

    /// A subagent's message and a background report show as what they are,
    /// not as the prompts that delivered them; a sent message keeps its text.
    #[test]
    fn history_shows_subagent_messages_and_reports() {
        let report = "## callers\nStatus: completed\nThree callers.\n\n## perf\nStatus: completed\nOne query.\n";
        let items = history_items(&[
            json!({"role": "user", "content": crate::agent::agent_message("perf", "tests pass")}),
            json!({"role": "user", "content": crate::agent::report_prompt(report)}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "send_message", "arguments": "{\"to\": \"perf\", \"message\": \"keep the keys\"}"},
                }],
            }),
            json!({"role": "tool", "name": "send_message", "tool_call_id": "call_1", "content": "Sent."}),
        ]);
        assert_eq!(items[0]["text"], "tests pass");
        assert_eq!(items[0]["from"], json!({"title": "perf", "agent": true}));
        assert_eq!(items[1]["text"], report);
        assert_eq!(items[1]["report"], true);
        assert_eq!(items[1]["from"]["title"], "callers, perf");
        assert_eq!(items[2]["command"], "perf");
        assert_eq!(items[2]["message"], "keep the keys");
    }

    /// A reopened thread shows what each call was for, as the live one did.
    #[test]
    fn history_keeps_what_each_call_was_for() {
        let items = history_items(&[
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "run_command",
                     "arguments": "{\"command\": \"git status -sb\", \"description\": \"Checking which branch this is\"}"}},
                    {"id": "call_2", "type": "function", "function": {"name": "run_command",
                     "arguments": "{\"command\": \"ls\"}"}},
                ],
            }),
            json!({"role": "tool", "name": "run_command", "tool_call_id": "call_1", "content": "## main"}),
            json!({"role": "tool", "name": "run_command", "tool_call_id": "call_2", "content": "src"}),
        ]);
        assert_eq!(items[0]["command"], "git status -sb");
        assert_eq!(items[0]["description"], "Checking which branch this is");
        assert!(items[1].get("description").is_none());
    }

    /// A reopened thread shows where compaction cut its history, and how
    /// much it folded away, between the opening and what was kept. Every
    /// other system message stays out of the transcript.
    #[test]
    fn history_shows_where_compaction_cut_it() {
        let items = history_items(&[
            json!({"role": "system", "content": "scaffolding"}),
            json!({"role": "user", "content": "fix the importer"}),
            json!({"role": "system", "content": "", "compacted": {"messages": 48, "tokens": 92_000, "summarised": true}}),
            json!({"role": "system", "content": "<session_context layer=\"summary\">\nfixed it\n</session_context>"}),
            json!({"role": "user", "content": "and the tests?"}),
            json!({"role": "assistant", "content": "They pass."}),
        ]);
        let types: Vec<&str> = items.iter().map(|item| item["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["userMessage", "contextCompaction", "userMessage", "agentMessage"]);
        assert_eq!(
            items[1],
            json!({"id": "history_2", "type": "contextCompaction", "messages": 48, "tokens": 92_000, "summarised": true})
        );
    }

    #[test]
    fn history_does_not_show_a_refused_call_as_run() {
        let items = history_items(&[
            json!({"role": "tool", "name": "run_command", "content": "Blocked by AUTO mode: nobody asked. It was not run."}),
            json!({"role": "tool", "name": "run_command", "content": "User rejected this tool call. Do not retry it."}),
            json!({"role": "tool", "name": "run_command", "content": "removed build/"}),
        ]);
        let statuses: Vec<&str> = items.iter().map(|item| item["status"].as_str().unwrap()).collect();
        assert_eq!(statuses, ["declined", "declined", "completed"]);
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
    fn history_leaves_out_the_screenshots_tools_showed() {
        let shown = crate::context::with_images(
            json!("The image(s) browser_screenshot returned:"),
            vec!["data:image/png;base64,QUJD".into()],
        );
        let items = history_items(&[
            json!({"role": "user", "content": "how does the page look?"}),
            json!({"role": "tool", "name": "browser_screenshot", "content": "Screenshot of the page"}),
            json!({"role": "user", "content": shown}),
        ]);
        let types: Vec<&str> = items.iter().map(|item| item["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["userMessage", "functionCall"]);
    }

    #[test]
    fn a_context_meter_learns_where_compaction_steps_in() {
        let config = crate::config::Config::for_endpoint(
            std::path::PathBuf::from("."),
            "https://api.example.com/v1".into(),
            "model".into(),
            crate::config::AbacusPaths::under(std::path::PathBuf::from("/nonexistent")),
        );
        let marks = serde_json::to_value(config.compaction_marks()).unwrap();
        let window = config.model_limits.context_window as u64;
        let (trim, summary) = (marks["trim"].as_u64().unwrap(), marks["summary"].as_u64().unwrap());
        assert!(0 < trim && trim < summary && summary < window, "{marks} of {window}");
        assert!(marks["idle"].as_u64().is_some_and(|idle| idle < summary), "{marks}");
    }

    #[test]
    fn history_marks_the_turns_a_goal_started_by_itself() {
        let goal = crate::goal::GoalState::default();
        goal.create("Ship the parser").unwrap();
        let Some(crate::goal::Next::Turn(prompt)) = goal.after_turn(true) else { panic!("the goal carries on") };
        let items = history_items(&[
            json!({"role": "user", "content": "Ship the parser"}),
            json!({"role": "assistant", "content": "Parsed half of it."}),
            json!({"role": "user", "content": prompt}),
        ]);
        assert_eq!(items.len(), 3);
        assert_eq!(items[2]["goal"], true, "not shown as the person's message");
        assert_eq!(items[2]["from"]["title"], "Goal");
        assert!(!items[2]["text"].as_str().unwrap().contains("goal_update"), "nor as the prompt");
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
        assert_eq!(item_type("run_python"), "commandExecution", "a chat's Python runs like a command");
        assert_eq!(item_type("apply_patch"), "fileChange");
        assert_eq!(item_type("grep"), "functionCall");
    }

    #[test]
    fn goals_reach_the_client_with_status_and_progress() {
        assert_eq!(goal_item(None), Value::Null);
        let goal = crate::goal::GoalState::default();
        goal.create("Ship the parser").unwrap();
        goal.increment_iteration().unwrap();
        goal.execute("goal_update", r#"{"status":"paused","note":"waiting on review"}"#).unwrap();
        assert_eq!(
            goal_item(goal.snapshot()),
            json!({"objective": "Ship the parser", "status": "paused", "iterations": 1, "note": "waiting on review"})
        );
    }
}
