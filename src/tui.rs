use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, Stdout},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate, Utc};
use crossterm::{
    cursor::Show,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};
use unicode_width::UnicodeWidthStr;

use crate::{
    activity::ActivityReporter,
    agent::{
        AgentEvent, AgentMode, ApprovalDecision, ApprovalRequest, TurnOptions, UserQuestionRequest,
        compact_messages, initial_messages, message_chars, run_turn,
    },
    compaction::CompactionState,
    config::{Config, Credentials, PermissionMode, ProviderProtocol, SETTINGS_VERSION, Settings},
    context::expand_file_references,
    diff::{DiffDocument, DiffLineKind},
    goal::GoalState,
    input::{InputBuffer, InputMode},
    markdown::{MarkdownTheme, render as render_markdown},
    provider::Provider,
    ralph::{RalphLoop, RalphStatus},
    services::AgentServices,
    session::{Session, SessionStore, SessionUsage},
    task::TaskList,
    theme::{Theme, ThemeChoice, ThemeMode},
};

// Colors resolve from the active (Empero-derived) theme so the UI adapts to a
// dark or light terminal. These thin accessors keep the many draw helpers terse
// while reading the process-global theme on each frame.
fn primary() -> Color {
    crate::theme::active().primary
}
fn secondary() -> Color {
    crate::theme::active().secondary
}
fn success() -> Color {
    crate::theme::active().success
}
fn warning() -> Color {
    crate::theme::active().warning
}
fn danger() -> Color {
    crate::theme::active().danger
}
fn muted() -> Color {
    crate::theme::active().muted
}
fn border() -> Color {
    crate::theme::active().border
}
fn surface() -> Color {
    crate::theme::active().surface
}
fn text() -> Color {
    crate::theme::active().text
}
fn inverse() -> Color {
    crate::theme::active().inverse
}

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/goal", "Set or manage a persistent goal"),
    ("/loop", "Start or inspect a Ralph loop"),
    ("/cancel-loop", "Cancel the active Ralph loop"),
    ("/swarm", "Delegate an objective to parallel subagents"),
    ("/config", "Change live settings"),
    ("/theme", "Switch dark, light, or auto theme"),
    ("/feedback", "Send product feedback"),
    ("/mode", "Set auto, plan, or build mode"),
    ("/plan", "Toggle plan pin"),
    ("/model", "Inspect or switch model"),
    ("/usage", "View local usage and activity"),
    ("/sessions", "Browse saved sessions"),
    ("/new", "Start a new session"),
    ("/compact", "Compact conversation context"),
    ("/skills", "Browse Agent Skills"),
    ("/plugins", "Inspect plugins"),
    ("/mcps", "Inspect MCP tools"),
    ("/tools", "List all active tools"),
    ("/help", "Show shortcuts and commands"),
    ("/quit", "Exit Abacus"),
    ("/exit", "Exit Abacus"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    User,
    Assistant,
    Tool,
    System,
    Error,
}

#[derive(Debug, Clone)]
struct Entry {
    kind: EntryKind,
    text: String,
}

struct PendingApproval {
    tool: String,
    summary: String,
    details: String,
    diff: Option<DiffDocument>,
    view: ApprovalView,
    respond: tokio::sync::oneshot::Sender<ApprovalDecision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalView {
    Unified,
    Raw,
}

/// Open modal for an `ask_user` tool call. The user navigates the options with
/// arrow keys (and toggles each with `space` when multi-select), then confirms
/// with `enter`. They can edit the custom text field with character input and
/// append it on `enter` if no option was selected.
struct PendingUserQuestion {
    header: String,
    question: String,
    options: Vec<String>,
    multi_select: bool,
    /// One `bool` per option; `true` means toggled on (multi-select only).
    selected: Vec<bool>,
    cursor: usize,
    custom: InputBuffer,
    /// Whether the user is currently editing the custom text field rather than
    /// navigating options.
    editing_custom: bool,
    respond: tokio::sync::oneshot::Sender<crate::agent::UserAnswer>,
}

impl PendingUserQuestion {
    fn new(
        header: String,
        question: String,
        options: Vec<String>,
        multi_select: bool,
        respond: tokio::sync::oneshot::Sender<crate::agent::UserAnswer>,
    ) -> Self {
        let selected = vec![false; options.len()];
        Self {
            header,
            question,
            options,
            multi_select,
            selected,
            cursor: 0,
            custom: InputBuffer::new(),
            editing_custom: false,
            respond,
        }
    }

    fn resolve_answer(&self) -> crate::agent::UserAnswer {
        let mut selected_labels = Vec::new();
        for (index, on) in self.selected.iter().enumerate() {
            if *on {
                // Strip the trailing " — description" added for display, keeping
                // just the option label so the LLM sees clean identifiers.
                let raw = self.options[index]
                    .split(" — ")
                    .next()
                    .unwrap_or(&self.options[index]);
                selected_labels.push(raw.to_owned());
            }
        }
        let custom_text = self.custom.text();
        let custom = if custom_text.trim().is_empty() {
            None
        } else {
            Some(custom_text)
        };
        crate::agent::UserAnswer {
            selected_labels,
            custom_text: custom,
        }
    }
}

struct Picker {
    title: String,
    items: Vec<(String, String)>,
    selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageTab {
    Overview,
    Models,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageRange {
    AllTime,
    Last7Days,
    Last30Days,
}

impl UsageRange {
    fn next(self) -> Self {
        match self {
            Self::AllTime => Self::Last7Days,
            Self::Last7Days => Self::Last30Days,
            Self::Last30Days => Self::AllTime,
        }
    }

    fn includes(self, date: NaiveDate, today: NaiveDate) -> bool {
        match self {
            Self::AllTime => true,
            Self::Last7Days => date >= today - ChronoDuration::days(6),
            Self::Last30Days => date >= today - ChronoDuration::days(29),
        }
    }
}

struct UsagePanel {
    records: Vec<SessionUsage>,
    tab: UsageTab,
    range: UsageRange,
}

#[derive(Default)]
struct UsageStats {
    sessions: usize,
    total_tokens: u64,
    tokens_estimated: bool,
    favorite_model: Option<String>,
    active_days: usize,
    most_active_day: Option<NaiveDate>,
    longest_session: u64,
    longest_streak: usize,
    current_streak: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigKey {
    Profile,
    Model,
    BaseUrl,
    Protocol,
    Permission,
    VimMode,
    Animations,
    Tooltips,
    MaxSteps,
    ToolOutputLimit,
    ProjectTrust,
    FeedbackEnabled,
    FeedbackDiagnostics,
    FeedbackEndpoint,
    AdvancedToml,
}

const CONFIG_KEYS: &[ConfigKey] = &[
    ConfigKey::Profile,
    ConfigKey::Model,
    ConfigKey::BaseUrl,
    ConfigKey::Protocol,
    ConfigKey::Permission,
    ConfigKey::VimMode,
    ConfigKey::Animations,
    ConfigKey::Tooltips,
    ConfigKey::MaxSteps,
    ConfigKey::ToolOutputLimit,
    ConfigKey::ProjectTrust,
    ConfigKey::FeedbackEnabled,
    ConfigKey::FeedbackDiagnostics,
    ConfigKey::FeedbackEndpoint,
    ConfigKey::AdvancedToml,
];

struct ConfigPanel {
    selected: usize,
    editing: Option<(ConfigKey, InputBuffer)>,
}

struct RawConfigEditor {
    input: InputBuffer,
    error: Option<String>,
}

struct FeedbackForm {
    input: InputBuffer,
    category: usize,
    include_diagnostics: bool,
    sending: bool,
    error: Option<String>,
}

const FEEDBACK_CATEGORIES: &[&str] = &["General", "Bug", "Feature", "Performance"];

struct FeedbackResult {
    result: std::result::Result<crate::feedback::FeedbackReceipt, String>,
}

struct ServicesResult {
    result: std::result::Result<AgentServices, String>,
}

struct App {
    config: Config,
    settings: Settings,
    credentials: Credentials,
    provider: Provider,
    messages: Vec<Value>,
    session: Option<Session>,
    session_store: Option<SessionStore>,
    services: Arc<AgentServices>,
    goal: GoalState,
    tasks: TaskList,
    compaction: CompactionState,
    ralph_loop: Option<RalphLoop>,
    entries: Vec<Entry>,
    input: InputBuffer,
    mode: InputMode,
    running: Option<JoinHandle<()>>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    event_rx: mpsc::UnboundedReceiver<AgentEvent>,
    approval: Option<PendingApproval>,
    approval_scroll: u16,
    approval_horizontal: u16,
    question: Option<PendingUserQuestion>,
    picker: Option<Picker>,
    usage_panel: Option<UsagePanel>,
    config_panel: Option<ConfigPanel>,
    raw_config: Option<RawConfigEditor>,
    feedback_form: Option<FeedbackForm>,
    feedback_tx: mpsc::UnboundedSender<FeedbackResult>,
    feedback_rx: mpsc::UnboundedReceiver<FeedbackResult>,
    reload_services: bool,
    services_reloading: bool,
    services_tx: mpsc::UnboundedSender<ServicesResult>,
    services_rx: mpsc::UnboundedReceiver<ServicesResult>,
    allow_mutations: Arc<AtomicBool>,
    receiving_delta: bool,
    follow: bool,
    scroll: u16,
    transcript_height: u16,
    status: String,
    /// Live estimate of context-window usage in chars, updated from streaming
    /// events so the footer's `ctx %` reflects what's happening *during* a turn,
    /// not just the snapshot from the last `Done`. Resynched from `messages` on
    /// `Done`/`Failed`/`resume` so it stays accurate between turns.
    ctx_chars: usize,
    /// Submitted-prompt history for arrow-up/down recall. `history_index` is the
    /// cursor into it; `None` means "at the live input, not browsing history".
    input_history: Vec<String>,
    input_history_index: Option<usize>,
    /// Prompt queued while a turn is running so it fires the moment the agent
    /// finishes. Only one slot — the latest queued message wins.
    queued_message: Option<String>,
    show_help: bool,
    normal_prefix: Option<char>,
    agent_mode: AgentMode,
    resolved_agent_mode: Option<AgentMode>,
    /// Shared session token counter; reused when the provider is rebuilt on a
    /// model switch so the running total survives.
    tokens: Arc<AtomicU64>,
    session_initial_active_secs: u64,
    started: Instant,
    last_ctrl_c: Option<Instant>,
    quit: bool,
}

impl App {
    fn new(
        config: Config,
        mut settings: Settings,
        credentials: Credentials,
        session: Option<Session>,
        session_store: Option<SessionStore>,
        services: Arc<AgentServices>,
    ) -> Result<Self> {
        if let Some(profile) = settings.profiles.get_mut(&config.profile) {
            profile.model = config.model.clone();
            profile.base_url = config.base_url.clone();
            profile.protocol = config.protocol;
        } else {
            settings.profiles.insert(
                config.profile.clone(),
                crate::config::ProviderProfile {
                    name: "Current CLI overrides".to_owned(),
                    base_url: config.base_url.clone(),
                    model: config.model.clone(),
                    protocol: config.protocol,
                    api_key_env: None,
                },
            );
        }
        settings.default_profile = config.profile.clone();
        settings.agent.max_steps = config.max_steps;
        settings.agent.tool_output_limit = config.tool_output_limit;
        if config.yes {
            settings.ui.permission_mode = PermissionMode::AlwaysApprove;
        }
        let initial_tokens = session
            .as_ref()
            .map(|session| session.tokens_used)
            .unwrap_or(0);
        let session_initial_active_secs = session
            .as_ref()
            .map(|session| session.active_secs)
            .unwrap_or(0);
        let tokens = Arc::new(AtomicU64::new(initial_tokens));
        let provider = Provider::with_tokens(&config, tokens.clone())?;
        let goal = GoalState::new(session.as_ref().and_then(|session| session.goal.clone()));
        let tasks = TaskList::new(
            session
                .as_ref()
                .map(|session| session.tasks.clone())
                .unwrap_or_default(),
        );
        let compaction = session
            .as_ref()
            .and_then(|session| session.compaction.clone())
            .unwrap_or_default();
        let ralph_loop = session
            .as_ref()
            .and_then(|session| session.ralph_loop.clone());
        let messages = session
            .as_ref()
            .map(|value| value.messages.clone())
            .unwrap_or_else(|| initial_messages(&config.workspace));
        let ctx_chars = message_chars(&messages);
        let mut entries = entries_from_messages(&messages);
        if !entries.is_empty() {
            entries.push(Entry {
                kind: EntryKind::System,
                text: "Session resumed.".to_owned(),
            });
        }
        for diagnostic in services.diagnostics() {
            entries.push(Entry {
                kind: EntryKind::Error,
                text: format!("Extension warning: {diagnostic}"),
            });
        }
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (feedback_tx, feedback_rx) = mpsc::unbounded_channel();
        let (services_tx, services_rx) = mpsc::unbounded_channel();
        let yes = config.yes;
        Ok(Self {
            config,
            settings,
            credentials,
            provider,
            messages,
            session,
            session_store,
            services,
            goal,
            tasks,
            compaction,
            ralph_loop,
            entries,
            input: InputBuffer::new(),
            mode: InputMode::Insert,
            running: None,
            event_tx,
            event_rx,
            approval: None,
            approval_scroll: 0,
            approval_horizontal: 0,
            question: None,
            picker: None,
            usage_panel: None,
            config_panel: None,
            raw_config: None,
            feedback_form: None,
            feedback_tx,
            feedback_rx,
            reload_services: false,
            services_reloading: false,
            services_tx,
            services_rx,
            allow_mutations: Arc::new(AtomicBool::new(yes)),
            receiving_delta: false,
            follow: true,
            scroll: 0,
            transcript_height: 1,
            status: "ready".to_owned(),
            ctx_chars,
            input_history: Vec::new(),
            input_history_index: None,
            queued_message: None,
            show_help: false,
            normal_prefix: None,
            agent_mode: AgentMode::Auto,
            resolved_agent_mode: None,
            tokens,
            session_initial_active_secs,
            started: Instant::now(),
            last_ctrl_c: None,
            quit: false,
        })
    }

    fn drain_agent_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.event_rx.try_recv() {
            changed = true;
            match event {
                AgentEvent::Delta(delta) => {
                    if !self.receiving_delta {
                        self.entries.push(Entry {
                            kind: EntryKind::Assistant,
                            text: String::new(),
                        });
                        self.receiving_delta = true;
                    }
                    if let Some(entry) = self.entries.last_mut() {
                        entry.text.push_str(&delta);
                    }
                    // Grow the live context estimate: each delta char is roughly
                    // 1 JSON char in the assistant message (+ small JSON wrapper).
                    self.ctx_chars = self.ctx_chars.saturating_add(delta.len() + 40);
                    self.status = "thinking".to_owned();
                }
                AgentEvent::Approval(request) => self.set_approval(request),
                AgentEvent::UserQuestion(request) => self.set_user_question(request),
                AgentEvent::ToolStarted { name, summary } => {
                    self.receiving_delta = false;
                    self.entries.push(Entry {
                        kind: EntryKind::Tool,
                        text: format!("{name}  {summary}\n… running"),
                    });
                    self.status = format!("running {name}");
                }
                AgentEvent::ToolFinished { name, output } => {
                    self.receiving_delta = false;
                    let preview = tool_preview(&output);
                    // The full tool result (not the preview) lands in the
                    // messages array; estimate its JSON size for the live ctx %.
                    self.ctx_chars = self
                        .ctx_chars
                        .saturating_add(output.len() + name.len() + 80);
                    if let Some(entry) = self
                        .entries
                        .last_mut()
                        .filter(|entry| entry.kind == EntryKind::Tool)
                    {
                        entry.text = format!("{name}\n{preview}");
                    } else {
                        self.entries.push(Entry {
                            kind: EntryKind::Tool,
                            text: format!("{name}\n{preview}"),
                        });
                    }
                    self.status = "thinking".to_owned();
                }
                AgentEvent::ModeChanged { mode, reason } => {
                    self.resolved_agent_mode = Some(mode);
                    self.entries.push(Entry {
                        kind: EntryKind::System,
                        text: format!("{} mode · {reason}", mode.label()),
                    });
                    self.status = format!("{} mode", mode.label().to_ascii_lowercase());
                }
                AgentEvent::Done { messages } => {
                    let assistant_output = latest_assistant_text(&messages);
                    self.messages = messages;
                    // Resynthe live ctx estimate from the authoritative messages.
                    self.ctx_chars = message_chars(&self.messages);
                    let mut continue_loop = false;
                    if let Some(state) = &mut self.ralph_loop {
                        if state.is_active() {
                            let completed = state.observe_output(&assistant_output);
                            continue_loop = state.is_active();
                            self.status = if completed {
                                format!("loop completed after {} iteration(s)", state.iteration)
                            } else if state.status == RalphStatus::MaxIterations {
                                format!("loop stopped at {} iteration(s)", state.iteration)
                            } else {
                                "loop continuing".to_owned()
                            };
                        } else if state.status == RalphStatus::Paused {
                            self.status = "loop paused".to_owned();
                        }
                    }
                    self.persist_session();
                    self.running = None;
                    self.resolved_agent_mode = None;
                    self.receiving_delta = false;
                    if continue_loop {
                        self.continue_ralph_loop();
                    } else if !matches!(
                        self.ralph_loop.as_ref().map(|state| state.status),
                        Some(
                            RalphStatus::Completed
                                | RalphStatus::MaxIterations
                                | RalphStatus::Paused
                        )
                    ) {
                        self.status = "ready".to_owned();
                    }
                    self.drain_queued_message();
                }
                AgentEvent::Failed { error, messages } => {
                    self.messages = messages;
                    self.ctx_chars = message_chars(&self.messages);
                    self.running = None;
                    self.resolved_agent_mode = None;
                    self.receiving_delta = false;
                    self.approval = None;
                    self.entries.push(Entry {
                        kind: EntryKind::Error,
                        text: error,
                    });
                    self.status = "error".to_owned();
                    if let Some(state) = &mut self.ralph_loop {
                        let _ = state.pause();
                    }
                    self.persist_session();
                    self.follow = true;
                    self.queued_message = None;
                }
            }
        }
        changed
    }

    fn set_approval(&mut self, request: ApprovalRequest) {
        self.status = format!("approval needed: {}", request.tool);
        let diff = DiffDocument::parse(&request.details);
        self.approval = Some(PendingApproval {
            tool: request.tool,
            summary: request.summary,
            details: request.details,
            view: if diff.is_some() {
                ApprovalView::Unified
            } else {
                ApprovalView::Raw
            },
            diff,
            respond: request.respond,
        });
        self.approval_scroll = 0;
        self.approval_horizontal = 0;
    }

    fn set_user_question(&mut self, request: UserQuestionRequest) {
        self.status = format!("waiting for answer: {}", request.header);
        self.question = Some(PendingUserQuestion::new(
            request.header,
            request.question,
            request.options,
            request.multi_select,
            request.respond,
        ));
    }

    /// Resolve an open user question and return the oneshot to the agent loop.
    /// Dropping the pending state implicitly cancels the question.
    fn answer_user_question(&mut self) {
        if let Some(question) = self.question.take() {
            let answer = question.resolve_answer();
            let _ = question.respond.send(answer);
            self.status = "ready".to_owned();
        }
    }

    fn decide(&mut self, decision: ApprovalDecision) {
        if let Some(approval) = self.approval.take() {
            let _ = approval.respond.send(decision);
            self.status = match decision {
                ApprovalDecision::Once | ApprovalDecision::Always => "approved".to_owned(),
                ApprovalDecision::Reject => "rejected".to_owned(),
            };
        }
    }

    fn submit(&mut self) {
        let pending = self.input.text();
        let prompt = pending.trim();
        if prompt.is_empty() {
            return;
        }
        if self.running.is_some() {
            if prompt.starts_with('/') {
                let prompt = self.input.take();
                self.slash_command(prompt.trim());
            } else {
                let prompt = self.input.take();
                let prompt = prompt.trim().to_owned();
                self.queued_message = Some(prompt.clone());
                self.entries.push(Entry {
                    kind: EntryKind::System,
                    text: format!("Queued: {prompt}"),
                });
                self.follow = true;
                self.status = "Queued · runs when agent finishes".to_owned();
            }
            return;
        }
        let prompt = self.input.take();
        let prompt = prompt.trim().to_owned();
        self.record_history(&prompt);
        self.submit_prompt(prompt);
    }

    /// Resolve a prompt (slash command, extension, or plain prompt) and start a
    /// turn. Shared by `submit` and the queued-message flush.
    fn submit_prompt(&mut self, prompt: String) {
        if self.slash_command(&prompt) {
            return;
        }
        let (command, argument) = prompt.split_once(' ').unwrap_or((&prompt, ""));
        let command_name = command.strip_prefix('/');
        let extension_prompt = command_name.and_then(|name| {
            self.services
                .skills
                .get(name)
                .map(|_| self.services.skills.invocation(name, argument))
                .or_else(|| {
                    self.services.plugins.command(name).map(|plugin_command| {
                        Ok(plugin_command.prompt.replace("{{args}}", argument))
                    })
                })
        });
        let effective_prompt = match extension_prompt {
            Some(Ok(prompt)) => prompt,
            Some(Err(error)) => {
                self.status = format!("extension error: {error}");
                return;
            }
            None => prompt.clone(),
        };

        self.start_turn(prompt, effective_prompt, true);
    }

    fn start_turn(&mut self, display_prompt: String, effective_prompt: String, display: bool) {
        if self.running.is_some() {
            return;
        }
        if display {
            self.entries.push(Entry {
                kind: EntryKind::User,
                text: display_prompt.clone(),
            });
        }
        let model_prompt = if display {
            expand_file_references(&self.config.workspace, &effective_prompt).unwrap_or_else(
                |error| {
                    self.status = format!("file reference warning: {error}");
                    effective_prompt
                },
            )
        } else {
            // Ralph iterations must receive the exact same prompt bytes every time.
            effective_prompt
        };
        self.messages
            .push(json!({"role": "user", "content": model_prompt}));
        // Account for the user message just added to the context.
        self.ctx_chars = self.ctx_chars.saturating_add(model_prompt.len() + 40);
        self.persist_session();
        self.receiving_delta = false;
        self.follow = true;
        self.status = "connecting".to_owned();
        self.resolved_agent_mode = Some(self.agent_mode);

        let provider = self.provider.clone();
        let messages = self.messages.clone();
        let agent_mode = self.agent_mode;
        let allow_mutations = self.allow_mutations.clone();
        let events = self.event_tx.clone();
        let options = TurnOptions {
            workspace: self.config.workspace.clone(),
            max_steps: self.config.max_steps,
            tool_output_limit: self.config.tool_output_limit,
            mode: agent_mode,
            allow_mutations,
            services: self.services.clone(),
            session_id: self.session.as_ref().map(|session| session.id.to_string()),
            goal: self.goal.clone(),
            tasks: self.tasks.clone(),
            compaction: self.compaction.clone(),
            compaction_budget: self.config.model_limits.compaction_budget(),
            allow_subagents: true,
            web_search: self.config.web_search.clone(),
        };
        self.running = Some(tokio::spawn(async move {
            run_turn(provider, messages, options, events).await;
        }));
    }

    fn slash_command(&mut self, input: &str) -> bool {
        let (command, argument) = input.split_once(' ').unwrap_or((input, ""));
        match command {
            "/help" => {
                self.show_help = true;
                true
            }
            "/clear" | "/new" => {
                self.new_session();
                true
            }
            "/quit" | "/q" | "/exit" => {
                self.quit = true;
                true
            }
            "/model" => {
                if argument.trim().is_empty() {
                    self.entries.push(Entry {
                        kind: EntryKind::System,
                        text: format!(
                            "Model: {}\nEndpoint: {}\n\nSwitch with /model <id>; discover IDs with `abacus models`.",
                            self.config.model, self.config.base_url
                        ),
                    });
                } else {
                    let model = argument.trim().to_owned();
                    let result = self.active_profile_mut().map(|profile| {
                        profile.model = model.clone();
                    });
                    match result.and_then(|()| self.save_and_apply_settings()) {
                        Ok(()) => self.status = format!("model: {} · saved", self.config.model),
                        Err(error) => self.status = format!("model switch failed: {error:#}"),
                    }
                }
                self.follow = true;
                true
            }
            "/sessions" => {
                self.list_sessions();
                true
            }
            "/usage" => {
                self.open_usage();
                true
            }
            "/resume" => {
                self.resume_session(argument);
                true
            }
            "/rename" => {
                self.rename_session(argument);
                true
            }
            "/tools" => {
                let mut names = self
                    .services
                    .tool_specs()
                    .into_iter()
                    .filter_map(|spec| spec["function"]["name"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>();
                names.extend([
                    "goal_status".to_owned(),
                    "goal_update".to_owned(),
                    "spawn_subagents".to_owned(),
                ]);
                self.entries.push(Entry {
                    kind: EntryKind::System,
                    text: format!("Tools: {}", names.join(", ")),
                });
                self.follow = true;
                true
            }
            "/skills" => {
                let text = self
                    .services
                    .skills
                    .list()
                    .map(|skill| format!("/{}  {}", skill.name, skill.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.entries.push(Entry {
                    kind: EntryKind::System,
                    text: if text.is_empty() {
                        "No skills discovered.".to_owned()
                    } else {
                        format!("Skills\n{text}")
                    },
                });
                self.follow = true;
                true
            }
            "/plugins" => {
                let text = self
                    .services
                    .plugins
                    .list()
                    .map(|plugin| {
                        format!("{} {}  {}", plugin.name, plugin.version, plugin.description)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                self.entries.push(Entry {
                    kind: EntryKind::System,
                    text: if text.is_empty() {
                        "No plugins enabled.".to_owned()
                    } else {
                        format!("Plugins\n{text}")
                    },
                });
                self.follow = true;
                true
            }
            "/mcps" => {
                let text = self
                    .services
                    .mcp
                    .tools()
                    .map(|tool| format!("{}  {}", tool.exposed_name, tool.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.entries.push(Entry {
                    kind: EntryKind::System,
                    text: if text.is_empty() {
                        "No MCP tools connected.".to_owned()
                    } else {
                        format!("MCP tools\n{text}")
                    },
                });
                self.follow = true;
                true
            }
            "/plan" => {
                self.agent_mode = if self.agent_mode == AgentMode::Plan {
                    AgentMode::Auto
                } else {
                    AgentMode::Plan
                };
                self.status = format!("{} mode", self.agent_mode.label().to_ascii_lowercase());
                true
            }
            "/mode" => {
                let requested = match argument.trim().to_ascii_lowercase().as_str() {
                    "" => None,
                    "auto" => Some(AgentMode::Auto),
                    "plan" => Some(AgentMode::Plan),
                    "build" => Some(AgentMode::Build),
                    _ => {
                        self.entries.push(Entry {
                            kind: EntryKind::Error,
                            text: "Usage: /mode auto|plan|build".to_owned(),
                        });
                        self.follow = true;
                        return true;
                    }
                };
                if let Some(mode) = requested {
                    self.agent_mode = mode;
                    self.status = format!("{} mode", mode.label().to_ascii_lowercase());
                } else {
                    self.entries.push(Entry {
                        kind: EntryKind::System,
                        text: format!(
                            "Mode: {}\nAUTO lets the model choose PLAN or BUILD per turn; pinned modes enforce your choice.",
                            self.agent_mode.label()
                        ),
                    });
                    self.follow = true;
                }
                true
            }
            "/goal" => {
                self.goal_command(argument);
                true
            }
            "/loop" => {
                self.loop_command(argument);
                true
            }
            "/swarm" => {
                self.swarm_command(argument);
                true
            }
            "/cancel-loop" | "/cancel-ralph" => {
                self.cancel_ralph_loop();
                true
            }
            "/config" => {
                self.open_config(argument);
                true
            }
            "/theme" => {
                self.theme_command(argument);
                true
            }
            "/feedback" => {
                self.open_feedback();
                true
            }
            "/compact" => {
                // Manual quick-compaction: a synchronous drop-only shrink for when
                // the user wants to cut context immediately. The rolling LLM
                // summary compaction (compaction::compact) runs automatically each
                // turn and maintains `self.compaction`; this command does not touch
                // that state, so any prior rolling summary is preserved.
                let before = self.messages.len();
                self.messages = compact_messages(&self.messages, 160_000);
                self.persist_session();
                self.entries.push(Entry {
                    kind: EntryKind::System,
                    text: format!(
                        "Quick-compacted conversation from {before} to {} messages. \
                         Rolling-summary compaction also runs automatically as the context grows.",
                        self.messages.len()
                    ),
                });
                self.follow = true;
                true
            }
            value if value.starts_with('/') => {
                self.entries.push(Entry {
                    kind: EntryKind::Error,
                    text: format!("Unknown command: {value}"),
                });
                self.follow = true;
                true
            }
            _ => false,
        }
    }

    fn persist_session(&mut self) {
        let Some(store) = &self.session_store else {
            return;
        };
        // Lazy session creation: create the session record on first persist
        // (first message sent) instead of at startup, so opening Abacus without
        // sending anything doesn't leave an empty session behind.
        if self.session.is_none() {
            self.session = store
                .create(
                    self.config.profile.clone(),
                    self.config.model.clone(),
                    self.messages.clone(),
                )
                .map_err(|error| self.status = format!("session create failed: {error}"))
                .ok();
        }
        let Some(session) = &mut self.session else {
            return;
        };
        session.update_messages(self.messages.clone());
        session.goal = self.goal.snapshot();
        session.tasks = self.tasks.snapshot();
        session.compaction = Some(self.compaction.clone());
        session.ralph_loop = self.ralph_loop.clone();
        session.tokens_used = self.provider.tokens_used();
        session.active_secs = self
            .session_initial_active_secs
            .saturating_add(self.started.elapsed().as_secs());
        if let Err(error) = store.save(session) {
            self.status = format!("session save failed: {error}");
        }
    }

    fn toggle_agent_mode(&mut self) {
        self.agent_mode = match self.agent_mode {
            AgentMode::Auto => AgentMode::Plan,
            AgentMode::Plan => AgentMode::Build,
            AgentMode::Build => AgentMode::Auto,
        };
        self.status = format!("{} mode", self.agent_mode.label().to_ascii_lowercase());
    }

    fn goal_command(&mut self, argument: &str) {
        let argument = argument.trim();
        let (result, start_prompt) = if argument.is_empty() {
            (
                Ok(self
                    .goal
                    .snapshot()
                    .map(|goal| {
                        format!(
                            "Goal · {:?}\n{}{}",
                            goal.status,
                            goal.objective,
                            goal.note
                                .map(|note| format!("\n\nLatest update: {note}"))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| {
                        "No goal is set. Use /goal <objective>, ideally after /plan.".to_owned()
                    })),
                None,
            )
        } else if argument == "pause" {
            let result = self
                .goal
                .pause()
                .map(|_| "Goal paused. Use /goal resume when ready.".to_owned());
            if result.is_ok()
                && let Some(handle) = self.running.take()
            {
                handle.abort();
                self.approval = None;
                self.receiving_delta = false;
                if let Some(state) = &mut self.ralph_loop
                    && state.is_active()
                {
                    let _ = state.pause();
                }
            }
            (result, None)
        } else if argument == "resume" {
            match self.goal.resume() {
                Ok(goal) => (
                    Ok("Goal resumed.".to_owned()),
                    Some(("Resume goal".to_owned(), goal.objective)),
                ),
                Err(error) => (Err(error), None),
            }
        } else if argument == "clear" {
            (
                self.goal.set(None).map(|()| "Goal cleared.".to_owned()),
                None,
            )
        } else if matches!(argument, "done" | "complete") {
            (
                Ok(self
                    .goal
                    .execute("goal_update", r#"{"status":"complete"}"#)
                    .unwrap_or_else(|| "Error: no goal is set".to_owned())),
                None,
            )
        } else if let Some(objective) = argument.strip_prefix("edit ") {
            (
                self.goal
                    .edit(objective)
                    .map(|goal| format!("Goal updated: {}", goal.objective)),
                None,
            )
        } else {
            match self.goal.create(argument) {
                Ok(goal) => (
                    Ok(format!("Goal set: {}", goal.objective)),
                    Some((goal.objective.clone(), goal.objective)),
                ),
                Err(error) => (Err(error), None),
            }
        };
        match result {
            Ok(text) => self.entries.push(Entry {
                kind: EntryKind::System,
                text,
            }),
            Err(error) => self.entries.push(Entry {
                kind: EntryKind::Error,
                text: format!("Goal error: {error:#}"),
            }),
        }
        self.persist_session();
        self.follow = true;
        if let Some((display, prompt)) = start_prompt {
            self.start_turn(display, prompt, true);
        }
    }

    /// `/swarm <objective>` asks the model to decompose the objective into
    /// independent units and delegate them in a single `spawn_subagents` call.
    /// It reuses the normal turn path, so the spawn still goes through approval,
    /// worktree isolation, and the worker limits — this is just a user-facing
    /// nudge toward parallel delegation, not a separate execution path.
    fn swarm_command(&mut self, argument: &str) {
        let objective = argument.trim();
        if objective.is_empty() {
            self.entries.push(Entry {
                kind: EntryKind::System,
                text: "Usage: /swarm <objective>. Abacus splits the objective into independent \
                       units and delegates them to parallel subagents (one approval, isolated git \
                       worktrees). Best for separable work; a single repository is required."
                    .to_owned(),
            });
            self.follow = true;
            return;
        }
        let prompt = format!(
            "Tackle this objective by delegating independent units of work to parallel subagents. \
             Identify the genuinely separable tasks — independent files, modules, or fixes that \
             need no shared intermediate state — and run them together in a single spawn_subagents \
             call, one worker per task, each with a self-contained prompt that states exactly what \
             to change and how to verify it. Afterward, integrate and verify the combined result. \
             If the objective does not split into at least two independent tasks, do not force a \
             split: say so briefly and complete it directly.\n\nObjective: {objective}"
        );
        self.start_turn(objective.to_owned(), prompt, true);
    }

    fn loop_command(&mut self, argument: &str) {
        let argument = argument.trim();
        if argument.is_empty() || argument == "status" {
            let text = self.ralph_loop.as_ref().map_or_else(
                || "No Ralph loop is configured.\n\nUsage: /loop \"<prompt>\" --max-iterations 20 --completion-promise \"DONE\"".to_owned(),
                |state| format!(
                    "Ralph loop · {:?}\nIteration: {}{}\nCompletion promise: {}\n\n{}",
                    state.status,
                    state.iteration,
                    state.max_iterations.map(|limit| format!(" / {limit}")).unwrap_or_else(|| " / unlimited".to_owned()),
                    state.completion_promise,
                    state.prompt
                ),
            );
            self.entries.push(Entry {
                kind: EntryKind::System,
                text,
            });
            self.follow = true;
            return;
        }
        if argument == "pause" {
            let result = self
                .ralph_loop
                .as_mut()
                .context("no Ralph loop is configured")
                .and_then(RalphLoop::pause);
            self.status = result
                .map(|()| "loop pauses after the current turn".to_owned())
                .unwrap_or_else(|error| format!("loop pause failed: {error}"));
            self.persist_session();
            return;
        }
        if argument == "resume" {
            let result = self
                .ralph_loop
                .as_mut()
                .context("no Ralph loop is configured")
                .and_then(RalphLoop::resume);
            match result {
                Ok(()) => self.continue_ralph_loop(),
                Err(error) => self.status = format!("loop resume failed: {error}"),
            }
            return;
        }
        match RalphLoop::from_command(argument) {
            Ok(state) => {
                self.ralph_loop = Some(state);
                self.persist_session();
                self.continue_ralph_loop();
            }
            Err(error) => {
                self.entries.push(Entry {
                    kind: EntryKind::Error,
                    text: format!("Could not start loop: {error:#}\n\nUsage: /loop \"<prompt>\" --max-iterations 20 --completion-promise \"DONE\""),
                });
                self.follow = true;
            }
        }
    }

    fn continue_ralph_loop(&mut self) {
        if self.running.is_some() {
            return;
        }
        let Some(state) = &mut self.ralph_loop else {
            return;
        };
        if !state.is_active() {
            return;
        }
        let iteration = match state.begin_iteration() {
            Ok(iteration) => iteration,
            Err(error) => {
                self.status = format!("loop stopped: {error}");
                self.persist_session();
                return;
            }
        };
        let prompt = state.prompt.clone();
        self.entries.push(Entry {
            kind: EntryKind::System,
            text: format!("Ralph loop · iteration {iteration}"),
        });
        self.persist_session();
        self.start_turn(prompt.clone(), prompt, false);
    }

    fn cancel_ralph_loop(&mut self) {
        let Some(state) = &mut self.ralph_loop else {
            self.status = "no Ralph loop is active".to_owned();
            return;
        };
        state.cancel();
        if let Some(handle) = self.running.take() {
            handle.abort();
            self.approval = None;
            self.receiving_delta = false;
        }
        self.persist_session();
        self.status = "Ralph loop cancelled".to_owned();
        self.entries.push(Entry {
            kind: EntryKind::System,
            text: "Ralph loop cancelled by user.".to_owned(),
        });
        self.follow = true;
    }

    fn new_session(&mut self) {
        self.persist_session();
        self.messages = initial_messages(&self.config.workspace);
        self.session = None; // persist_session recreates lazily on first send
        self.entries.clear();
        self.goal = GoalState::default();
        self.tasks = TaskList::default();
        self.compaction = CompactionState::default();
        self.ralph_loop = None;
        self.tokens.store(0, Ordering::Relaxed);
        self.session_initial_active_secs = 0;
        self.started = Instant::now();
        self.entries.push(Entry {
            kind: EntryKind::System,
            text: "New session.".to_owned(),
        });
        self.scroll = 0;
        self.follow = true;
        self.queued_message = None;
        self.ctx_chars = message_chars(&self.messages);
    }

    fn open_usage(&mut self) {
        self.persist_session();
        let records = if let Some(store) = &self.session_store {
            match store.usage() {
                Ok(records) => records,
                Err(error) => {
                    self.status = format!("could not load usage: {error}");
                    return;
                }
            }
        } else {
            let elapsed = self.started.elapsed();
            let created_at = Utc::now()
                - ChronoDuration::from_std(elapsed).unwrap_or_else(|_| ChronoDuration::zero());
            vec![SessionUsage {
                id: uuid::Uuid::nil(),
                model: self.config.model.clone(),
                created_at,
                updated_at: Utc::now(),
                message_count: self.messages.len().saturating_sub(1),
                tokens_used: self.provider.tokens_used(),
                tokens_estimated: false,
                active_secs: elapsed.as_secs(),
            }]
        };
        self.usage_panel = Some(UsagePanel {
            records,
            tab: UsageTab::Overview,
            range: UsageRange::AllTime,
        });
    }

    fn list_sessions(&mut self) {
        let Some(store) = &self.session_store else {
            self.status = "sessions are disabled".to_owned();
            return;
        };
        match store.list() {
            Ok(sessions) if sessions.is_empty() => self.entries.push(Entry {
                kind: EntryKind::System,
                text: "No saved sessions for this workspace.".to_owned(),
            }),
            Ok(sessions) => {
                self.picker = Some(Picker {
                    title: "sessions".to_owned(),
                    items: sessions
                        .into_iter()
                        .take(50)
                        .map(|session| {
                            (
                                format!(
                                    "{}  {}  {}",
                                    &session.id.to_string()[..8],
                                    session.updated_at.format("%m-%d %H:%M"),
                                    session.title
                                ),
                                session.id.to_string(),
                            )
                        })
                        .collect(),
                    selected: 0,
                });
            }
            Err(error) => self.entries.push(Entry {
                kind: EntryKind::Error,
                text: format!("Could not list sessions: {error}"),
            }),
        }
        self.follow = true;
    }

    fn resume_session(&mut self, id: &str) {
        if id.trim().is_empty() {
            self.list_sessions();
            return;
        }
        self.persist_session();
        let Some(store) = &self.session_store else {
            self.status = "sessions are disabled".to_owned();
            return;
        };
        match store.load(id.trim()) {
            Ok(session) => {
                self.messages = session.messages.clone();
                self.entries = entries_from_messages(&self.messages);
                self.entries.push(Entry {
                    kind: EntryKind::System,
                    text: format!(
                        "Resumed {} ({})",
                        session.title,
                        &session.id.to_string()[..8]
                    ),
                });
                self.goal = GoalState::new(session.goal.clone());
                self.tasks = TaskList::new(session.tasks.clone());
                self.compaction = session.compaction.clone().unwrap_or_default();
                self.ralph_loop = session.ralph_loop.clone();
                self.tokens.store(session.tokens_used, Ordering::Relaxed);
                self.session_initial_active_secs = session.active_secs;
                self.started = Instant::now();
                self.session = Some(session);
                self.follow = true;
                self.status = "ready".to_owned();
                self.ctx_chars = message_chars(&self.messages);
            }
            Err(error) => {
                self.entries.push(Entry {
                    kind: EntryKind::Error,
                    text: format!("Could not resume session: {error}"),
                });
                self.follow = true;
            }
        }
    }

    fn rename_session(&mut self, title: &str) {
        let (Some(store), Some(session)) = (&self.session_store, &mut self.session) else {
            self.status = "sessions are disabled".to_owned();
            return;
        };
        match store.rename(session, title) {
            Ok(()) => self.status = format!("renamed session to {}", session.title),
            Err(error) => self.status = format!("rename failed: {error}"),
        }
    }

    fn open_config(&mut self, argument: &str) {
        if self.running.is_some() {
            self.status =
                "finish or interrupt the active turn before changing configuration".to_owned();
            return;
        }
        if argument.trim() == "raw" {
            self.open_raw_config();
        } else {
            self.config_panel = Some(ConfigPanel {
                selected: 0,
                editing: None,
            });
        }
    }

    fn open_raw_config(&mut self) {
        match toml::to_string_pretty(&self.settings) {
            Ok(text) => {
                let mut input = InputBuffer::new();
                input.insert_str(&text);
                self.config_panel = None;
                self.raw_config = Some(RawConfigEditor { input, error: None });
            }
            Err(error) => self.status = format!("could not encode configuration: {error}"),
        }
    }

    fn save_raw_config(&mut self) {
        let Some(editor) = &self.raw_config else {
            return;
        };
        let content = editor.input.text();
        let result = (|| {
            let mut settings: Settings =
                toml::from_str(&content).context("configuration is not valid TOML")?;
            if settings.version > SETTINGS_VERSION {
                bail!(
                    "configuration version {} is newer than supported version {SETTINGS_VERSION}",
                    settings.version
                );
            }
            settings.version = SETTINGS_VERSION;
            validate_settings(&settings)?;
            settings.save(&self.config.paths)?;
            self.settings = settings;
            self.apply_settings()?;
            Ok::<_, anyhow::Error>(())
        })();
        match result {
            Ok(()) => {
                self.raw_config = None;
                self.status = "configuration saved and applied".to_owned();
            }
            Err(error) => {
                if let Some(editor) = &mut self.raw_config {
                    editor.error = Some(format!("{error:#}"));
                }
            }
        }
    }

    fn apply_settings(&mut self) -> Result<()> {
        validate_settings(&self.settings)?;
        let profile_name = self.settings.default_profile.clone();
        let profile = self
            .settings
            .profiles
            .get(&profile_name)
            .context("default profile no longer exists")?
            .clone();
        let prior_profile = self.config.profile.clone();
        let prior_key = self.config.api_key.clone();
        self.config.profile = profile_name.clone();
        self.config.model = profile.model;
        self.config.base_url = profile.base_url.trim_end_matches('/').to_owned();
        self.config.protocol = profile.protocol;
        self.config.api_key = profile
            .api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .or_else(|| self.credentials.keys.get(&profile_name).cloned())
            .or_else(|| {
                (profile_name == prior_profile)
                    .then_some(prior_key)
                    .flatten()
            });
        self.config.max_steps = self.settings.agent.max_steps.clamp(1, 128);
        self.config.tool_output_limit = self.settings.agent.tool_output_limit.clamp(2_000, 200_000);
        let always = self.settings.ui.permission_mode == PermissionMode::AlwaysApprove;
        self.config.yes = always;
        self.allow_mutations
            .store(always, std::sync::atomic::Ordering::Relaxed);
        if !self.settings.ui.vim_mode {
            self.mode = InputMode::Insert;
        }
        self.provider = Provider::with_tokens(&self.config, self.tokens.clone())?;
        if let Some(session) = &mut self.session {
            session.profile = self.config.profile.clone();
            session.model = self.config.model.clone();
        }
        self.persist_session();
        self.reload_services = true;
        Ok(())
    }

    fn save_and_apply_settings(&mut self) -> Result<()> {
        self.settings.version = SETTINGS_VERSION;
        validate_settings(&self.settings)?;
        self.settings.save(&self.config.paths)?;
        self.apply_settings()
    }

    /// `/theme [auto|dark|light]` — switch the palette live and persist it.
    fn theme_command(&mut self, argument: &str) {
        let choice = match argument.trim().to_ascii_lowercase().as_str() {
            "" => {
                let resolved = self.settings.ui.theme.resolve();
                self.entries.push(Entry {
                    kind: EntryKind::System,
                    text: format!(
                        "Theme: {} (showing {}). Switch with /theme dark, /theme light, or /theme auto.",
                        self.settings.ui.theme.label(),
                        if resolved == ThemeMode::Dark { "dark" } else { "light" },
                    ),
                });
                self.follow = true;
                return;
            }
            "auto" => ThemeChoice::Auto,
            "dark" => ThemeChoice::Dark,
            "light" => ThemeChoice::Light,
            _ => {
                self.entries.push(Entry {
                    kind: EntryKind::Error,
                    text: "Usage: /theme auto|dark|light".to_owned(),
                });
                self.follow = true;
                return;
            }
        };
        self.settings.ui.theme = choice;
        crate::theme::set_active(Theme::for_mode(choice.resolve()));
        match self.settings.save(&self.config.paths) {
            Ok(()) => self.status = format!("theme: {} · saved", choice.label()),
            Err(error) => self.status = format!("theme save failed: {error:#}"),
        }
        self.follow = true;
    }

    fn cycle_config_value(&mut self, key: ConfigKey) -> Result<()> {
        match key {
            ConfigKey::Profile => {
                let profiles = self.settings.profiles.keys().cloned().collect::<Vec<_>>();
                let current = profiles
                    .iter()
                    .position(|name| name == &self.settings.default_profile)
                    .unwrap_or(0);
                self.settings.default_profile = profiles[(current + 1) % profiles.len()].clone();
            }
            ConfigKey::Protocol => {
                let profile = self.active_profile_mut()?;
                profile.protocol = match profile.protocol {
                    ProviderProtocol::ChatCompletions => ProviderProtocol::Responses,
                    ProviderProtocol::Responses => ProviderProtocol::ChatCompletions,
                };
            }
            ConfigKey::Permission => {
                self.settings.ui.permission_mode =
                    if self.settings.ui.permission_mode == PermissionMode::Ask {
                        PermissionMode::AlwaysApprove
                    } else {
                        PermissionMode::Ask
                    };
            }
            ConfigKey::VimMode => self.settings.ui.vim_mode = !self.settings.ui.vim_mode,
            ConfigKey::Animations => self.settings.ui.animations = !self.settings.ui.animations,
            ConfigKey::Tooltips => self.settings.ui.show_tooltips = !self.settings.ui.show_tooltips,
            ConfigKey::ProjectTrust => {
                let trusted = self.settings.trust.contains(&self.config.workspace);
                self.settings.trust.set(&self.config.workspace, !trusted);
            }
            ConfigKey::FeedbackEnabled => {
                self.settings.feedback.enabled = !self.settings.feedback.enabled
            }
            ConfigKey::FeedbackDiagnostics => {
                self.settings.feedback.include_diagnostics =
                    !self.settings.feedback.include_diagnostics
            }
            ConfigKey::AdvancedToml => {
                self.open_raw_config();
                return Ok(());
            }
            _ => return Ok(()),
        }
        self.save_and_apply_settings()?;
        self.status = format!("{} updated", config_label(key));
        Ok(())
    }

    fn active_profile_mut(&mut self) -> Result<&mut crate::config::ProviderProfile> {
        self.settings
            .profiles
            .get_mut(&self.settings.default_profile)
            .context("default profile does not exist")
    }

    fn begin_config_edit(&mut self, key: ConfigKey) {
        let value = self.config_value(key);
        let mut input = InputBuffer::new();
        input.insert_str(&value);
        if let Some(panel) = &mut self.config_panel {
            panel.editing = Some((key, input));
        }
    }

    fn commit_config_edit(&mut self) {
        let edit = self
            .config_panel
            .as_mut()
            .and_then(|panel| panel.editing.take());
        let Some((key, input)) = edit else {
            return;
        };
        let value = input.text();
        let result = match key {
            ConfigKey::Model => self.active_profile_mut().map(|profile| {
                profile.model = value.trim().to_owned();
            }),
            ConfigKey::BaseUrl => self.active_profile_mut().map(|profile| {
                profile.base_url = value.trim().trim_end_matches('/').to_owned();
            }),
            ConfigKey::MaxSteps => value
                .trim()
                .parse::<usize>()
                .context("max steps must be a number")
                .and_then(|number| {
                    if !(1..=128).contains(&number) {
                        bail!("max steps must be between 1 and 128");
                    }
                    self.settings.agent.max_steps = number;
                    Ok(())
                }),
            ConfigKey::ToolOutputLimit => value
                .trim()
                .parse::<usize>()
                .context("tool output limit must be a number")
                .and_then(|number| {
                    if !(2_000..=200_000).contains(&number) {
                        bail!("tool output limit must be between 2000 and 200000");
                    }
                    self.settings.agent.tool_output_limit = number;
                    Ok(())
                }),
            ConfigKey::FeedbackEndpoint => {
                crate::feedback::FeedbackClient::new(value.trim()).map(|_| {
                    self.settings.feedback.endpoint = value.trim().to_owned();
                })
            }
            _ => Ok(()),
        }
        .and_then(|()| self.save_and_apply_settings());
        match result {
            Ok(()) => self.status = format!("{} saved", config_label(key)),
            Err(error) => {
                self.status = format!("configuration error: {error:#}");
                self.begin_config_edit(key);
            }
        }
    }

    fn config_value(&self, key: ConfigKey) -> String {
        let profile = self.settings.profiles.get(&self.settings.default_profile);
        match key {
            ConfigKey::Profile => self.settings.default_profile.clone(),
            ConfigKey::Model => profile.map(|value| value.model.clone()).unwrap_or_default(),
            ConfigKey::BaseUrl => profile
                .map(|value| value.base_url.clone())
                .unwrap_or_default(),
            ConfigKey::Protocol => profile
                .map(|value| format!("{:?}", value.protocol))
                .unwrap_or_default(),
            ConfigKey::Permission => format!("{:?}", self.settings.ui.permission_mode),
            ConfigKey::VimMode => on_off(self.settings.ui.vim_mode),
            ConfigKey::Animations => on_off(self.settings.ui.animations),
            ConfigKey::Tooltips => on_off(self.settings.ui.show_tooltips),
            ConfigKey::MaxSteps => self.settings.agent.max_steps.to_string(),
            ConfigKey::ToolOutputLimit => self.settings.agent.tool_output_limit.to_string(),
            ConfigKey::ProjectTrust => on_off(self.settings.trust.contains(&self.config.workspace)),
            ConfigKey::FeedbackEnabled => on_off(self.settings.feedback.enabled),
            ConfigKey::FeedbackDiagnostics => on_off(self.settings.feedback.include_diagnostics),
            ConfigKey::FeedbackEndpoint => self.settings.feedback.endpoint.clone(),
            ConfigKey::AdvancedToml => format!(
                "{} skills · {} plugins · {} MCP servers",
                self.settings.skills.paths.len(),
                self.settings.plugins.paths.len(),
                self.settings.mcp.len()
            ),
        }
    }

    fn open_feedback(&mut self) {
        if !self.settings.feedback.enabled {
            self.entries.push(Entry {
                kind: EntryKind::Error,
                text: "Feedback is disabled. Enable it in /config.".to_owned(),
            });
            self.follow = true;
            return;
        }
        self.feedback_form = Some(FeedbackForm {
            input: InputBuffer::new(),
            category: 0,
            include_diagnostics: self.settings.feedback.include_diagnostics,
            sending: false,
            error: None,
        });
    }

    fn submit_feedback(&mut self) {
        let Some(form) = &mut self.feedback_form else {
            return;
        };
        let message = form.input.text();
        if message.trim().is_empty() {
            form.error = Some("Describe what happened or what you would like changed.".to_owned());
            return;
        }
        form.sending = true;
        form.error = None;
        let include_diagnostics = form.include_diagnostics;
        let category = FEEDBACK_CATEGORIES[form.category].to_ascii_lowercase();
        let payload = crate::feedback::FeedbackPayload {
            category,
            message: message.trim().to_owned(),
            include_diagnostics,
            diagnostics: if include_diagnostics {
                self.services.diagnostics()
            } else {
                Vec::new()
            },
            session_id: self.session.as_ref().map(|session| session.id.to_string()),
            workspace: self.config.workspace_name().to_owned(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
        };
        let endpoint = self.settings.feedback.endpoint.clone();
        let sender = self.feedback_tx.clone();
        tokio::spawn(async move {
            let result = match crate::feedback::FeedbackClient::new(&endpoint) {
                Ok(client) => client
                    .submit(&payload)
                    .await
                    .map_err(|error| format!("{error:#}")),
                Err(error) => Err(format!("{error:#}")),
            };
            let _ = sender.send(FeedbackResult { result });
        });
    }

    fn drain_feedback_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.feedback_rx.try_recv() {
            changed = true;
            match event.result {
                Ok(receipt) => {
                    self.feedback_form = None;
                    let reference = receipt
                        .id
                        .map(|id| format!(" Reference: {id}."))
                        .unwrap_or_default();
                    self.entries.push(Entry {
                        kind: EntryKind::System,
                        text: format!("Thank you — your feedback was sent.{reference}"),
                    });
                    self.status = "feedback sent".to_owned();
                    self.follow = true;
                }
                Err(error) => {
                    if let Some(form) = &mut self.feedback_form {
                        form.sending = false;
                        form.error = Some(format!(
                            "Could not send feedback: {error}\nThe endpoint is a placeholder until the Empero API is available."
                        ));
                    }
                }
            }
        }
        changed
    }

    fn start_services_reload(&mut self) {
        if !self.reload_services || self.services_reloading || self.running.is_some() {
            return;
        }
        self.reload_services = false;
        self.services_reloading = true;
        self.status = "reloading extensions".to_owned();
        let workspace = self.config.workspace.clone();
        let paths = self.config.paths.clone();
        let settings = self.settings.clone();
        let sender = self.services_tx.clone();
        tokio::spawn(async move {
            let result = AgentServices::discover(&workspace, &paths, &settings)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(ServicesResult { result });
        });
    }

    fn drain_services_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.services_rx.try_recv() {
            changed = true;
            self.services_reloading = false;
            match event.result {
                Ok(services) => {
                    self.services = Arc::new(services);
                    self.status = "configuration active".to_owned();
                }
                Err(error) => {
                    self.entries.push(Entry {
                        kind: EntryKind::Error,
                        text: format!(
                            "Configuration saved, but extensions could not reload: {error}"
                        ),
                    });
                    self.status = "extension reload failed".to_owned();
                    self.follow = true;
                }
            }
        }
        changed
    }

    /// Ctrl+C is contextual: the first press interrupts an active turn or clears
    /// a non-empty prompt; a second press within the window exits. This gives the
    /// familiar "press Ctrl+C twice to quit" escape hatch without making a single
    /// stray press tear down the session.
    fn handle_ctrl_c(&mut self) {
        const DOUBLE_TAP: Duration = Duration::from_secs(2);
        let now = Instant::now();
        if self
            .last_ctrl_c
            .is_some_and(|previous| now.duration_since(previous) < DOUBLE_TAP)
        {
            self.quit = true;
            return;
        }
        self.last_ctrl_c = Some(now);
        if self.running.is_some() {
            self.interrupt();
            self.queued_message = None;
            self.status = "interrupted · Ctrl+C again to exit".to_owned();
        } else if !self.input.text().trim().is_empty() {
            self.input.clear();
            self.status = "cleared · Ctrl+C again to exit".to_owned();
        } else {
            self.status = "Press Ctrl+C again to exit".to_owned();
        }
    }

    fn interrupt(&mut self) {
        if let Some(state) = &mut self.ralph_loop {
            state.cancel();
        }
        if let Some(handle) = self.running.take() {
            handle.abort();
            self.approval = None;
            self.receiving_delta = false;
            self.entries.push(Entry {
                kind: EntryKind::System,
                text: "Interrupted.".to_owned(),
            });
            self.status = "interrupted".to_owned();
            self.follow = true;
        }
        self.persist_session();
    }

    fn scroll_up(&mut self, amount: u16) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(amount);
    }

    fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount);
    }

    /// Record a submitted prompt into history (deduplicated against the most
    /// recent entry so repeats from queued-message resend don't clutter it).
    fn record_history(&mut self, prompt: &str) {
        if prompt.is_empty() {
            return;
        }
        if self.input_history.last().is_none_or(|last| last != prompt) {
            self.input_history.push(prompt.to_owned());
        }
        self.input_history_index = None;
    }

    /// Recall the previous prompt from history (arrow up). The first press saves
    /// the current live input so Down can restore it.
    fn history_prev(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let (row, _) = self.input.cursor_position();
        // Only navigate history when on the first line of the input; otherwise
        // Up moves the cursor within a multi-line input.
        if row > 0 {
            self.input.move_up();
            return;
        }
        if self.input_history_index.is_none() {
            // Save current input and jump to the latest entry.
            self.input_history_index = Some(self.input_history.len());
        }
        if let Some(index) = self.input_history_index
            && index > 0
        {
            let target = index - 1;
            self.input_history_index = Some(target);
            let entry = self.input_history[target].clone();
            self.input.clear();
            self.input.insert_str(&entry);
        }
    }

    /// Navigate forward through history (arrow down), restoring the live input
    /// when we run past the oldest entry.
    fn history_next(&mut self) {
        let (row, _) = self.input.cursor_position();
        let lines = self.input.line_count();
        // Only navigate history when on the last line of the input; otherwise
        // Down moves the cursor within a multi-line input.
        if row + 1 < lines {
            self.input.move_down();
            return;
        }
        if let Some(index) = &mut self.input_history_index {
            *index += 1;
            if *index >= self.input_history.len() {
                // Past the end — restore the live (now empty) input.
                self.input_history_index = None;
                self.input.clear();
            } else {
                self.input.clear();
                self.input.insert_str(&self.input_history[*index]);
            }
        } else {
            self.input.move_down();
        }
    }

    /// Fire a message queued with `submit` while a turn was running. Called once
    /// the agent finishes (Done) so the user can steer without retyping.
    fn drain_queued_message(&mut self) {
        if self.running.is_some() {
            return;
        }
        if let Some(prompt) = self.queued_message.take() {
            self.submit_prompt(prompt);
        }
    }
}

pub async fn run(
    config: Config,
    settings: Settings,
    credentials: Credentials,
    session: Option<Session>,
    session_store: Option<SessionStore>,
    services: Arc<AgentServices>,
) -> Result<()> {
    // Resolve dark/light (auto-detecting the terminal/OS appearance) before the
    // first frame so the Empero palette matches the surrounding terminal.
    crate::theme::set_active(crate::theme::Theme::for_mode(settings.ui.theme.resolve()));
    let session_id = session.as_ref().map(|session| session.id.to_string());
    services
        .run_hooks(
            "session_start",
            session_id.as_deref(),
            &json!({"workspace":config.workspace,"mode":"tui"}),
        )
        .await?;
    // Anonymous activity ping for the Empero dashboard (best-effort, opt-out).
    let reporter = ActivityReporter::new(
        settings.activity.enabled,
        &settings.activity.endpoint,
        &config.paths,
    );
    let activity_session = session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let activity_model = config.model.clone();
    if let Some(reporter) = &reporter {
        reporter
            .report_start(&activity_session, &activity_model)
            .await;
    }
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        SetTitle(format!("Abacus — {}", config.workspace_name()))
    )?;
    // Kitty keyboard protocol: lets the terminal distinguish Shift+Enter from
    // plain Enter (and report press/release/repeat). The escape sequence is
    // harmless on terminals that don't understand it — they simply ignore it —
    // so we push it unconditionally rather than gating on a capability query
    // that returns false on macOS Terminal.app and many SSH muxers.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
        )
    );
    let restore = TerminalRestore {
        keyboard_enhanced: true,
    };
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let workspace = config.workspace.clone();
    let mut app = App::new(
        config,
        settings,
        credentials,
        session,
        session_store,
        services.clone(),
    )?;
    // Heartbeat the open session so the dashboard shows live tokens and so a
    // session that is killed (terminal closed) drops off "active" instead of
    // lingering. The shared token counter survives model switches.
    let heartbeat = reporter.clone().map(|reporter| {
        let tokens = app.tokens.clone();
        let session = activity_session.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(
                crate::activity::HEARTBEAT_INTERVAL_SECS,
            ));
            ticker.tick().await; // the first tick fires immediately; skip it
            loop {
                ticker.tick().await;
                reporter
                    .report_heartbeat(&session, tokens.load(std::sync::atomic::Ordering::Relaxed))
                    .await;
            }
        })
    });
    let result = event_loop(&mut terminal, &mut app).await;
    if let Some(handle) = heartbeat {
        handle.abort();
    }
    let end_services = app.services.clone();
    let end_session_id = app.session.as_ref().map(|session| session.id.to_string());
    let tokens_used = app.provider.tokens_used();
    let duration_secs = app.started.elapsed().as_secs();
    drop(terminal);
    drop(restore);
    let status = if result.is_ok() {
        "completed"
    } else {
        "failed"
    };
    let hook_result = end_services
        .run_hooks(
            "session_end",
            end_session_id.as_deref(),
            &json!({"workspace":workspace,"mode":"tui","status":status}),
        )
        .await;
    if let Some(reporter) = &reporter {
        reporter
            .report_end(&activity_session, tokens_used, duration_secs)
            .await;
    }
    result?;
    hook_result?;
    Ok(())
}

struct TerminalRestore {
    keyboard_enhanced: bool,
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        if self.keyboard_enhanced {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            Show
        );
    }
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut dirty = true;
    while !app.quit {
        dirty |= app.drain_agent_events();
        dirty |= app.drain_feedback_events();
        dirty |= app.drain_services_events();
        app.start_services_reload();
        if dirty {
            terminal.draw(|frame| draw(frame, app))?;
            dirty = false;
        }

        let wait = if app.running.is_some() { 60 } else { 150 };
        if event::poll(Duration::from_millis(wait))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    handle_key(app, key);
                    dirty = true;
                }
                Event::Mouse(mouse) => {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => app.scroll_up(3),
                        MouseEventKind::ScrollDown => app.scroll_down(3),
                        _ => {}
                    }
                    dirty = true;
                }
                Event::Paste(text) if app.approval.is_none() => {
                    if let Some(editor) = &mut app.raw_config {
                        editor.input.insert_str(&text);
                    } else if let Some(form) = &mut app.feedback_form {
                        if !form.sending {
                            form.input.insert_str(&text);
                        }
                    } else if let Some((_, input)) = app
                        .config_panel
                        .as_mut()
                        .and_then(|panel| panel.editing.as_mut())
                    {
                        input.insert_str(&text);
                    } else if app.usage_panel.is_none() && app.mode == InputMode::Insert {
                        app.input.insert_str(&text);
                    }
                    dirty = true;
                }
                _ => {}
            }
        }
        if app.running.is_some() {
            dirty = true;
        }
    }
    app.interrupt();
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    // The "Ctrl+C twice to exit" window only counts consecutive Ctrl+C presses;
    // any other key cancels a pending exit so a later interrupt is never misread
    // as a quit.
    let is_ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    if !is_ctrl_c {
        app.last_ctrl_c = None;
    }
    if app.approval.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => app.decide(ApprovalDecision::Once),
            KeyCode::Char('a') => app.decide(ApprovalDecision::Always),
            KeyCode::Char('n') | KeyCode::Esc => app.decide(ApprovalDecision::Reject),
            KeyCode::Char('k') | KeyCode::Up => {
                app.approval_scroll = app.approval_scroll.saturating_sub(1)
            }
            KeyCode::Char('j') | KeyCode::Down => {
                app.approval_scroll = app.approval_scroll.saturating_add(1)
            }
            KeyCode::PageUp => app.approval_scroll = app.approval_scroll.saturating_sub(10),
            KeyCode::PageDown => app.approval_scroll = app.approval_scroll.saturating_add(10),
            KeyCode::Char('v') => {
                if let Some(approval) = &mut app.approval
                    && approval.diff.is_some()
                {
                    approval.view = if approval.view == ApprovalView::Unified {
                        ApprovalView::Raw
                    } else {
                        ApprovalView::Unified
                    };
                    app.approval_scroll = 0;
                    app.approval_horizontal = 0;
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                app.approval_horizontal = app.approval_horizontal.saturating_sub(4)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                app.approval_horizontal = app.approval_horizontal.saturating_add(4)
            }
            KeyCode::Home => {
                app.approval_scroll = 0;
                app.approval_horizontal = 0;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.handle_ctrl_c()
            }
            _ => {}
        }
        return;
    }

    // ask_user modal: navigate options, toggle selected (multi-select),
    // type a custom answer, confirm with Enter, cancel with Esc.
    if let Some(question) = &mut app.question {
        if question.editing_custom {
            match key.code {
                KeyCode::Esc => {
                    question.editing_custom = false;
                }
                KeyCode::Enter => {
                    // Confirm: submit whatever is in the custom field,
                    // merging in any toggled options.
                    app.answer_user_question();
                }
                KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    question.custom.delete_word_backward()
                }
                KeyCode::Backspace => question.custom.backspace(),
                KeyCode::Delete => question.custom.delete(),
                KeyCode::Left
                    if key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                {
                    question.custom.move_word_backward()
                }
                KeyCode::Right
                    if key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
                {
                    question.custom.move_word_forward()
                }
                KeyCode::Left => question.custom.move_left(),
                KeyCode::Right => question.custom.move_right(),
                KeyCode::Home => question.custom.move_start(),
                KeyCode::End => question.custom.move_end(),
                KeyCode::Up => question.custom.move_up(),
                KeyCode::Down => question.custom.move_down(),
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    question.custom.delete_word_backward()
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    question.custom.delete_to_start()
                }
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    question.custom.delete_to_end()
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    question.custom.insert(ch)
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => {
                // Cancelling sends the current state — whatever is
                // toggled/typed. If nothing's set, the agent sees the
                // "User skipped the question" message above.
                app.answer_user_question();
            }
            KeyCode::Enter => app.answer_user_question(),
            KeyCode::Up | KeyCode::Char('k') => {
                if question.cursor == 0 && !question.options.is_empty() {
                    question.cursor = question.options.len() - 1;
                } else {
                    question.cursor = question.cursor.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if question.options.is_empty() {
                    question.cursor = 0;
                } else {
                    question.cursor = (question.cursor + 1) % question.options.len();
                }
            }
            KeyCode::Char(' ') => {
                if !question.options.is_empty() && question.multi_select {
                    let idx = question.cursor;
                    if let Some(slot) = question.selected.get_mut(idx) {
                        *slot = !*slot;
                    }
                }
            }
            KeyCode::Char('x') => {
                if !question.options.is_empty() {
                    if question.multi_select {
                        if let Some(slot) = question.selected.get_mut(question.cursor) {
                            *slot = !*slot;
                        }
                    } else {
                        // Single-select: clear all and toggle this one on,
                        // then confirm immediately.
                        for slot in question.selected.iter_mut() {
                            *slot = false;
                        }
                        if let Some(slot) = question.selected.get_mut(question.cursor) {
                            *slot = true;
                        }
                        app.answer_user_question();
                    }
                }
            }
            KeyCode::Char('t') => {
                // Tab to the custom text field.
                question.editing_custom = true;
            }
            _ => {}
        }
        return;
    }

    if app.raw_config.is_some() {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            app.save_raw_config();
        } else if key.code == KeyCode::Esc {
            app.raw_config = None;
            app.status = "configuration edit cancelled".to_owned();
        } else if let Some(editor) = &mut app.raw_config {
            edit_buffer(&mut editor.input, key, true);
        }
        return;
    }

    if app.feedback_form.is_some() {
        let sending = app.feedback_form.as_ref().is_some_and(|form| form.sending);
        if sending {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            app.submit_feedback();
        } else if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            if let Some(form) = &mut app.feedback_form {
                form.include_diagnostics = !form.include_diagnostics;
            }
        } else if key.code == KeyCode::Tab {
            if let Some(form) = &mut app.feedback_form {
                form.category = (form.category + 1) % FEEDBACK_CATEGORIES.len();
            }
        } else if key.code == KeyCode::Esc {
            app.feedback_form = None;
        } else if let Some(form) = &mut app.feedback_form {
            edit_buffer(&mut form.input, key, true);
        }
        return;
    }

    if app.config_panel.is_some() {
        let editing = app
            .config_panel
            .as_ref()
            .and_then(|panel| panel.editing.as_ref())
            .is_some();
        if editing {
            if key.code == KeyCode::Esc {
                if let Some(panel) = &mut app.config_panel {
                    panel.editing = None;
                }
            } else if key.code == KeyCode::Enter {
                app.commit_config_edit();
            } else if let Some((_, input)) = app
                .config_panel
                .as_mut()
                .and_then(|panel| panel.editing.as_mut())
            {
                edit_buffer(input, key, false);
            }
        } else {
            let mut activate = false;
            if let Some(panel) = &mut app.config_panel {
                match key.code {
                    KeyCode::Char('k') | KeyCode::Up => {
                        panel.selected = panel.selected.saturating_sub(1)
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        panel.selected = (panel.selected + 1).min(CONFIG_KEYS.len() - 1)
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => activate = true,
                    KeyCode::Esc | KeyCode::Char('q') => app.config_panel = None,
                    _ => {}
                }
            }
            if activate {
                let key = app
                    .config_panel
                    .as_ref()
                    .map(|panel| CONFIG_KEYS[panel.selected]);
                if let Some(key) = key {
                    if config_key_is_editable(key) {
                        app.begin_config_edit(key);
                    } else if let Err(error) = app.cycle_config_value(key) {
                        app.status = format!("configuration error: {error:#}");
                    }
                }
            }
        }
        return;
    }

    if let Some(panel) = &mut app.usage_panel {
        match key.code {
            KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
                panel.tab = if panel.tab == UsageTab::Overview {
                    UsageTab::Models
                } else {
                    UsageTab::Overview
                };
            }
            KeyCode::Char('r') => panel.range = panel.range.next(),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => app.usage_panel = None,
            _ => {}
        }
        return;
    }

    if app.picker.is_some() {
        let mut accept = false;
        let mut cancel = false;
        if let Some(picker) = &mut app.picker {
            match key.code {
                KeyCode::Char('k') | KeyCode::Up => {
                    picker.selected = picker.selected.saturating_sub(1)
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    picker.selected =
                        (picker.selected + 1).min(picker.items.len().saturating_sub(1))
                }
                KeyCode::Enter => accept = true,
                KeyCode::Esc | KeyCode::Char('q') => cancel = true,
                _ => {}
            }
        }
        if accept {
            let value = app
                .picker
                .as_ref()
                .and_then(|picker| picker.items.get(picker.selected))
                .map(|(_, value)| value.clone());
            app.picker = None;
            if let Some(value) = value {
                app.resume_session(&value);
            }
        } else if cancel {
            app.picker = None;
        }
        return;
    }

    if app.show_help {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
            app.show_help = false;
        }
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('q') => {
                app.quit = true;
                return;
            }
            KeyCode::Char('c') => {
                app.handle_ctrl_c();
                return;
            }
            _ => {}
        }
    }
    if key.code == KeyCode::F(1) {
        app.show_help = true;
        return;
    }
    if key.code == KeyCode::BackTab {
        app.toggle_agent_mode();
        return;
    }

    match app.mode {
        InputMode::Insert => handle_insert_key(app, key),
        InputMode::Normal => handle_normal_key(app, key),
    }
}

fn handle_insert_key(app: &mut App, key: KeyEvent) {
    let modified_enter = key
        .modifiers
        .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT);
    match key.code {
        KeyCode::Esc if app.settings.ui.vim_mode => app.mode = InputMode::Normal,
        KeyCode::Esc => {}
        KeyCode::Enter if modified_enter => app.input.insert('\n'),
        KeyCode::Enter => app.submit(),
        // Ctrl+J is a real control byte every terminal forwards, so it is the
        // reliable newline even where Shift/Alt+Enter are indistinguishable from
        // plain Enter (e.g. macOS Terminal.app). It may arrive as Char('j')+Ctrl
        // or, on some terminals, fold into Enter+Ctrl (covered by modified_enter).
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.insert('\n')
        }
        // Ctrl+O is the universal newline fallback: it sends 0x0f, a distinct
        // byte that no terminal confuses with Enter. Use this when Shift+Enter
        // doesn't work (macOS Terminal.app, many SSH muxers).
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.insert('\n')
        }
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.delete_word_backward()
        }
        KeyCode::Backspace => app.input.backspace(),
        KeyCode::Delete => app.input.delete(),
        KeyCode::Left
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            app.input.move_word_backward()
        }
        KeyCode::Right
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            app.input.move_word_forward()
        }
        KeyCode::Left => app.input.move_left(),
        KeyCode::Right => app.input.move_right(),
        KeyCode::Up => app.history_prev(),
        KeyCode::Down => app.history_next(),
        KeyCode::Home => app.input.move_start(),
        KeyCode::End => app.input.move_end(),
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.delete_word_backward()
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.delete_to_start()
        }
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.delete_to_end()
        }
        KeyCode::PageUp => app.scroll_up(app.transcript_height.saturating_sub(2)),
        KeyCode::PageDown => app.scroll_down(app.transcript_height.saturating_sub(2)),
        KeyCode::Tab => complete_at_cursor(app),
        // Ctrl+V: paste from the system clipboard. Bracketed paste
        // (EnableBracketedPaste) already handles terminals that send paste
        // contents as an Event::Paste; this covers terminals that send the raw
        // Ctrl+V byte instead, and any terminal without bracketed-paste support.
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(text) = clipboard_text() {
                app.input.insert_str(&text);
            }
        }
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.input.insert(ch)
        }
        _ => {}
    }
}

fn handle_normal_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('u') => app.scroll_up(app.transcript_height / 2),
            KeyCode::Char('d') => app.scroll_down(app.transcript_height / 2),
            _ => {}
        }
        return;
    }

    let prefix = app.normal_prefix.take();
    match (prefix, key.code) {
        (Some('d'), KeyCode::Char('d')) => app.input.clear(),
        (Some('g'), KeyCode::Char('g')) => {
            app.scroll = 0;
            app.follow = false;
        }
        (_, KeyCode::Char('d')) => app.normal_prefix = Some('d'),
        (_, KeyCode::Char('g')) => app.normal_prefix = Some('g'),
        (_, KeyCode::Char('i')) => app.mode = InputMode::Insert,
        (_, KeyCode::Char('a')) => {
            app.input.move_right();
            app.mode = InputMode::Insert;
        }
        (_, KeyCode::Char('A')) => {
            app.input.move_end();
            app.mode = InputMode::Insert;
        }
        (_, KeyCode::Char('I')) => {
            app.input.move_start();
            app.mode = InputMode::Insert;
        }
        (_, KeyCode::Char('h')) | (_, KeyCode::Left) => app.input.move_left(),
        (_, KeyCode::Char('l')) | (_, KeyCode::Right) => app.input.move_right(),
        (_, KeyCode::Char('w')) => app.input.move_word_forward(),
        (_, KeyCode::Char('b')) => app.input.move_word_backward(),
        (_, KeyCode::Char('0')) | (_, KeyCode::Home) => app.input.move_start(),
        (_, KeyCode::Char('$')) | (_, KeyCode::End) => app.input.move_end(),
        (_, KeyCode::Char('x')) | (_, KeyCode::Delete) => app.input.delete(),
        (_, KeyCode::Char('j')) | (_, KeyCode::Down) => app.scroll_down(1),
        (_, KeyCode::Char('k')) | (_, KeyCode::Up) => app.scroll_up(1),
        (_, KeyCode::PageUp) => app.scroll_up(app.transcript_height.saturating_sub(2)),
        (_, KeyCode::PageDown) => app.scroll_down(app.transcript_height.saturating_sub(2)),
        (_, KeyCode::Char('G')) => {
            app.follow = true;
        }
        (_, KeyCode::Enter) => app.mode = InputMode::Insert,
        (_, KeyCode::Char('?')) => app.show_help = true,
        (_, KeyCode::Char('q')) if app.running.is_none() => app.quit = true,
        _ => {}
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let input_height = (app.input.line_count() as u16 + 2).clamp(3, 9);
    let task_height = u16::from(app.goal.snapshot().is_some() || app.ralph_loop.is_some()) * 2
        + u16::from(!app.tasks.is_empty());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(task_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, chunks[0], app);
    let transcript = content_rect(chunks[1], 112);
    draw_transcript(frame, transcript, app);
    if task_height > 0 {
        draw_task_bar(frame, content_rect(chunks[2], 112), app);
    }
    let input = content_rect(chunks[3], 112);
    draw_input(frame, input, app);
    draw_footer(frame, content_rect(chunks[4], 112), app);
    draw_completion_popup(frame, input, app);
    if app.raw_config.is_some() {
        draw_raw_config(frame, area, app);
    } else if app.feedback_form.is_some() {
        draw_feedback(frame, area, app);
    } else if app.config_panel.is_some() {
        draw_config(frame, area, app);
    } else if app.usage_panel.is_some() {
        draw_usage(frame, area, app);
    } else if app.show_help {
        draw_help(frame, area);
    } else if app.approval.is_some() {
        draw_approval(frame, area, app);
    } else if app.question.is_some() {
        draw_user_question(frame, area, app);
    } else if app.picker.is_some() {
        draw_picker(frame, area, app);
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let visible_agent_mode = app.resolved_agent_mode.unwrap_or(app.agent_mode);
    let state = if app.running.is_some() && app.settings.ui.animations {
        let frames = ["·", "×", "+", "×"];
        let index = (app.started.elapsed().as_millis() / 180) as usize % frames.len();
        frames[index]
    } else if app.running.is_some() {
        "•"
    } else {
        "·"
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);
    let first = vec![
        Span::styled(
            " ABACUS ",
            Style::default()
                .fg(inverse())
                .bg(secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {state}  {}", app.config.workspace_name()),
            Style::default().fg(text()),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(first)), columns[0]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                single_line(&app.config.model, 34),
                Style::default().fg(text()),
            ),
            Span::styled("  ·  ", Style::default().fg(border())),
            Span::styled(
                visible_agent_mode.label(),
                Style::default()
                    .fg(match visible_agent_mode {
                        AgentMode::Auto => primary(),
                        AgentMode::Plan => warning(),
                        AgentMode::Build => success(),
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .alignment(Alignment::Right),
        columns[1],
    );
    let second = Rect {
        x: area.x,
        y: area.y.saturating_add(1),
        width: area.width,
        height: 1,
    };
    let project = app.config.workspace.to_string_lossy();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(single_line(&project, 70), Style::default().fg(muted())),
            Span::styled(
                format!("  ·  profile {}", app.config.profile),
                Style::default().fg(muted()),
            ),
        ])),
        second,
    );
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    app.transcript_height = area.height;
    if app.entries.is_empty() {
        draw_welcome(frame, area, app);
        return;
    }
    let width = area.width.max(1) as usize;
    let (mut text, visual_lines) = transcript_text(&app.entries, width);
    // Pad the bottom with 2 blank lines so the last content lines always have
    // breathing room above the input bar, regardless of how the visual-line
    // estimate drifts from ratatui's actual wrapping.
    text.lines.push(Line::from(""));
    text.lines.push(Line::from(""));
    let max_scroll = (visual_lines + 2)
        .saturating_sub(area.height as usize)
        .min(u16::MAX as usize) as u16;
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll >= max_scroll {
            app.follow = true;
        }
    }
    let paragraph = Paragraph::new(text)
        .scroll((app.scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_welcome(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let compact = area.width < 72 || area.height < 15;
    let height = if app.settings.ui.show_tooltips && !compact {
        13
    } else {
        7
    };
    let welcome = centered_rect(area.width.min(76), height, area);
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "ABACUS",
            Style::default()
                .fg(secondary())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "A focused coding agent for your terminal",
            Style::default().fg(muted()),
        )),
        Line::from(""),
    ];
    if app.settings.ui.show_tooltips && !compact {
        lines.extend([
            Line::from(vec![
                Span::styled(
                    "  Build  ",
                    Style::default().fg(success()).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Describe a change and Abacus will inspect, edit, and verify."),
            ]),
            Line::from(vec![
                Span::styled(
                    "  Plan   ",
                    Style::default().fg(warning()).add_modifier(Modifier::BOLD),
                ),
                Span::raw("AUTO chooses the workflow; Shift+Tab pins a mode."),
            ]),
            Line::from(vec![
                Span::styled(
                    "  Goal   ",
                    Style::default().fg(primary()).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Use /goal for a persistent definition of done."),
            ]),
            Line::from(vec![
                Span::styled(
                    "  Loop   ",
                    Style::default()
                        .fg(secondary())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("Use /loop for autonomous, promise-driven iteration."),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Type / for commands  ·  @file to attach context  ·  F1 for help",
                Style::default().fg(muted()),
            )),
        ]);
    } else {
        lines.push(Line::from(Span::styled(
            "Type a request or / for commands",
            Style::default().fg(muted()),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border())),
            ),
        welcome,
    );
}

fn draw_task_bar(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();
    if let Some(goal) = app.goal.snapshot() {
        let (icon, color) = match goal.status {
            crate::goal::GoalStatus::Active => ("●", primary()),
            crate::goal::GoalStatus::Paused => ("Ⅱ", warning()),
            crate::goal::GoalStatus::Complete => ("✓", success()),
            crate::goal::GoalStatus::Cancelled => ("×", muted()),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {icon} Goal  "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                single_line(&goal.objective, 74),
                Style::default().fg(text()),
            ),
            Span::styled(
                "   /goal pause · edit · clear",
                Style::default().fg(muted()),
            ),
        ]));
    }
    if let Some(state) = &app.ralph_loop {
        let color = match state.status {
            RalphStatus::Active => secondary(),
            RalphStatus::Paused => warning(),
            RalphStatus::Completed => success(),
            RalphStatus::Cancelled | RalphStatus::MaxIterations => muted(),
        };
        let limit = state
            .max_iterations
            .map(|value| value.to_string())
            .unwrap_or_else(|| "∞".to_owned());
        lines.push(Line::from(vec![
            Span::styled(
                " ↻ Loop  ",
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{} / {limit}  ·  promise: {}",
                    state.iteration,
                    single_line(&state.completion_promise, 28)
                ),
                Style::default().fg(text()),
            ),
            Span::styled("   /cancel-loop", Style::default().fg(muted())),
        ]));
    }
    let tasks = app.tasks.snapshot();
    if !tasks.is_empty() {
        let done = tasks.iter().filter(|task| task.done).count();
        let next = tasks
            .iter()
            .find(|task| !task.done)
            .map(|task| task.text.as_str());
        let progress = format!(" {done}/{} done", tasks.len());
        let mut spans = vec![
            Span::styled(
                " ▦ Tasks  ",
                Style::default()
                    .fg(secondary())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(progress, Style::default().fg(text())),
        ];
        if let Some(text) = next {
            spans.push(Span::styled(
                format!("   next: {}", single_line(text, 60)),
                Style::default().fg(muted()),
            ));
        }
        lines.push(Line::from(spans));
    }
    while lines.len() < area.height as usize {
        lines.push(Line::from(""));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default().bg(surface())),
        area,
    );
}

fn draw_completion_popup(frame: &mut Frame<'_>, input_area: Rect, app: &App) {
    if app.running.is_some()
        || app.config_panel.is_some()
        || app.raw_config.is_some()
        || app.feedback_form.is_some()
        || app.usage_panel.is_some()
    {
        return;
    }
    let Some((suggestions, title)) = active_completion(app) else {
        return;
    };
    // Clamp to the rows available above the input so a long list never overruns
    // the screen; if it doesn't all fit, the last row says how many remain.
    let room = (input_area.y as usize).saturating_sub(2).clamp(1, 14);
    let visible = suggestions.len().min(room);
    let truncated = suggestions.len() - visible;
    let mut lines = suggestions
        .iter()
        .take(visible)
        .map(|(value, description)| {
            Line::from(vec![
                Span::styled(
                    format!(" {value:<22}"),
                    Style::default().fg(primary()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(description.clone(), Style::default().fg(muted())),
            ])
        })
        .collect::<Vec<_>>();
    if truncated > 0 {
        lines.push(Line::from(Span::styled(
            format!(" … {truncated} more — keep typing to filter"),
            Style::default().fg(muted()),
        )));
    }
    let height = lines.len() as u16 + 2;
    let area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width: input_area.width.min(72),
        height,
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border()))
                .title(title),
        ),
        area,
    );
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let color = match app.mode {
        InputMode::Insert => primary(),
        InputMode::Normal => secondary(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            format!(" {} ", app.mode.label()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let text = app.input.text();
    let (cursor_row, cursor_col) = app.input.cursor_position();
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let visible_rows = area.height.saturating_sub(2).max(1) as usize;
    let input_scroll = cursor_row.saturating_sub(visible_rows.saturating_sub(1));

    // Cell width of the text before the cursor on its line. A long line is
    // scrolled horizontally so the cursor (and the text around it) stays on
    // screen instead of running off the right edge invisibly.
    let cursor_line = text.split('\n').nth(cursor_row).unwrap_or("");
    let cursor_prefix: String = cursor_line.chars().take(cursor_col).collect();
    let display_col = UnicodeWidthStr::width(cursor_prefix.as_str());
    let h_scroll = display_col.saturating_sub(inner_width.saturating_sub(1));

    let paragraph = if text.is_empty() {
        Paragraph::new(Span::styled(
            "Ask Abacus to inspect, explain, or change the code…",
            Style::default().fg(muted()),
        ))
    } else {
        Paragraph::new(text.as_str())
    }
    .scroll((input_scroll as u16, h_scroll as u16));
    frame.render_widget(paragraph.block(block), area);

    if app.approval.is_none()
        && !app.show_help
        && app.config_panel.is_none()
        && app.raw_config.is_none()
        && app.feedback_form.is_none()
        && app.usage_panel.is_none()
    {
        let x = area.x + 1 + (display_col - h_scroll) as u16;
        let visible_row = cursor_row.saturating_sub(input_scroll) as u16;
        let y = (area.y + 1 + visible_row).min(area.bottom().saturating_sub(2));
        frame.set_cursor_position((x, y));
    }
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let line = if let Some(approval) = &app.approval {
        Line::from(vec![
            Span::styled(
                " ALLOW? ",
                Style::default()
                    .fg(inverse())
                    .bg(warning())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " {}  {}  ",
                    approval.tool,
                    single_line(&approval.summary, 72)
                ),
                Style::default().fg(warning()),
            ),
            Span::styled(
                "y",
                Style::default().fg(success()).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" once  "),
            Span::styled(
                "a",
                Style::default().fg(success()).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" session  "),
            Span::styled(
                "n",
                Style::default().fg(danger()).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" reject"),
        ])
    } else {
        // Context-bar spans: show how full the live context window is and how
        // close to the auto-compaction threshold. Uses `ctx_chars` (updated from
        // streaming events during the turn) so the percentage moves live, not
        // just on Done. Chars-to-tokens uses the same 4:1 ratio as compaction.
        let chars = app.ctx_chars;
        let ctx_tokens = (chars / 4).max(1) as u64;
        let ctx_window = app.config.model_limits.context_window.max(1) as u64;
        let pct = ((ctx_tokens * 100) / ctx_window).min(100) as u16;
        let budget = app.config.model_limits.compaction_budget();
        let compact_tokens = (budget.compact_at_chars / 4).max(1) as u64;
        let near_compact = ctx_tokens >= compact_tokens;
        let ctx_color = if near_compact { warning() } else { muted() };
        Line::from(vec![
            Span::styled(format!(" {} ", app.status), Style::default().fg(muted())),
            Span::styled("/", Style::default().fg(primary())),
            Span::raw(" commands  "),
            Span::styled("ctrl+c", Style::default().fg(primary())),
            Span::raw(" interrupt  "),
            Span::styled("shift+tab", Style::default().fg(primary())),
            Span::raw(" mode  "),
            Span::styled(
                format!("{} tokens", format_count(app.provider.tokens_used())),
                Style::default().fg(muted()),
            ),
            Span::raw("  "),
            Span::styled(format!("ctx {pct}%"), Style::default().fg(ctx_color)),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(82, 24, area);
    frame.render_widget(Clear, popup);
    let text = Text::from(vec![
        Line::from(Span::styled(
            "ABACUS KEYS",
            Style::default()
                .fg(secondary())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        key_line("Enter", "send prompt"),
        key_line("Ctrl+J / Ctrl+O / Shift+Enter", "new line"),
        key_line("Ctrl+V", "paste from clipboard"),
        key_line("PgUp / PgDn / Ctrl+u / Ctrl+d", "scroll transcript"),
        key_line("Esc", "normal mode"),
        key_line("i / a / A / I", "enter insert mode"),
        key_line("h j k l / w b / 0 $", "Vim movement and scroll"),
        key_line("gg / G / Ctrl+u / Ctrl+d", "transcript navigation"),
        key_line("x / dd", "delete character / clear prompt"),
        key_line("Ctrl+c", "interrupt / clear prompt; twice to exit"),
        key_line("Shift+Tab", "cycle AUTO / PLAN / BUILD"),
        key_line("Ctrl+q / q", "quit"),
        Line::from(""),
        Line::from(Span::styled(
            "/goal  /loop  /cancel-loop  /config  /feedback  /mode  /model  /usage  /sessions",
            Style::default().fg(primary()),
        )),
        Line::from(Span::styled(
            "/swarm  /theme  /new  /compact  /skills  /plugins  /mcps  /tools  /quit",
            Style::default().fg(primary()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Press Esc, Enter, or ? to close",
            Style::default().fg(muted()),
        )),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(secondary()))
        .title(" help ");
    frame.render_widget(
        Paragraph::new(text).block(block).alignment(Alignment::Left),
        popup,
    );
}

fn draw_usage(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(panel) = &app.usage_panel else {
        return;
    };
    let width = area.width.saturating_sub(4).clamp(24, 112);
    let height = area.height.saturating_sub(2).clamp(12, 29);
    let popup = centered_rect(width, height, area);
    frame.render_widget(Clear, popup);

    let today = Local::now().date_naive();
    let records = panel
        .records
        .iter()
        .filter(|record| panel.range.includes(usage_date(record), today))
        .collect::<Vec<_>>();
    let inner_width = popup.width.saturating_sub(2) as usize;
    let mut lines = vec![usage_tabs(panel.tab), Line::from("")];
    match panel.tab {
        UsageTab::Overview => {
            lines.extend(usage_heatmap_lines(&records, inner_width));
            lines.push(usage_legend());
            lines.push(Line::from(""));
            lines.push(usage_range_line(panel.range));
            lines.push(Line::from(""));
            let stats = usage_stats(&records, today);
            if records.is_empty() {
                lines.push(Line::from(Span::styled(
                    " No activity in this date range yet.",
                    Style::default().fg(muted()),
                )));
            } else if inner_width >= 70 {
                lines.extend(usage_stats_wide(&stats, inner_width));
            } else {
                lines.extend(usage_stats_compact(&stats));
            }
        }
        UsageTab::Models => {
            lines.push(usage_range_line(panel.range));
            lines.push(Line::from(""));
            lines.extend(usage_model_lines(&records, &app.config.model, inner_width));
        }
    }
    let visible = popup.height.saturating_sub(2) as usize;
    lines.truncate(visible);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(secondary()))
        .title(Span::styled(
            " Usage ",
            Style::default().fg(text()).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(
            Line::from(" Tab view  ·  r dates  ·  Esc close ").alignment(Alignment::Right),
        );
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), popup);
}

fn usage_tabs(selected: UsageTab) -> Line<'static> {
    let tab = |label, active| {
        Span::styled(
            format!(" {label} "),
            if active {
                Style::default()
                    .fg(inverse())
                    .bg(primary())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted())
            },
        )
    };
    Line::from(vec![
        Span::raw(" "),
        tab("Overview", selected == UsageTab::Overview),
        Span::raw("  "),
        tab("Models", selected == UsageTab::Models),
    ])
}

fn usage_range_line(selected: UsageRange) -> Line<'static> {
    let choice = |label, range| {
        Span::styled(
            label,
            if selected == range {
                Style::default().fg(primary()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted())
            },
        )
    };
    Line::from(vec![
        Span::raw(" "),
        choice("All time", UsageRange::AllTime),
        Span::styled("  ·  ", Style::default().fg(border())),
        choice("Last 7 days", UsageRange::Last7Days),
        Span::styled("  ·  ", Style::default().fg(border())),
        choice("Last 30 days", UsageRange::Last30Days),
    ])
}

fn usage_heatmap_lines(records: &[&SessionUsage], width: usize) -> Vec<Line<'static>> {
    let today = Local::now().date_naive();
    let weeks = width.saturating_sub(4).div_ceil(2).clamp(8, 52);
    let this_monday = today - ChronoDuration::days(today.weekday().num_days_from_monday() as i64);
    let start = this_monday - ChronoDuration::weeks(weeks.saturating_sub(1) as i64);
    let mut daily = BTreeMap::<NaiveDate, u64>::new();
    for record in records {
        *daily.entry(usage_date(record)).or_default() += record.tokens_used.max(1);
    }
    let maximum = daily.values().copied().max().unwrap_or(1);

    let chart_width = 4 + weeks * 2;
    let mut months = vec![' '; chart_width];
    let mut previous_month = 0;
    for week in 0..weeks {
        let date = start + ChronoDuration::weeks(week as i64);
        if week == 0 || date.month() != previous_month {
            for (offset, character) in date.format("%b").to_string().chars().enumerate() {
                let position = 4 + week * 2 + offset;
                if position < months.len() {
                    months[position] = character;
                }
            }
        }
        previous_month = date.month();
    }
    let mut lines = vec![Line::from(Span::styled(
        months.into_iter().collect::<String>(),
        Style::default().fg(muted()),
    ))];
    for weekday in 0..7 {
        let label = match weekday {
            0 => "Mon ",
            2 => "Wed ",
            4 => "Fri ",
            _ => "    ",
        };
        let mut spans = vec![Span::styled(label, Style::default().fg(muted()))];
        for week in 0..weeks {
            let date = start + ChronoDuration::weeks(week as i64) + ChronoDuration::days(weekday);
            if date > today {
                spans.push(Span::raw("  "));
                continue;
            }
            let value = daily.get(&date).copied().unwrap_or(0);
            if value == 0 {
                spans.push(Span::styled("· ", Style::default().fg(border())));
                continue;
            }
            let level = ((value.saturating_mul(4).saturating_sub(1)) / maximum).clamp(0, 3);
            let (symbol, color, modifier) = match level {
                0 => ("▪ ", border(), Modifier::DIM),
                1 => ("▪ ", secondary(), Modifier::empty()),
                2 => ("■ ", secondary(), Modifier::BOLD),
                _ => ("■ ", primary(), Modifier::BOLD),
            };
            spans.push(Span::styled(
                symbol,
                Style::default().fg(color).add_modifier(modifier),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn usage_legend() -> Line<'static> {
    Line::from(vec![
        Span::styled("    Less  ", Style::default().fg(muted())),
        Span::styled("· ", Style::default().fg(border())),
        Span::styled("▪ ", Style::default().fg(border())),
        Span::styled("▪ ", Style::default().fg(secondary())),
        Span::styled("■ ", Style::default().fg(secondary())),
        Span::styled(
            "■ ",
            Style::default().fg(primary()).add_modifier(Modifier::BOLD),
        ),
        Span::styled("More", Style::default().fg(muted())),
    ])
}

fn usage_stats(records: &[&SessionUsage], today: NaiveDate) -> UsageStats {
    let mut stats = UsageStats {
        sessions: records.len(),
        ..UsageStats::default()
    };
    let mut dates = HashSet::new();
    let mut daily = BTreeMap::<NaiveDate, u64>::new();
    let mut models = HashMap::<String, (usize, u64)>::new();
    for record in records {
        let date = usage_date(record);
        dates.insert(date);
        *daily.entry(date).or_default() += record.tokens_used.max(1);
        let model = models.entry(record.model.clone()).or_default();
        model.0 += 1;
        model.1 = model.1.saturating_add(record.tokens_used);
        stats.total_tokens = stats.total_tokens.saturating_add(record.tokens_used);
        stats.tokens_estimated |= record.tokens_estimated;
        stats.longest_session = stats.longest_session.max(record.active_secs);
    }
    stats.active_days = dates.len();
    stats.favorite_model = models
        .into_iter()
        .max_by_key(|(_, (sessions, tokens))| (*tokens, *sessions))
        .map(|(model, _)| model);
    stats.most_active_day = daily
        .into_iter()
        .max_by_key(|(_, activity)| *activity)
        .map(|(date, _)| date);

    let mut sorted_dates = dates.into_iter().collect::<Vec<_>>();
    sorted_dates.sort_unstable();
    let mut run = 0;
    let mut previous = None;
    for date in &sorted_dates {
        run = if previous.is_some_and(|value| *date == value + ChronoDuration::days(1)) {
            run + 1
        } else {
            1
        };
        stats.longest_streak = stats.longest_streak.max(run);
        previous = Some(*date);
    }
    if let Some(last) = sorted_dates.last().copied()
        && last >= today - ChronoDuration::days(1)
    {
        let mut date = last;
        while sorted_dates.binary_search(&date).is_ok() {
            stats.current_streak += 1;
            date -= ChronoDuration::days(1);
        }
    }
    stats
}

fn usage_stats_wide(stats: &UsageStats, width: usize) -> Vec<Line<'static>> {
    let left_width = width / 2;
    vec![
        usage_stat_pair(
            "Favorite model",
            stats.favorite_model.as_deref().unwrap_or("—"),
            "Total tokens",
            &format!(
                "{}{}",
                if stats.tokens_estimated { "~" } else { "" },
                format_count(stats.total_tokens)
            ),
            left_width,
        ),
        usage_stat_pair(
            "Sessions",
            &stats.sessions.to_string(),
            "Longest session",
            &format_duration(stats.longest_session),
            left_width,
        ),
        usage_stat_pair(
            "Active days",
            &stats.active_days.to_string(),
            "Longest streak",
            &format!("{} days", stats.longest_streak),
            left_width,
        ),
        usage_stat_pair(
            "Most active day",
            &stats
                .most_active_day
                .map(|date| date.format("%b %-d").to_string())
                .unwrap_or_else(|| "—".to_owned()),
            "Current streak",
            &format!("{} days", stats.current_streak),
            left_width,
        ),
    ]
}

fn usage_stat_pair(
    left_label: &str,
    left_value: &str,
    right_label: &str,
    right_value: &str,
    left_width: usize,
) -> Line<'static> {
    let left_used = 2 + 17 + left_value.chars().count();
    let gap = left_width.saturating_sub(left_used).max(2);
    Line::from(vec![
        Span::styled(format!(" {left_label:<17}"), Style::default().fg(muted())),
        Span::styled(
            left_value.to_owned(),
            Style::default().fg(primary()).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(gap)),
        Span::styled(format!("{right_label:<17}"), Style::default().fg(muted())),
        Span::styled(
            right_value.to_owned(),
            Style::default().fg(primary()).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn usage_stats_compact(stats: &UsageStats) -> Vec<Line<'static>> {
    vec![
        usage_stat_line("Sessions", &stats.sessions.to_string()),
        usage_stat_line(
            "Total tokens",
            &format!(
                "{}{}",
                if stats.tokens_estimated { "~" } else { "" },
                format_count(stats.total_tokens)
            ),
        ),
        usage_stat_line(
            "Favorite model",
            stats.favorite_model.as_deref().unwrap_or("—"),
        ),
        usage_stat_line("Active days", &stats.active_days.to_string()),
    ]
}

fn usage_stat_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {label:<18}"), Style::default().fg(muted())),
        Span::styled(
            value.to_owned(),
            Style::default().fg(primary()).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn usage_model_lines(
    records: &[&SessionUsage],
    current_model: &str,
    width: usize,
) -> Vec<Line<'static>> {
    if records.is_empty() {
        return vec![Line::from(Span::styled(
            " No model activity in this date range yet.",
            Style::default().fg(muted()),
        ))];
    }
    let mut models = HashMap::<String, (usize, u64, u64)>::new();
    for record in records {
        let usage = models.entry(record.model.clone()).or_default();
        usage.0 += 1;
        usage.1 = usage.1.saturating_add(record.tokens_used);
        usage.2 = usage.2.saturating_add(record.active_secs);
    }
    let mut models = models.into_iter().collect::<Vec<_>>();
    models.sort_by_key(|(_, (sessions, tokens, _))| std::cmp::Reverse((*tokens, *sessions)));
    let maximum = models
        .iter()
        .map(|(_, (_, tokens, _))| *tokens)
        .max()
        .unwrap_or(1)
        .max(1);
    let bar_width = width.saturating_sub(55).clamp(6, 28);
    let mut lines = vec![Line::from(vec![
        Span::styled("   Model", Style::default().fg(muted())),
        Span::styled(
            "                    Sessions   Tokens",
            Style::default().fg(muted()),
        ),
    ])];
    for (model, (sessions, tokens, duration)) in models.into_iter().take(12) {
        let filled = ((tokens as u128 * bar_width as u128) / maximum as u128) as usize;
        let marker = if model == current_model { "●" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {marker} "),
                Style::default().fg(if model == current_model {
                    primary()
                } else {
                    muted()
                }),
            ),
            Span::styled(
                format!("{:<24}", single_line(&model, 23)),
                Style::default().fg(text()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{sessions:>8}  "), Style::default().fg(muted())),
            Span::styled(
                format!("{:>8}  ", format_count(tokens)),
                Style::default().fg(primary()),
            ),
            Span::styled("█".repeat(filled.max(1)), Style::default().fg(secondary())),
            Span::styled(
                "░".repeat(bar_width - filled.max(1)),
                Style::default().fg(border()),
            ),
            Span::styled(
                format!("  {}", format_duration(duration)),
                Style::default().fg(muted()),
            ),
        ]));
    }
    lines
}

fn usage_date(record: &SessionUsage) -> NaiveDate {
    record.created_at.with_timezone(&Local).date_naive()
}

fn format_count(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.1}b", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

/// Read the system clipboard synchronously via the platform's native CLI.
/// Returns `None` on any failure so the caller can no-op. macOS uses `pbpaste`;
/// Linux uses `xclip`/`xsel` (whichever is available); Windows uses `clip`.
fn clipboard_text() -> Option<String> {
    use std::process::Command;
    let result = if cfg!(target_os = "macos") {
        Command::new("pbpaste").output()
    } else if cfg!(target_os = "linux") {
        Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .output()
            .or_else(|_| {
                Command::new("xsel")
                    .args(["--clipboard", "--output"])
                    .output()
            })
    } else if cfg!(target_os = "windows") {
        // `clip` on Windows only supports output (copy), not input. PowerShell
        // can read the clipboard; fall back to it for paste.
        Command::new("powershell")
            .args(["-NoProfile", "-Command", "Get-Clipboard -Raw"])
            .output()
    } else {
        return None;
    };
    let output = result.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if text.is_empty() { None } else { Some(text) }
}

fn format_duration(seconds: u64) -> String {
    if seconds == 0 {
        return "—".to_owned();
    }
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

fn draw_config(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(panel) = &app.config_panel else {
        return;
    };
    let popup = centered_rect(area.width.saturating_sub(8).min(96), 23, area);
    frame.render_widget(Clear, popup);
    let inner_height = popup.height.saturating_sub(4) as usize;
    let start = panel
        .selected
        .saturating_sub(inner_height.saturating_sub(1));
    let mut lines = Vec::new();
    for (index, key) in CONFIG_KEYS
        .iter()
        .copied()
        .enumerate()
        .skip(start)
        .take(inner_height)
    {
        let selected = index == panel.selected;
        let marker = if selected { "›" } else { " " };
        let label_style = if selected {
            Style::default()
                .fg(text())
                .bg(surface())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(muted())
        };
        let value_style = if selected {
            Style::default().fg(primary()).bg(surface())
        } else {
            Style::default().fg(text())
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} {:<25}", config_label(key)), label_style),
            Span::styled(single_line(&app.config_value(key), 54), value_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter edit/toggle  ·  j/k move  ·  Esc close  ·  changes save immediately",
        Style::default().fg(muted()),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(secondary()))
        .title(Span::styled(
            " Configuration ",
            Style::default().fg(text()).add_modifier(Modifier::BOLD),
        ));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), popup);

    if let Some((key, input)) = &panel.editing {
        let editor = centered_rect(popup.width.saturating_sub(10), 7, popup);
        frame.render_widget(Clear, editor);
        let text = input.text();
        let edit_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(primary()))
            .title(format!(" {} ", config_label(*key)));
        frame.render_widget(
            Paragraph::new(text.as_str())
                .block(edit_block)
                .wrap(Wrap { trim: false }),
            editor,
        );
        let (_, column) = input.cursor_position();
        frame.set_cursor_position((
            (editor.x + 1 + column as u16).min(editor.right().saturating_sub(2)),
            editor.y + 1,
        ));
    }
}

fn draw_raw_config(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(editor) = &app.raw_config else {
        return;
    };
    let popup = centered_rect(
        area.width.saturating_sub(6).min(112),
        area.height.saturating_sub(4),
        area,
    );
    frame.render_widget(Clear, popup);
    let text = editor.input.text();
    let (row, column) = editor.input.cursor_position();
    let visible = popup.height.saturating_sub(4).max(1) as usize;
    let scroll = row.saturating_sub(visible.saturating_sub(1));
    let title = if let Some(error) = &editor.error {
        format!(" Advanced configuration · {} ", single_line(error, 70))
    } else {
        " Advanced configuration · TOML ".to_owned()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if editor.error.is_some() {
            danger()
        } else {
            secondary()
        }))
        .title(title)
        .title_bottom(
            Line::from(" Ctrl+S save & apply  ·  Esc discard ").alignment(Alignment::Right),
        );
    frame.render_widget(
        Paragraph::new(text.as_str())
            .block(block)
            .scroll((scroll as u16, 0))
            .wrap(Wrap { trim: false }),
        popup,
    );
    let visible_row = row.saturating_sub(scroll) as u16;
    frame.set_cursor_position((
        (popup.x + 1 + column as u16).min(popup.right().saturating_sub(2)),
        (popup.y + 1 + visible_row).min(popup.bottom().saturating_sub(2)),
    ));
}

fn draw_feedback(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(form) = &app.feedback_form else {
        return;
    };
    let popup = centered_rect(area.width.saturating_sub(10).min(88), 18, area);
    frame.render_widget(Clear, popup);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(2),
            Constraint::Length(2),
        ])
        .split(popup);
    let category = FEEDBACK_CATEGORIES[form.category];
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Category  ", Style::default().fg(muted())),
            Span::styled(
                category,
                Style::default().fg(primary()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   Tab to change", Style::default().fg(muted())),
        ])),
        sections[0],
    );
    let text = form.input.text();
    frame.render_widget(
        Paragraph::new(if text.is_empty() {
            Text::from(Span::styled(
                "What should we improve? Please avoid secrets or sensitive source code.",
                Style::default().fg(muted()),
            ))
        } else {
            Text::from(text.as_str())
        })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(primary()))
                .title(" Feedback "),
        )
        .wrap(Wrap { trim: false }),
        sections[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if form.include_diagnostics {
                    "[x]"
                } else {
                    "[ ]"
                },
                Style::default().fg(if form.include_diagnostics {
                    success()
                } else {
                    muted()
                }),
            ),
            Span::raw(" Include extension diagnostics  "),
            Span::styled("Ctrl+D", Style::default().fg(primary())),
            Span::styled(
                "  ·  Never includes your transcript",
                Style::default().fg(muted()),
            ),
        ])),
        sections[2],
    );
    let footer = if form.sending {
        Line::from(Span::styled(
            "Sending feedback…",
            Style::default().fg(warning()),
        ))
    } else if let Some(error) = &form.error {
        Line::from(Span::styled(
            single_line(error, 82),
            Style::default().fg(danger()),
        ))
    } else {
        Line::from(vec![
            Span::styled("Ctrl+S", Style::default().fg(primary())),
            Span::raw(" send  ·  "),
            Span::styled("Enter", Style::default().fg(primary())),
            Span::raw(" new line  ·  "),
            Span::styled("Esc", Style::default().fg(primary())),
            Span::raw(" cancel"),
        ])
    };
    frame.render_widget(Paragraph::new(footer), sections[3]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(secondary()))
        .title(" Send feedback to Empero ");
    frame.render_widget(block, popup);
    if !form.sending {
        let (row, column) = form.input.cursor_position();
        frame.set_cursor_position((
            (sections[1].x + 1 + column as u16).min(sections[1].right().saturating_sub(2)),
            (sections[1].y + 1 + row as u16).min(sections[1].bottom().saturating_sub(2)),
        ));
    }
}

fn draw_user_question(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(question) = &app.question else {
        return;
    };
    // Sizing: 80 cols wide, with a sensible height based on content.
    let opts = question.options.len() as u16;
    let option_rows = opts.max(2); // reserve at least 2 lines even when 0 options
    let custom_rows: u16 = 3; // border + 1 inner line + border
    let height = (8 + option_rows + custom_rows + 3).min(area.height.saturating_sub(2));
    let width = 96u16.min(area.width.saturating_sub(4));
    let popup = centered_rect(width, height, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(secondary()))
        .title(Span::styled(
            if question.header.is_empty() {
                " Question ".to_owned()
            } else {
                format!(" {} ", question.header)
            },
            Style::default()
                .fg(inverse())
                .bg(secondary())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Inner layout: question text, spacer, option list, spacer, custom input.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // question label + value
            Constraint::Length(1), // spacer
            Constraint::Min(3),    // options
            Constraint::Length(1), // spacer
            Constraint::Length(3), // custom prompt
            Constraint::Length(2), // footer hints
        ])
        .split(inner);

    // Question text.
    let question_text = Text::from(vec![
        Line::from(Span::styled(
            "QUESTION",
            Style::default().fg(muted()).add_modifier(Modifier::BOLD),
        )),
        Line::from(question.question.as_str()),
    ]);
    frame.render_widget(
        Paragraph::new(question_text).wrap(Wrap { trim: false }),
        chunks[0],
    );

    // Options list.
    let option_entries: Vec<Line> = if question.options.is_empty() {
        vec![Line::from(Span::styled(
            "(no options — type a custom answer and press Enter)",
            Style::default().fg(muted()),
        ))]
    } else {
        let mut lines = Vec::with_capacity(question.options.len());
        for (idx, opt) in question.options.iter().enumerate() {
            let is_cursor = idx == question.cursor && !question.editing_custom;
            let is_on = question.selected.get(idx).copied().unwrap_or(false);
            let marker = if question.multi_select {
                if is_on { "[x]" } else { "[ ]" }
            } else if is_on {
                "(•)"
            } else {
                "( )"
            };
            let arrow = if is_cursor { "▶" } else { " " };
            let style = if is_cursor {
                Style::default().fg(primary()).add_modifier(Modifier::BOLD)
            } else if is_on {
                Style::default().fg(success())
            } else {
                Style::default().fg(text())
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{}{} ", arrow, marker), style),
                Span::styled(opt.as_str(), style),
            ]));
        }
        lines
    };
    frame.render_widget(Paragraph::new(option_entries), chunks[1]);

    // Custom-answer field.
    let custom_label = if question.editing_custom { "> " } else { "  " };
    let custom_focused = question.editing_custom;
    let custom_color = if custom_focused { primary() } else { muted() };
    let placeholder = if custom_focused {
        String::new()
    } else if question.options.is_empty() {
        // Free-text-only mode: tell the user to focus this with `t`.
        "(press t to type an answer)".to_owned()
    } else {
        "(optional — type to add a custom answer)".to_owned()
    };
    let value = question.custom.text();
    let custom_line = if value.is_empty() {
        Line::from(vec![
            Span::styled(custom_label.to_string(), Style::default().fg(custom_color)),
            Span::styled(placeholder, Style::default().fg(muted())),
        ])
    } else {
        Line::from(vec![
            Span::styled(custom_label.to_string(), Style::default().fg(custom_color)),
            Span::styled(value.as_str(), Style::default().fg(custom_color)),
        ])
    };
    let custom_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(custom_color))
        .title(Span::styled(" custom ", Style::default().fg(custom_color)));
    frame.render_widget(Paragraph::new(custom_line).block(custom_block), chunks[2]);

    // Footer hints.
    let hint_switch = if question.editing_custom {
        Line::from(vec![
            Span::styled("Esc", Style::default().fg(primary())),
            Span::raw(" leave field  "),
            Span::styled("Enter", Style::default().fg(primary())),
            Span::raw(" submit"),
        ])
    } else if question.multi_select {
        Line::from(vec![
            Span::styled("↑↓", Style::default().fg(primary())),
            Span::raw(" navigate  "),
            Span::styled("space/x", Style::default().fg(primary())),
            Span::raw(" toggle  "),
            Span::styled("t", Style::default().fg(primary())),
            Span::raw(" custom  "),
            Span::styled("Enter", Style::default().fg(primary())),
            Span::raw(" submit  "),
            Span::styled("Esc", Style::default().fg(primary())),
            Span::raw(" cancel"),
        ])
    } else {
        Line::from(vec![
            Span::styled("↑↓", Style::default().fg(primary())),
            Span::raw(" navigate  "),
            Span::styled("x", Style::default().fg(primary())),
            Span::raw(" choose & submit  "),
            Span::styled("t", Style::default().fg(primary())),
            Span::raw(" custom  "),
            Span::styled("Esc", Style::default().fg(primary())),
            Span::raw(" cancel"),
        ])
    };
    frame.render_widget(Paragraph::new(hint_switch), chunks[3]);

    // Place the cursor inside the custom-answer field when it's focused, so
    // typing actually inserts at the right position.
    if question.editing_custom {
        let (row, col) = question.custom.cursor_position();
        let inner_width = chunks[2].width.saturating_sub(2).max(1);
        let h = row.min(chunks[2].height.saturating_sub(2).max(1) as usize);
        let v = col.min(inner_width as usize);
        frame.set_cursor_position((
            (chunks[2].x + 1 + v as u16).min(chunks[2].right().saturating_sub(2)),
            (chunks[2].y + 1 + h as u16).min(chunks[2].bottom().saturating_sub(2)),
        ));
    }
}

fn draw_approval(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(approval) = &app.approval else {
        return;
    };
    let width = area.width.saturating_sub(4).min(120);
    let height = area.height.saturating_sub(2).max(10);
    let popup = centered_rect(width, height, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(warning()))
        .title(Line::from(vec![
            Span::styled(
                " APPROVAL REQUIRED ",
                Style::default()
                    .fg(inverse())
                    .bg(warning())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {} ", approval.tool),
                Style::default().fg(muted()),
            ),
        ]));
    frame.render_widget(block, popup);
    let content = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(content);

    let mut header = vec![Line::from(Span::styled(
        single_line(&approval.summary, 108),
        Style::default().fg(text()).add_modifier(Modifier::BOLD),
    ))];
    if let Some(diff) = &approval.diff {
        header.push(Line::from(vec![
            Span::styled(
                format!(
                    "{} file{}",
                    diff.file_count(),
                    if diff.file_count() == 1 { "" } else { "s" }
                ),
                Style::default().fg(muted()),
            ),
            Span::styled(
                format!("  +{}", diff.additions),
                Style::default().fg(success()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  -{}", diff.deletions),
                Style::default().fg(danger()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ·  {:?} view", approval.view),
                Style::default().fg(muted()),
            ),
        ]));
    } else {
        header.push(Line::from(Span::styled(
            "Review the operation below before allowing it to run.",
            Style::default().fg(muted()),
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(header)), sections[0]);

    let body = if approval.view == ApprovalView::Unified {
        approval
            .diff
            .as_ref()
            .map(diff_text)
            .unwrap_or_else(|| raw_approval_text(&approval.details))
    } else {
        raw_approval_text(&approval.details)
    };
    frame.render_widget(
        Paragraph::new(body)
            .scroll((app.approval_scroll, app.approval_horizontal))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border()))
                    .title(if approval.diff.is_some() {
                        " Changes "
                    } else {
                        " Operation "
                    }),
            ),
        sections[1],
    );

    let mut controls = vec![
        Span::styled(
            " y ",
            Style::default()
                .fg(inverse())
                .bg(success())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" once  "),
        Span::styled(
            " a ",
            Style::default()
                .fg(inverse())
                .bg(primary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" session  "),
        Span::styled(
            " n ",
            Style::default()
                .fg(text())
                .bg(danger())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" reject   "),
        Span::styled("j/k", Style::default().fg(primary())),
        Span::raw(" scroll  "),
        Span::styled("h/l", Style::default().fg(primary())),
        Span::raw(" pan"),
    ];
    if approval.diff.is_some() {
        controls.push(Span::raw("  "));
        controls.push(Span::styled("v", Style::default().fg(primary())));
        controls.push(Span::raw(" view"));
    }
    frame.render_widget(Paragraph::new(Line::from(controls)), sections[2]);
}

fn diff_text(diff: &DiffDocument) -> Text<'static> {
    let mut lines = Vec::new();
    for (index, file) in diff.files.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().bg(surface())),
            Span::styled(
                file.display_path().to_owned(),
                Style::default()
                    .fg(text())
                    .bg(surface())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   +{}  -{} ", file.additions, file.deletions),
                Style::default().fg(muted()).bg(surface()),
            ),
        ]));
        for line in &file.lines {
            match line.kind {
                DiffLineKind::Hunk => lines.push(Line::from(Span::styled(
                    format!("     {}", line.text),
                    Style::default()
                        .fg(primary())
                        .bg(crate::theme::active().code_bg),
                ))),
                DiffLineKind::Addition | DiffLineKind::Deletion | DiffLineKind::Context => {
                    let palette = crate::theme::active();
                    let (marker, foreground, background) = match line.kind {
                        DiffLineKind::Addition => ("+", palette.add_fg, palette.add_bg),
                        DiffLineKind::Deletion => ("-", palette.del_fg, palette.del_bg),
                        _ => (" ", text(), Color::Reset),
                    };
                    let number_style = Style::default().fg(muted()).bg(background);
                    lines.push(Line::from(vec![
                        Span::styled(format_line_number(line.old_line), number_style),
                        Span::styled(" ", number_style),
                        Span::styled(format_line_number(line.new_line), number_style),
                        Span::styled(
                            format!(" {marker} "),
                            Style::default().fg(foreground).bg(background),
                        ),
                        Span::styled(
                            line.text.clone(),
                            Style::default().fg(foreground).bg(background),
                        ),
                    ]));
                }
                DiffLineKind::Metadata => lines.push(Line::from(Span::styled(
                    format!("     {}", line.text),
                    Style::default().fg(muted()),
                ))),
            }
        }
    }
    Text::from(lines)
}

fn raw_approval_text(details: &str) -> Text<'static> {
    Text::from(
        details
            .lines()
            .map(|line| {
                let style = if line.starts_with('$') {
                    Style::default().fg(warning()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(text())
                };
                Line::from(Span::styled(line.to_owned(), style))
            })
            .collect::<Vec<_>>(),
    )
}

fn format_line_number(value: Option<u32>) -> String {
    value.map_or_else(|| "    ".to_owned(), |line| format!("{line:>4}"))
}

fn draw_picker(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(picker) = &app.picker else {
        return;
    };
    let max_height = area.height.saturating_sub(4).max(3);
    let height = (picker.items.len() as u16 + 4).min(max_height).max(3);
    let popup = centered_rect(area.width.saturating_sub(12).min(100), height, area);
    frame.render_widget(Clear, popup);
    let visible = popup.height.saturating_sub(3) as usize;
    let start = picker.selected.saturating_sub(visible.saturating_sub(1));
    let mut lines = Vec::new();
    for (index, (label, _)) in picker.items.iter().enumerate().skip(start).take(visible) {
        let selected = index == picker.selected;
        let prefix = if selected { "› " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(inverse())
                .bg(primary())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(text())
        };
        lines.push(Line::from(Span::styled(format!("{prefix}{label}"), style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "enter open   j/k select   esc close",
        Style::default().fg(muted()),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(primary()))
        .title(format!(" {} ", picker.title));
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), popup);
}

fn transcript_text(entries: &[Entry], width: usize) -> (Text<'static>, usize) {
    let mut lines = Vec::new();
    let mut visual = 0;
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
            visual += 1;
        }
        let (label, color) = match entry.kind {
            EntryKind::User => ("› YOU", primary()),
            EntryKind::Assistant => ("◆ ABACUS", secondary()),
            EntryKind::Tool => ("┌ TOOL", warning()),
            EntryKind::System => ("• SYSTEM", muted()),
            EntryKind::Error => ("! ERROR", danger()),
        };
        lines.push(Line::from(Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        visual += 1;

        if entry.kind == EntryKind::Assistant {
            let rendered = render_markdown(
                &entry.text,
                MarkdownTheme {
                    text: text(),
                    muted: muted(),
                    heading: secondary(),
                    accent: primary(),
                    code: success(),
                    code_background: crate::theme::active().code_bg,
                    quote: primary(),
                    link: primary(),
                },
            );
            if rendered.lines.is_empty() {
                lines.push(Line::from(""));
                visual += 1;
            } else {
                for line in rendered.lines {
                    let line_width = line
                        .spans
                        .iter()
                        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                        .sum::<usize>();
                    visual += line_width.max(1).div_ceil(width.max(1));
                    lines.push(line);
                }
            }
            continue;
        }

        let mut code = false;
        for raw in entry.text.lines() {
            let trimmed = raw.trim_start();
            if trimmed.starts_with("```") {
                code = !code;
            }
            let style = if code {
                Style::default().fg(success())
            } else if trimmed.starts_with('#') {
                Style::default().fg(text()).add_modifier(Modifier::BOLD)
            } else if entry.kind == EntryKind::Tool {
                Style::default().fg(muted())
            } else {
                Style::default().fg(text())
            };
            lines.push(Line::from(Span::styled(raw.to_owned(), style)));
            let line_width = UnicodeWidthStr::width(raw);
            visual += line_width.max(1).div_ceil(width.max(1));
        }
        if entry.text.is_empty() {
            lines.push(Line::from(""));
            visual += 1;
        }
    }
    (Text::from(lines), visual)
}

fn tool_preview(output: &str) -> String {
    let mut preview = output.lines().take(8).collect::<Vec<_>>().join("\n");
    if output.lines().count() > 8 {
        preview.push_str("\n…");
    }
    if preview.len() > 1_200 {
        let mut boundary = 1_200;
        while !preview.is_char_boundary(boundary) {
            boundary -= 1;
        }
        preview.truncate(boundary);
        preview.push('…');
    }
    preview
}

fn single_line(value: &str, max: usize) -> String {
    let value = value.replace(['\n', '\r'], " ");
    if value.chars().count() <= max {
        value
    } else {
        format!("{}…", value.chars().take(max).collect::<String>())
    }
}

fn entries_from_messages(messages: &[Value]) -> Vec<Entry> {
    let mut entries = Vec::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or_default();
        let Some(content) = message["content"].as_str() else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        match role {
            "user" => entries.push(Entry {
                kind: EntryKind::User,
                text: content
                    .split("\n\n<attached_file path=\"")
                    .next()
                    .unwrap_or(content)
                    .to_owned(),
            }),
            "assistant" => entries.push(Entry {
                kind: EntryKind::Assistant,
                text: content.to_owned(),
            }),
            "tool" => entries.push(Entry {
                kind: EntryKind::Tool,
                text: format!(
                    "{}\n{}",
                    message["name"].as_str().unwrap_or("tool"),
                    tool_preview(content)
                ),
            }),
            _ => {}
        }
    }
    entries
}

fn key_line<'a>(key: &'a str, description: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{key:<24}"), Style::default().fg(primary())),
        Span::raw(description),
    ])
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn content_rect(area: Rect, max_width: u16) -> Rect {
    let width = area.width.min(max_width);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y,
        width,
        height: area.height,
    }
}

fn validate_settings(settings: &Settings) -> Result<()> {
    if !settings.profiles.contains_key(&settings.default_profile) {
        bail!(
            "default profile `{}` does not exist",
            settings.default_profile
        );
    }
    let profile = &settings.profiles[&settings.default_profile];
    if profile.model.trim().is_empty() {
        bail!("the active profile needs a model");
    }
    reqwest::Url::parse(&profile.base_url).context("provider URL is invalid")?;
    if !(1..=128).contains(&settings.agent.max_steps) {
        bail!("max steps must be between 1 and 128");
    }
    if !(2_000..=200_000).contains(&settings.agent.tool_output_limit) {
        bail!("tool output limit must be between 2000 and 200000");
    }
    if settings.feedback.enabled {
        crate::feedback::FeedbackClient::new(&settings.feedback.endpoint)?;
    }
    Ok(())
}

fn config_label(key: ConfigKey) -> &'static str {
    match key {
        ConfigKey::Profile => "Active profile",
        ConfigKey::Model => "Model",
        ConfigKey::BaseUrl => "Provider URL",
        ConfigKey::Protocol => "Wire protocol",
        ConfigKey::Permission => "Permission mode",
        ConfigKey::VimMode => "Vim keybindings",
        ConfigKey::Animations => "Animations",
        ConfigKey::Tooltips => "Welcome tips",
        ConfigKey::MaxSteps => "Maximum agent steps",
        ConfigKey::ToolOutputLimit => "Tool output limit",
        ConfigKey::ProjectTrust => "Trust this project",
        ConfigKey::FeedbackEnabled => "Feedback",
        ConfigKey::FeedbackDiagnostics => "Feedback diagnostics",
        ConfigKey::FeedbackEndpoint => "Feedback endpoint",
        ConfigKey::AdvancedToml => "Advanced configuration",
    }
}

fn config_key_is_editable(key: ConfigKey) -> bool {
    matches!(
        key,
        ConfigKey::Model
            | ConfigKey::BaseUrl
            | ConfigKey::MaxSteps
            | ConfigKey::ToolOutputLimit
            | ConfigKey::FeedbackEndpoint
    )
}

fn on_off(value: bool) -> String {
    if value { "On" } else { "Off" }.to_owned()
}

fn edit_buffer(input: &mut InputBuffer, key: KeyEvent, multiline: bool) {
    match key.code {
        KeyCode::Enter if multiline => input.insert('\n'),
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.delete_word_backward()
        }
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Up => input.move_up(),
        KeyCode::Down => input.move_down(),
        KeyCode::Home => input.move_start(),
        KeyCode::End => input.move_end(),
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.delete_word_backward()
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            input.insert(character)
        }
        _ => {}
    }
}

fn slash_suggestions(input: &str) -> Vec<(&'static str, &'static str)> {
    let query = input.trim();
    if !query.starts_with('/') || query.contains(char::is_whitespace) {
        return Vec::new();
    }
    // Return every match; the popup clamps how many it renders to the space it
    // has, so a bare `/` lists all commands instead of an arbitrary first six.
    SLASH_COMMANDS
        .iter()
        .copied()
        .filter(|(command, _)| command.starts_with(query))
        .collect()
}

fn complete_slash_command(input: &mut InputBuffer) {
    let value = input.text();
    let Some((command, _)) = slash_suggestions(&value).first().copied() else {
        return;
    };
    input.clear();
    input.insert_str(command);
    input.insert(' ');
}

/// Up to eight workspace files matching `partial` (the text after `@`), used for
/// `@file` mention completion. gitignore-aware and bounded so it stays cheap to
/// recompute on each keystroke; prefix matches rank ahead of substring matches.
fn file_suggestions(workspace: &std::path::Path, partial: &str) -> Vec<String> {
    const MAX_RESULTS: usize = 8;
    const MAX_SCANNED: usize = 8_000;
    let needle = partial.to_ascii_lowercase();
    let mut prefix = Vec::new();
    let mut contains = Vec::new();
    let mut scanned = 0_usize;
    for entry in ignore::WalkBuilder::new(workspace)
        .max_depth(Some(12))
        .build()
        .flatten()
    {
        if scanned >= MAX_SCANNED {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(workspace) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        scanned += 1;
        let lower = relative.to_ascii_lowercase();
        if needle.is_empty() || lower.starts_with(&needle) {
            prefix.push(relative);
        } else if lower.contains(&needle) {
            contains.push(relative);
        }
        if prefix.len() >= MAX_RESULTS && !needle.is_empty() {
            break;
        }
    }
    prefix.sort();
    contains.sort();
    prefix
        .into_iter()
        .chain(contains)
        .take(MAX_RESULTS)
        .collect()
}

/// What the completion popup is currently offering: the entries (value, hint)
/// and a title. Slash commands complete the whole line; `@file` mentions
/// complete just the token at the cursor.
fn active_completion(app: &App) -> Option<(Vec<(String, String)>, &'static str)> {
    let text = app.input.text();
    let slash = slash_suggestions(&text);
    if !slash.is_empty() {
        let items = slash
            .into_iter()
            .map(|(command, description)| (command.to_owned(), description.to_owned()))
            .collect();
        return Some((items, " commands · Tab to complete "));
    }
    let token = app.input.token_before_cursor();
    if let Some(partial) = token.strip_prefix('@') {
        let files = file_suggestions(&app.config.workspace, partial);
        if files.is_empty() {
            return None;
        }
        let items = files
            .into_iter()
            .map(|path| (format!("@{path}"), String::new()))
            .collect();
        return Some((items, " files · Tab to complete "));
    }
    None
}

/// Apply the top completion: replace the `@token` at the cursor, or fall back to
/// whole-line slash completion.
fn complete_at_cursor(app: &mut App) {
    let token = app.input.token_before_cursor();
    if let Some(partial) = token.strip_prefix('@') {
        if let Some(first) = file_suggestions(&app.config.workspace, partial)
            .into_iter()
            .next()
        {
            app.input.replace_token_before_cursor(&format!("@{first}"));
            app.input.insert(' ');
        }
    } else if app.input.text().trim_start().starts_with('/') {
        complete_slash_command(&mut app.input);
    }
}

fn latest_assistant_text(messages: &[Value]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant" && message["content"].is_string())
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{AbacusPaths, ProviderProfile},
        services::AgentServices,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use tempfile::{TempDir, tempdir};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::oneshot,
        time::{Duration, sleep},
    };

    #[test]
    fn tool_preview_is_bounded() {
        let output = (0..20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = tool_preview(&output);
        assert!(preview.lines().count() <= 9);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn transcript_estimates_wrapped_height() {
        let entries = vec![Entry {
            kind: EntryKind::Assistant,
            text: "1234567890".to_owned(),
        }];
        let (_, height) = transcript_text(&entries, 5);
        assert_eq!(height, 3); // label plus two wrapped lines
    }

    #[test]
    fn polished_layout_renders_at_standard_and_compact_sizes() {
        for (width, height) in [(120, 36), (80, 24), (60, 20)] {
            let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let rendered = buffer_text(terminal.backend().buffer(), width, height);
            assert!(rendered.contains("ABACUS"));
            assert!(rendered.contains("focused coding agent") || width < 72);
            assert!(rendered.contains("commands"));

            app.config_panel = Some(ConfigPanel {
                selected: 0,
                editing: None,
            });
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let rendered = buffer_text(terminal.backend().buffer(), width, height);
            assert!(rendered.contains("Configuration"));
            assert!(rendered.contains("Active profile"));

            app.config_panel = None;
            app.open_feedback();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let rendered = buffer_text(terminal.backend().buffer(), width, height);
            assert!(rendered.contains("Send feedback"));
            assert!(rendered.contains("Category"));
        }
    }

    #[test]
    fn transcript_renders_markdown_semantically() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.entries = vec![Entry {
            kind: EntryKind::Assistant,
            text: "# Result\n\nUse **cargo test** and `cargo clippy`.\n\n```rust\nfn main() {}\n```\n\n| Check | State |\n|---|---|\n| tests | green |".into(),
        }];
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), 80, 30);
        assert!(rendered.contains("◆ ABACUS"));
        assert!(rendered.contains("Result"));
        assert!(rendered.contains("cargo test"));
        assert!(rendered.contains("╭─ rust"));
        assert!(rendered.contains("tests"));
        assert!(rendered.contains("green"));
        assert!(!rendered.contains("**cargo test**"));
        assert!(!rendered.contains("```rust"));
    }

    #[test]
    fn semantic_diff_approval_renders_at_standard_and_compact_sizes() {
        let patch = concat!(
            "--- a/src/main.rs\n",
            "+++ b/src/main.rs\n",
            "@@ -1,2 +1,2 @@\n",
            " fn main() {\n",
            "-    println!(\"old\");\n",
            "+    println!(\"new\");\n",
            " }\n"
        );
        for (width, height) in [(100, 28), (60, 20)] {
            let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
            let (respond, _receive) = oneshot::channel();
            app.set_approval(ApprovalRequest {
                tool: "apply_patch".into(),
                summary: "workspace patch".into(),
                details: patch.into(),
                respond,
            });
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let rendered = buffer_text(terminal.backend().buffer(), width, height);
            assert!(rendered.contains("APPROVAL REQUIRED"));
            assert!(rendered.contains("src/main.rs"));
            assert!(rendered.contains("+1"));
            assert!(rendered.contains("-1"));
            assert!(rendered.contains("println!"));
            assert!(rendered.contains("once"));
            assert!(rendered.contains("reject"));
        }
    }

    #[test]
    fn config_changes_are_saved_and_live_immediately() {
        let (directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.settings.profiles.get_mut("test").unwrap().model = "new-model".into();
        app.settings.agent.max_steps = 64;
        app.settings.ui.vim_mode = false;
        app.save_and_apply_settings().unwrap();
        assert_eq!(app.config.model, "new-model");
        assert_eq!(app.config.max_steps, 64);
        assert_eq!(app.mode, InputMode::Insert);
        assert!(app.reload_services);
        let saved = Settings::load(&AbacusPaths::under(directory.path().join("home"))).unwrap();
        assert_eq!(saved.profiles["test"].model, "new-model");
        assert_eq!(saved.agent.max_steps, 64);
    }

    #[test]
    fn advanced_config_editor_saves_complete_settings_document() {
        let (directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let skill_path = directory.path().join("skills");
        let mut settings = app.settings.clone();
        settings.skills.paths.push(skill_path.clone());
        settings.feedback.include_diagnostics = true;
        settings.trust.set(&app.config.workspace, true);
        let text = toml::to_string_pretty(&settings).unwrap();
        let mut input = InputBuffer::new();
        input.insert_str(&text);
        app.raw_config = Some(RawConfigEditor { input, error: None });
        app.save_raw_config();
        assert!(app.raw_config.is_none());
        assert!(app.settings.feedback.include_diagnostics);
        assert!(app.settings.trust.contains(&app.config.workspace));
        assert_eq!(app.settings.skills.paths, vec![skill_path]);
        assert!(app.reload_services);
        let saved = Settings::load(&AbacusPaths::under(directory.path().join("home"))).unwrap();
        assert!(saved.feedback.include_diagnostics);
        assert!(saved.trust.contains(&app.config.workspace));
    }

    #[tokio::test]
    async fn goal_text_becomes_the_starting_prompt_and_can_pause() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.goal_command("Finish the migration and keep tests green");
        let goal = app.goal.snapshot().unwrap();
        assert_eq!(goal.objective, "Finish the migration and keep tests green");
        assert_eq!(goal.status, crate::goal::GoalStatus::Active);
        assert_eq!(
            app.messages.last().unwrap()["content"],
            "Finish the migration and keep tests green"
        );
        assert!(app.running.is_some());
        app.goal_command("pause");
        assert_eq!(
            app.goal.snapshot().unwrap().status,
            crate::goal::GoalStatus::Paused
        );
        assert!(app.running.is_none());
        app.goal_command("edit Finish migration with all release checks");
        assert_eq!(
            app.goal.snapshot().unwrap().objective,
            "Finish migration with all release checks"
        );
        app.goal_command("clear");
        assert!(app.goal.snapshot().is_none());
    }

    #[test]
    fn slash_palette_lists_every_command_not_just_six() {
        // Regression: a bare `/` used to surface only the first six commands.
        let all = slash_suggestions("/");
        assert_eq!(all.len(), SLASH_COMMANDS.len());
        assert!(all.len() > 6);
        assert!(all.iter().any(|(command, _)| *command == "/swarm"));
        assert!(all.iter().any(|(command, _)| *command == "/usage"));
    }

    #[test]
    fn usage_dashboard_renders_and_switches_views() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let now = Utc::now();
        app.usage_panel = Some(UsagePanel {
            records: vec![
                SessionUsage {
                    id: uuid::Uuid::new_v4(),
                    model: "abacus-pro".into(),
                    created_at: now - ChronoDuration::days(2),
                    updated_at: now - ChronoDuration::days(2),
                    message_count: 8,
                    tokens_used: 12_400,
                    tokens_estimated: false,
                    active_secs: 3_900,
                },
                SessionUsage {
                    id: uuid::Uuid::new_v4(),
                    model: "abacus-pro".into(),
                    created_at: now - ChronoDuration::days(1),
                    updated_at: now - ChronoDuration::days(1),
                    message_count: 5,
                    tokens_used: 7_600,
                    tokens_estimated: false,
                    active_secs: 1_200,
                },
            ],
            tab: UsageTab::Overview,
            range: UsageRange::AllTime,
        });
        let backend = TestBackend::new(110, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), 110, 32);
        assert!(rendered.contains("Usage"));
        assert!(rendered.contains("Overview"));
        assert!(rendered.contains("Favorite model"));
        assert!(rendered.contains("abacus-pro"));
        assert!(rendered.contains("20.0k"));

        handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), 110, 32);
        assert!(rendered.contains("Sessions"));
        assert!(rendered.contains("Tokens"));
        assert!(rendered.contains("abacus-pro"));
    }

    #[tokio::test]
    async fn at_mention_completion_finds_and_inserts_workspace_files() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        std::fs::create_dir_all(app.config.workspace.join("src")).unwrap();
        std::fs::write(app.config.workspace.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(app.config.workspace.join("README.md"), "# hi").unwrap();

        let hits = file_suggestions(&app.config.workspace, "main");
        assert!(hits.iter().any(|path| path == "src/main.rs"));

        app.input.insert_str("look at @mai");
        let (items, title) = active_completion(&app).expect("file completion");
        assert!(title.contains("files"));
        assert!(items.iter().any(|(value, _)| value == "@src/main.rs"));

        complete_at_cursor(&mut app);
        assert_eq!(app.input.text(), "look at @src/main.rs ");
    }

    #[tokio::test]
    async fn exit_command_quits() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        assert!(app.slash_command("/exit"));
        assert!(app.quit);
    }

    #[tokio::test]
    async fn ctrl_c_interrupts_then_a_second_press_exits() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        // First press while idle with empty input only arms the exit prompt.
        app.handle_ctrl_c();
        assert!(!app.quit);
        assert!(app.last_ctrl_c.is_some());
        assert!(app.status.contains("Ctrl+C again to exit"));
        // A consecutive press exits.
        app.handle_ctrl_c();
        assert!(app.quit);
    }

    #[tokio::test]
    async fn ctrl_c_arm_resets_so_a_later_interrupt_is_not_a_quit() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.handle_ctrl_c();
        assert!(app.last_ctrl_c.is_some());
        // Any other key cancels the pending exit.
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()),
        );
        assert!(app.last_ctrl_c.is_none());
        // So the next Ctrl+C starts over rather than quitting.
        app.handle_ctrl_c();
        assert!(!app.quit);
    }

    #[tokio::test]
    async fn swarm_command_turns_an_objective_into_a_delegation_prompt() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        // An empty objective only prints usage; it must not start a turn.
        app.swarm_command("   ");
        assert!(app.running.is_none());
        assert!(
            app.entries
                .iter()
                .any(|entry| entry.text.contains("Usage: /swarm"))
        );
        // A real objective is expanded into a spawn_subagents instruction that
        // still carries the user's words, and it starts a turn.
        app.swarm_command("port modules A and B independently");
        let sent = app.messages.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(sent.contains("spawn_subagents"));
        assert!(sent.contains("port modules A and B independently"));
        assert!(app.running.is_some());
    }

    #[tokio::test]
    async fn ralph_replays_the_exact_prompt_until_the_promise_appears() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for content in ["still working", "DONE"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_http_request(&mut stream).await);
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\ndata: [DONE]\n\n"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        let (_directory, mut app) = test_app(&format!("http://{address}/v1"));
        std::fs::write(app.config.workspace.join("task.md"), "mutable task details").unwrap();
        app.ralph_loop =
            Some(RalphLoop::new("Use @task.md exactly".into(), "DONE".into(), Some(3)).unwrap());
        app.continue_ralph_loop();
        for _ in 0..200 {
            sleep(Duration::from_millis(10)).await;
            app.drain_agent_events();
            if app
                .ralph_loop
                .as_ref()
                .is_some_and(|state| state.status == RalphStatus::Completed)
            {
                break;
            }
        }
        assert_eq!(app.ralph_loop.as_ref().unwrap().iteration, 2);
        assert_eq!(
            app.ralph_loop.as_ref().unwrap().status,
            RalphStatus::Completed
        );
        let requests = server.await.unwrap();
        for (index, request) in requests.iter().enumerate() {
            let body = request.split("\r\n\r\n").nth(1).unwrap();
            let value: Value = serde_json::from_str(body).unwrap();
            let repeats = value["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| {
                    message["role"] == "user" && message["content"] == "Use @task.md exactly"
                })
                .count();
            assert_eq!(repeats, index + 1);
        }
    }

    fn test_app(base_url: &str) -> (TempDir, App) {
        let directory = tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let paths = AbacusPaths::under(directory.path().join("home"));
        let mut settings = Settings {
            default_profile: "test".into(),
            ..Settings::default()
        };
        settings.profiles.insert(
            "test".into(),
            ProviderProfile {
                name: "Test".into(),
                base_url: base_url.into(),
                model: "test-model".into(),
                protocol: ProviderProtocol::ChatCompletions,
                api_key_env: None,
            },
        );
        let config = Config {
            workspace: workspace.clone(),
            profile: "test".into(),
            model: "test-model".into(),
            base_url: base_url.into(),
            protocol: ProviderProtocol::ChatCompletions,
            api_key: None,
            max_steps: 8,
            tool_output_limit: 30_000,
            yes: false,
            no_session: true,
            model_limits: crate::model_info::ModelLimits::default(),
            tool_format: crate::tool_format::ToolFormat::default(),
            web_search: crate::web::WebConfig::default(),
            paths,
        };
        let app = App::new(
            config,
            settings,
            Credentials::default(),
            None,
            None,
            Arc::new(AgentServices::empty(workspace)),
        )
        .unwrap();
        (directory, app)
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
        let mut output = String::new();
        for y in 0..height {
            for x in 0..width {
                output.push_str(buffer[(x, y)].symbol());
            }
            output.push('\n');
        }
        output
    }

    async fn read_http_request(stream: &mut TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(header_end) = buffer.windows(4).position(|value| value == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buffer[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(str::trim)
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if buffer.len() >= header_end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8(buffer).unwrap()
    }
}
