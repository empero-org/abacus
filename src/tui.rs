use std::{
    cell::RefCell,
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
        MouseButton, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};
use unicode_width::UnicodeWidthStr;

use crate::{
    activity::ActivityReporter,
    agent::{
        AgentEvent, AgentMode, ApprovalDecision, ApprovalRequest, DoneReason, TurnOptions,
        UserQuestionRequest, compact_messages, initial_messages, message_chars, run_turn,
    },
    compaction::CompactionState,
    config::{Config, Credentials, PermissionMode, ProviderProtocol, SETTINGS_VERSION, Settings},
    context::expand_file_references,
    diff::{DiffDocument, DiffLineKind},
    goal::GoalState,
    input::{InputBuffer, InputMode},
    provider::Provider,
    ralph::{RalphLoop, RalphStatus},
    services::AgentServices,
    session::{Session, SessionStore, SessionUsage},
    task::TaskList,
    theme::{Theme, ThemeChoice, ThemeMode},
    ui::{self, Entry, EntryKind, ToolCall, ToolStatus},
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
fn rail() -> Color {
    crate::theme::active().rail
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
    ("/thinking", "Show or hide the model's reasoning"),
    (
        "/effort",
        "Set reasoning effort: minimal, low, medium, high, xhigh, max, auto",
    ),
    (
        "/btw",
        "Note a side question without derailing the running turn",
    ),
    ("/model", "Inspect or switch model"),
    (
        "/providers",
        "Pin which upstream providers may serve the model",
    ),
    ("/usage", "View local usage and activity"),
    ("/sessions", "Browse saved sessions"),
    ("/new", "Start a new session"),
    ("/compact", "Compact conversation context"),
    ("/repair", "Fix corrupted session history"),
    ("/papercuts", "List or delete recorded lessons"),
    ("/memories", "List or delete stored memories"),
    ("/skills", "Browse Agent Skills"),
    ("/plugins", "Inspect plugins"),
    ("/mcps", "Inspect MCP tools"),
    ("/tools", "List all active tools"),
    ("/help", "Show shortcuts and commands"),
    ("/quit", "Exit Abacus"),
    ("/exit", "Exit Abacus"),
];

/// Clickable regions recorded while drawing, so the mouse handler can act on
/// what is actually on screen rather than re-deriving the layout. Rebuilt every
/// frame; a region that was not drawn cannot be clicked.
#[derive(Default)]
struct Hits {
    completion: Vec<(Rect, usize)>,
    config: Vec<(Rect, usize)>,
    picker: Vec<(Rect, usize)>,
    transcript: Vec<(Rect, usize)>,
}

impl Hits {
    fn clear(&mut self) {
        self.completion.clear();
        self.config.clear();
        self.picker.clear();
        self.transcript.clear();
    }
}

/// The row of `regions` containing `(column, row)`, if any.
fn hit(regions: &[(Rect, usize)], column: u16, row: u16) -> Option<usize> {
    regions
        .iter()
        .find(|(rect, _)| {
            column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
        })
        .map(|(_, index)| *index)
}

/// How the last turn ended. The status bar reads this instead of matching on
/// the status *text*, which is a display string and free to change wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnOutcome {
    Failed,
    Interrupted,
}

/// A provider mid-creation: which profile was added, and what to restore if
/// the user backs out before giving it a model.
struct PendingProvider {
    profile: String,
    previous: String,
}

/// Picker values that mean "not a real row" — they open a further step rather
/// than selecting something.
const NEW_PROVIDER_SENTINEL: &str = "\u{0}new-provider";
const CUSTOM_PROVIDER_SENTINEL: &str = "\u{0}custom-provider";
/// Prefix marking a provider-picker row that selects a scripted endpoint from
/// ~/.abacus/endpoints; the rest of the value is the endpoint name.
const ENDPOINT_SENTINEL_PREFIX: &str = "\u{0}endpoint:";

/// Fingerprint deciding whether the memoised transcript is still valid: the
/// entries revision, the render width, and the spinner phase. The phase only
/// participates while a tool is running, so an idle transcript is wrapped once
/// and then reused until something actually changes it.
type TranscriptKey = (u64, u16, usize, Option<usize>, bool);

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

/// What accepting a picker row does. The picker itself is a plain list; this
/// is how one list widget serves sessions, profiles, and providers without
/// three near-identical modals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerAction {
    ResumeSession,
    SwitchProfile,
    AddProvider,
}

struct Picker {
    title: String,
    items: Vec<(String, String)>,
    selected: usize,
    action: PickerAction,
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
    AuxModel,
    Effort,
    BaseUrl,
    Protocol,
    Providers,
    Fallbacks,
    ApiKey,
    Permission,
    ContextWindow,
    MaxOutput,
    VimMode,
    ShowThinking,
    TokenRate,
    Animations,
    Tooltips,
    DraftReplies,
    TraceLogging,
    MaxSteps,
    ToolOutputLimit,
    ProjectTrust,
    FeedbackEnabled,
    FeedbackDiagnostics,
    FeedbackEndpoint,
    AdvancedToml,
}

/// The config panel's rows: section headings interleaved with settings, so a
/// long flat list becomes something you can scan by area of concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigRow {
    Heading(&'static str),
    Key(ConfigKey),
}

const CONFIG_ROWS: &[ConfigRow] = &[
    ConfigRow::Heading("PROVIDER"),
    ConfigRow::Key(ConfigKey::Profile),
    ConfigRow::Key(ConfigKey::Model),
    ConfigRow::Key(ConfigKey::AuxModel),
    ConfigRow::Key(ConfigKey::Effort),
    ConfigRow::Key(ConfigKey::BaseUrl),
    ConfigRow::Key(ConfigKey::Protocol),
    ConfigRow::Key(ConfigKey::Providers),
    ConfigRow::Key(ConfigKey::Fallbacks),
    ConfigRow::Key(ConfigKey::ApiKey),
    ConfigRow::Heading("AGENT"),
    ConfigRow::Key(ConfigKey::Permission),
    ConfigRow::Key(ConfigKey::MaxSteps),
    ConfigRow::Key(ConfigKey::ContextWindow),
    ConfigRow::Key(ConfigKey::MaxOutput),
    ConfigRow::Key(ConfigKey::ToolOutputLimit),
    ConfigRow::Key(ConfigKey::ProjectTrust),
    ConfigRow::Heading("INTERFACE"),
    ConfigRow::Key(ConfigKey::VimMode),
    ConfigRow::Key(ConfigKey::ShowThinking),
    ConfigRow::Key(ConfigKey::TokenRate),
    ConfigRow::Key(ConfigKey::Animations),
    ConfigRow::Key(ConfigKey::Tooltips),
    ConfigRow::Key(ConfigKey::DraftReplies),
    ConfigRow::Key(ConfigKey::TraceLogging),
    ConfigRow::Heading("PRIVACY"),
    ConfigRow::Key(ConfigKey::FeedbackEnabled),
    ConfigRow::Key(ConfigKey::FeedbackDiagnostics),
    ConfigRow::Key(ConfigKey::FeedbackEndpoint),
    ConfigRow::Heading("ADVANCED"),
    ConfigRow::Key(ConfigKey::AdvancedToml),
];

/// One line explaining what a setting actually does, shown for whichever row
/// the cursor is on. A settings screen that only lists names makes the reader
/// guess; this is the cheapest possible fix for that.
fn config_help(key: ConfigKey) -> &'static str {
    match key {
        ConfigKey::Profile => "Which stored provider profile this session talks to.",
        ConfigKey::Model => "Model ID sent to the provider. /model lists what the endpoint offers.",
        ConfigKey::AuxModel => {
            "Cheaper model on this same endpoint for background calls (rethink, drafts, tether, command checks). Blank = same as the main model."
        }
        ConfigKey::Effort => {
            "How hard the model thinks: minimal, low, medium, high — or blank to leave it to the provider. Models without reasoning ignore it."
        }
        ConfigKey::BaseUrl => "OpenAI-compatible endpoint, including the /v1 suffix.",
        ConfigKey::Protocol => {
            "Chat Completions suits most providers; Responses is OpenAI and xAI."
        }
        ConfigKey::Providers => {
            "OpenRouter only: which suppliers may serve this model, best first. `abacus providers` lists them."
        }
        ConfigKey::Fallbacks => {
            "Off pins strictly — a request fails rather than landing on an unpinned provider."
        }
        ConfigKey::ApiKey => {
            "Stored in credentials.toml with owner-only permissions. Never shown back."
        }
        ConfigKey::Permission => "Ask before every mutation, or allow them for the session.",
        ConfigKey::VimMode => "Esc enters normal mode in the composer instead of clearing it.",
        ConfigKey::ShowThinking => {
            "Show the model's reasoning, where the provider streams it apart from the answer."
        }
        ConfigKey::TokenRate => "Show a live generation rate while a turn runs. Estimated.",
        ConfigKey::Animations => "Spinners and the wave on running tool calls.",
        ConfigKey::Tooltips => "The guidance block on the welcome screen.",
        ConfigKey::DraftReplies => {
            "Predict a likely follow-up in the empty composer. One short model call per turn."
        }
        ConfigKey::TraceLogging => {
            "Append every model call to ~/.abacus/traces as JSONL for fine-tuning. Stays local."
        }
        ConfigKey::MaxSteps => "How many tool calls one turn may make before it stops.",
        ConfigKey::ContextWindow => {
            "Context window override in tokens (accepts 128k / 1m). Blank returns to auto-detection."
        }
        ConfigKey::MaxOutput => {
            "Max output tokens sent as max_tokens (accepts 8k / 64k). Blank returns to auto — set this when the provider rejects the detected value."
        }
        ConfigKey::ToolOutputLimit => "Characters of tool output kept before truncation.",
        ConfigKey::ProjectTrust => {
            "Allow this project's own plugins, hooks, and MCP servers to run."
        }
        ConfigKey::FeedbackEnabled => "Whether /feedback is available at all.",
        ConfigKey::FeedbackDiagnostics => {
            "Attach extension diagnostics to feedback. Never your transcript."
        }
        ConfigKey::FeedbackEndpoint => "Where /feedback submissions are sent.",
        ConfigKey::AdvancedToml => "Open the raw TOML for settings without a row here.",
    }
}

/// The selectable settings, in the order `CONFIG_ROWS` displays them.
///
/// `ConfigPanel::selected` indexes this, and the panel derives its cursor from
/// display position, so the two orderings must agree exactly —
/// `config_rows_and_keys_agree` pins that. They diverged once, and the symptom
/// was the panel editing a different setting from the one under the cursor.
const CONFIG_KEYS: &[ConfigKey] = &[
    ConfigKey::Profile,
    ConfigKey::Model,
    ConfigKey::AuxModel,
    ConfigKey::Effort,
    ConfigKey::BaseUrl,
    ConfigKey::Protocol,
    ConfigKey::Providers,
    ConfigKey::Fallbacks,
    ConfigKey::ApiKey,
    ConfigKey::Permission,
    ConfigKey::MaxSteps,
    ConfigKey::ContextWindow,
    ConfigKey::MaxOutput,
    ConfigKey::ToolOutputLimit,
    ConfigKey::ProjectTrust,
    ConfigKey::VimMode,
    ConfigKey::ShowThinking,
    ConfigKey::TokenRate,
    ConfigKey::Animations,
    ConfigKey::Tooltips,
    ConfigKey::DraftReplies,
    ConfigKey::TraceLogging,
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
    /// The auxiliary-model provider for secondary calls (drafts here; the
    /// agent loop builds its own for rethink/tether/classification).
    aux_provider: Provider,
    messages: Vec<Value>,
    session: Option<Session>,
    session_store: Option<SessionStore>,
    services: Arc<AgentServices>,
    goal: GoalState,
    papercuts: crate::papercuts::PapercutStore,
    memories: crate::memories::MemoryStore,
    tether: crate::tether::TetherState,
    hive: crate::hive::HiveHandle,
    /// Mid-turn arrivals: user steering and finished background subagents.
    injections: crate::agent::InjectionQueue,
    /// Mode-discipline counts behind the escalating reminder.
    modes: crate::modes::ModeCoach,
    /// Whether Abacus is holding the mouse. Holding it enables wheel scrolling
    /// and clickable rows but takes click-drag away from the terminal, which is
    /// how you select and copy text — so it is releasable.
    mouse_captured: bool,
    /// Ctrl+P: the subagent detail overlay.
    hive_overlay: bool,
    hive_scroll: u16,
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
    /// When the in-flight tool call started, so the settled row can report how
    /// long it took.
    tool_started: Option<Instant>,
    /// When the current turn started, for the footer's live elapsed readout.
    turn_started: Option<Instant>,
    /// Characters generated this turn — answer and reasoning both — behind the
    /// optional tokens-per-second readout.
    turn_output_chars: usize,
    /// The turn's accumulated reasoning, mined for a live status header so the
    /// footer says what the model is actually doing, not just "thinking".
    turn_reasoning: String,
    /// Whether any tool ran this turn — gates the "Worked for …" separator.
    turn_had_tools: bool,
    /// Whether the open block is reasoning rather than the answer.
    receiving_thinking: bool,
    /// How the previous turn ended, or `None` if it completed cleanly.
    last_outcome: Option<TurnOutcome>,
    /// When Esc was last pressed on an idle, empty composer — the first press
    /// arms the rewind, a second within the window performs it.
    rewind_armed: Option<Instant>,
    /// Ctrl+O: an open approval/question stepped aside so the transcript
    /// behind it can be read. Reset whenever a new dialog arrives.
    overlay_hidden: bool,
    /// Clickable regions from the last frame. `RefCell` because the draw
    /// helpers take `&App` — recording where something landed is not a
    /// meaningful mutation of application state.
    hits: RefCell<Hits>,
    /// When the last scroll event arrived, used to tell a trackpad's dense
    /// stream from a mouse wheel's discrete notches.
    last_scroll: Option<Instant>,
    /// A predicted next message, offered in the empty composer. Cleared the
    /// moment the user types — it is a suggestion, never a commitment.
    draft: Option<String>,
    draft_tx: mpsc::UnboundedSender<Option<String>>,
    draft_rx: mpsc::UnboundedReceiver<Option<String>>,
    draft_task: Option<JoinHandle<()>>,
    /// Appends one training record per model call. `None` when disabled or when
    /// the session has not been saved yet, since a trace is keyed by session id.
    trace: Option<crate::sft::TraceWriter>,
    /// Raised to ask a running turn to stop. The turn finishes reporting what
    /// it did instead of being killed, which is what kept its tool results.
    cancel: Arc<AtomicBool>,
    /// A provider added but not yet given a model. Abandoning the prompt rolls
    /// it back rather than leaving the session pointed at a profile that
    /// cannot run.
    pending_provider: Option<PendingProvider>,
    /// Selected transcript block, when the user is navigating the scrollback.
    /// `None` means the transcript is just being read, not steered.
    cursor: Option<usize>,
    /// Set when the cursor moves, so the next frame — which is where the row
    /// offsets are actually known — can scroll it into view.
    cursor_pending: bool,
    /// Bumped on every mutation of `entries`; the transcript cache keys on it
    /// so a change to *any* entry invalidates the wrap, not just a change to
    /// the last one.
    entries_rev: u64,
    /// Branch shown in the header. Refreshed between turns rather than per
    /// frame — it only changes when the agent (or the user) moves HEAD.
    git_branch: Option<String>,
    /// Highlighted row in the completion popup, and whether the user has
    /// dismissed it for the text currently in the composer.
    completion_index: usize,
    completion_dismissed: bool,
    /// Wrapped transcript rows, memoised across frames. Re-wrapping the whole
    /// scrollback is the one genuinely expensive thing on the draw path, so it
    /// is recomputed only when the content, width, or spinner phase changes.
    transcript_cache: Option<(TranscriptKey, ui::Transcript)>,
    follow: bool,
    scroll: u16,
    transcript_height: u16,
    /// Text width of the composer from the last frame, so key handling can
    /// move by wrapped rows the same way the renderer lays them out.
    composer_width: u16,
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
                    aux_model: None,
                    reasoning_effort: None,
                    endpoint: None,
                    providers: Vec::new(),
                    allow_fallbacks: true,
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
        let aux_provider = aux_provider_for(&config, &provider);
        let goal = GoalState::new(session.as_ref().and_then(|session| session.goal.clone()));
        let tether = crate::tether::TetherState::new(
            session.as_ref().and_then(|session| session.intent.clone()),
        );
        let hive = crate::hive::HiveHandle::load(config.paths.hive_file.clone());
        let modes = crate::modes::ModeCoach::load(config.paths.modes_file.clone());
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
            entries.push(Entry::new(EntryKind::System, "Session resumed.".to_owned()));
        }
        for diagnostic in services.diagnostics() {
            entries.push(Entry::new(
                EntryKind::Error,
                format!("Extension warning: {diagnostic}"),
            ));
        }
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (feedback_tx, feedback_rx) = mpsc::unbounded_channel();
        let (services_tx, services_rx) = mpsc::unbounded_channel();
        let (draft_tx, draft_rx) = mpsc::unbounded_channel();
        let yes = config.yes;
        let branch = git_branch(&config.workspace);
        let papercuts = crate::papercuts::PapercutStore::load(
            config.paths.papercuts_file.clone(),
            &config.workspace,
        );
        let memories = crate::memories::MemoryStore::load(
            config.paths.memories_file.clone(),
            &config.workspace,
        );
        Ok(Self {
            config,
            settings,
            credentials,
            provider,
            aux_provider,
            messages,
            session,
            session_store,
            services,
            goal,
            papercuts,
            memories,
            tether,
            hive,
            injections: crate::agent::InjectionQueue::default(),
            modes,
            mouse_captured: true,
            hive_overlay: false,
            hive_scroll: 0,
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
            tool_started: None,
            turn_started: None,
            turn_output_chars: 0,
            turn_reasoning: String::new(),
            turn_had_tools: false,
            receiving_thinking: false,
            last_outcome: None,
            rewind_armed: None,
            overlay_hidden: false,
            trace: None,
            last_scroll: None,
            draft: None,
            draft_tx,
            draft_rx,
            draft_task: None,
            cancel: Arc::new(AtomicBool::new(false)),
            pending_provider: None,
            hits: RefCell::new(Hits::default()),
            cursor: None,
            cursor_pending: false,
            entries_rev: 0,
            git_branch: branch,
            completion_index: 0,
            completion_dismissed: false,
            transcript_cache: None,
            follow: true,
            scroll: 0,
            transcript_height: 1,
            composer_width: 40,
            status: "ready".to_owned(),
            ctx_chars,
            input_history: Vec::new(),
            input_history_index: None,
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

    /// Append a transcript entry, invalidating the memoised wrap.
    fn push_entry(&mut self, entry: Entry) {
        self.entries_rev = self.entries_rev.wrapping_add(1);
        self.entries.push(entry);
    }

    /// Replace the whole transcript, as a session resume does.
    fn set_entries(&mut self, entries: Vec<Entry>) {
        self.entries_rev = self.entries_rev.wrapping_add(1);
        self.entries = entries;
    }

    /// The in-flight tool row, if the last entry is one. Bumps the revision
    /// because the caller is about to mutate what it hands back.
    fn open_tool(&mut self) -> Option<&mut ToolCall> {
        self.entries_rev = self.entries_rev.wrapping_add(1);
        self.entries
            .last_mut()
            .filter(|entry| entry.kind == EntryKind::Tool)
            .and_then(|entry| entry.tool.as_mut())
    }

    /// Collapse a run of successful read-only tool rows into one "explored"
    /// row. A session step that reads five files and greps twice becomes a
    /// single `explored read a.rs · grep 'x' · …` line instead of seven rows
    /// of near-identical noise; expanding it shows every result, labelled.
    /// A failed call, a mutation, or any prose between calls breaks the run —
    /// failures and writes must stay individually visible.
    fn group_exploration(&mut self) {
        let count = self.entries.len();
        if count < 2 {
            return;
        }
        let explorable = |call: &ToolCall| {
            call.status == ToolStatus::Ok
                && !call.expanded
                && crate::agent::tool_reads_only(&call.name)
        };
        let current_fits = self.entries[count - 1]
            .tool
            .as_ref()
            .is_some_and(explorable);
        let previous = self.entries[count - 2].tool.as_ref();
        let previous_is_group =
            previous.is_some_and(|call| call.name == "explored" && call.status == ToolStatus::Ok);
        let previous_fits = previous.is_some_and(explorable);
        if !current_fits || (!previous_is_group && !previous_fits) {
            return;
        }
        let Some(current) = self.entries.pop().and_then(|entry| entry.tool) else {
            return;
        };
        let Some(target) = self.open_tool() else {
            return;
        };
        if !previous_is_group {
            target.full = format!(
                "── {} {} ──\n{}",
                target.name,
                target.summary,
                if target.full.is_empty() {
                    "(no output)"
                } else {
                    &target.full
                }
            );
            target.summary = format!("{} {}", explore_verb(&target.name), target.summary);
            target.name = "explored".to_owned();
            // The group header is the content; per-call previews live behind
            // the fold.
            target.output = String::new();
        }
        target.summary = ui::truncate(
            &format!(
                "{} · {} {}",
                target.summary,
                explore_verb(&current.name),
                current.summary
            ),
            200,
        );
        target.full.push_str(&format!(
            "\n\n── {} {} ──\n{}",
            current.name,
            current.summary,
            if current.full.is_empty() {
                "(no output)"
            } else {
                &current.full
            }
        ));
        if target.full.len() > ui::MAX_RETAINED_OUTPUT {
            let mut boundary = ui::MAX_RETAINED_OUTPUT;
            while !target.full.is_char_boundary(boundary) {
                boundary -= 1;
            }
            target.full.truncate(boundary);
        }
        target.duration_ms = match (target.duration_ms, current.duration_ms) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
    }

    /// Kick off a prediction of the user's next message. Only when the composer
    /// is genuinely idle: a draft that appears over something half-typed, or
    /// while the user is mid-thought, would be noise.
    fn start_draft(&mut self) {
        self.draft = None;
        if !self.settings.ui.draft_replies || !self.input.is_empty() || self.running.is_some() {
            return;
        }
        if let Some(task) = self.draft_task.take() {
            task.abort();
        }
        // The next-message recommendation is a secondary call — use the aux
        // model so a heavy main model does not pay for a throwaway guess.
        let provider = self.aux_provider.clone();
        let messages = self.messages.clone();
        let sender = self.draft_tx.clone();
        self.draft_task = Some(tokio::spawn(async move {
            let draft = crate::agent::draft_reply(&provider, &messages).await;
            let _ = sender.send(draft);
        }));
    }

    /// Drop a pending or shown draft. Called as soon as the user does anything
    /// that makes it stale.
    fn clear_draft(&mut self) {
        self.draft = None;
        if let Some(task) = self.draft_task.take() {
            task.abort();
        }
    }

    fn drain_draft_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(draft) = self.draft_rx.try_recv() {
            changed = true;
            // Discard anything that arrived after the user started typing.
            self.draft = draft.filter(|_| self.input.is_empty() && self.running.is_none());
        }
        changed
    }

    fn drain_agent_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.event_rx.try_recv() {
            changed = true;
            match event {
                AgentEvent::Delta(delta) => {
                    self.turn_output_chars = self.turn_output_chars.saturating_add(delta.len());
                    self.receiving_thinking = false;
                    if !self.receiving_delta {
                        self.push_entry(Entry::new(EntryKind::Assistant, String::new()));
                        self.receiving_delta = true;
                    }
                    if let Some(entry) = self.entries.last_mut() {
                        entry.text.push_str(&delta);
                    }
                    // Growing the open assistant entry in place is the one
                    // mutation that does not go through `push_entry`, so it
                    // invalidates the wrap itself.
                    self.entries_rev = self.entries_rev.wrapping_add(1);
                    // Grow the live context estimate: each delta char is roughly
                    // 1 JSON char in the assistant message (+ small JSON wrapper).
                    self.ctx_chars = self.ctx_chars.saturating_add(delta.len() + 40);
                    self.status = "thinking".to_owned();
                }
                AgentEvent::TraceFailed { error } => {
                    // Reported once; capture is already disabled for the run.
                    self.trace = None;
                    self.push_entry(Entry::new(
                        EntryKind::Error,
                        format!("Training trace disabled — {error}"),
                    ));
                }
                AgentEvent::Notice(notice) => {
                    self.push_entry(Entry::new(EntryKind::System, notice));
                    self.follow = true;
                }
                AgentEvent::Reasoning(piece) => {
                    self.turn_output_chars = self.turn_output_chars.saturating_add(piece.len());
                    // The status header follows the reasoning even when the
                    // reasoning itself is hidden — it is the footer's job to
                    // say what the model is doing either way.
                    self.turn_reasoning.push_str(&piece);
                    self.status = reasoning_header(&self.turn_reasoning)
                        .unwrap_or_else(|| "thinking".to_owned());
                    if !self.settings.ui.show_thinking {
                        continue;
                    }
                    // Reasoning accumulates into its own block, so a later
                    // answer starts a fresh one rather than appending to it.
                    if !self.receiving_thinking {
                        self.push_entry(Entry::new(EntryKind::Thinking, String::new()));
                        self.receiving_thinking = true;
                        self.receiving_delta = false;
                    }
                    if let Some(entry) = self.entries.last_mut() {
                        entry.text.push_str(&piece);
                    }
                    self.entries_rev = self.entries_rev.wrapping_add(1);
                    self.status = "thinking".to_owned();
                }
                AgentEvent::Approval(request) => self.set_approval(request),
                AgentEvent::UserQuestion(request) => self.set_user_question(request),
                AgentEvent::ToolStarted { name, summary } => {
                    self.receiving_delta = false;
                    self.turn_had_tools = true;
                    self.tool_started = Some(Instant::now());
                    self.push_entry(Entry::tool(ToolCall {
                        name: name.clone(),
                        summary,
                        status: ToolStatus::Running,
                        output: String::new(),
                        full: String::new(),
                        duration_ms: None,
                        expanded: false,
                    }));
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
                    let duration_ms = self
                        .tool_started
                        .take()
                        .map(|started| started.elapsed().as_millis() as u64);
                    let status = if tool_failed(&output) {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Ok
                    };
                    // Settle the row the matching `ToolStarted` opened, keeping
                    // the argument summary it already shows rather than
                    // replacing the row wholesale.
                    let retained = retain_output(&output);
                    if let Some(call) = self.open_tool() {
                        call.status = status;
                        call.output = preview;
                        call.full = retained;
                        call.duration_ms = duration_ms;
                    } else {
                        self.push_entry(Entry::tool(ToolCall {
                            name,
                            summary: String::new(),
                            status,
                            output: preview,
                            full: retained,
                            duration_ms,
                            expanded: false,
                        }));
                    }
                    self.group_exploration();
                    self.status = "thinking".to_owned();
                }
                AgentEvent::ModeChanged { mode, reason } => {
                    self.resolved_agent_mode = Some(mode);
                    self.push_entry(Entry::new(
                        EntryKind::System,
                        format!("{} mode — {reason}", mode.label()),
                    ));
                    self.status = format!("{} mode", mode.label().to_ascii_lowercase());
                }
                AgentEvent::Done { messages, reason } => {
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
                    self.last_outcome = match reason {
                        DoneReason::Complete => None,
                        DoneReason::Interrupted => Some(TurnOutcome::Interrupted),
                        DoneReason::StepLimit => Some(TurnOutcome::Interrupted),
                    };
                    // A separator after turns that did real work for a while,
                    // so long sessions read in scannable blocks. Conversational
                    // turns get nothing — the rule marks work, not chat.
                    if self.turn_had_tools
                        && let Some(started) = self.turn_started
                        && started.elapsed().as_secs() >= 60
                    {
                        self.push_entry(Entry::new(
                            EntryKind::Rule,
                            format!(
                                "Worked for {}",
                                ui::format_elapsed(started.elapsed().as_millis() as u64)
                            ),
                        ));
                    }
                    // A turn cut short used to look exactly like a finished one.
                    match reason {
                        DoneReason::Complete => {}
                        DoneReason::Interrupted => {
                            self.push_entry(Entry::new(EntryKind::System, "Interrupted."));
                            self.status = "interrupted".to_owned();
                        }
                        DoneReason::StepLimit => {
                            self.push_entry(Entry::new(
                                EntryKind::System,
                                format!(
                                    "Stopped after {} steps — the step limit for one turn. \
                                     Send another message to continue, or raise \
                                     `Maximum agent steps` in /config.",
                                    self.config.max_steps
                                ),
                            ));
                            self.status = "step limit reached".to_owned();
                        }
                    }
                    self.turn_started = None;
                    self.tool_started = None;
                    self.resolved_agent_mode = None;
                    self.receiving_delta = false;
                    self.receiving_thinking = false;
                    if reason == DoneReason::Complete && !continue_loop {
                        self.start_draft();
                    }
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
                }
                AgentEvent::Failed { error, messages } => {
                    self.messages = messages;
                    self.ctx_chars = message_chars(&self.messages);
                    self.running = None;
                    self.last_outcome = Some(TurnOutcome::Failed);
                    self.turn_started = None;
                    self.tool_started = None;
                    self.resolved_agent_mode = None;
                    self.receiving_delta = false;
                    self.approval = None;
                    // Provider rejections are very often not transient: an
                    // interrupted turn leaves history that strict providers
                    // refuse on every retry. Point at the way out.
                    let provider_rejection = error.contains("provider stream error")
                        || error.contains("provider returned");
                    self.push_entry(Entry::new(EntryKind::Error, error));
                    if provider_rejection {
                        self.push_entry(Entry::new(
                            EntryKind::System,
                            "If this error repeats, the session history may be corrupted — \
                             run /repair to check and fix it."
                                .to_owned(),
                        ));
                    }
                    self.status = "error".to_owned();
                    if let Some(state) = &mut self.ralph_loop {
                        let _ = state.pause();
                    }
                    self.persist_session();
                    self.follow = true;
                }
            }
        }
        changed
    }

    fn set_approval(&mut self, request: ApprovalRequest) {
        self.overlay_hidden = false;
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
        self.overlay_hidden = false;
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
                // Steering, not queueing: the message is handed to the running
                // turn, which picks it up after its current tool call. Waiting
                // for the whole turn to end makes a correction arrive too late
                // to change what it was correcting.
                let prompt = self.input.take();
                let prompt = prompt.trim().to_owned();
                self.record_history(&prompt);
                self.push_entry(Entry::new(EntryKind::User, prompt.clone()));
                self.injections
                    .push(crate::agent::Injection::UserMessage(prompt));
                self.follow = true;
                self.status = "steering · delivered after the current step".to_owned();
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

    /// Esc-esc: fork the session from just before the most recent prompt.
    /// The prompt returns to the composer for editing; the turn it produced
    /// is discarded from history (and from the saved session — this is a
    /// fork, not an undo stack). Repeating steps back one prompt at a time.
    fn rewind_to_previous_prompt(&mut self) {
        let Some(entry_index) = self
            .entries
            .iter()
            .rposition(|entry| entry.kind == EntryKind::User)
        else {
            self.status = "nothing to rewind".to_owned();
            return;
        };
        let Some(message_index) = self
            .messages
            .iter()
            .rposition(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        else {
            self.status = "nothing to rewind".to_owned();
            return;
        };
        let prompt = self.entries[entry_index].text.clone();
        self.entries.truncate(entry_index);
        self.entries_rev = self.entries_rev.wrapping_add(1);
        self.clear_cursor();
        self.messages.truncate(message_index);
        self.ctx_chars = message_chars(&self.messages);
        self.clear_draft();
        self.input.clear();
        self.input.insert_str(&prompt);
        self.follow = true;
        self.persist_session();
        self.status = "rewound — edit and resend, or esc esc to step further back".to_owned();
    }

    /// Copy the block under the transcript cursor to the system clipboard —
    /// the full tool output where there is one, not the truncated preview.
    fn yank_selected_block(&mut self) {
        let Some(index) = self.cursor else {
            self.status = "no block selected — j/k to pick one".to_owned();
            return;
        };
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let text = match &entry.tool {
            Some(call) if !call.full.is_empty() => call.full.clone(),
            Some(call) => format!("{} {}\n{}", call.name, call.summary, call.output),
            None => entry.text.clone(),
        };
        self.copy_to_clipboard(&text, "block");
    }

    /// Copy the most recent assistant reply — the thing most often wanted.
    fn yank_last_reply(&mut self) {
        let Some(entry) = self
            .entries
            .iter()
            .rev()
            .find(|entry| entry.kind == EntryKind::Assistant)
        else {
            self.status = "no reply to copy yet".to_owned();
            return;
        };
        let text = entry.text.clone();
        self.copy_to_clipboard(&text, "reply");
    }

    fn copy_to_clipboard(&mut self, text: &str, what: &str) {
        if text.trim().is_empty() {
            self.status = format!("{what} is empty");
            return;
        }
        match crate::clipboard::write_text(text) {
            Ok(()) => {
                let lines = text.lines().count();
                self.status = format!("copied {what} — {lines} line(s)");
            }
            Err(error) => self.status = format!("could not copy: {error:#}"),
        }
    }

    /// Ctrl+V. An image on the clipboard is saved under the attachments
    /// directory and referenced from the composer with a short `[image:…]`
    /// token the user can still edit around or delete; text is inserted as-is.
    fn paste_from_clipboard(&mut self) {
        match crate::clipboard::read_image() {
            Ok(Some(image)) => {
                match crate::clipboard::save_attachment(&self.config.paths.attachments_dir, &image)
                {
                    Ok((token, _)) => {
                        let needs_space =
                            !self.input.is_empty() && !self.input.text().ends_with(' ');
                        if needs_space {
                            self.input.insert(' ');
                        }
                        self.input.insert_str(&token);
                        self.status = format!("image attached ({}x{})", image.width, image.height);
                    }
                    Err(error) => self.status = format!("could not save image: {error:#}"),
                }
                return;
            }
            Ok(None) => {}
            Err(error) => {
                // No clipboard backend at all: still try the text utilities
                // before reporting, so plain text paste keeps working on
                // setups arboard cannot reach.
                if let Some(text) = clipboard_text() {
                    self.input.insert_str(&text);
                } else {
                    self.status = format!("{error:#}");
                }
                return;
            }
        }
        if let Some(text) = clipboard_text() {
            self.input.insert_str(&text);
        }
    }

    fn start_turn(&mut self, display_prompt: String, effective_prompt: String, display: bool) {
        if self.running.is_some() {
            return;
        }
        self.turn_started = Some(Instant::now());
        self.turn_output_chars = 0;
        self.hive.board.clear();
        self.turn_reasoning.clear();
        self.turn_had_tools = false;
        self.receiving_thinking = false;
        self.last_outcome = None;
        self.clear_draft();
        self.cancel.store(false, Ordering::Relaxed);
        if display {
            self.push_entry(Entry::new(EntryKind::User, display_prompt.clone()));
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
        // Resolve `[image:…]` paste tokens and `@file.png` references into
        // vision content parts; a text-only prompt stays a plain string.
        let content = crate::context::user_content(
            &self.config.workspace,
            &self.config.paths.attachments_dir,
            &model_prompt,
        );
        let message = json!({"role": "user", "content": content});
        self.ctx_chars = self
            .ctx_chars
            .saturating_add(crate::agent::message_chars_one(&message));
        self.messages.push(message);
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
            trace: self.trace.clone(),
            cancel: self.cancel.clone(),
            workspace: self.config.workspace.clone(),
            max_steps: self.config.max_steps,
            tool_output_limit: self.config.tool_output_limit,
            mode: agent_mode,
            allow_mutations,
            services: self.services.clone(),
            session_id: self.session.as_ref().map(|session| session.id.to_string()),
            goal: self.goal.clone(),
            papercuts: self.papercuts.clone(),
            memories: self.memories.clone(),
            tether: self.tether.clone(),
            hive: self.hive.clone(),
            aux_model: self.config.aux_model.clone(),
            injections: self.injections.clone(),
            modes: self.modes.clone(),
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
            "/btw" => {
                let note = argument.trim().to_owned();
                if note.is_empty() {
                    self.push_entry(Entry::new(
                        EntryKind::Error,
                        "Usage: /btw <side question or remark>".to_owned(),
                    ));
                    self.follow = true;
                    return true;
                }
                if self.running.is_none() {
                    // With nothing running there is nothing to avoid derailing,
                    // and a note the model only sees "later" would just be lost.
                    self.push_entry(Entry::new(
                        EntryKind::System,
                        "/btw is for while a turn is running — ask it directly instead.".to_owned(),
                    ));
                    self.follow = true;
                    return true;
                }
                self.push_entry(Entry::new(
                    EntryKind::System,
                    format!("Noted, by the way: {note}"),
                ));
                self.injections
                    .push(crate::agent::Injection::SideNote(note));
                self.status = "noted · delivered after the current step".to_owned();
                self.follow = true;
                true
            }
            "/effort" => {
                let argument = argument.trim();
                if argument.is_empty() {
                    let current = self
                        .config
                        .reasoning_effort
                        .map(|effort| effort.label().to_owned())
                        .unwrap_or_else(|| "auto (provider default)".to_owned());
                    self.push_entry(Entry::new(
                        EntryKind::System,
                        format!(
                            "Reasoning effort: {current}. Set it with /effort \
                             minimal|low|medium|high|xhigh|max, or /effort auto to leave it to the \
                             provider."
                        ),
                    ));
                    self.follow = true;
                    return true;
                }
                let cleared = matches!(
                    argument.to_ascii_lowercase().as_str(),
                    "auto" | "default" | "unset"
                );
                let parsed = crate::config::ReasoningEffort::parse(argument);
                if !cleared && parsed.is_none() {
                    self.push_entry(Entry::new(
                        EntryKind::Error,
                        "Usage: /effort minimal|low|medium|high|xhigh|max|auto".to_owned(),
                    ));
                    self.follow = true;
                    return true;
                }
                if let Ok(profile) = self.active_profile_mut() {
                    profile.reasoning_effort = parsed;
                }
                match self.save_and_apply_settings() {
                    Ok(()) => {
                        let described = parsed
                            .map(|effort| effort.label().to_owned())
                            .unwrap_or_else(|| "auto".to_owned());
                        self.status = format!("effort {described}");
                        self.push_entry(Entry::new(
                            EntryKind::System,
                            match parsed {
                                Some(_) => format!(
                                    "Reasoning effort set to {described}. Sent with every request \
                                     on this profile; models without reasoning ignore it."
                                ),
                                None => "Reasoning effort cleared — the provider's own default \
                                         applies."
                                    .to_owned(),
                            },
                        ));
                    }
                    Err(error) => self.status = format!("configuration error: {error:#}"),
                }
                self.follow = true;
                true
            }
            "/model" => {
                if argument.trim().is_empty() {
                    self.push_entry(Entry::new(EntryKind::System, format!(
                            "Model: {}\nEndpoint: {}\n\nSwitch with /model <id>; discover IDs with `abacus models`.",
                            self.config.model, self.config.base_url
                        )
                    ));
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
            // OpenRouter fronts many suppliers for one model and they differ in
            // context length and quantization, so which one serves a request is
            // a decision worth making rather than accepting by default.
            "/providers" => {
                let argument = argument.trim();
                if argument.is_empty() {
                    let profile = self.settings.profiles.get(&self.settings.default_profile);
                    let pinned = profile
                        .map(|profile| profile.providers.clone())
                        .unwrap_or_default();
                    let body = if pinned.is_empty() {
                        "No providers pinned — the endpoint chooses.".to_owned()
                    } else {
                        format!(
                            "Pinned, in order: {}\nFallbacks: {}",
                            pinned.join(", "),
                            on_off(profile.is_none_or(|profile| profile.allow_fallbacks))
                                .to_ascii_lowercase()
                        )
                    };
                    self.push_entry(Entry::new(
                        EntryKind::System,
                        format!(
                            "{body}\n\nSet with /providers <name, name>; \
                             /providers clear removes the pin; \
                             /providers strict|fallback controls whether anything else may serve it. \
                             List what is available with `abacus providers`."
                        ),
                    ));
                    self.follow = true;
                    return true;
                }
                let lowered = argument.to_ascii_lowercase();
                let outcome = match lowered.as_str() {
                    "clear" | "none" | "off" => self.active_profile_mut().map(|profile| {
                        profile.providers.clear();
                        "providers unpinned".to_owned()
                    }),
                    "strict" => self.active_profile_mut().map(|profile| {
                        profile.allow_fallbacks = false;
                        "strict: only pinned providers may serve this model".to_owned()
                    }),
                    "fallback" | "fallbacks" => self.active_profile_mut().map(|profile| {
                        profile.allow_fallbacks = true;
                        "fallbacks allowed".to_owned()
                    }),
                    _ => {
                        let order = crate::config::Routing::parse_order(argument);
                        self.active_profile_mut().map(|profile| {
                            let summary = order.join(", ");
                            profile.providers = order;
                            format!("pinned to {summary}")
                        })
                    }
                };
                match outcome.and_then(|summary| {
                    self.save_and_apply_settings()?;
                    Ok(summary)
                }) {
                    Ok(summary) => self.status = summary,
                    Err(error) => self.status = format!("routing error: {error:#}"),
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
                self.push_entry(Entry::new(
                    EntryKind::System,
                    format!("Tools: {}", names.join(", ")),
                ));
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
                self.push_entry(Entry::new(
                    EntryKind::System,
                    if text.is_empty() {
                        "No skills discovered.".to_owned()
                    } else {
                        format!("Skills\n{text}")
                    },
                ));
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
                self.push_entry(Entry::new(
                    EntryKind::System,
                    if text.is_empty() {
                        "No plugins enabled.".to_owned()
                    } else {
                        format!("Plugins\n{text}")
                    },
                ));
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
                self.push_entry(Entry::new(
                    EntryKind::System,
                    if text.is_empty() {
                        "No MCP tools connected.".to_owned()
                    } else {
                        format!("MCP tools\n{text}")
                    },
                ));
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
            // Worth a command of its own rather than only a /config row: whether
            // you want to watch a model reason changes from task to task, and
            // reaching for it should not mean opening a panel.
            "/thinking" => {
                let requested = match argument.trim().to_ascii_lowercase().as_str() {
                    "" => Some(!self.settings.ui.show_thinking),
                    "on" | "show" | "yes" => Some(true),
                    "off" | "hide" | "no" => Some(false),
                    _ => None,
                };
                let Some(show) = requested else {
                    self.push_entry(Entry::new(
                        EntryKind::Error,
                        "Usage: /thinking [on|off]".to_owned(),
                    ));
                    self.follow = true;
                    return true;
                };
                self.settings.ui.show_thinking = show;
                match self.save_and_apply_settings() {
                    Ok(()) => {
                        self.status = if show {
                            "thinking shown".to_owned()
                        } else {
                            "thinking hidden".to_owned()
                        };
                        // Say where it went, so hiding it does not look like
                        // the reasoning stopped being captured.
                        self.push_entry(Entry::new(
                            EntryKind::System,
                            if show {
                                "Reasoning will be shown above each reply.".to_owned()
                            } else {
                                "Reasoning hidden. It is still recorded in training traces."
                                    .to_owned()
                            },
                        ));
                    }
                    Err(error) => {
                        self.settings.ui.show_thinking = !show;
                        self.status = format!("configuration error: {error:#}");
                    }
                }
                self.follow = true;
                true
            }
            "/mode" => {
                let requested = match argument.trim().to_ascii_lowercase().as_str() {
                    "" => None,
                    "auto" => Some(AgentMode::Auto),
                    "plan" => Some(AgentMode::Plan),
                    "build" => Some(AgentMode::Build),
                    _ => {
                        self.push_entry(Entry::new(
                            EntryKind::Error,
                            "Usage: /mode auto|plan|build".to_owned(),
                        ));
                        self.follow = true;
                        return true;
                    }
                };
                if let Some(mode) = requested {
                    self.agent_mode = mode;
                    self.status = format!("{} mode", mode.label().to_ascii_lowercase());
                } else {
                    self.push_entry(Entry::new(EntryKind::System, format!(
                            "Mode: {}\nAUTO lets the model choose PLAN or BUILD per turn; pinned modes enforce your choice.",
                            self.agent_mode.label()
                        )
                    ));
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
                // Sized from the model's own window, not a fixed number. A
                // hardcoded 160k chars cut a 1M-context session down to a few
                // percent of what it could hold, and under-compacted a small one.
                let budget = self.config.model_limits.compaction_budget();
                self.messages = compact_messages(&self.messages, budget.recent_budget_chars);
                self.persist_session();
                self.push_entry(Entry::new(
                    EntryKind::System,
                    format!(
                        "Quick-compacted conversation from {before} to {} messages, \
                         targeting {} chars for this model. Dropped messages are not summarised; \
                         rolling-summary compaction runs automatically as the context grows.",
                        self.messages.len(),
                        budget.recent_budget_chars
                    ),
                ));
                self.follow = true;
                true
            }
            "/repair" => {
                // An interrupted or failed turn can leave the saved history in
                // a state strict providers reject wholesale (a tool call whose
                // streamed arguments were cut short, a call with no result).
                // The failure mode is every turn erroring from then on, so the
                // fix has to be reachable from inside the stuck session.
                if self.running.is_some() {
                    self.status = "cannot repair while a turn is running".to_owned();
                    return true;
                }
                let fixes = crate::session::repair_messages(&mut self.messages);
                if fixes.is_empty() {
                    self.push_entry(Entry::new(
                        EntryKind::System,
                        "No corruption found: every tool call parses and has a result.".to_owned(),
                    ));
                } else {
                    self.ctx_chars = message_chars(&self.messages);
                    self.persist_session();
                    self.push_entry(Entry::new(
                        EntryKind::System,
                        format!("Repaired the session history: {}.", fixes.join("; ")),
                    ));
                }
                self.follow = true;
                true
            }
            "/papercuts" => {
                let argument = argument.trim();
                let snapshot = self.papercuts.snapshot();
                if let Some(target) = argument.strip_prefix("delete") {
                    let target = target.trim();
                    let removed = target
                        .parse::<usize>()
                        .ok()
                        .and_then(|number| snapshot.get(number.saturating_sub(1)))
                        .map(|papercut| (papercut.title.clone(), papercut.id));
                    match removed {
                        Some((title, id)) if self.papercuts.remove(id) => {
                            self.push_entry(Entry::new(
                                EntryKind::System,
                                format!("Papercut \"{title}\" deleted."),
                            ));
                        }
                        _ => {
                            self.push_entry(Entry::new(
                                EntryKind::Error,
                                "Usage: /papercuts delete <number> — numbers from /papercuts"
                                    .to_owned(),
                            ));
                        }
                    }
                } else if snapshot.is_empty() {
                    self.push_entry(Entry::new(
                        EntryKind::System,
                        "No papercuts yet. When Abacus works through a snag it records the \
                         lesson here and recalls it the next time a tripwire matches."
                            .to_owned(),
                    ));
                } else {
                    let now = chrono::Utc::now();
                    let mut lines = vec![format!(
                        "{} papercut(s) for this workspace:",
                        snapshot.len()
                    )];
                    for (index, papercut) in snapshot.iter().enumerate() {
                        lines.push(format!(
                            "{}. {} — tripped {}x, recalled {}x, strength {:.1}\n   fix: {}\n   tripwires: {}",
                            index + 1,
                            papercut.title,
                            papercut.trip_count,
                            papercut.recall_count,
                            papercut.decayed_strength(now),
                            papercut.fix,
                            papercut.tripwires.join(" · "),
                        ));
                    }
                    lines.push("Delete one with /papercuts delete <number>.".to_owned());
                    self.push_entry(Entry::new(EntryKind::System, lines.join("\n")));
                }
                self.follow = true;
                true
            }
            "/memories" => {
                let argument = argument.trim();
                // One ordering for display and delete alike, or the numbers
                // the user sees would target different entries.
                let mut snapshot = self.memories.snapshot();
                snapshot.sort_by_key(|memory| std::cmp::Reverse(memory.updated_at));
                if let Some(target) = argument.strip_prefix("delete") {
                    let target = target.trim();
                    let removed = target
                        .parse::<usize>()
                        .ok()
                        .and_then(|number| snapshot.get(number.saturating_sub(1)))
                        .map(|memory| (memory.title.clone(), memory.id));
                    match removed {
                        Some((title, id)) if self.memories.remove(id) => {
                            self.push_entry(Entry::new(
                                EntryKind::System,
                                format!("Memory \"{title}\" deleted."),
                            ));
                        }
                        _ => {
                            self.push_entry(Entry::new(
                                EntryKind::Error,
                                "Usage: /memories delete <number> — numbers from /memories"
                                    .to_owned(),
                            ));
                        }
                    }
                } else if snapshot.is_empty() {
                    self.push_entry(Entry::new(
                        EntryKind::System,
                        "No memories yet. Abacus records durable knowledge here — on its \
                         own after long turns (rethink), or whenever the model calls \
                         memory_record — and injects it into future sessions."
                            .to_owned(),
                    ));
                } else {
                    let mut lines = vec![format!(
                        "{} memori(es) for this workspace, newest first:",
                        snapshot.len()
                    )];
                    for (index, memory) in snapshot.iter().enumerate() {
                        lines.push(format!(
                            "{}. {} — {}",
                            index + 1,
                            memory.title,
                            crate::ui::truncate(&memory.body, 120),
                        ));
                    }
                    lines.push("Delete one with /memories delete <number>.".to_owned());
                    self.push_entry(Entry::new(EntryKind::System, lines.join("\n")));
                }
                self.follow = true;
                true
            }
            value if value.starts_with('/') => {
                self.push_entry(Entry::new(
                    EntryKind::Error,
                    format!("Unknown command: {value}"),
                ));
                self.follow = true;
                true
            }
            _ => false,
        }
    }

    fn persist_session(&mut self) {
        // A turn may have switched branches or committed; re-read cheaply here
        // rather than on the draw path.
        self.git_branch = git_branch(&self.config.workspace);

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
        // The trace is keyed by session id, so it can only be opened once the
        // session exists — which is here, on the first persist. `start_turn`
        // persists before spawning, so the first model call is already covered.
        if self.trace.is_none()
            && self.config.trace_enabled
            && let Some(session) = &self.session
        {
            match crate::sft::TraceWriter::open(
                &self.config.paths.traces_dir,
                &session.id.to_string(),
            ) {
                Ok(writer) => self.trace = Some(writer),
                Err(error) => self.status = format!("training trace disabled: {error:#}"),
            }
        }
        let Some(session) = &mut self.session else {
            return;
        };
        session.update_messages(self.messages.clone());
        session.intent = self.tether.intent();
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
            Ok(text) => self.push_entry(Entry::new(EntryKind::System, text)),
            Err(error) => self.push_entry(Entry::new(
                EntryKind::Error,
                format!("Goal error: {error:#}"),
            )),
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
            self.push_entry(Entry::new(
                EntryKind::System,
                "Usage: /swarm <objective>. Abacus splits the objective into independent \
                       units and delegates them to parallel subagents (one approval, isolated git \
                       worktrees). Best for separable work; a single repository is required."
                    .to_owned(),
            ));
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
            self.push_entry(Entry::new(EntryKind::System, text));
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
                self.push_entry(Entry::new(EntryKind::Error, format!("Could not start loop: {error:#}\n\nUsage: /loop \"<prompt>\" --max-iterations 20 --completion-promise \"DONE\"")
                ));
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
        self.push_entry(Entry::new(
            EntryKind::System,
            format!("Ralph loop · iteration {iteration}"),
        ));
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
        self.push_entry(Entry::new(
            EntryKind::System,
            "Ralph loop cancelled by user.".to_owned(),
        ));
        self.follow = true;
    }

    fn new_session(&mut self) {
        self.persist_session();
        self.messages = initial_messages(&self.config.workspace);
        self.session = None; // persist_session recreates lazily on first send
        self.set_entries(Vec::new());
        self.goal = GoalState::default();
        self.tasks = TaskList::default();
        self.compaction = CompactionState::default();
        self.ralph_loop = None;
        self.tokens.store(0, Ordering::Relaxed);
        self.session_initial_active_secs = 0;
        self.started = Instant::now();
        self.push_entry(Entry::new(EntryKind::System, "New session.".to_owned()));
        self.scroll = 0;
        self.follow = true;
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
            Ok(sessions) if sessions.is_empty() => self.push_entry(Entry::new(
                EntryKind::System,
                "No saved sessions for this workspace.".to_owned(),
            )),
            Ok(sessions) => {
                self.picker = Some(Picker {
                    title: "sessions".to_owned(),
                    action: PickerAction::ResumeSession,
                    items: sessions
                        .into_iter()
                        .take(50)
                        .map(|session| {
                            (
                                format!(
                                    "{}  {}  {}",
                                    &session.id.to_string()[..8],
                                    session
                                        .updated_at
                                        .with_timezone(&Local)
                                        .format("%m-%d %H:%M"),
                                    session.title
                                ),
                                session.id.to_string(),
                            )
                        })
                        .collect(),
                    selected: 0,
                });
            }
            Err(error) => self.push_entry(Entry::new(
                EntryKind::Error,
                format!("Could not list sessions: {error}"),
            )),
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
                self.set_entries(entries_from_messages(&self.messages));
                self.push_entry(Entry::new(
                    EntryKind::System,
                    format!(
                        "Resumed {} ({})",
                        session.title,
                        &session.id.to_string()[..8]
                    ),
                ));
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
                self.push_entry(Entry::new(
                    EntryKind::Error,
                    format!("Could not resume session: {error}"),
                ));
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
        self.config.aux_model = profile
            .aux_model
            .clone()
            .filter(|model| !model.trim().is_empty());
        self.config.reasoning_effort = profile.reasoning_effort;
        self.config.base_url = profile.base_url.trim_end_matches('/').to_owned();
        self.config.protocol = profile.protocol;
        // Re-resolve the scripted endpoint for the new profile — without this a
        // switch away from a scripted profile (e.g. an Anthropic OAuth one)
        // left the old endpoint's URL, auth, and wire format attached, so the
        // "switched" profile kept talking to the previous endpoint.
        self.config.endpoint = match &profile.endpoint {
            Some(reference) => {
                match crate::endpoint::ScriptedEndpoint::resolve(
                    reference,
                    &self.config.paths.endpoints_dir,
                ) {
                    Ok(endpoint) => {
                        // A scripted endpoint is the authority on its own URL,
                        // protocol, and (when the profile omits it) model.
                        self.config.base_url = endpoint.url.trim_end_matches('/').to_owned();
                        self.config.protocol = endpoint.protocol;
                        if self.config.model.trim().is_empty()
                            && let Some(model) = &endpoint.model
                        {
                            self.config.model = model.clone();
                        }
                        Some(endpoint)
                    }
                    Err(error) => {
                        return Err(error).context("scripted endpoint for this profile");
                    }
                }
            }
            None => None,
        };
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
        // Re-resolve the model limits when an override is in play — either
        // newly set (it must reach the provider and the compaction budget
        // immediately) or newly cleared (the Override source must not stick).
        // With no override involved, startup detection is left alone.
        if self.settings.agent.context_window.is_some()
            || self.settings.agent.max_output_tokens.is_some()
            || self.config.model_limits.source == crate::model_info::LimitSource::Override
        {
            self.config.model_limits = crate::model_info::ModelLimits::resolve_from_name(
                &self.config.model,
                self.settings.agent.context_window,
                self.settings.agent.max_output_tokens,
            );
        }
        let always = self.settings.ui.permission_mode == PermissionMode::AlwaysApprove;
        self.config.yes = always;
        self.allow_mutations
            .store(always, std::sync::atomic::Ordering::Relaxed);
        if !self.settings.ui.vim_mode {
            self.mode = InputMode::Insert;
        }
        self.provider = Provider::with_tokens(&self.config, self.tokens.clone())?;
        self.aux_provider = aux_provider_for(&self.config, &self.provider);
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
                self.push_entry(Entry::new(EntryKind::System, format!(
                        "Theme: {} (showing {}). Switch with /theme dark, /theme light, or /theme auto.",
                        self.settings.ui.theme.label(),
                        if resolved == ThemeMode::Dark { "dark" } else { "light" },
                    )
                ));
                self.follow = true;
                return;
            }
            "auto" => ThemeChoice::Auto,
            "dark" => ThemeChoice::Dark,
            "light" => ThemeChoice::Light,
            _ => {
                self.push_entry(Entry::new(
                    EntryKind::Error,
                    "Usage: /theme auto|dark|light".to_owned(),
                ));
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
                self.open_profile_picker();
                return Ok(());
            }
            ConfigKey::Fallbacks => {
                let profile = self.active_profile_mut()?;
                profile.allow_fallbacks = !profile.allow_fallbacks;
            }
            ConfigKey::Protocol => {
                let profile = self.active_profile_mut()?;
                // Rotate through all three, wrapping at the end:
                // chat-completions → responses → anthropic → chat-completions.
                profile.protocol = match profile.protocol {
                    ProviderProtocol::ChatCompletions => ProviderProtocol::Responses,
                    ProviderProtocol::Responses => ProviderProtocol::Anthropic,
                    ProviderProtocol::Anthropic => ProviderProtocol::ChatCompletions,
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
            ConfigKey::ShowThinking => {
                self.settings.ui.show_thinking = !self.settings.ui.show_thinking
            }
            ConfigKey::TokenRate => {
                self.settings.ui.show_token_rate = !self.settings.ui.show_token_rate
            }
            ConfigKey::Animations => self.settings.ui.animations = !self.settings.ui.animations,
            ConfigKey::Tooltips => self.settings.ui.show_tooltips = !self.settings.ui.show_tooltips,
            ConfigKey::DraftReplies => {
                self.settings.ui.draft_replies = !self.settings.ui.draft_replies;
                if !self.settings.ui.draft_replies {
                    self.clear_draft();
                }
            }
            ConfigKey::TraceLogging => {
                self.settings.trace.enabled = !self.settings.trace.enabled;
                self.config.trace_enabled = self.settings.trace.enabled;
                if self.settings.trace.enabled {
                    // Opened on the next persist, which keeps one code path
                    // responsible for creating it.
                    self.persist_session();
                } else {
                    self.trace = None;
                }
            }
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
        // A secret is never seeded into the editor: `config_value` reports only
        // where the key came from, so pre-filling would put that description
        // into the field and, worse, invite showing the real value.
        if key == ConfigKey::ApiKey {
            if let Some(panel) = &mut self.config_panel {
                panel.editing = Some((key, InputBuffer::new()));
            }
            return;
        }
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
            ConfigKey::AuxModel => self.active_profile_mut().map(|profile| {
                let trimmed = value.trim();
                profile.aux_model = (!trimmed.is_empty()).then(|| trimmed.to_owned());
            }),
            ConfigKey::Effort => {
                let trimmed = value.trim();
                let parsed = crate::config::ReasoningEffort::parse(trimmed);
                if !trimmed.is_empty()
                    && !matches!(trimmed.to_ascii_lowercase().as_str(), "auto" | "default")
                    && parsed.is_none()
                {
                    Err(anyhow::anyhow!(
                        "effort must be minimal, low, medium, high, xhigh, max, or auto"
                    ))
                } else {
                    self.active_profile_mut().map(|profile| {
                        profile.reasoning_effort = parsed;
                    })
                }
            }
            ConfigKey::BaseUrl => self.active_profile_mut().map(|profile| {
                profile.base_url = value.trim().trim_end_matches('/').to_owned();
            }),
            ConfigKey::Providers => self.active_profile_mut().map(|profile| {
                profile.providers = crate::config::Routing::parse_order(&value);
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
            // Blank returns the limit to auto-resolution; a value (with `k`/`m`
            // suffixes accepted) becomes a hard override sent to the provider.
            ConfigKey::ContextWindow => parse_optional_tokens(&value).map(|tokens| {
                self.settings.agent.context_window = tokens;
            }),
            ConfigKey::MaxOutput => parse_optional_tokens(&value).map(|tokens| {
                self.settings.agent.max_output_tokens = tokens;
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
            // The key goes to credentials.toml, which is written separately
            // from settings and kept owner-only.
            ConfigKey::ApiKey => {
                let profile = self.settings.default_profile.clone();
                let trimmed = value.trim().to_owned();
                if trimmed.is_empty() {
                    self.credentials.keys.remove(&profile);
                } else {
                    self.credentials.keys.insert(profile, trimmed);
                }
                self.credentials.save(&self.config.paths)
            }
            _ => Ok(()),
        }
        .and_then(|()| self.save_and_apply_settings());
        match result {
            Ok(()) => {
                if key == ConfigKey::Model {
                    self.pending_provider = None;
                }
                self.status = format!("{} saved", config_label(key));
            }
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
            ConfigKey::AuxModel => profile
                .and_then(|value| value.aux_model.clone())
                .filter(|model| !model.trim().is_empty())
                .unwrap_or_else(|| "(same as main)".to_owned()),
            ConfigKey::Effort => profile
                .and_then(|value| value.reasoning_effort)
                .map(|effort| effort.label().to_owned())
                .unwrap_or_else(|| "auto".to_owned()),
            ConfigKey::BaseUrl => profile
                .map(|value| value.base_url.clone())
                .unwrap_or_default(),
            ConfigKey::Protocol => profile
                .map(|value| format!("{:?}", value.protocol))
                .unwrap_or_default(),
            ConfigKey::Providers => {
                let pinned = profile
                    .map(|profile| profile.providers.clone())
                    .unwrap_or_default();
                if pinned.is_empty() {
                    "any (endpoint chooses)".to_owned()
                } else {
                    pinned.join(", ")
                }
            }
            ConfigKey::Fallbacks => on_off(profile.is_none_or(|profile| profile.allow_fallbacks)),
            // Report where the credential comes from, never the credential.
            ConfigKey::ApiKey => {
                let env = profile.and_then(|value| value.api_key_env.as_deref());
                let in_env =
                    env.is_some_and(|name| std::env::var(name).is_ok_and(|v| !v.trim().is_empty()));
                if in_env {
                    format!("set · {} from environment", env.unwrap_or_default())
                } else if self
                    .credentials
                    .keys
                    .get(&self.settings.default_profile)
                    .is_some_and(|key| !key.trim().is_empty())
                {
                    "set · stored locally".to_owned()
                } else if env.is_some() {
                    format!(
                        "not set · export {} or press enter",
                        env.unwrap_or_default()
                    )
                } else {
                    "not set".to_owned()
                }
            }
            ConfigKey::Permission => format!("{:?}", self.settings.ui.permission_mode),
            ConfigKey::VimMode => on_off(self.settings.ui.vim_mode),
            ConfigKey::ShowThinking => on_off(self.settings.ui.show_thinking),
            ConfigKey::TokenRate => on_off(self.settings.ui.show_token_rate),
            ConfigKey::Animations => on_off(self.settings.ui.animations),
            ConfigKey::Tooltips => on_off(self.settings.ui.show_tooltips),
            ConfigKey::DraftReplies => on_off(self.settings.ui.draft_replies),
            ConfigKey::TraceLogging => match (&self.trace, self.settings.trace.enabled) {
                (Some(trace), true) => format!("On · {} records", trace.steps()),
                (None, true) => "On".to_owned(),
                (_, false) => "Off".to_owned(),
            },
            ConfigKey::MaxSteps => self.settings.agent.max_steps.to_string(),
            // The override when set; otherwise what auto-resolution landed on
            // and where it came from, so "auto" is never a mystery value.
            ConfigKey::ContextWindow => match self.settings.agent.context_window {
                Some(tokens) => ui::format_count(tokens as u64),
                None => format!(
                    "auto — {} ({})",
                    ui::format_count(self.config.model_limits.context_window as u64),
                    limit_source_label(self.config.model_limits.source),
                ),
            },
            ConfigKey::MaxOutput => match self.settings.agent.max_output_tokens {
                Some(tokens) => ui::format_count(tokens as u64),
                None => match self.config.model_limits.configured_output_tokens {
                    Some(tokens) => format!(
                        "auto — {} ({})",
                        ui::format_count(tokens as u64),
                        limit_source_label(self.config.model_limits.source),
                    ),
                    None => "auto — server default".to_owned(),
                },
            },
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

    /// Offer the stored profiles, plus a way to add one. Cycling was the only
    /// way to switch before, which does nothing visible when there is a single
    /// profile and gives no way to create a second.
    /// Undo a provider that never got a model, restoring the previous profile.
    fn cancel_pending_provider(&mut self) {
        let Some(pending) = self.pending_provider.take() else {
            return;
        };
        // Only roll back a profile that is still unusable; if the user gave it
        // a model by another route, leave it alone.
        let unfinished = self
            .settings
            .profiles
            .get(&pending.profile)
            .is_some_and(|profile| profile.model.trim().is_empty());
        if !unfinished {
            return;
        }
        self.settings.profiles.remove(&pending.profile);
        self.settings.default_profile = pending.previous;
        let _ = self.settings.save(&self.config.paths);
        self.status = "provider discarded — no model was set".to_owned();
    }

    fn open_profile_picker(&mut self) {
        let mut items = self
            .settings
            .profiles
            .iter()
            .map(|(id, profile)| {
                let marker = if *id == self.settings.default_profile {
                    "● "
                } else {
                    "  "
                };
                (
                    format!("{marker}{id}  —  {}  ·  {}", profile.name, profile.model),
                    id.clone(),
                )
            })
            .collect::<Vec<_>>();
        items.push((
            "  + Add a provider…".to_owned(),
            NEW_PROVIDER_SENTINEL.to_owned(),
        ));
        self.picker = Some(Picker {
            title: "profile".to_owned(),
            items,
            selected: 0,
            action: PickerAction::SwitchProfile,
        });
    }

    /// The provider list from `abacus setup`, offered inside the TUI so a
    /// second provider does not require quitting and re-running the wizard.
    fn open_provider_picker(&mut self) {
        let mut items = crate::setup::PRESETS
            .iter()
            .map(|preset| {
                let key = match preset.env_key {
                    Some(name) if crate::setup::key_in_env(preset) => format!("✓ {name}"),
                    Some(name) => format!("  {name}"),
                    None => "  no key needed".to_owned(),
                };
                (
                    format!("  {:<18}{:<32}{key}", preset.name, preset.hint),
                    preset.id.to_owned(),
                )
            })
            .collect::<Vec<_>>();
        // Scripted endpoints from ~/.abacus/endpoints, so a YAML-defined
        // provider (OAuth, custom headers, Anthropic protocol) is selectable
        // here rather than only by hand-editing config.toml.
        for name in self.scripted_endpoint_names() {
            items.push((
                format!("  {name:<18}scripted endpoint (~/.abacus/endpoints)"),
                format!("{ENDPOINT_SENTINEL_PREFIX}{name}"),
            ));
        }
        items.push((
            "  Custom OpenAI-compatible endpoint".to_owned(),
            CUSTOM_PROVIDER_SENTINEL.to_owned(),
        ));
        self.picker = Some(Picker {
            title: "provider".to_owned(),
            items,
            selected: 0,
            action: PickerAction::AddProvider,
        });
    }

    /// Names of the scripted endpoints defined under ~/.abacus/endpoints,
    /// sorted, for the provider picker.
    fn scripted_endpoint_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.config.paths.endpoints_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let extension = path.extension().and_then(|value| value.to_str());
                if matches!(extension, Some("yaml") | Some("yml"))
                    && let Some(stem) = path.file_stem().and_then(|value| value.to_str())
                {
                    names.push(stem.to_owned());
                }
            }
        }
        names.sort();
        names
    }

    /// Create a profile that references a scripted endpoint. The endpoint's
    /// URL, model, and protocol are copied onto the profile so it validates and
    /// applies immediately; the `endpoint` reference drives auth, headers, and
    /// body overrides. When the endpoint declares no model, the model field is
    /// opened for the user to fill.
    fn add_scripted_provider(&mut self, name: &str) {
        let endpoint = match crate::endpoint::ScriptedEndpoint::resolve(
            name,
            &self.config.paths.endpoints_dir,
        ) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.status = format!("could not load endpoint {name}: {error:#}");
                return;
            }
        };
        let mut profile_id = name.to_owned();
        let mut suffix = 2;
        while self.settings.profiles.contains_key(&profile_id) {
            profile_id = format!("{name}-{suffix}");
            suffix += 1;
        }
        let model = endpoint.model.clone().unwrap_or_default();
        self.settings.profiles.insert(
            profile_id.clone(),
            crate::config::ProviderProfile {
                name: endpoint.display_name().to_owned(),
                base_url: endpoint.url.clone(),
                model: model.clone(),
                protocol: endpoint.protocol,
                api_key_env: None,
                aux_model: None,
                reasoning_effort: None,
                endpoint: Some(name.to_owned()),
                providers: Vec::new(),
                allow_fallbacks: true,
            },
        );
        let previous = self.settings.default_profile.clone();
        self.settings.default_profile = profile_id.clone();

        // With a model from the endpoint the profile is complete — apply it.
        // Without one, persist-but-don't-apply and open the model field, the
        // same contract the preset path uses.
        if model.trim().is_empty() {
            self.pending_provider = Some(PendingProvider {
                profile: profile_id.clone(),
                previous,
            });
            if let Err(error) = self.settings.save(&self.config.paths) {
                self.status = format!("could not add endpoint: {error:#}");
                return;
            }
            self.status = format!("{profile_id} added — set a model");
            if self.config_panel.is_some()
                && let Some(index) = CONFIG_KEYS.iter().position(|key| *key == ConfigKey::Model)
            {
                if let Some(panel) = &mut self.config_panel {
                    panel.selected = index;
                }
                self.begin_config_edit(ConfigKey::Model);
            }
        } else {
            match self.save_and_apply_settings() {
                Ok(()) => {
                    self.status = format!("{profile_id} active — {}", endpoint.display_name())
                }
                Err(error) => {
                    self.settings.default_profile = previous;
                    self.status = format!("could not apply endpoint: {error:#}");
                }
            }
        }
    }

    /// Accept the highlighted picker row, or `index` when a click named one.
    fn accept_picker(&mut self, index: Option<usize>) {
        let Some(picker) = &self.picker else {
            return;
        };
        let index = index.unwrap_or(picker.selected);
        let Some((_, value)) = picker.items.get(index).cloned() else {
            return;
        };
        let action = picker.action;
        self.picker = None;
        match action {
            PickerAction::ResumeSession => self.resume_session(&value),
            PickerAction::SwitchProfile if value == NEW_PROVIDER_SENTINEL => {
                self.open_provider_picker()
            }
            PickerAction::SwitchProfile => {
                self.settings.default_profile = value.clone();
                if let Err(error) = self.save_and_apply_settings() {
                    self.status = format!("could not switch profile: {error:#}");
                } else {
                    self.status = format!("profile {value}");
                }
            }
            PickerAction::AddProvider => self.add_provider(&value),
        }
    }

    /// Create a profile from a preset (or a blank custom one), make it active,
    /// and open the field that most needs filling in next.
    fn add_provider(&mut self, id: &str) {
        if let Some(name) = id.strip_prefix(ENDPOINT_SENTINEL_PREFIX) {
            return self.add_scripted_provider(name);
        }
        let preset = crate::setup::PRESETS.iter().find(|preset| preset.id == id);
        let (name, base_url, protocol, env_key) = match preset {
            Some(preset) => (
                preset.name.to_owned(),
                preset.base_url.to_owned(),
                preset.protocol,
                preset.env_key.map(str::to_owned),
            ),
            None => (
                "Custom".to_owned(),
                "http://localhost:8000/v1".to_owned(),
                ProviderProtocol::ChatCompletions,
                None,
            ),
        };
        // Never silently replace an existing profile of the same name.
        let mut profile_id = if preset.is_some() {
            id.to_owned()
        } else {
            "custom".to_owned()
        };
        let mut suffix = 2;
        while self.settings.profiles.contains_key(&profile_id) {
            profile_id = format!("{}-{suffix}", preset.map(|p| p.id).unwrap_or("custom"));
            suffix += 1;
        }
        self.settings.profiles.insert(
            profile_id.clone(),
            crate::config::ProviderProfile {
                name,
                base_url,
                model: String::new(),
                protocol,
                api_key_env: env_key.clone(),
                aux_model: None,
                reasoning_effort: None,
                endpoint: None,
                providers: Vec::new(),
                allow_fallbacks: true,
            },
        );
        // Deliberately persisted but *not* applied: a profile with no model
        // fails validation, and applying a half-made provider would break the
        // running session. The live config keeps pointing at the old profile
        // until a model is committed, at which point the normal save-and-apply
        // path picks it up.
        self.pending_provider = Some(PendingProvider {
            profile: profile_id.clone(),
            previous: self.settings.default_profile.clone(),
        });
        self.settings.default_profile = profile_id.clone();
        if let Err(error) = self.settings.save(&self.config.paths) {
            self.status = format!("could not add provider: {error:#}");
            return;
        }
        let needs_key = env_key
            .as_deref()
            .is_some_and(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()));
        self.status = if needs_key {
            format!("{profile_id} added — set a model")
        } else if env_key.is_some() {
            format!(
                "{profile_id} added — set a model, then an API key ({})",
                env_key.as_deref().unwrap_or_default()
            )
        } else {
            format!("{profile_id} added — set a model")
        };
        // A new profile has no model, which is the one field it cannot run
        // without; open it rather than leaving the user to find it.
        if self.config_panel.is_some()
            && let Some(index) = CONFIG_KEYS.iter().position(|key| *key == ConfigKey::Model)
        {
            if let Some(panel) = &mut self.config_panel {
                panel.selected = index;
            }
            self.begin_config_edit(ConfigKey::Model);
        }
    }

    fn open_feedback(&mut self) {
        if !self.settings.feedback.enabled {
            self.push_entry(Entry::new(
                EntryKind::Error,
                "Feedback is disabled. Enable it in /config.".to_owned(),
            ));
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
                    self.push_entry(Entry::new(
                        EntryKind::System,
                        format!("Thank you — your feedback was sent.{reference}"),
                    ));
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
                    self.push_entry(Entry::new(
                        EntryKind::Error,
                        format!("Configuration saved, but extensions could not reload: {error}"),
                    ));
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
            let escalated = self.request_interrupt();
            self.status = if escalated {
                "interrupted · Ctrl+C again to exit".to_owned()
            } else {
                "interrupting… · Ctrl+C again to force".to_owned()
            };
        } else if !self.input.text().trim().is_empty() {
            self.input.clear();
            self.status = "cleared · Ctrl+C again to exit".to_owned();
        } else {
            self.status = "Press Ctrl+C again to exit".to_owned();
        }
    }

    /// Ask a running turn to stop. The first request is cooperative: the turn
    /// finishes its current tool, reports everything it did, and the transcript
    /// keeps it. A second request — while the first is still pending — escalates
    /// to a hard abort, which is the old behaviour and does lose the turn.
    ///
    /// Returns true when it escalated.
    fn request_interrupt(&mut self) -> bool {
        if self.running.is_none() {
            return false;
        }
        if self.cancel.swap(true, Ordering::Relaxed) {
            self.interrupt();
            return true;
        }
        if let Some(state) = &mut self.ralph_loop {
            state.cancel();
        }
        self.status = "interrupting…".to_owned();
        false
    }

    fn interrupt(&mut self) {
        if let Some(state) = &mut self.ralph_loop {
            state.cancel();
        }
        if let Some(handle) = self.running.take() {
            handle.abort();
            self.approval = None;
            self.receiving_delta = false;
            // The aborted task will never send its `ToolFinished`, so settle
            // the open tool row here — otherwise it spins forever.
            let elapsed = self
                .tool_started
                .map(|started| started.elapsed().as_millis() as u64);
            if let Some(call) = self
                .open_tool()
                .filter(|call| call.status == ToolStatus::Running)
            {
                call.status = ToolStatus::Failed;
                call.output = "interrupted".to_owned();
                call.duration_ms = elapsed;
            }
            self.push_entry(Entry::new(EntryKind::System, "Interrupted.".to_owned()));
            self.status = "interrupted".to_owned();
            self.last_outcome = Some(TurnOutcome::Interrupted);
            self.follow = true;
        }
        self.turn_started = None;
        self.tool_started = None;
        self.persist_session();
    }

    /// Move the transcript cursor by `delta` blocks, starting from the last
    /// block when nothing is selected yet. Selecting stops follow-mode: the
    /// user is reading history, and yanking them back to the tail on the next
    /// token would be hostile.
    fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        let next = match self.cursor {
            Some(current) => (current as isize + delta).clamp(0, last as isize) as usize,
            None if delta < 0 => last,
            None => 0,
        };
        self.cursor = Some(next);
        self.cursor_pending = true;
        self.follow = false;
    }

    /// Fold or unfold the selected tool row. Returns whether anything moved, so
    /// the caller can fall through to another binding when it did not.
    fn toggle_cursor_fold(&mut self, expand: Option<bool>) -> bool {
        let Some(index) = self.cursor else {
            return false;
        };
        let Some(call) = self
            .entries
            .get_mut(index)
            .and_then(|entry| entry.tool.as_mut())
        else {
            return false;
        };
        if !call.has_more() {
            return false;
        }
        let next = expand.unwrap_or(!call.expanded);
        if next == call.expanded {
            return false;
        }
        call.expanded = next;
        self.entries_rev = self.entries_rev.wrapping_add(1);
        self.cursor_pending = true;
        true
    }

    fn clear_cursor(&mut self) {
        self.cursor = None;
        self.cursor_pending = false;
    }

    /// Lines to move for one scroll event, chosen from how fast the events are
    /// arriving.
    ///
    /// A mouse wheel sends one chunky notch at a time; a trackpad sends a dense
    /// stream of small ones. Moving three lines per event suits the wheel and
    /// makes a trackpad fly past whatever you were reading, so a burst is
    /// treated as a trackpad and moves one line. The first event after a pause
    /// keeps the wheel's larger step, which is what makes a single notch still
    /// feel responsive.
    /// Generation rate for the running turn, or `None` before there is enough
    /// to measure.
    ///
    /// Estimated from characters, since the provider only reports token counts
    /// once the reply is finished and the point of this readout is to move
    /// while it is being produced. The same 4:1 ratio compaction uses.
    fn token_rate(&self) -> Option<f64> {
        let elapsed = self.turn_started?.elapsed().as_secs_f64();
        // Below half a second the divisor is small enough to produce wild
        // numbers that say nothing.
        if elapsed < 0.5 || self.turn_output_chars == 0 {
            return None;
        }
        Some((self.turn_output_chars as f64 / 4.0) / elapsed)
    }

    fn scroll_step(&mut self) -> u16 {
        const TRACKPAD_GAP: Duration = Duration::from_millis(80);
        let now = Instant::now();
        let rapid = self
            .last_scroll
            .is_some_and(|previous| now.duration_since(previous) < TRACKPAD_GAP);
        self.last_scroll = Some(now);
        if rapid { 1 } else { 3 }
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
    /// A background subagent that finished after its turn ended still has a
    /// report to deliver. Start a turn to hand it over, the same way a running
    /// turn would have picked it up between tool calls.
    fn deliver_pending_injections(&mut self) -> bool {
        if self.running.is_some() || self.injections.is_empty() {
            return false;
        }
        let pending = self.injections.drain();
        let mut delivered = false;
        for injection in pending {
            let crate::agent::Injection::SubagentReport(report) = injection else {
                match injection {
                    // A steering message with no turn to steer is just a prompt.
                    crate::agent::Injection::UserMessage(text) => {
                        self.submit_prompt(text);
                        delivered = true;
                    }
                    // A side note whose turn ended before it landed has nothing
                    // to nudge; surface it rather than dropping it silently.
                    crate::agent::Injection::SideNote(note) => {
                        self.push_entry(Entry::new(
                            EntryKind::System,
                            format!("Side note not delivered — the turn ended first: {note}"),
                        ));
                        delivered = true;
                    }
                    crate::agent::Injection::SubagentReport(_) => {}
                }
                continue;
            };
            self.push_entry(Entry::new(
                EntryKind::System,
                "A background subagent finished.".to_owned(),
            ));
            self.submit_prompt(format!(
                "[background subagent finished] {report}\n\nFold this into the work; if it \
                 changes the plan, say so."
            ));
            delivered = true;
            // One turn at a time: anything still queued rides the next idle tick.
            break;
        }
        delivered
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
        dirty |= app.drain_draft_events();
        // A worker that finished after its turn ended delivers here.
        dirty |= app.deliver_pending_injections();
        // A running turn animates (spinner, shimmer, elapsed) even while the
        // stream is silent — e.g. during a long tool call — so redraw on every
        // poll tick rather than only when an event arrives.
        dirty |= app.running.is_some() && app.settings.ui.animations;
        app.start_services_reload();
        if dirty {
            terminal.draw(|frame| draw(frame, app))?;
            dirty = false;
        }

        let wait = if app.running.is_some() { 60 } else { 150 };
        if event::poll(Duration::from_millis(wait))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let before = app.input.text();
                    handle_key(app, key);
                    // Editing the prompt invalidates the highlighted
                    // suggestion, so reconcile the popup after every key.
                    app.sync_completion(&before);
                    dirty = true;
                }
                Event::Mouse(mouse) => {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            let step = app.scroll_step();
                            app.scroll_up(step)
                        }
                        MouseEventKind::ScrollDown => {
                            let step = app.scroll_step();
                            app.scroll_down(step)
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            handle_click(app, mouse.column, mouse.row)
                        }
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
                        let before = app.input.text();
                        app.input.insert_str(&text);
                        app.sync_completion(&before);
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
    // With a selection up, Ctrl+C means copy — the meaning it has everywhere
    // else. Without one it keeps its terminal meaning of interrupt/clear/quit.
    if is_ctrl_c && let Some(selected) = app.input.selected_text() {
        app.copy_to_clipboard(&selected, "selection");
        app.input.clear_selection();
        return;
    }
    // Likewise an armed rewind: only an immediate second Esc may fire it.
    if key.code != KeyCode::Esc {
        app.rewind_armed = None;
    }
    // F3 flips reasoning visibility everywhere — including blocks already in
    // the transcript — so a busy answer can be read without the deliberation.
    if key.code == KeyCode::F(3) {
        app.settings.ui.show_thinking = !app.settings.ui.show_thinking;
        let _ = app.save_and_apply_settings();
        app.status = if app.settings.ui.show_thinking {
            "thinking shown".to_owned()
        } else {
            "thinking hidden".to_owned()
        };
        return;
    }
    // Ctrl+O steps an open approval or question aside so the output behind it
    // can be read (and scrolled); pressing it again brings the dialog back.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('o')
        && (app.approval.is_some() || app.question.is_some())
    {
        app.overlay_hidden = !app.overlay_hidden;
        app.status = if app.overlay_hidden {
            "dialog hidden — ctrl+o to answer".to_owned()
        } else {
            "dialog restored".to_owned()
        };
        return;
    }
    // Ctrl+G jumps back to the live tail from anywhere, in any mode.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g') {
        app.follow = true;
        app.clear_cursor();
        return;
    }
    // F2: hand the mouse back to the terminal so click-drag selects text the
    // way it does anywhere else, and take it back when you want the wheel and
    // clickable rows again. A TUI that holds the mouse cannot be copied out of,
    // which makes it useless as a place to read from.
    if key.code == KeyCode::F(2) {
        app.mouse_captured = !app.mouse_captured;
        let mut out = io::stdout();
        let switched = if app.mouse_captured {
            execute!(out, EnableMouseCapture).is_ok()
        } else {
            execute!(out, DisableMouseCapture).is_ok()
        };
        app.status = if !switched {
            "could not switch mouse mode".to_owned()
        } else if app.mouse_captured {
            "mouse captured — wheel scrolls, rows click · F2 to select text".to_owned()
        } else {
            "selection mode — drag to select · PgUp/PgDn still scroll · F2 restores the mouse"
                .to_owned()
        };
        return;
    }
    // Ctrl+P: the subagent board overlay. Approval and question dialogs keep
    // priority — a swarm detail view must never shadow a pending decision.
    let dialog_open = (app.approval.is_some() || app.question.is_some()) && !app.overlay_hidden;
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('p')
        && !dialog_open
    {
        app.hive_overlay = !app.hive_overlay;
        app.hive_scroll = 0;
        return;
    }
    if app.hive_overlay && !dialog_open {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => app.hive_overlay = false,
            KeyCode::Char('j') | KeyCode::Down => {
                app.hive_scroll = app.hive_scroll.saturating_add(1)
            }
            KeyCode::Char('k') | KeyCode::Up => app.hive_scroll = app.hive_scroll.saturating_sub(1),
            KeyCode::PageDown => app.hive_scroll = app.hive_scroll.saturating_add(10),
            KeyCode::PageUp => app.hive_scroll = app.hive_scroll.saturating_sub(10),
            _ => {}
        }
        return;
    }
    if app.approval.is_some() && !app.overlay_hidden {
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
    if !app.overlay_hidden
        && let Some(question) = &mut app.question
    {
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

    // Routed before the panels: a picker opened from /config sits on top of it
    // and must own the keys while it is up.
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
            app.accept_picker(None);
        } else if cancel {
            app.picker = None;
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
                app.cancel_pending_provider();
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

/// Route a left click to whatever was drawn under it.
///
/// Regions are tested in the order they stack on screen — the completion popup
/// and any open panel float above the transcript — so a click never falls
/// through to a block hidden behind an overlay. A first click selects; a second
/// click on an already-selected row activates it, which keeps a stray click
/// from editing a setting or resuming a session outright.
fn handle_click(app: &mut App, column: u16, row: u16) {
    let hits = app.hits.borrow();
    let completion = hit(&hits.completion, column, row);
    let config = hit(&hits.config, column, row);
    let picker = hit(&hits.picker, column, row);
    let transcript = hit(&hits.transcript, column, row);
    drop(hits);

    if let Some(index) = completion {
        app.completion_index = index;
        app.accept_completion();
        return;
    }
    // Same precedence as drawing: a picker floats above the config panel, so a
    // click landing on both belongs to the picker.
    if let Some(index) = picker {
        let already = app
            .picker
            .as_ref()
            .is_some_and(|picker| picker.selected == index);
        if let Some(picker) = &mut app.picker {
            picker.selected = index;
        }
        if already {
            app.accept_picker(Some(index));
        }
        return;
    }
    if let Some(index) = config {
        let already = app
            .config_panel
            .as_ref()
            .is_some_and(|panel| panel.selected == index);
        if let Some(panel) = &mut app.config_panel {
            panel.selected = index;
        }
        if already {
            let key = CONFIG_KEYS[index];
            if config_key_is_editable(key) {
                app.begin_config_edit(key);
            } else if let Err(error) = app.cycle_config_value(key) {
                app.status = format!("configuration error: {error:#}");
            }
        }
        return;
    }
    if let Some(index) = transcript {
        // Clicking a block selects it; clicking the one already selected folds
        // or unfolds it, matching the two-step the panels use.
        let already = app.cursor == Some(index);
        app.cursor = Some(index);
        app.follow = false;
        if already {
            app.toggle_cursor_fold(None);
        }
    }
}

fn handle_insert_key(app: &mut App, key: KeyEvent) {
    let modified_enter = key
        .modifiers
        .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT);
    // While the completion popup is up it owns the navigation keys: up/down
    // move the highlight, Enter and Tab insert it, and Esc dismisses the list
    // rather than leaving insert mode. Everything else falls through to normal
    // editing, so typing keeps filtering.
    let completing = app.visible_completion().is_some();
    match key.code {
        KeyCode::Esc if completing => app.completion_dismissed = true,
        // A running turn owns Esc: stopping the agent is the more urgent
        // action, and it is what the status bar advertises.
        KeyCode::Esc if app.running.is_some() => {
            app.request_interrupt();
        }
        // Modified arrows scroll the transcript from insert mode; both Alt and
        // Shift are accepted because terminals differ in which they deliver.
        KeyCode::Up
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
        {
            app.scroll_up(3)
        }
        KeyCode::Down
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
        {
            app.scroll_down(3)
        }
        KeyCode::Up if completing => app.move_completion(-1),
        KeyCode::Down if completing => app.move_completion(1),
        // When the popup declines — because the command is already typed out in
        // full — Enter must still send. Consuming it here regardless is what
        // made a finished command unsendable.
        KeyCode::Enter if completing && !modified_enter => {
            if !app.accept_completion() {
                app.submit();
            }
        }
        KeyCode::Esc if app.settings.ui.vim_mode => app.mode = InputMode::Normal,
        // Esc on an idle, empty composer: double-press rewinds to the previous
        // prompt for editing. Two presses because the rewind discards the
        // turn that followed — it must not fire off a stray Esc.
        KeyCode::Esc if app.input.is_empty() => match app.rewind_armed {
            Some(armed) if armed.elapsed() < Duration::from_secs(3) => {
                app.rewind_armed = None;
                app.rewind_to_previous_prompt();
            }
            _ => {
                if app
                    .entries
                    .iter()
                    .any(|entry| entry.kind == EntryKind::User)
                {
                    app.rewind_armed = Some(Instant::now());
                    app.status = "esc again to rewind and edit your last message".to_owned();
                }
            }
        },
        // Outside vim mode Esc is otherwise inert; use it to abandon a draft,
        // which is what every other composer does.
        KeyCode::Esc => app.input.clear(),
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
        // Within a multi-line draft the arrows move through it; only at the
        // top or bottom edge do they reach for prompt history.
        KeyCode::Up => {
            let width = app.composer_width as usize;
            if !app.input.move_up_wrapped(width) {
                app.history_prev();
            }
        }
        KeyCode::Down => {
            let width = app.composer_width as usize;
            if !app.input.move_down_wrapped(width) {
                app.history_next();
            }
        }
        // Ctrl+Home/End jump the transcript; plain Home/End stay line motions.
        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => app.scroll_up(u16::MAX),
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.follow = true;
            app.clear_cursor();
        }
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
        KeyCode::Tab => {
            // With an empty composer there is no completion to accept, so Tab
            // is free to take the drafted follow-up.
            if let Some(draft) = app.draft.take().filter(|_| app.input.is_empty()) {
                app.input.insert_str(&draft);
                app.clear_draft();
            } else {
                app.accept_completion();
            }
        }
        // Editor keys people expect from every other text box. Ctrl+A selects
        // all, Ctrl+Z/Ctrl+Y walk the undo history.
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.select_all();
            app.status = "selected all — ^C copies, typing replaces".to_owned();
        }
        KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !app.input.undo() {
                app.status = "nothing to undo".to_owned();
            }
        }
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !app.input.redo() {
                app.status = "nothing to redo".to_owned();
            }
        }
        // Ctrl+V: paste from the system clipboard. Images first — a terminal's
        // own paste can only ever deliver text, so this key is the sole route
        // by which a screenshot on the clipboard can reach the prompt. With no
        // image present it falls through to text, covering terminals that send
        // the raw Ctrl+V byte instead of a bracketed paste.
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.paste_from_clipboard();
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
            KeyCode::Char('y') => app.scroll_up(1),
            KeyCode::Char('e') => app.scroll_down(1),
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
            app.clear_cursor();
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
        (_, KeyCode::Char('w')) => app.input.move_word_forward(),
        (_, KeyCode::Char('b')) => app.input.move_word_backward(),
        (_, KeyCode::Char('0')) | (_, KeyCode::Home) => app.input.move_start(),
        (_, KeyCode::Char('$')) | (_, KeyCode::End) => app.input.move_end(),
        (_, KeyCode::Char('x')) | (_, KeyCode::Delete) => app.input.delete(),
        // j/k walk the transcript block by block. Line-at-a-time scrolling
        // moved to Ctrl+E / Ctrl+Y: a cursor you can act on is worth more than
        // one-row nudges, which the wheel and PgUp/PgDn already cover.
        (_, KeyCode::Char('j')) | (_, KeyCode::Down) => app.move_cursor(1),
        (_, KeyCode::Char('k')) | (_, KeyCode::Up) => app.move_cursor(-1),
        // Copy without needing the terminal at all: `y` takes the selected
        // block, `Y` the last assistant reply.
        (_, KeyCode::Char('y')) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.yank_selected_block()
        }
        (_, KeyCode::Char('Y')) => app.yank_last_reply(),
        (_, KeyCode::Char('o')) | (_, KeyCode::Char(' ')) => {
            app.toggle_cursor_fold(None);
        }
        (_, KeyCode::Char('l')) | (_, KeyCode::Right) => {
            if !app.toggle_cursor_fold(Some(true)) {
                app.input.move_right();
            }
        }
        (_, KeyCode::Char('h')) | (_, KeyCode::Left) => {
            if !app.toggle_cursor_fold(Some(false)) {
                app.input.move_left();
            }
        }
        (_, KeyCode::PageUp) => app.scroll_up(app.transcript_height.saturating_sub(2)),
        (_, KeyCode::PageDown) => app.scroll_down(app.transcript_height.saturating_sub(2)),
        (_, KeyCode::Char('G')) => {
            app.follow = true;
            app.clear_cursor();
        }
        // With a cursor active Esc clears it; otherwise the same esc-esc
        // rewind as insert mode, so vim users are not locked out of it.
        (_, KeyCode::Esc) => {
            if app.cursor.is_some() {
                app.clear_cursor();
            } else if app.running.is_none() && app.input.is_empty() {
                match app.rewind_armed {
                    Some(armed) if armed.elapsed() < Duration::from_secs(3) => {
                        app.rewind_armed = None;
                        app.rewind_to_previous_prompt();
                        app.mode = InputMode::Insert;
                    }
                    _ => {
                        if app
                            .entries
                            .iter()
                            .any(|entry| entry.kind == EntryKind::User)
                        {
                            app.rewind_armed = Some(Instant::now());
                            app.status =
                                "esc again to rewind and edit your last message".to_owned();
                        }
                    }
                }
            }
        }
        (_, KeyCode::Enter) => {
            if !app.toggle_cursor_fold(None) {
                app.mode = InputMode::Insert;
            }
        }
        (_, KeyCode::Char('?')) => app.show_help = true,
        (_, KeyCode::Char('q')) if app.running.is_none() => app.quit = true,
        _ => {}
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    app.hits.borrow_mut().clear();
    // Grow with the text as it wraps, not just with explicit newlines: the
    // composer text column is the frame minus borders, padding, and the prompt
    // gutter. Capped so a long draft never crowds out the transcript.
    let composer_text_width = area.width.saturating_sub(6).max(1) as usize;
    app.composer_width = composer_text_width as u16;
    let input_height = (app.input.wrapped_line_count(composer_text_width) as u16 + 2).clamp(3, 12);
    let task_height = u16::from(app.goal.snapshot().is_some() || app.ralph_loop.is_some()) * 2
        + u16::from(!app.tasks.is_empty())
        + app.hive.board.strip_rows();
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
    let transcript = ui::measure(chunks[1], 112);
    draw_transcript(frame, transcript, app);
    if task_height > 0 {
        draw_task_bar(frame, ui::measure(chunks[2], 112), app);
    }
    let input = ui::measure(chunks[3], 112);
    draw_input(frame, input, app);
    draw_footer(frame, ui::measure(chunks[4], 112), app);
    draw_completion_popup(frame, input, app);
    // The picker is checked first because it can be opened *from* the config
    // panel; behind it, the panel would be drawn over its own child and the
    // selection would be invisible.
    if app.picker.is_some() {
        draw_picker(frame, area, app);
    } else if app.raw_config.is_some() {
        draw_raw_config(frame, area, app);
    } else if app.feedback_form.is_some() {
        draw_feedback(frame, area, app);
    } else if app.config_panel.is_some() {
        draw_config(frame, area, app);
    } else if app.usage_panel.is_some() {
        draw_usage(frame, area, app);
    } else if app.show_help {
        draw_help(frame, area);
    } else if app.approval.is_some() && !app.overlay_hidden {
        draw_approval(frame, area, app);
    } else if app.question.is_some() && !app.overlay_hidden {
        draw_user_question(frame, area, app);
    } else if app.hive_overlay {
        draw_hive(frame, area, app);
    }
}

/// The current git branch, read from `.git` rather than by shelling out — the
/// header refreshes after every turn and a subprocess each time would be
/// noticeable on a large repository. Handles linked worktrees, where `.git` is
/// a file pointing at the real git directory.
fn git_branch(workspace: &std::path::Path) -> Option<String> {
    let dot_git = workspace.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let path = pointer.trim().strip_prefix("gitdir: ")?.to_owned();
        let path = std::path::PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        }
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_owned()),
        // Detached HEAD: the short object id is the useful thing to show.
        None => head.get(..7).map(str::to_owned),
    }
}

/// The colour that stands for an agent mode, used consistently by the header
/// badge, the welcome screen, and the mode-change notices.
fn mode_color(mode: AgentMode) -> Color {
    match mode {
        AgentMode::Auto => primary(),
        AgentMode::Plan => warning(),
        AgentMode::Build => success(),
    }
}

/// Two rows: identity and target on the left, model and mode on the right, over
/// a hairline that separates the chrome from the conversation.
fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mode = app.resolved_agent_mode.unwrap_or(app.agent_mode);

    let mut left = vec![
        ui::badge("ABACUS", secondary()),
        Span::styled(
            format!("  {}", app.config.workspace_name()),
            Style::default().fg(text()).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(branch) = &app.git_branch {
        left.push(Span::styled(
            format!("  {} {}", ui::glyphs().branch, ui::truncate(branch, 24)),
            Style::default().fg(muted()),
        ));
    }
    if app.config.profile != "default" {
        left.push(ui::dot());
        left.push(Span::styled(
            ui::truncate(&app.config.profile, 18),
            Style::default().fg(muted()),
        ));
    }

    let right = vec![
        Span::styled(
            ui::truncate(&app.config.model, 34),
            Style::default().fg(muted()),
        ),
        Span::raw("  "),
        ui::badge(mode.label(), mode_color(mode)),
    ];

    // Give the left cluster its own clipped rect rather than letting the
    // right-aligned one paint over it. Overlapping them truncates the branch
    // mid-word on a narrow terminal; clipping ends it cleanly instead.
    let row = Rect { height: 1, ..area };
    let reserved = ui::spans_width(&right) as u16 + 2;
    frame.render_widget(
        Paragraph::new(Line::from(left)),
        Rect {
            width: row.width.saturating_sub(reserved),
            ..row
        },
    );
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        row,
    );
    frame.render_widget(
        Paragraph::new(ui::rule(area.width)),
        Rect {
            y: area.y.saturating_add(1),
            height: 1,
            ..area
        },
    );
}

impl App {
    /// Ensure the wrapped transcript matches `width` and the current content,
    /// re-wrapping only when the fingerprint moves.
    fn wrapped_transcript(&mut self, width: u16, spinner: &str, phase: usize) -> &ui::Transcript {
        let key: TranscriptKey = (
            self.entries_rev,
            width,
            phase,
            self.cursor,
            self.settings.ui.show_thinking,
        );
        let stale = self
            .transcript_cache
            .as_ref()
            .is_none_or(|(cached, _)| *cached != key);
        if stale {
            let rendered = ui::transcript(
                &self.entries,
                width as usize,
                spinner,
                self.cursor,
                self.settings.ui.show_thinking,
            );
            self.transcript_cache = Some((key, rendered));
        }
        &self.transcript_cache.as_ref().expect("just populated").1
    }

    /// Bring the selected block fully into view, preferring to show its start.
    /// Only meaningful once the frame has been wrapped, which is why it runs
    /// from the draw path rather than from the key handler.
    fn reveal_cursor(&mut self, height: u16, max_scroll: u16) {
        if !std::mem::take(&mut self.cursor_pending) {
            return;
        }
        let Some(index) = self.cursor else {
            return;
        };
        let Some((start, len)) = self
            .transcript_cache
            .as_ref()
            .and_then(|(_, rendered)| rendered.spans.get(index).copied())
        else {
            return;
        };
        let height = height as usize;
        let start = start as u16;
        let end = (start as usize + len).saturating_sub(1) as u16;
        if start < self.scroll {
            self.scroll = start;
        } else if end >= self.scroll.saturating_add(height as u16) {
            // Anchor to the block's start when it is taller than the viewport,
            // so an expanded row opens at its header instead of its tail.
            self.scroll = if len >= height {
                start
            } else {
                end.saturating_sub(height as u16 - 1)
            };
        }
        self.scroll = self.scroll.min(max_scroll);
    }
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    // Keep one row clear above the composer so the last line of output never
    // butts up against the input frame. That freed row is where the "you have
    // scrolled away" marker sits, so the marker never covers content.
    let full = area;
    let area = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    app.transcript_height = area.height;
    if app.entries.is_empty() {
        draw_welcome(frame, area, app);
        return;
    }
    // The scrollbar gutter is reserved whether or not a scrollbar is showing.
    // Claiming it only once the content overflows would re-wrap the whole
    // transcript the moment it passed one screen, which reads as a glitch.
    let body = Rect {
        width: area.width.saturating_sub(SCROLLBAR_COLUMNS),
        ..area
    };
    // A running tool animates, so the spinner phase joins the cache key; when
    // nothing is running the phase is pinned and the wrap is reused verbatim.
    let running = app
        .entries
        .last()
        .and_then(|entry| entry.tool.as_ref())
        .is_some_and(|call| call.status == ToolStatus::Running);
    let animated = app.settings.ui.animations;
    let phase = if running && animated {
        (app.started.elapsed().as_millis() / 90) as usize % ui::SPINNER_FRAMES
    } else {
        usize::MAX
    };
    let spinner = if running {
        ui::spinner_frame(app.started.elapsed(), animated)
    } else {
        ui::glyphs().still
    };

    let height = body.height as usize;
    let total = app
        .wrapped_transcript(body.width.max(1), spinner, phase)
        .lines
        .len();
    let max_scroll = total.saturating_sub(height).min(u16::MAX as usize) as u16;
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        // Re-entering follow because the view happens to sit at the bottom is
        // right for a reader, but not while a block is selected — that would
        // drag the cursor along with new output.
        if app.scroll >= max_scroll && app.cursor.is_none() {
            app.follow = true;
        }
    }
    app.reveal_cursor(body.height, max_scroll);

    // Slice out just the rows on screen. The wrap above is exact, so this is a
    // direct index rather than a scroll offset ratatui has to re-derive.
    let start = app.scroll as usize;
    let visible = app
        .transcript_cache
        .as_ref()
        .map(|(_, rendered)| {
            rendered
                .lines
                .iter()
                .skip(start)
                .take(height)
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    frame.render_widget(Paragraph::new(Text::from(visible)), body);

    // Map each on-screen block back to the entry it came from, so a click can
    // select and unfold it.
    if let Some((_, rendered)) = app.transcript_cache.as_ref() {
        let mut hits = app.hits.borrow_mut();
        for (index, (offset, len)) in rendered.spans.iter().copied().enumerate() {
            let top = offset.max(start);
            let bottom = (offset + len).min(start + height);
            if top >= bottom {
                continue;
            }
            hits.transcript.push((
                Rect {
                    x: body.x,
                    y: body.y + (top - start) as u16,
                    width: body.width,
                    height: (bottom - top) as u16,
                },
                index,
            ));
        }
    }

    if total > height {
        draw_scrollbar(frame, area, total, start, height);
        // When the user has scrolled away from the tail, say so and name the
        // key that gets them back — otherwise live output appears to have
        // stopped arriving.
        if !app.follow {
            draw_follow_pill(frame, full);
        }
    }
}

/// Columns held back for the scrollbar: one blank gap, one track.
const SCROLLBAR_COLUMNS: u16 = 2;

/// A hairline scrollbar on the right edge of the transcript. Drawn only when
/// the content actually overflows, so a short session has no chrome at all.
fn draw_scrollbar(frame: &mut Frame<'_>, area: Rect, total: usize, position: usize, height: usize) {
    if area.width < 2 || height == 0 {
        return;
    }
    let track = area.height as usize;
    let thumb = ((height * track) / total.max(1)).clamp(1, track);
    let span = total.saturating_sub(height).max(1);
    let offset = ((position * (track - thumb)) / span).min(track - thumb);
    let x = area.right().saturating_sub(1);
    for row in 0..track {
        let inside = row >= offset && row < offset + thumb;
        let set = ui::glyphs();
        let (glyph, color) = if inside {
            (set.thumb, primary())
        } else {
            (set.track, rail())
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(glyph, Style::default().fg(color)))),
            Rect {
                x,
                y: area.y + row as u16,
                width: 1,
                height: 1,
            },
        );
    }
}

/// Floating "you are not at the bottom" affordance.
fn draw_follow_pill(frame: &mut Frame<'_>, area: Rect) {
    let label = format!(" {} latest · G ", ui::glyphs().down);
    let width = UnicodeWidthStr::width(label.as_str()) as u16;
    if area.width < width + SCROLLBAR_COLUMNS {
        return;
    }
    let pill = Rect {
        x: area.right().saturating_sub(width + SCROLLBAR_COLUMNS),
        y: area.bottom().saturating_sub(1),
        width,
        height: 1,
    };
    frame.render_widget(Clear, pill);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(label, ui::fill_style(primary())))),
        pill,
    );
}

/// The empty-transcript splash. Vertically centred, left-aligned inside a
/// measure narrow enough to read — a centred paragraph of tips looks like a
/// marketing page, a left-aligned facts block looks like a tool.
fn draw_welcome(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let compact = area.width < 64 || area.height < 18;
    let mode = app.resolved_agent_mode.unwrap_or(app.agent_mode);
    let info = ui::Welcome {
        version: env!("CARGO_PKG_VERSION"),
        workspace: &app.config.workspace.to_string_lossy(),
        model: &app.config.model,
        mode: mode.label(),
        branch: app.git_branch.as_deref(),
        tips: app.settings.ui.show_tooltips && !compact,
    };
    let lines = ui::welcome(&info, area.width.min(68) as usize);
    let height = lines.len() as u16;
    let width = area.width.min(68);
    let panel = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height: height.min(area.height),
    };
    frame.render_widget(Paragraph::new(Text::from(lines)), panel);
}

/// The persistent-work strip between transcript and composer: goal, loop, and
/// task-list state. Filled with the surface colour so it reads as a pinned
/// band rather than as more transcript.
fn draw_task_bar(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();
    if let Some(goal) = app.goal.snapshot() {
        let set = ui::glyphs();
        let (glyph, color) = match goal.status {
            crate::goal::GoalStatus::Active => (set.goal, primary()),
            crate::goal::GoalStatus::Paused => (set.paused, warning()),
            crate::goal::GoalStatus::Complete => (set.ok, success()),
            crate::goal::GoalStatus::Cancelled => (set.failed, muted()),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {glyph} GOAL  "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                ui::truncate(&goal.objective, 72),
                Style::default().fg(text()),
            ),
            Span::styled("   /goal pause · edit · clear", Style::default().fg(rail())),
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
                format!(" {} LOOP  ", ui::glyphs().repeat),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} / {limit}", state.iteration),
                Style::default().fg(text()),
            ),
            ui::dot(),
            Span::styled(
                format!("promise: {}", ui::truncate(&state.completion_promise, 32)),
                Style::default().fg(muted()),
            ),
            Span::styled("   /cancel-loop", Style::default().fg(rail())),
        ]));
    }
    let tasks = app.tasks.snapshot();
    if !tasks.is_empty() {
        let done = tasks.iter().filter(|task| task.done).count();
        let percent = ((done * 100) / tasks.len().max(1)) as u16;
        let mut spans = vec![
            Span::styled(
                format!(" {} TASKS  ", ui::glyphs().tasks),
                Style::default()
                    .fg(secondary())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{done}/{} ", tasks.len()),
                Style::default().fg(text()),
            ),
        ];
        spans.extend(ui::meter(percent, 10, success()));
        if let Some(next) = tasks.iter().find(|task| !task.done) {
            spans.push(Span::styled(
                format!("   next: {}", ui::truncate(&next.text, 52)),
                Style::default().fg(muted()),
            ));
        }
        lines.push(Line::from(spans));
    }
    // The subagent board: each worker pinned with its live activity while a
    // small swarm runs; a large swarm clusters into one summary line and the
    // detail lives behind Ctrl+P.
    let workers = app.hive.board.snapshot();
    if !workers.is_empty() {
        let set = ui::glyphs();
        if workers.len() <= crate::hive::CLUSTER_THRESHOLD {
            for worker in &workers {
                let (glyph, color) = match worker.state {
                    crate::hive::WorkerState::Running => (
                        ui::spinner_frame(worker.started.elapsed(), app.settings.ui.animations),
                        primary(),
                    ),
                    crate::hive::WorkerState::Done => (set.ok, success()),
                    crate::hive::WorkerState::Failed => (set.failed, danger()),
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {glyph} "),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{} {}  ", worker.role, worker.name),
                        Style::default().fg(text()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        ui::truncate(
                            &worker.activity,
                            (area.width as usize)
                                .saturating_sub(worker.role.len() + worker.name.len() + 20),
                        ),
                        Style::default().fg(muted()),
                    ),
                    Span::styled(
                        format!("  {}", ui::format_count(worker.tokens_used())),
                        Style::default().fg(rail()),
                    ),
                ]));
            }
        } else {
            let running = workers
                .iter()
                .filter(|worker| worker.state == crate::hive::WorkerState::Running)
                .count();
            let failed = workers
                .iter()
                .filter(|worker| worker.state == crate::hive::WorkerState::Failed)
                .count();
            let done = workers.len() - running - failed;
            let swarm_tokens: u64 = workers.iter().map(|worker| worker.tokens_used()).sum();
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} HIVE  ", set.tasks),
                    Style::default().fg(primary()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "{} subagent(s) · {running} running · {done} done · {failed} failed · {}",
                        workers.len(),
                        ui::format_count(swarm_tokens)
                    ),
                    Style::default().fg(text()),
                ),
                Span::styled("   ^P details", Style::default().fg(rail())),
            ]));
        }
    }
    while lines.len() < area.height as usize {
        lines.push(Line::from(""));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default().bg(surface())),
        area,
    );
}

/// The Ctrl+P overlay: every worker in the current swarm with role, state,
/// elapsed time, and latest activity, plus the workspace's delegation record.
fn draw_hive(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let popup = ui::centered(
        area.width.saturating_sub(6).min(100),
        area.height.saturating_sub(4).max(8),
        area,
    );
    let hints: &[(&str, &str)] = &[("j/k", "scroll"), ("esc", "close")];
    let inner = open_overlay(frame, popup, "SUBAGENTS", primary(), hints);

    let workers = app.hive.board.snapshot();
    let set = ui::glyphs();
    let mut lines: Vec<Line<'static>> = Vec::new();
    if workers.is_empty() {
        lines.push(Line::from(Span::styled(
            "No subagents in this turn yet. The board fills when the model calls spawn_subagents.",
            Style::default().fg(muted()),
        )));
    }
    for worker in &workers {
        let (glyph, color, state) = match worker.state {
            crate::hive::WorkerState::Running => (
                ui::spinner_frame(worker.started.elapsed(), app.settings.ui.animations),
                primary(),
                "running",
            ),
            crate::hive::WorkerState::Done => (set.ok, success(), "done"),
            crate::hive::WorkerState::Failed => (set.failed, danger(), "failed"),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{glyph} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} ", worker.role),
                Style::default()
                    .fg(secondary())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                worker.name.clone(),
                Style::default().fg(text()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  {state} · {} · {} tok",
                    ui::format_elapsed(worker.started.elapsed().as_millis() as u64),
                    ui::format_count(worker.tokens_used())
                ),
                Style::default().fg(muted()),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!("    {}", ui::truncate(&worker.activity, 90)),
            Style::default().fg(muted()),
        )));
        lines.push(Line::from(""));
    }
    let stats = app.hive.stats();
    lines.push(Line::from(Span::styled(
        format!(
            "delegation record: {} swarm(s), {} clean · {} worker(s), {} failed · tier {}",
            stats.runs,
            stats.clean_runs,
            stats.workers,
            stats.worker_failures,
            stats.tier().label()
        ),
        Style::default().fg(rail()),
    )));
    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((app.hive_scroll, 0)),
        inner,
    );
}

/// Slash-command and `@file` suggestions, floated above the composer with the
/// highlighted row filled. Selection is real here: the popup is navigable, and
/// what is highlighted is what Tab or Enter will insert.
fn draw_completion_popup(frame: &mut Frame<'_>, input_area: Rect, app: &App) {
    if app.config_panel.is_some()
        || app.raw_config.is_some()
        || app.feedback_form.is_some()
        || app.usage_panel.is_some()
    {
        return;
    }
    let Some((suggestions, title)) = app.visible_completion() else {
        return;
    };

    // Clamp to the rows available above the composer, keeping the selected row
    // in view by scrolling the window rather than the list.
    let room = (input_area.y as usize).saturating_sub(3).clamp(1, 12);
    let visible = suggestions.len().min(room);
    let first = app
        .completion_index
        .saturating_sub(visible.saturating_sub(1))
        .min(suggestions.len().saturating_sub(visible));

    let width = input_area.width.min(76);
    let inner = width.saturating_sub(4) as usize;
    let mut lines = Vec::with_capacity(visible);
    for (index, (value, description)) in suggestions.iter().enumerate().skip(first).take(visible) {
        let selected = index == app.completion_index;
        let fill = if selected {
            crate::theme::active().selection
        } else {
            crate::theme::active().overlay
        };
        let label_width = 22.min(inner.saturating_sub(4));
        let mut spans = vec![
            Span::styled(
                if selected { ui::glyphs().bar } else { " " },
                Style::default().fg(primary()).bg(fill),
            ),
            Span::styled(
                format!(" {:<label_width$}", ui::truncate(value, label_width)),
                Style::default()
                    .fg(if selected { text() } else { primary() })
                    .bg(fill)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if !description.is_empty() {
            let room = inner.saturating_sub(label_width + 2);
            spans.push(Span::styled(
                ui::truncate(description, room),
                Style::default().fg(muted()).bg(fill),
            ));
        }
        // Pad the row to the full width so the selection highlight is a solid
        // band instead of stopping at the end of the text.
        let used = ui::spans_width(&spans);
        if used < inner + 2 {
            spans.push(Span::styled(
                " ".repeat(inner + 2 - used),
                Style::default().bg(fill),
            ));
        }
        app.hits.borrow_mut().completion.push((
            Rect {
                x: input_area.x + 1,
                y: 0, // resolved below, once the popup's origin is known
                width: width.saturating_sub(2),
                height: 1,
            },
            index,
        ));
        lines.push(Line::from(spans));
    }

    let hidden = suggestions.len() - visible;
    let footer = if hidden > 0 {
        ui::overlay_hints(&[("↑↓", "select"), ("⇥", "insert"), ("esc", "dismiss")])
            .spans
            .into_iter()
            .chain(std::iter::once(Span::styled(
                format!("   +{hidden} more"),
                Style::default().fg(rail()),
            )))
            .collect::<Vec<_>>()
            .into()
    } else {
        ui::overlay_hints(&[("↑↓", "select"), ("⇥", "insert"), ("esc", "dismiss")])
    };

    let height = lines.len() as u16 + 2;
    let area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width,
        height,
    };
    // The rows were recorded before the popup's origin was known; place them
    // now that it is.
    for (offset, (rect, _)) in app.hits.borrow_mut().completion.iter_mut().enumerate() {
        rect.y = area.y + 1 + offset as u16;
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(ui::overlay_block(title, primary(), Some(footer))),
        area,
    );
}

/// The composer. The frame carries the state: accent border and mode badge when
/// it is your turn, dimmed with a queue-oriented placeholder while the agent is
/// working, so the box itself tells you what pressing Enter will do.
fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let running = app.running.is_some();
    let accent = match app.mode {
        InputMode::Insert => primary(),
        InputMode::Normal => secondary(),
    };
    let frame_color = if running { rail() } else { accent };

    let mut top_right = vec![Span::styled(
        if running { " ⏎ steer" } else { " ⏎ send" },
        Style::default().fg(rail()),
    )];
    // Count `@file` mentions so the composer can say what will be attached
    // before the prompt is sent.
    let mentions = app
        .input
        .text()
        .split_whitespace()
        .filter(|token| token.len() > 1 && token.starts_with('@'))
        .count();
    if mentions > 0 {
        top_right.insert(
            0,
            Span::styled(
                format!(" {} {mentions} attached ", ui::glyphs().attached),
                Style::default().fg(primary()),
            ),
        );
    }
    top_right.push(Span::raw(" "));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(frame_color))
        .padding(Padding::horizontal(1))
        .title_top(Line::from(vec![
            Span::raw(" "),
            ui::badge(app.mode.label(), frame_color),
            Span::raw(" "),
        ]))
        .title_top(Line::from(top_right).right_aligned());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 3 || inner.height == 0 {
        return;
    }

    // The prompt arrow sits in its own column so wrapped and continuation rows
    // hang under the text rather than under the marker.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            ui::glyphs().prompt,
            Style::default()
                .fg(frame_color)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect {
            width: 1,
            height: 1,
            ..inner
        },
    );
    let text_area = Rect {
        x: inner.x + 2,
        width: inner.width - 2,
        ..inner
    };

    let composed = app.input.text();
    let inner_width = text_area.width.max(1) as usize;
    let visible_rows = text_area.height.max(1) as usize;
    // Text wraps rather than scrolling sideways, so the cursor's position is
    // measured in wrapped rows.
    let rows = app.input.wrapped_rows(inner_width);
    let (cursor_row, cursor_col) = app.input.wrapped_cursor(inner_width);
    let input_scroll = cursor_row.saturating_sub(visible_rows.saturating_sub(1));
    let characters: Vec<char> = composed.chars().collect();
    let selection = app.input.selection();
    let display_col = {
        let row = rows.get(cursor_row).copied().unwrap_or((0, 0));
        let prefix: String = characters[row.0..(row.0 + cursor_col).min(characters.len())]
            .iter()
            .collect();
        UnicodeWidthStr::width(prefix.as_str())
    };

    let paragraph = if composed.is_empty() {
        // A predicted follow-up stands in for the hint when there is one, with
        // the key that accepts it spelled out — otherwise it reads as text the
        // composer already contains.
        match (&app.draft, running) {
            (Some(draft), false) => Paragraph::new(Line::from(vec![
                Span::styled(
                    ui::truncate(draft, inner.width.saturating_sub(14) as usize),
                    Style::default().fg(muted()).add_modifier(Modifier::ITALIC),
                ),
                Span::styled("  ⇥ use", Style::default().fg(rail())),
            ])),
            (_, true) => Paragraph::new(Span::styled(
                "Type to steer — delivered after the current step…",
                Style::default().fg(rail()),
            )),
            (None, false) => Paragraph::new(Span::styled(
                "Ask Abacus to inspect, explain, or change the code…",
                Style::default().fg(rail()),
            )),
        }
    } else {
        // One rendered line per wrapped row, with any selection tinted so
        // select-all is visible rather than invisible state.
        let lines: Vec<Line<'static>> = rows
            .iter()
            .map(|&(start, end)| {
                let slice: String = characters[start..end.min(characters.len())]
                    .iter()
                    .collect();
                match selection {
                    Some((from, to)) if from < end && to > start => Line::from(Span::styled(
                        slice,
                        Style::default()
                            .fg(text())
                            .bg(crate::theme::active().selection),
                    )),
                    _ => Line::from(Span::styled(slice, Style::default().fg(text()))),
                }
            })
            .collect();
        Paragraph::new(Text::from(lines))
    }
    .scroll((input_scroll as u16, 0));
    frame.render_widget(paragraph, text_area);

    if app.approval.is_none()
        && !app.show_help
        && app.config_panel.is_none()
        && app.raw_config.is_none()
        && app.feedback_form.is_none()
        && app.usage_panel.is_none()
    {
        let x = (text_area.x + display_col as u16).min(text_area.right().saturating_sub(1));
        let visible_row = cursor_row.saturating_sub(input_scroll) as u16;
        let y = (text_area.y + visible_row).min(text_area.bottom().saturating_sub(1));
        frame.set_cursor_position((x, y));
    }
}

/// The status bar: what the agent is doing on the left, the keys that matter
/// right now in the middle, and the session's budget on the right.
///
/// The right-hand readout is laid out first and the hints are dropped from the
/// end until the rest fits, so a narrow terminal degrades by shedding the least
/// important information instead of truncating mid-word.
fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if let Some(approval) = &app.approval
        && !app.overlay_hidden
    {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                ui::badge("APPROVAL", warning()),
                Span::styled(
                    format!(" {}  ", approval.tool),
                    Style::default().fg(warning()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    ui::truncate(&approval.summary, area.width.saturating_sub(46) as usize),
                    Style::default().fg(muted()),
                ),
                Span::styled(
                    "  y",
                    Style::default().fg(success()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" once ", Style::default().fg(muted())),
                Span::styled(
                    "a",
                    Style::default().fg(primary()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" session ", Style::default().fg(muted())),
                Span::styled(
                    "n",
                    Style::default().fg(danger()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" reject", Style::default().fg(muted())),
            ])),
            area,
        );
        return;
    }

    // How full the window is. The provider's own prompt count is exact, so it
    // wins; the character estimate is the fallback for endpoints that report no
    // usage, and it also carries the live movement *within* a turn, before the
    // next reply reports a new figure.
    let estimated = (app.ctx_chars / 4).max(1) as u64;
    let reported = app.provider.context_tokens();
    let ctx_tokens = if reported > 0 {
        reported.max(estimated)
    } else {
        estimated
    };
    let ctx_window = app.config.model_limits.context_window.max(1) as u64;
    let percent = ((ctx_tokens * 100) / ctx_window).min(100) as u16;
    let compact_at = (app.config.model_limits.compaction_budget().compact_at_chars / 4).max(1);
    let ctx_color = if ctx_tokens >= compact_at as u64 || percent >= 75 {
        warning()
    } else {
        muted()
    };

    // Two different quantities, so they are labelled as such. Side by side and
    // both called "tokens", the running session total reads as the size of the
    // context, and the two never agree.
    let mut right = vec![
        Span::styled(
            format!("{} used", ui::format_count(app.provider.tokens_used())),
            Style::default().fg(muted()),
        ),
        ui::dot(),
        Span::styled(
            format!(
                "ctx {}/{} ",
                ui::format_count(ctx_tokens),
                ui::format_count(ctx_window)
            ),
            Style::default().fg(ctx_color),
        ),
    ];
    right.extend(ui::meter(percent, 8, ctx_color));
    let right_width = ui::spans_width(&right) as u16;

    let running = app.running.is_some();
    let mut left = if running {
        let elapsed = app
            .turn_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
        let mut spans = vec![Span::styled(
            format!(
                " {} ",
                ui::spinner_frame(elapsed, app.settings.ui.animations)
            ),
            Style::default().fg(primary()).add_modifier(Modifier::BOLD),
        )];
        // Wide enough for a reasoning-derived header, which carries real
        // information; hints yield first when the row runs out of room.
        spans.extend(ui::shimmer(
            &ui::truncate(&app.status, 44),
            elapsed,
            app.settings.ui.animations,
        ));
        spans.push(Span::styled(
            format!("  {}", ui::format_elapsed(elapsed.as_millis() as u64)),
            Style::default().fg(muted()),
        ));
        if app.settings.ui.show_token_rate
            && let Some(rate) = app.token_rate()
        {
            spans.push(Span::styled(
                format!("  {rate:.0} tok/s"),
                Style::default().fg(rail()),
            ));
        }
        spans
    } else {
        let set = ui::glyphs();
        let (glyph, color) = match app.last_outcome {
            Some(TurnOutcome::Failed) => (set.failed, danger()),
            Some(TurnOutcome::Interrupted) => (set.paused, warning()),
            None => (set.still, success()),
        };
        vec![
            Span::styled(
                format!(" {glyph} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(ui::truncate(&app.status, 28), Style::default().fg(muted())),
        ]
    };

    // Contextual hints: only the keys that do something in the current state.
    let pairs: &[(&str, &str)] = if running {
        // Typing during a turn queues rather than being lost — worth saying,
        // since most tools silently drop input here.
        &[
            ("⏎", "steer"),
            ("esc", "to interrupt"),
            ("^C", "twice to quit"),
        ]
    } else if app.mode == InputMode::Normal {
        &[
            ("j/k", "blocks"),
            ("o", "unfold"),
            ("i", "insert"),
            ("?", "help"),
        ]
    } else if app.input.is_empty() {
        &[
            ("/", "commands"),
            ("@", "files"),
            ("PgUp/PgDn", "scroll"),
            ("⇧⇥", "mode"),
            ("F2", "select text"),
            ("F1", "help"),
        ]
    } else {
        &[("⏎", "send"), ("^J", "newline"), ("^C", "clear")]
    };
    let mut hints = pairs.to_vec();
    let budget = area.width.saturating_sub(right_width + 2) as usize;
    while !hints.is_empty() {
        let candidate = ui::spans_width(&left) + 3 + ui::spans_width(&ui::hints(&hints));
        if candidate <= budget {
            break;
        }
        hints.pop();
    }
    if !hints.is_empty() {
        left.push(Span::styled("   ", Style::default()));
        left.extend(ui::hints(&hints));
    }

    frame.render_widget(
        Paragraph::new(Line::from(left)),
        Rect {
            width: area.width.saturating_sub(right_width + 1),
            ..area
        },
    );
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        area,
    );
}

/// One row of a selectable list. The cursor row is filled edge to edge and
/// marked with a rail, so the highlight reads as a band rather than as text
/// that happens to be a different colour.
fn list_row(selected: bool, width: usize, content: Vec<Span<'static>>) -> Line<'static> {
    let palette = crate::theme::active();
    // Without colour a background fill says nothing, so a selected row leans on
    // its rail and bold text instead of a band.
    let fill = if selected {
        palette.selection
    } else {
        palette.overlay
    };
    let mark = if selected {
        Style::default()
            .fg(primary())
            .bg(fill)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(fill)
    };
    let mut spans = vec![Span::styled(
        if selected {
            format!("{} ", ui::glyphs().bar)
        } else {
            "  ".to_owned()
        },
        mark,
    )];
    for span in content {
        let mut style = span.style.bg(fill);
        if selected && palette.plain {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(span.content, style));
    }
    let used = ui::spans_width(&spans);
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(fill),
        ));
    }
    Line::from(spans)
}

/// Frame an overlay and hand back its inner area, already cleared.
fn open_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    accent: Color,
    hints: &[(&str, &str)],
) -> Rect {
    frame.render_widget(Clear, area);
    let block = ui::overlay_block(title, accent, Some(ui::overlay_hints(hints)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// The key reference, grouped so a reader can find the section they need
/// instead of scanning one long list.
fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    const SECTIONS: &[(&str, &[(&str, &str)])] = &[
        (
            "COMPOSE",
            &[
                ("Enter", "send the prompt"),
                ("Ctrl+J · Ctrl+O · Shift+Enter", "insert a newline"),
                ("Tab", "accept the highlighted suggestion"),
                ("↑ ↓", "browse prompt history, or the suggestion list"),
                ("Ctrl+V", "paste text or an image from the clipboard"),
                ("Ctrl+A", "select the whole draft"),
                (
                    "Ctrl+C",
                    "copy the selection (or interrupt when nothing is selected)",
                ),
                ("Ctrl+Z · Ctrl+Y", "undo and redo"),
                ("PgUp · PgDn", "scroll the transcript a page"),
                ("Alt/Shift+↑ ↓", "scroll the transcript a few lines"),
                ("Ctrl+Home · Ctrl+End", "jump to the top, or back to live"),
                ("Esc", "clear the draft"),
            ],
        ),
        (
            "TRANSCRIPT (normal mode)",
            &[
                (
                    "F2",
                    "release the mouse so the terminal can select and copy text",
                ),
                ("j · k", "move between blocks"),
                ("o · space · enter", "fold or unfold a tool result"),
                ("h · l", "fold or unfold explicitly"),
                ("PgUp · PgDn", "scroll a page"),
                ("Ctrl+U · Ctrl+D", "scroll half a page"),
                ("Ctrl+Y · Ctrl+E", "scroll one line"),
                ("gg · G", "jump to the top, or back to live"),
                ("y · Y", "copy the selected block, or the last reply"),
                ("Esc", "drop the selection"),
                ("i a A I", "return to insert mode"),
            ],
        ),
        (
            "SESSION",
            &[
                ("Shift+Tab", "cycle AUTO / PLAN / BUILD"),
                ("Ctrl+C", "interrupt the turn; twice to exit"),
                ("Ctrl+Q", "quit"),
            ],
        ),
    ];

    let width = 84.min(area.width.saturating_sub(4));
    // Two border columns plus the frame's one-column padding on each side.
    let measure = width.saturating_sub(4) as usize;

    let mut lines = Vec::new();
    for (heading, keys) in SECTIONS {
        lines.push(Line::from(Span::styled(
            *heading,
            Style::default().fg(muted()).add_modifier(Modifier::BOLD),
        )));
        for (key, description) in *keys {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {key:<30}"),
                    Style::default().fg(primary()).add_modifier(Modifier::BOLD),
                ),
                Span::styled((*description).to_owned(), Style::default().fg(text())),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "COMMANDS",
        Style::default().fg(muted()).add_modifier(Modifier::BOLD),
    )));
    // Built from the same table that drives the palette, so the two can never
    // drift apart.
    let commands = SLASH_COMMANDS
        .iter()
        .map(|(command, _)| *command)
        .collect::<Vec<_>>()
        .join("  ");
    lines.extend(ui::wrap(
        &[Span::styled(commands, Style::default().fg(primary()))],
        measure,
        &[Span::raw("  ")],
        &[Span::raw("  ")],
    ));

    // Size the frame to the content rather than to a guess, so nothing is
    // silently clipped when a section grows.
    let popup = ui::centered(width, lines.len() as u16 + 2, area);
    let inner = open_overlay(
        frame,
        popup,
        "KEYS",
        secondary(),
        &[("esc", "close"), ("?", "toggle")],
    );
    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_usage(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(panel) = &app.usage_panel else {
        return;
    };
    let width = area.width.saturating_sub(4).clamp(24, 112);
    let height = area.height.saturating_sub(2).clamp(12, 29);
    let popup = ui::centered(width, height, area);
    let inner = open_overlay(
        frame,
        popup,
        "USAGE",
        secondary(),
        &[("tab", "view"), ("r", "dates"), ("esc", "close")],
    );

    let today = Local::now().date_naive();
    let records = panel
        .records
        .iter()
        .filter(|record| panel.range.includes(usage_date(record), today))
        .collect::<Vec<_>>();
    let inner_width = inner.width as usize;
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
    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
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
    // Native clipboard first: works on Wayland, X11, macOS and Windows with
    // no external tools. The subprocess paths below remain as a fallback for
    // environments where arboard cannot connect (odd SSH/X forwarding).
    if let Ok(mut clipboard) = arboard::Clipboard::new()
        && let Ok(text) = clipboard.get_text()
        && !text.is_empty()
    {
        return Some(text);
    }
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
    // Two rows of chrome, one blank, one help line — sized to the content so
    // the panel never leaves a band of dead space.
    let body_rows = CONFIG_ROWS.len() as u16;
    let popup = ui::centered(area.width.saturating_sub(8).min(96), body_rows + 4, area);
    let inner = open_overlay(
        frame,
        popup,
        "CONFIGURATION",
        secondary(),
        &[
            ("↑↓", "move"),
            ("enter", "edit"),
            ("esc", "close"),
            ("", "saved immediately"),
        ],
    );
    // The last two rows are a separator and the help line for the selected row.
    let help_height = 2u16.min(inner.height);
    let list = Rect {
        height: inner.height.saturating_sub(help_height),
        ..inner
    };

    let width = list.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut key_index = 0usize;
    let mut selected_row = 0usize;
    // Build every row first, then window around the cursor: with headings
    // interleaved, a row's position is no longer its index in CONFIG_KEYS.
    let mut rows: Vec<(Option<usize>, Line<'static>)> = Vec::new();
    for row in CONFIG_ROWS {
        match row {
            ConfigRow::Heading(title) => rows.push((
                None,
                Line::from(Span::styled(
                    format!("  {title}"),
                    Style::default().fg(rail()).add_modifier(Modifier::BOLD),
                )),
            )),
            ConfigRow::Key(key) => {
                let index = key_index;
                key_index += 1;
                let selected = index == panel.selected;
                if selected {
                    selected_row = rows.len();
                }
                rows.push((
                    Some(index),
                    list_row(
                        selected,
                        width,
                        vec![
                            Span::styled(
                                format!("{:<26}", config_label(*key)),
                                Style::default()
                                    .fg(if selected { text() } else { muted() })
                                    .add_modifier(if selected {
                                        Modifier::BOLD
                                    } else {
                                        Modifier::empty()
                                    }),
                            ),
                            Span::styled(
                                ui::truncate(&app.config_value(*key), width.saturating_sub(30)),
                                Style::default().fg(if selected { primary() } else { text() }),
                            ),
                        ],
                    ),
                ));
            }
        }
    }
    let visible = list.height as usize;
    let first = selected_row
        .saturating_sub(visible.saturating_sub(1))
        .min(rows.len().saturating_sub(visible.min(rows.len())));
    for (offset, (index, line)) in rows.into_iter().skip(first).take(visible).enumerate() {
        if let Some(index) = index {
            app.hits.borrow_mut().config.push((
                Rect {
                    x: list.x,
                    y: list.y + offset as u16,
                    width: list.width,
                    height: 1,
                },
                index,
            ));
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), list);

    if help_height == 2 {
        let help = Rect {
            y: list.bottom(),
            height: 2,
            ..inner
        };
        let key = CONFIG_KEYS[panel.selected.min(CONFIG_KEYS.len() - 1)];
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                ui::rule(inner.width),
                Line::from(Span::styled(
                    ui::truncate(config_help(key), inner.width as usize),
                    Style::default().fg(muted()),
                )),
            ])),
            help,
        );
    }

    if let Some((key, input)) = &panel.editing {
        let editor = ui::centered(popup.width.saturating_sub(10), 3, popup);
        frame.render_widget(Clear, editor);
        let block = ui::overlay_block(config_label(*key), primary(), None);
        let field = block.inner(editor);
        frame.render_widget(block, editor);
        let value = input.text();
        let shown = if *key == ConfigKey::ApiKey {
            "•".repeat(value.chars().count())
        } else {
            value.clone()
        };
        frame.render_widget(Paragraph::new(shown.as_str()), field);
        let (_, column) = input.cursor_position();
        frame.set_cursor_position((
            (field.x + column as u16).min(field.right().saturating_sub(1)),
            field.y,
        ));
    }
}

fn draw_raw_config(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(editor) = &app.raw_config else {
        return;
    };
    let popup = ui::centered(
        area.width.saturating_sub(6).min(112),
        area.height.saturating_sub(4),
        area,
    );
    // A parse error takes over the accent so the panel itself reports that the
    // document will not save in its current state.
    let accent = if editor.error.is_some() {
        danger()
    } else {
        secondary()
    };
    let inner = open_overlay(
        frame,
        popup,
        "ADVANCED · TOML",
        accent,
        &[("^S", "save & apply"), ("esc", "discard")],
    );

    let mut body = inner;
    if let Some(error) = &editor.error {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{} ", ui::glyphs().failed),
                    Style::default().fg(danger()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    ui::truncate(error, inner.width.saturating_sub(3) as usize),
                    Style::default().fg(danger()),
                ),
            ])),
            Rect { height: 1, ..inner },
        );
        body = Rect {
            y: inner.y + 1,
            height: inner.height.saturating_sub(1),
            ..inner
        };
    }

    let text = editor.input.text();
    let (row, column) = editor.input.cursor_position();
    let visible = body.height.max(1) as usize;
    let scroll = row.saturating_sub(visible.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(text.as_str())
            .scroll((scroll as u16, 0))
            .wrap(Wrap { trim: false }),
        body,
    );
    let visible_row = row.saturating_sub(scroll) as u16;
    frame.set_cursor_position((
        (body.x + column as u16).min(body.right().saturating_sub(1)),
        (body.y + visible_row).min(body.bottom().saturating_sub(1)),
    ));
}

fn draw_feedback(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(form) = &app.feedback_form else {
        return;
    };
    let popup = ui::centered(area.width.saturating_sub(10).min(88), 18, area);
    // The hint strip doubles as the status line: while a send is in flight or
    // has failed, that is the only thing worth saying down there.
    let hints: &[(&str, &str)] = if form.sending {
        &[("", "sending…")]
    } else {
        &[("^S", "send"), ("^D", "diagnostics"), ("esc", "cancel")]
    };
    let inner = open_overlay(frame, popup, "FEEDBACK", secondary(), hints);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Category  ", Style::default().fg(muted())),
            Span::styled(
                FEEDBACK_CATEGORIES[form.category],
                Style::default().fg(primary()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   tab to change", Style::default().fg(rail())),
        ])),
        sections[0],
    );

    let body = form.input.text();
    let field = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(rail()))
        .padding(Padding::horizontal(1));
    let field_inner = field.inner(sections[1]);
    frame.render_widget(field, sections[1]);
    frame.render_widget(
        Paragraph::new(if body.is_empty() {
            Text::from(Span::styled(
                "What should we improve? Please avoid secrets or sensitive source code.",
                Style::default().fg(rail()),
            ))
        } else {
            Text::from(body.as_str())
        })
        .wrap(Wrap { trim: false }),
        field_inner,
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
            Span::styled(
                " Include extension diagnostics",
                Style::default().fg(text()),
            ),
            ui::dot(),
            Span::styled(
                "your transcript is never included",
                Style::default().fg(muted()),
            ),
        ])),
        sections[2],
    );

    if let Some(error) = &form.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                ui::truncate(error, sections[3].width as usize),
                Style::default().fg(danger()),
            ))),
            sections[3],
        );
    }

    if !form.sending {
        let (row, column) = form.input.cursor_position();
        frame.set_cursor_position((
            (field_inner.x + column as u16).min(field_inner.right().saturating_sub(1)),
            (field_inner.y + row as u16).min(field_inner.bottom().saturating_sub(1)),
        ));
    }
}

/// The `ask_user` modal: the agent's question, its options, and a free-text
/// field for anything the options don't cover.
fn draw_user_question(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(question) = &app.question else {
        return;
    };
    let options = question.options.len().max(1) as u16;
    let height = (10 + options).min(area.height.saturating_sub(2));
    let popup = ui::centered(96.min(area.width.saturating_sub(4)), height, area);

    let hints: &[(&str, &str)] = if question.editing_custom {
        &[("esc", "leave field"), ("enter", "submit")]
    } else if question.multi_select {
        &[
            ("↑↓", "move"),
            ("space", "toggle"),
            ("t", "type"),
            ("enter", "submit"),
            ("^O", "peek output"),
        ]
    } else {
        &[
            ("↑↓", "move"),
            ("x", "choose"),
            ("t", "type"),
            ("esc", "skip"),
            ("^O", "peek output"),
        ]
    };
    let title = if question.header.is_empty() {
        "QUESTION".to_owned()
    } else {
        question.header.to_uppercase()
    };
    let inner = open_overlay(frame, popup, &title, secondary(), hints);

    // Every section gets its own row budget up front; the options list takes
    // whatever is left, which is what keeps a long list from pushing the
    // custom-answer field off the bottom of the frame.
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // question, wrapped to two rows
            Constraint::Min(2),    // options
            Constraint::Length(1), // spacer
            Constraint::Length(3), // custom answer
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Text::from(ui::wrap(
            &[Span::styled(
                question.question.clone(),
                Style::default().fg(text()).add_modifier(Modifier::BOLD),
            )],
            sections[0].width as usize,
            &[],
            &[],
        ))),
        sections[0],
    );

    let width = sections[1].width as usize;
    let mut lines = Vec::new();
    if question.options.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No options — type an answer below and press Enter.",
            Style::default().fg(muted()),
        )));
    }
    for (index, option) in question.options.iter().enumerate() {
        let on_cursor = index == question.cursor && !question.editing_custom;
        let checked = question.selected.get(index).copied().unwrap_or(false);
        let marker = match (question.multi_select, checked) {
            (true, true) => "[x]",
            (true, false) => "[ ]",
            (false, true) => "(•)",
            (false, false) => "( )",
        };
        lines.push(list_row(
            on_cursor,
            width,
            vec![
                Span::styled(
                    format!("{marker} "),
                    Style::default().fg(if checked { success() } else { muted() }),
                ),
                Span::styled(
                    ui::truncate(option, width.saturating_sub(7)),
                    Style::default()
                        .fg(if on_cursor { text() } else { muted() })
                        .add_modifier(if on_cursor {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ],
        ));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), sections[1]);

    let focused = question.editing_custom;
    let accent = if focused { primary() } else { rail() };
    let field = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .padding(Padding::horizontal(1))
        .title_top(Line::from(Span::styled(
            " your answer ",
            Style::default().fg(accent),
        )));
    let field_inner = field.inner(sections[3]);
    frame.render_widget(field, sections[3]);

    let value = question.custom.text();
    let line = if value.is_empty() && !focused {
        Line::from(Span::styled(
            if question.options.is_empty() {
                "press t to type an answer"
            } else {
                "optional — press t to add your own answer"
            },
            Style::default().fg(rail()),
        ))
    } else {
        // Window the cursor's line horizontally: an answer longer than the
        // field slides left so the insertion point stays visible, instead of
        // typing continuing invisibly past the right edge.
        let field_width = field_inner.width.max(1) as usize;
        let (cursor_row, cursor_column) = question.custom.cursor_position();
        let current_line = value.split('\n').nth(cursor_row).unwrap_or("");
        let skip = cursor_column.saturating_sub(field_width.saturating_sub(1));
        let visible: String = current_line.chars().skip(skip).take(field_width).collect();
        Line::from(Span::styled(visible, Style::default().fg(text())))
    };
    frame.render_widget(Paragraph::new(line), field_inner);

    if focused {
        let (_, column) = question.custom.cursor_position();
        let field_width = field_inner.width.max(1) as usize;
        let skip = column.saturating_sub(field_width.saturating_sub(1));
        frame.set_cursor_position((
            (field_inner.x + (column - skip) as u16).min(field_inner.right().saturating_sub(1)),
            field_inner.y.min(field_inner.bottom().saturating_sub(1)),
        ));
    }
}

/// The approval gate. This is the one screen where a wrong click costs the user
/// something, so it gets the widest frame, the warning accent, and controls
/// spelled out as labelled chips rather than bare letters.
fn draw_approval(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(approval) = &app.approval else {
        return;
    };
    let popup = ui::centered(
        area.width.saturating_sub(4).min(120),
        area.height.saturating_sub(2).max(10),
        area,
    );
    let mut hints: Vec<(&str, &str)> = vec![
        ("y", "allow once"),
        ("a", "allow session"),
        ("n", "reject"),
        ("j/k", "scroll"),
        ("^O", "peek output"),
    ];
    if approval.diff.is_some() {
        hints.push(("v", "raw/unified"));
    }
    let inner = open_overlay(frame, popup, "APPROVAL REQUIRED", warning(), &hints);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(4),
        ])
        .split(inner);

    let mut header = vec![Line::from(vec![
        Span::styled(
            format!("{}  ", approval.tool),
            Style::default().fg(warning()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            ui::truncate(
                &approval.summary,
                sections[0].width.saturating_sub(20) as usize,
            ),
            Style::default().fg(text()),
        ),
    ])];
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
                format!("   +{}", diff.additions),
                Style::default().fg(success()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  -{}", diff.deletions),
                Style::default().fg(danger()).add_modifier(Modifier::BOLD),
            ),
            ui::dot(),
            Span::styled(
                match approval.view {
                    ApprovalView::Unified => "unified view",
                    ApprovalView::Raw => "raw view",
                },
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
                    .border_style(Style::default().fg(rail()))
                    .title_top(Line::from(Span::styled(
                        if approval.diff.is_some() {
                            " changes "
                        } else {
                            " operation "
                        },
                        Style::default().fg(muted()),
                    ))),
            ),
        sections[1],
    );

    // The choices spelled out as sentences. The rejection line matters most:
    // saying what rejection *leads to* keeps it from reading as a dead end.
    let option = |key: &str, label: &str, lead: bool| {
        Line::from(vec![
            Span::styled(
                format!("  {key}  "),
                Style::default()
                    .fg(if lead { success() } else { primary() })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                label.to_owned(),
                Style::default().fg(if lead { text() } else { muted() }),
            ),
        ])
    };
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(""),
            option("y", "Yes, run it once", true),
            option(
                "a",
                "Yes, and allow this for the rest of the session",
                false,
            ),
            option(
                "n",
                "No — reject it, then tell Abacus in chat what to do instead",
                false,
            ),
        ])),
        sections[2],
    );
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
        let mut past_first_hunk = false;
        for line in &file.lines {
            match line.kind {
                // The line-number gutter already says where a hunk sits, so
                // the noisy `@@ -a,b +c,d @@` header earns no row. Hunks after
                // the first get a quiet elision mark for the skipped lines.
                DiffLineKind::Hunk => {
                    if past_first_hunk {
                        lines.push(Line::from(Span::styled(
                            format!("     {}", ui::glyphs().gap),
                            Style::default().fg(muted()),
                        )));
                    }
                    past_first_hunk = true;
                }
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
    let height = (picker.items.len() as u16 + 2)
        .min(area.height.saturating_sub(4))
        .max(3);
    let popup = ui::centered(area.width.saturating_sub(12).min(100), height, area);
    let inner = open_overlay(
        frame,
        popup,
        &picker.title.to_uppercase(),
        primary(),
        &[("↑↓", "select"), ("enter", "open"), ("esc", "close")],
    );

    let rows = inner.height as usize;
    let width = inner.width as usize;
    let start = picker.selected.saturating_sub(rows.saturating_sub(1));
    let mut lines = Vec::new();
    if picker.items.is_empty() {
        lines.push(Line::from(Span::styled(
            "  Nothing to show yet.",
            Style::default().fg(muted()),
        )));
    }
    for (index, (label, _)) in picker.items.iter().enumerate().skip(start).take(rows) {
        let selected = index == picker.selected;
        app.hits.borrow_mut().picker.push((
            Rect {
                x: inner.x,
                y: inner.y + lines.len() as u16,
                width: inner.width,
                height: 1,
            },
            index,
        ));
        lines.push(list_row(
            selected,
            width,
            vec![Span::styled(
                ui::truncate(label, width.saturating_sub(3)),
                Style::default()
                    .fg(if selected { text() } else { muted() })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )],
        ));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The slice of a tool result kept for expansion, bounded so one enormous
/// result cannot grow the session's footprint without limit.
fn retain_output(output: &str) -> String {
    if output.len() <= ui::MAX_RETAINED_OUTPUT {
        return output.to_owned();
    }
    let mut boundary = ui::MAX_RETAINED_OUTPUT;
    while !output.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}\n… truncated", &output[..boundary])
}

/// Whether a tool result reports a failure. The agent surfaces errors and
/// rejections as ordinary tool output, so the outcome has to be read back out
/// of the text — this is what colours the row's glyph red instead of green.
fn tool_failed(output: &str) -> bool {
    let head = output.trim_start();
    head.starts_with("Error:") || head.starts_with("error:") || head.starts_with("User rejected")
}

/// Short verb for a read-only tool inside an "explored" group summary.
fn explore_verb(name: &str) -> &'static str {
    match name {
        "read_file" | "read_files" => "read",
        "list_files" => "list",
        "glob" => "glob",
        "grep" => "grep",
        "tool_search" | "skill_search" => "search",
        "git_status" => "git status",
        "git_diff" => "git diff",
        "git_log" => "git log",
        "git_show" => "git show",
        "git_blame" => "git blame",
        "web_search" => "web",
        "read_page" => "fetch",
        _ => "read",
    }
}

/// Mine the streaming reasoning for a live status header: the most recent
/// complete `**bold**` span, which reasoning-trained models use as section
/// headers ("**Checking the parser**"). Returns `None` — and the footer keeps
/// its generic word — when the model reasons in plain prose.
fn reasoning_header(reasoning: &str) -> Option<String> {
    let mut header = None;
    let mut rest = reasoning;
    while let Some(start) = rest.find("**") {
        let after = &rest[start + 2..];
        let Some(length) = after.find("**") else {
            break;
        };
        let candidate = after[..length].trim();
        if !candidate.is_empty() && candidate.len() <= 64 && !candidate.contains('\n') {
            header = Some(candidate.to_owned());
        }
        rest = &after[length + 2..];
    }
    header
}

fn tool_preview(output: &str) -> String {
    if output.trim().is_empty() {
        return "(no output)".to_owned();
    }
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
            "user" => entries.push(Entry::new(
                EntryKind::User,
                content
                    .split("\n\n<attached_file path=\"")
                    .next()
                    .unwrap_or(content)
                    .to_owned(),
            )),
            "assistant" => entries.push(Entry::new(EntryKind::Assistant, content.to_owned())),
            // A restored session has no timings — the durations were never
            // persisted — but the outcome is still readable from the output, so
            // resumed tool rows keep their pass/fail colouring.
            "tool" => entries.push(Entry::tool(ToolCall {
                name: message["name"].as_str().unwrap_or("tool").to_owned(),
                summary: String::new(),
                status: if tool_failed(content) {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Ok
                },
                output: tool_preview(content),
                full: retain_output(content),
                duration_ms: None,
                expanded: false,
            })),
            _ => {}
        }
    }
    entries
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
        ConfigKey::AuxModel => "Auxiliary model",
        ConfigKey::Effort => "Reasoning effort",
        ConfigKey::BaseUrl => "Provider URL",
        ConfigKey::Protocol => "Wire protocol",
        ConfigKey::Providers => "Upstream providers",
        ConfigKey::Fallbacks => "Allow other providers",
        ConfigKey::ApiKey => "API key",
        ConfigKey::Permission => "Permission mode",
        ConfigKey::VimMode => "Vim keybindings",
        ConfigKey::ShowThinking => "Show thinking",
        ConfigKey::TokenRate => "Show tokens/second",
        ConfigKey::Animations => "Animations",
        ConfigKey::Tooltips => "Welcome tips",
        ConfigKey::DraftReplies => "Draft next message",
        ConfigKey::TraceLogging => "Training traces",
        ConfigKey::MaxSteps => "Maximum agent steps",
        ConfigKey::ContextWindow => "Context window",
        ConfigKey::MaxOutput => "Max output tokens",
        ConfigKey::ToolOutputLimit => "Tool output limit",
        ConfigKey::ProjectTrust => "Trust this project",
        ConfigKey::FeedbackEnabled => "Feedback",
        ConfigKey::FeedbackDiagnostics => "Feedback diagnostics",
        ConfigKey::FeedbackEndpoint => "Feedback endpoint",
        ConfigKey::AdvancedToml => "Advanced configuration",
    }
}

/// Parse an optional token count for a limit override: blank clears it, a
/// number (with `k`/`m` suffixes) sets it.
fn parse_optional_tokens(value: &str) -> Result<Option<usize>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    crate::model_info::parse_tokens(trimmed).map(Some)
}

fn limit_source_label(source: crate::model_info::LimitSource) -> &'static str {
    match source {
        crate::model_info::LimitSource::Override => "override",
        crate::model_info::LimitSource::Detected => "detected",
        crate::model_info::LimitSource::Heuristic => "known model",
        crate::model_info::LimitSource::Default => "default",
    }
}

/// The provider secondary calls use: the main one with the model swapped to
/// the configured aux model, or the main provider itself when none is set.
fn aux_provider_for(config: &Config, provider: &Provider) -> Provider {
    match config.aux_model.as_deref() {
        Some(model) if !model.trim().is_empty() && model != provider.model() => {
            provider.with_model(model)
        }
        _ => provider.clone(),
    }
}

fn config_key_is_editable(key: ConfigKey) -> bool {
    matches!(
        key,
        ConfigKey::Model
            | ConfigKey::AuxModel
            | ConfigKey::Effort
            | ConfigKey::BaseUrl
            | ConfigKey::Providers
            | ConfigKey::ApiKey
            | ConfigKey::MaxSteps
            | ConfigKey::ContextWindow
            | ConfigKey::MaxOutput
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
    // Only leading space is ignored. A *trailing* space means the user has
    // finished choosing — accepting a suggestion appends one — so trimming it
    // here would keep the popup open over a completed command and leave Enter
    // re-accepting it forever instead of sending.
    let query = input.trim_start();
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
        return Some((items, "COMMANDS"));
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
        return Some((items, "FILES"));
    }
    None
}

impl App {
    /// The completion list the popup should draw, or `None` when there is
    /// nothing to offer or the user has dismissed it for this text.
    fn visible_completion(&self) -> Option<(Vec<(String, String)>, &'static str)> {
        if self.completion_dismissed {
            return None;
        }
        active_completion(self)
    }

    /// Keep the highlighted row valid as the suggestion list changes under it.
    /// Editing the text resets the selection to the top and un-dismisses the
    /// popup — a fresh list deserves a fresh look.
    fn sync_completion(&mut self, previous: &str) {
        if !self.input.is_empty() {
            self.draft = None;
        }
        if self.input.text() != previous {
            self.completion_index = 0;
            self.completion_dismissed = false;
        }
        let count = active_completion(self)
            .map(|(items, _)| items.len())
            .unwrap_or(0);
        self.completion_index = self.completion_index.min(count.saturating_sub(1));
    }

    /// Move the highlight, wrapping at both ends so holding a key cycles.
    fn move_completion(&mut self, delta: isize) {
        let Some((items, _)) = self.visible_completion() else {
            return;
        };
        if items.is_empty() {
            return;
        }
        let count = items.len() as isize;
        let next = (self.completion_index as isize + delta).rem_euclid(count);
        self.completion_index = next as usize;
    }

    /// Insert the highlighted suggestion. A slash command replaces the whole
    /// line and leaves a trailing space ready for arguments; an `@file` mention
    /// replaces only the token under the cursor.
    fn accept_completion(&mut self) -> bool {
        let Some((items, _)) = self.visible_completion() else {
            return false;
        };
        let Some((value, _)) = items.get(self.completion_index).cloned() else {
            return false;
        };
        // Typing a command out in full leaves it highlighted; accepting it
        // again would only re-insert what is already there, so let the key
        // fall through to whatever it normally does.
        if self.input.text().trim_end() == value && !value.starts_with('@') {
            return false;
        }
        if let Some(path) = value.strip_prefix('@') {
            self.input.replace_token_before_cursor(&format!("@{path}"));
            self.input.insert(' ');
        } else {
            self.input.clear();
            self.input.insert_str(&value);
            self.input.insert(' ');
        }
        self.completion_index = 0;
        true
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

    #[tokio::test]
    async fn effort_command_sets_clears_and_reports() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        // Unset: reported as auto, and nothing is sent.
        assert!(app.slash_command("/effort"));
        assert!(
            app.entries.last().unwrap().text.contains("auto"),
            "{}",
            app.entries.last().unwrap().text
        );
        assert!(app.config.reasoning_effort.is_none());

        assert!(app.slash_command("/effort high"));
        assert_eq!(
            app.config.reasoning_effort,
            Some(crate::config::ReasoningEffort::High)
        );
        assert_eq!(app.config_value(ConfigKey::Effort), "high");
        assert!(app.status.contains("high"), "{}", app.status);

        // Aliases resolve, and `auto` clears back to the provider default.
        assert!(app.slash_command("/effort med"));
        assert_eq!(
            app.config.reasoning_effort,
            Some(crate::config::ReasoningEffort::Medium)
        );
        assert!(app.slash_command("/effort auto"));
        assert!(app.config.reasoning_effort.is_none());
        assert_eq!(app.config_value(ConfigKey::Effort), "auto");

        // Garbage is rejected without changing anything.
        assert!(app.slash_command("/effort ludicrous"));
        assert_eq!(app.entries.last().unwrap().kind, EntryKind::Error);
        assert!(app.config.reasoning_effort.is_none());
    }

    #[tokio::test]
    async fn btw_notes_a_side_question_without_derailing_the_turn() {
        // Registered, so completion offers it — a command nobody can find is
        // no command at all.
        assert!(SLASH_COMMANDS.iter().any(|(name, _)| *name == "/btw"));

        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        // With nothing running it declines rather than losing the note.
        assert!(app.slash_command("/btw is this thread safe?"));
        assert!(app.injections.is_empty());
        assert!(
            app.entries.last().unwrap().text.contains("ask it directly"),
            "{}",
            app.entries.last().unwrap().text
        );

        app.start_turn("do the thing".into(), "do the thing".into(), false);
        assert!(app.slash_command("/btw is this thread safe?"));
        assert!(!app.injections.is_empty(), "handed to the running turn");
        assert!(app.status.contains("noted"), "{}", app.status);
        // The turn is untouched — a side note is not an interrupt.
        assert!(app.running.is_some());

        // Empty notes are rejected.
        assert!(app.slash_command("/btw   "));
        assert_eq!(app.entries.last().unwrap().kind, EntryKind::Error);
    }

    #[tokio::test]
    async fn typing_during_a_turn_steers_instead_of_queueing() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.start_turn("do the thing".into(), "do the thing".into(), false);
        assert!(app.running.is_some());

        app.input.insert_str("actually use the other module");
        app.submit();

        // It goes to the running turn, not to the old wait-for-the-end queue.
        assert!(!app.injections.is_empty(), "handed to the running turn");
        assert!(app.status.contains("steering"), "{}", app.status);
        assert!(app.input.is_empty(), "composer cleared");
        // The user sees their own message immediately.
        let last = app.entries.last().expect("an entry");
        assert_eq!(last.kind, EntryKind::User);
        assert_eq!(last.text, "actually use the other module");
    }

    #[tokio::test]
    async fn a_background_report_arriving_while_idle_starts_a_delivery_turn() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        assert!(app.running.is_none());
        // Nothing pending → nothing happens.
        assert!(!app.deliver_pending_injections());

        app.injections.push(crate::agent::Injection::SubagentReport(
            "alpha: done".into(),
        ));
        assert!(app.deliver_pending_injections(), "a turn was started");
        assert!(app.running.is_some());
        let delivered = app
            .messages
            .iter()
            .rev()
            .find_map(|message| message["content"].as_str())
            .unwrap_or_default();
        assert!(delivered.contains("alpha: done"), "{delivered}");
        assert!(delivered.contains("background subagent finished"));
    }

    #[tokio::test]
    async fn aux_model_drives_the_secondary_provider_and_defaults_to_main() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        // No aux model set → the aux provider mirrors the main model.
        assert_eq!(app.aux_provider.model(), app.provider.model());

        // Setting it via the config commit path rebuilds the aux provider on
        // the same endpoint with the cheaper model.
        let mut input = InputBuffer::new();
        input.insert_str("cheap/model");
        app.config_panel = Some(ConfigPanel {
            selected: 0,
            editing: Some((ConfigKey::AuxModel, input)),
        });
        app.commit_config_edit();
        assert_eq!(app.aux_provider.model(), "cheap/model");
        assert_eq!(app.provider.model(), "test-model", "main model untouched");
        assert_eq!(
            app.config_value(ConfigKey::AuxModel),
            "cheap/model",
            "config shows the set value"
        );

        // Clearing it returns to "(same as main)".
        let mut blank = InputBuffer::new();
        blank.insert_str("  ");
        app.config_panel = Some(ConfigPanel {
            selected: 0,
            editing: Some((ConfigKey::AuxModel, blank)),
        });
        app.commit_config_edit();
        assert_eq!(app.aux_provider.model(), app.provider.model());
        assert_eq!(app.config_value(ConfigKey::AuxModel), "(same as main)");
    }

    #[tokio::test]
    async fn switching_away_from_a_scripted_profile_drops_the_endpoint() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let dir = &app.config.paths.endpoints_dir;
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("claude.yaml"),
            "url: https://api.anthropic.com/v1/messages\nprotocol: anthropic\nmodel: claude-opus-4-8\n",
        )
        .unwrap();
        // A scripted (Anthropic) profile and a plain chat-completions one.
        app.settings.profiles.insert(
            "claude".into(),
            ProviderProfile {
                name: "Claude".into(),
                base_url: "https://api.anthropic.com/v1/messages".into(),
                model: "claude-opus-4-8".into(),
                protocol: ProviderProtocol::Anthropic,
                api_key_env: None,
                aux_model: None,
                reasoning_effort: None,
                endpoint: Some("claude".into()),
                providers: Vec::new(),
                allow_fallbacks: true,
            },
        );
        app.settings.profiles.insert(
            "plain".into(),
            ProviderProfile {
                name: "Plain".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                model: "some/model".into(),
                protocol: ProviderProtocol::ChatCompletions,
                api_key_env: None,
                aux_model: None,
                reasoning_effort: None,
                endpoint: None,
                providers: Vec::new(),
                allow_fallbacks: true,
            },
        );

        app.settings.default_profile = "claude".into();
        app.apply_settings().unwrap();
        assert!(app.config.endpoint.is_some(), "scripted endpoint attached");
        assert_eq!(app.config.protocol, ProviderProtocol::Anthropic);

        // Switching to the plain profile must drop the scripted endpoint and
        // its wire format — the bug was it stayed attached.
        app.settings.default_profile = "plain".into();
        app.apply_settings().unwrap();
        assert!(app.config.endpoint.is_none(), "endpoint dropped on switch");
        assert_eq!(app.config.protocol, ProviderProtocol::ChatCompletions);
        assert!(app.config.base_url.contains("openrouter"));
    }

    #[test]
    fn scripted_endpoints_are_listed_and_selectable_in_the_provider_picker() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        // Drop a scripted endpoint into the app's endpoints dir.
        let dir = &app.config.paths.endpoints_dir;
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("claude-oauth.yaml"),
            "url: https://api.anthropic.com/v1/messages\nprotocol: anthropic\nmodel: claude-opus-4-8\n",
        )
        .unwrap();

        // It shows up as a provider-picker row with the endpoint sentinel.
        app.open_provider_picker();
        let picker = app.picker.as_ref().expect("provider picker");
        let row = picker
            .items
            .iter()
            .find(|(_, value)| value == &format!("{ENDPOINT_SENTINEL_PREFIX}claude-oauth"))
            .expect("claude-oauth is listed");
        assert!(row.0.contains("claude-oauth"), "{}", row.0);

        // Selecting it creates a live profile referencing the endpoint, with
        // the model/url/protocol copied from the YAML so it validates.
        app.add_provider(&format!("{ENDPOINT_SENTINEL_PREFIX}claude-oauth"));
        let profile = app
            .settings
            .profiles
            .get(&app.settings.default_profile)
            .expect("the new profile is active");
        assert_eq!(profile.endpoint.as_deref(), Some("claude-oauth"));
        assert_eq!(profile.model, "claude-opus-4-8");
        assert_eq!(profile.protocol, ProviderProtocol::Anthropic);
        assert!(profile.base_url.contains("anthropic.com"));
        assert!(app.status.contains("active"), "{}", app.status);
    }

    #[test]
    fn a_completed_command_can_be_sent() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        for ch in "/help".chars() {
            let before = app.input.text();
            handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
            );
            app.sync_completion(&before);
        }
        // Fully typed, so Enter must send rather than re-accept the suggestion.
        assert!(app.visible_completion().is_some(), "popup lists the match");
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        assert!(app.input.is_empty(), "Enter should have submitted");
        assert!(app.show_help, "/help should have run");
    }

    #[test]
    fn accepting_a_suggestion_closes_the_popup() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        for ch in "/comp".chars() {
            let before = app.input.text();
            handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
            );
            app.sync_completion(&before);
        }
        let before = app.input.text();
        handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        app.sync_completion(&before);
        assert_eq!(app.input.text(), "/compact ");
        // The trailing space is the signal that choosing is done. Trimming it
        // away was what trapped Enter in an accept loop.
        assert!(
            app.visible_completion().is_none(),
            "a completed command must not keep offering itself"
        );
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        assert!(app.input.is_empty(), "Enter should have submitted");
    }

    #[test]
    fn the_profile_row_opens_a_picker_that_switches_profiles() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.settings.profiles.insert(
            "second".to_owned(),
            crate::config::ProviderProfile {
                name: "Second".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "other-model".into(),
                protocol: ProviderProtocol::ChatCompletions,
                api_key_env: None,
                aux_model: None,
                reasoning_effort: None,
                endpoint: None,
                providers: Vec::new(),
                allow_fallbacks: true,
            },
        );
        app.open_profile_picker();
        let picker = app.picker.as_ref().expect("picker");
        assert_eq!(picker.action, PickerAction::SwitchProfile);
        // Every profile, plus the add-a-provider row.
        assert_eq!(picker.items.len(), app.settings.profiles.len() + 1);

        let index = picker
            .items
            .iter()
            .position(|(_, value)| value == "second")
            .expect("second profile listed");
        app.accept_picker(Some(index));
        assert_eq!(app.settings.default_profile, "second");
        assert!(app.picker.is_none());
    }

    #[test]
    fn adding_a_provider_creates_a_profile_and_asks_for_a_model() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.config_panel = Some(ConfigPanel {
            selected: 0,
            editing: None,
        });
        app.open_profile_picker();
        let index = app
            .picker
            .as_ref()
            .expect("picker")
            .items
            .iter()
            .position(|(_, value)| value == NEW_PROVIDER_SENTINEL)
            .expect("add-provider row");
        app.accept_picker(Some(index));

        // That row opens a second step rather than selecting anything.
        let picker = app.picker.as_ref().expect("provider picker");
        assert_eq!(picker.action, PickerAction::AddProvider);
        let xai = picker
            .items
            .iter()
            .position(|(_, value)| value == "xai")
            .expect("xai preset offered");
        app.accept_picker(Some(xai));

        let profile = app.settings.profiles.get("xai").expect("profile created");
        assert_eq!(profile.base_url, "https://api.x.ai/v1");
        assert_eq!(profile.api_key_env.as_deref(), Some("XAI_API_KEY"));
        assert_eq!(app.settings.default_profile, "xai");
        // Not applied yet: a profile with no model cannot validate, so the
        // running session stays on the old provider until one is given.
        assert_eq!(app.config.profile, "test");
        // A profile with no model cannot run, so that field opens straight away.
        let editing = app
            .config_panel
            .as_ref()
            .and_then(|panel| panel.editing.as_ref())
            .map(|(key, _)| *key);
        assert_eq!(editing, Some(ConfigKey::Model));
    }

    #[test]
    fn abandoning_the_model_prompt_rolls_the_new_provider_back() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.config_panel = Some(ConfigPanel {
            selected: 0,
            editing: None,
        });
        app.add_provider("groq");
        assert_eq!(app.settings.default_profile, "groq");

        // Esc out of the model prompt.
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(
            app.settings.default_profile, "test",
            "the previous profile should be restored"
        );
        assert!(
            !app.settings.profiles.contains_key("groq"),
            "an unusable profile should not be left behind"
        );
    }

    #[test]
    fn a_committed_model_keeps_the_new_provider() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.config_panel = Some(ConfigPanel {
            selected: 0,
            editing: None,
        });
        app.add_provider("groq");
        if let Some(panel) = &mut app.config_panel
            && let Some((_, input)) = panel.editing.as_mut()
        {
            input.insert_str("llama-3.3-70b");
        }
        app.commit_config_edit();
        assert_eq!(app.settings.default_profile, "groq");
        assert_eq!(app.config.model, "llama-3.3-70b", "now applied");
        assert!(app.pending_provider.is_none());

        // A later Esc must not undo a finished profile.
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert!(app.settings.profiles.contains_key("groq"));
    }

    #[test]
    fn adding_the_same_provider_twice_does_not_overwrite_the_first() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.add_provider("groq");
        app.settings.profiles.get_mut("groq").expect("first").model = "keep-me".into();
        app.add_provider("groq");
        assert_eq!(
            app.settings.profiles.get("groq").expect("first").model,
            "keep-me",
            "the existing profile must survive"
        );
        assert!(app.settings.profiles.contains_key("groq-2"));
        assert_eq!(app.settings.default_profile, "groq-2");
    }

    #[test]
    fn the_api_key_row_reports_provenance_and_never_the_secret() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        assert_eq!(app.config_value(ConfigKey::ApiKey), "not set");

        app.config_panel = Some(ConfigPanel {
            selected: 0,
            editing: None,
        });
        app.begin_config_edit(ConfigKey::ApiKey);
        // The editor starts empty rather than seeded with anything.
        let buffer = app
            .config_panel
            .as_ref()
            .and_then(|panel| panel.editing.as_ref())
            .map(|(_, input)| input.text())
            .expect("editing");
        assert!(buffer.is_empty());

        if let Some(panel) = &mut app.config_panel
            && let Some((_, input)) = panel.editing.as_mut()
        {
            input.insert_str("sk-secret-value");
        }
        app.commit_config_edit();
        let shown = app.config_value(ConfigKey::ApiKey);
        assert_eq!(shown, "set · stored locally");
        assert!(!shown.contains("sk-secret"), "the key must never be echoed");
        assert_eq!(
            app.credentials.keys.get("test").map(String::as_str),
            Some("sk-secret-value")
        );
    }

    #[test]
    fn a_picker_opened_from_config_is_visible_and_owns_the_keys() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.settings.profiles.insert(
            "second".to_owned(),
            crate::config::ProviderProfile {
                name: "Second".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "other".into(),
                protocol: ProviderProtocol::ChatCompletions,
                api_key_env: None,
                aux_model: None,
                reasoning_effort: None,
                endpoint: None,
                providers: Vec::new(),
                allow_fallbacks: true,
            },
        );
        app.config_panel = Some(ConfigPanel {
            selected: 0,
            editing: None,
        });
        app.open_profile_picker();

        // Drawn on top: the config panel must not paint over its own child.
        let backend = TestBackend::new(96, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), 96, 34);
        assert!(rendered.contains("PROFILE"), "picker should be visible");
        assert!(
            rendered.contains("Add a provider"),
            "picker rows should be visible"
        );

        // And it owns the keys, rather than them going to the panel behind it.
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        assert_eq!(app.picker.as_ref().expect("picker").selected, 1);
        assert_eq!(
            app.config_panel.as_ref().expect("panel").selected,
            0,
            "the panel behind must not have moved"
        );
    }

    #[test]
    fn a_draft_is_only_offered_to_an_idle_composer() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        assert!(app.settings.ui.draft_replies, "on by default");

        // Typing invalidates a shown draft immediately.
        app.draft = Some("run the tests".into());
        let before = app.input.text();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()),
        );
        app.sync_completion(&before);
        assert_eq!(app.draft, None, "a draft must not linger over typed text");

        // A draft arriving late, after the user has started typing, is dropped
        // rather than replacing what they wrote.
        let _ = app.draft_tx.send(Some("too late".into()));
        app.drain_draft_events();
        assert_eq!(app.draft, None);

        // With an empty composer it is kept.
        app.input.clear();
        let _ = app.draft_tx.send(Some("run the tests".into()));
        app.drain_draft_events();
        assert_eq!(app.draft.as_deref(), Some("run the tests"));
    }

    #[test]
    fn tab_takes_the_draft_and_leaves_completion_alone() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.draft = Some("add a regression test".into());
        handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(app.input.text(), "add a regression test");
        assert_eq!(app.draft, None, "accepting consumes it");

        // With text present, Tab is the completion key again, not a draft key.
        app.input.clear();
        app.input.insert_str("/mod");
        app.sync_completion("");
        app.draft = Some("should be ignored".into());
        handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert!(app.input.text().starts_with("/mod"));
        assert_ne!(app.input.text(), "should be ignored");
    }

    #[test]
    fn drafting_off_suppresses_the_request_entirely() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.settings.ui.draft_replies = false;
        app.start_draft();
        assert!(app.draft_task.is_none(), "no call should be made");

        // And a turn still running never triggers one either.
        app.settings.ui.draft_replies = true;
        app.input.insert_str("half typed");
        app.start_draft();
        assert!(app.draft_task.is_none(), "a busy composer is left alone");
    }

    #[test]
    fn a_trackpad_burst_scrolls_a_line_at_a_time() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        // First event after a pause is a wheel notch.
        assert_eq!(app.scroll_step(), 3);
        // Events arriving back to back are a trackpad, and move one line each so
        // the view does not shoot past what is being read.
        assert_eq!(app.scroll_step(), 1);
        assert_eq!(app.scroll_step(), 1);

        // After a pause it is a notch again.
        app.last_scroll = Some(Instant::now() - Duration::from_millis(400));
        assert_eq!(app.scroll_step(), 3);
    }

    #[test]
    fn the_footer_separates_session_total_from_context_size() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.entries = vec![Entry::new(EntryKind::Assistant, "hi")];
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), 100, 20);
        // "449.1k tokens · ctx 24%" read as one measurement of the same thing.
        // They are the running session total and the current window, so they
        // are named differently and the window shows its own denominator.
        assert!(
            rendered.contains("used"),
            "session total should be labelled"
        );
        assert!(rendered.contains("ctx "), "context should be labelled");
        assert!(
            !rendered.contains("tokens  ·  ctx"),
            "the two figures must not both read as token counts"
        );
    }

    #[test]
    fn a_trace_opens_with_the_session_and_records_the_toggle() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.session_store = Some(SessionStore::new(
            &app.config.paths,
            app.config.workspace.clone(),
        ));
        app.config.trace_enabled = true;
        assert!(app.trace.is_none(), "nothing to key on before a session");

        // The session is created on first persist, and the trace opens with it.
        app.persist_session();
        let trace = app
            .trace
            .as_ref()
            .expect("trace should open with the session");
        assert!(trace.path().exists());
        assert!(
            trace.path().starts_with(&app.config.paths.traces_dir),
            "traces belong under the traces directory"
        );

        // Turning it off in /config drops the writer.
        app.settings.trace.enabled = true;
        app.cycle_config_value(ConfigKey::TraceLogging).unwrap();
        assert!(!app.settings.trace.enabled);
        assert!(app.trace.is_none(), "disabling must stop capture at once");
        assert_eq!(app.config_value(ConfigKey::TraceLogging), "Off");

        // And back on.
        app.cycle_config_value(ConfigKey::TraceLogging).unwrap();
        assert!(app.settings.trace.enabled);
        assert!(app.trace.is_some(), "re-enabling reopens it");
    }

    /// Push a reasoning chunk through the real event path.
    fn reasoning(app: &mut App, piece: &str) {
        let _ = app
            .event_tx
            .send(crate::agent::AgentEvent::Reasoning(piece.to_owned()));
        app.drain_agent_events();
    }

    #[test]
    fn thinking_is_shown_by_default_and_kept_apart_from_the_answer() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        assert!(app.settings.ui.show_thinking, "on by default");

        reasoning(&mut app, "let me check the parser");
        let _ = app
            .event_tx
            .send(crate::agent::AgentEvent::Delta("Here is the fix.".into()));
        app.drain_agent_events();

        let kinds: Vec<EntryKind> = app.entries.iter().map(|entry| entry.kind).collect();
        assert_eq!(
            kinds,
            vec![EntryKind::Thinking, EntryKind::Assistant],
            "reasoning must not be appended to the answer"
        );
        assert_eq!(app.entries[0].text, "let me check the parser");
        assert_eq!(app.entries[1].text, "Here is the fix.");
    }

    #[test]
    fn thinking_off_leaves_no_block_but_still_counts_the_output() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.settings.ui.show_thinking = false;
        reasoning(&mut app, "a long private deliberation");
        assert!(
            app.entries.is_empty(),
            "nothing should be rendered: {:?}",
            app.entries
        );
        // It was still generated and billed, so the rate must account for it.
        assert_eq!(app.turn_output_chars, "a long private deliberation".len());
    }

    #[test]
    fn the_token_rate_waits_for_something_worth_measuring() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        assert_eq!(app.token_rate(), None, "no turn running");

        app.turn_started = Some(Instant::now());
        assert_eq!(app.token_rate(), None, "no output yet");

        // A fraction of a second in, the divisor is small enough to produce a
        // meaningless number.
        app.turn_output_chars = 400;
        assert_eq!(app.token_rate(), None, "too early to be meaningful");

        app.turn_started = Some(Instant::now() - Duration::from_secs(10));
        let rate = app.token_rate().expect("measurable");
        // 400 chars ≈ 100 tokens over 10s.
        assert!((rate - 10.0).abs() < 1.0, "got {rate}");
    }

    #[tokio::test]
    async fn the_rate_is_off_by_default_and_only_shows_while_running() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        assert!(!app.settings.ui.show_token_rate, "off by default");

        app.settings.ui.show_token_rate = true;
        app.turn_started = Some(Instant::now() - Duration::from_secs(10));
        app.turn_output_chars = 4_000;
        app.entries = vec![Entry::new(EntryKind::User, "go")];

        // Not running: the status bar has no rate to report.
        let backend = TestBackend::new(110, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(!buffer_text(terminal.backend().buffer(), 110, 16).contains("tok/s"));

        // Running with the toggle on, it appears beside the elapsed time.
        app.start_turn("go".into(), "go".into(), false);
        app.turn_started = Some(Instant::now() - Duration::from_secs(10));
        app.turn_output_chars = 4_000;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), 110, 16);
        assert!(rendered.contains("tok/s"), "{rendered}");
        assert!(rendered.contains("100 tok/s"), "4000 chars / 4 / 10s");

        // And stays hidden when the toggle is off, even mid-turn.
        app.settings.ui.show_token_rate = false;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(!buffer_text(terminal.backend().buffer(), 110, 16).contains("tok/s"));
    }

    #[test]
    fn ctrl_o_steps_a_question_aside_and_a_new_dialog_returns_visible() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let (respond, _receive) = tokio::sync::oneshot::channel();
        app.set_user_question(crate::agent::UserQuestionRequest {
            question: "Which one?".into(),
            header: "PICK".into(),
            options: vec!["a".into(), "b".into()],
            multi_select: false,
            respond,
        });
        assert!(!app.overlay_hidden);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        );
        assert!(app.overlay_hidden);
        // While hidden, transcript keys work instead of feeding the dialog.
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::empty()),
        );
        assert!(app.question.is_some(), "the question is parked, not lost");
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        );
        assert!(!app.overlay_hidden);
        // A fresh dialog always arrives visible.
        app.overlay_hidden = true;
        let (respond, _receive) = tokio::sync::oneshot::channel();
        app.set_approval(crate::agent::ApprovalRequest {
            tool: "write_file".into(),
            summary: "x".into(),
            details: "x".into(),
            respond,
        });
        assert!(!app.overlay_hidden);
    }

    #[test]
    fn f3_toggles_thinking_and_hides_streamed_reasoning_blocks() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.push_entry(Entry::new(EntryKind::Thinking, "step by step"));
        app.push_entry(Entry::new(EntryKind::Assistant, "the answer"));
        let visible = ui::transcript(&app.entries, 60, "•", None, true);
        let joined = |t: &ui::Transcript| {
            t.lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref().to_owned())
                .collect::<String>()
        };
        assert!(joined(&visible).contains("step by step"));

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::F(3), KeyModifiers::empty()),
        );
        assert!(!app.settings.ui.show_thinking);
        let hidden = ui::transcript(&app.entries, 60, "•", None, false);
        assert!(!joined(&hidden).contains("step by step"));
        assert!(joined(&hidden).contains("the answer"));
        // Entry indices stay aligned for the cursor and click hit-testing.
        assert_eq!(hidden.spans.len(), app.entries.len());
    }

    #[tokio::test]
    async fn ctrl_g_returns_to_the_live_tail() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.follow = false;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        );
        assert!(app.follow);
    }

    #[test]
    fn output_token_override_applies_live_and_clears_back_to_auto() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let edit = |app: &mut App, key: ConfigKey, text: &str| {
            let mut input = InputBuffer::new();
            input.insert_str(text);
            app.config_panel = Some(ConfigPanel {
                selected: 0,
                editing: Some((key, input)),
            });
            app.commit_config_edit();
        };

        // Setting the override reaches settings, the resolved limits, and the
        // wire value in one step — this is the /config escape hatch for a
        // provider that rejects the detected max_tokens.
        edit(&mut app, ConfigKey::MaxOutput, "64k");
        assert_eq!(app.settings.agent.max_output_tokens, Some(64_000));
        assert_eq!(
            app.config.model_limits.configured_output_tokens,
            Some(64_000)
        );
        assert!(app.status.contains("saved"), "{}", app.status);

        // Context window accepts m-suffixed values.
        edit(&mut app, ConfigKey::ContextWindow, "1m");
        assert_eq!(app.config.model_limits.context_window, 1_000_000);

        // Blank (or "auto") clears the override and re-resolves.
        edit(&mut app, ConfigKey::MaxOutput, "");
        assert_eq!(app.settings.agent.max_output_tokens, None);
        edit(&mut app, ConfigKey::ContextWindow, "auto");
        assert_eq!(app.settings.agent.context_window, None);
        assert_ne!(
            app.config.model_limits.source,
            crate::model_info::LimitSource::Override
        );

        // Garbage is rejected without changing anything.
        edit(&mut app, ConfigKey::MaxOutput, "lots");
        assert!(app.status.contains("configuration error"), "{}", app.status);
        assert_eq!(app.settings.agent.max_output_tokens, None);
    }

    #[test]
    fn consecutive_read_only_tools_collapse_into_an_explored_group() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        for (name, summary, output) in [
            ("read_file", "src/a.rs", "fn a() {}"),
            ("grep", "'pattern'", "src/a.rs:3: match"),
            ("read_file", "src/b.rs", "fn b() {}"),
        ] {
            let _ = app.event_tx.send(AgentEvent::ToolStarted {
                name: name.into(),
                summary: summary.into(),
            });
            let _ = app.event_tx.send(AgentEvent::ToolFinished {
                name: name.into(),
                output: output.into(),
            });
        }
        assert!(app.drain_agent_events());
        let tools: Vec<_> = app
            .entries
            .iter()
            .filter_map(|entry| entry.tool.as_ref())
            .collect();
        assert_eq!(tools.len(), 1, "three reads collapse to one row");
        let group = tools[0];
        assert_eq!(group.name, "explored");
        assert!(group.summary.contains("read src/a.rs"), "{}", group.summary);
        assert!(
            group.summary.contains("grep 'pattern'"),
            "{}",
            group.summary
        );
        // Expansion shows each call's full result, labelled.
        assert!(group.full.contains("── read_file src/a.rs ──"));
        assert!(group.full.contains("fn b() {}"));
    }

    #[test]
    fn writes_and_failures_do_not_join_an_exploration_group() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        for (name, output) in [
            ("read_file", "content"),
            ("write_file", "wrote 3 lines"),
            ("read_file", "Error: missing"),
        ] {
            let _ = app.event_tx.send(AgentEvent::ToolStarted {
                name: name.into(),
                summary: "x".into(),
            });
            let _ = app.event_tx.send(AgentEvent::ToolFinished {
                name: name.into(),
                output: output.into(),
            });
        }
        assert!(app.drain_agent_events());
        let tools: Vec<_> = app
            .entries
            .iter()
            .filter_map(|entry| entry.tool.as_ref())
            .collect();
        assert_eq!(tools.len(), 3, "a write and a failure stay individual rows");
        assert!(tools.iter().all(|call| call.name != "explored"));
    }

    #[tokio::test]
    async fn esc_esc_rewinds_to_the_previous_prompt() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        // Exercise the insert-mode path; vim mode routes Esc through Normal
        // mode first and has its own arm with the same behavior.
        app.settings.ui.vim_mode = false;
        app.messages = vec![
            serde_json::json!({"role":"system","content":"s"}),
            serde_json::json!({"role":"user","content":"first question"}),
            serde_json::json!({"role":"assistant","content":"first answer"}),
        ];
        app.push_entry(Entry::new(EntryKind::User, "first question"));
        app.push_entry(Entry::new(EntryKind::Assistant, "first answer"));

        // One Esc only arms; nothing is discarded yet.
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.messages.len(), 3);
        assert!(app.status.contains("esc again"), "{}", app.status);

        // A second Esc rewinds: prompt back in the composer, turn discarded.
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.input.text(), "first question");
        assert_eq!(app.messages.len(), 1, "only the system message remains");
        assert!(
            app.entries
                .iter()
                .all(|entry| entry.kind != EntryKind::User),
            "the user entry was rewound"
        );

        // Any other key disarms: Esc, type, Esc must not rewind.
        app.input.clear();
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()),
        );
        assert!(app.rewind_armed.is_none());
    }

    #[test]
    fn approval_modal_spells_out_the_choices() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let (respond, _receive) = tokio::sync::oneshot::channel();
        app.set_approval(crate::agent::ApprovalRequest {
            tool: "run_command".into(),
            summary: "rm -rf build/".into(),
            details: "$ rm -rf build/".into(),
            respond,
        });
        let backend = TestBackend::new(110, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), 110, 34);
        assert!(rendered.contains("Yes, run it once"), "{rendered}");
        assert!(
            rendered.contains("allow this for the rest of the session"),
            "{rendered}"
        );
        assert!(
            rendered.contains("tell Abacus in chat what to do instead"),
            "{rendered}"
        );
    }

    #[test]
    fn diff_hunks_render_a_gap_mark_instead_of_headers() {
        let diff = DiffDocument::parse(
            "--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n-a\n+b\n@@ -10,2 +10,2 @@\n-c\n+d\n",
        )
        .unwrap();
        let text = diff_text(&diff);
        let plain: Vec<String> = text
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(
            !plain.iter().any(|line| line.contains("@@")),
            "hunk headers must not render: {plain:?}"
        );
        assert_eq!(
            plain.iter().filter(|line| line.trim() == "⋮").count(),
            1,
            "one gap between two hunks: {plain:?}"
        );
    }

    #[test]
    fn shimmer_preserves_the_text_and_respects_the_animations_toggle() {
        let joined = |spans: &[ratatui::text::Span<'_>]| {
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let off = ui::shimmer("thinking", Duration::from_millis(500), false);
        assert_eq!(off.len(), 1);
        assert_eq!(joined(&off), "thinking");
        let on = ui::shimmer("thinking", Duration::from_millis(500), true);
        assert_eq!(on.len(), "thinking".len());
        assert_eq!(joined(&on), "thinking");
    }

    #[test]
    fn reasoning_header_takes_the_latest_complete_bold_span() {
        assert_eq!(reasoning_header("no markup at all"), None);
        assert_eq!(
            reasoning_header("**Reading the config** then prose"),
            Some("Reading the config".to_owned())
        );
        // The newest header wins, and an unterminated one is ignored.
        assert_eq!(
            reasoning_header("**First step** prose **Second step** more **half"),
            Some("Second step".to_owned())
        );
        // Emphasis spanning lines or overlong "headers" are not headers.
        assert_eq!(reasoning_header("**a\nb**"), None);
        let long = format!("**{}**", "x".repeat(80));
        assert_eq!(reasoning_header(&long), None);
    }

    #[tokio::test]
    async fn heavy_turns_get_a_worked_for_separator_and_chat_turns_do_not() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        // A long turn that ran tools → rule.
        app.turn_had_tools = true;
        app.turn_started = Some(Instant::now() - Duration::from_secs(61));
        let _ = app.event_tx.send(AgentEvent::Done {
            messages: vec![serde_json::json!({"role":"system","content":"s"})],
            reason: DoneReason::Complete,
        });
        assert!(app.drain_agent_events());
        let rule = app
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::Rule)
            .expect("a worked-for rule");
        assert!(rule.text.starts_with("Worked for 1m"), "{}", rule.text);

        // A quick conversational turn → no rule.
        let before = app.entries.len();
        app.turn_had_tools = false;
        app.turn_started = Some(Instant::now() - Duration::from_secs(200));
        let _ = app.event_tx.send(AgentEvent::Done {
            messages: vec![serde_json::json!({"role":"system","content":"s"})],
            reason: DoneReason::Complete,
        });
        assert!(app.drain_agent_events());
        assert!(
            app.entries[before..]
                .iter()
                .all(|entry| entry.kind != EntryKind::Rule)
        );
    }

    #[test]
    fn empty_tool_output_reads_as_no_output() {
        assert_eq!(tool_preview(""), "(no output)");
        assert_eq!(tool_preview("  \n "), "(no output)");
        assert!(!tool_preview("real content").contains("(no output)"));
    }

    #[test]
    fn repair_command_fixes_corruption_and_reports_a_clean_history() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.messages = vec![
            serde_json::json!({"role":"system","content":"s"}),
            serde_json::json!({"role":"assistant","content":"Let me write that.","tool_calls":[
                {"id":"cut","type":"function","function":{"name":"write_file","arguments":"{\"content\": \"trunc"}}
            ]}),
        ];
        app.ctx_chars = 0;

        assert!(app.slash_command("/repair"));
        let notice = app.entries.last().expect("a repair report");
        assert_eq!(notice.kind, EntryKind::System);
        assert!(notice.text.contains("Repaired"), "{}", notice.text);
        assert!(app.messages[1].get("tool_calls").is_none());
        assert!(app.ctx_chars > 0, "context estimate must be refreshed");

        // A second pass finds nothing and says so.
        assert!(app.slash_command("/repair"));
        let notice = app.entries.last().expect("a no-op report");
        assert_eq!(notice.kind, EntryKind::System);
        assert!(notice.text.contains("No corruption"), "{}", notice.text);
    }

    #[tokio::test]
    async fn repair_command_refuses_to_run_mid_turn() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.messages = vec![serde_json::json!({"role":"system","content":"s"})];
        let before = app.messages.clone();
        app.start_turn("go".into(), "go".into(), false);
        assert!(app.slash_command("/repair"));
        assert_eq!(app.status, "cannot repair while a turn is running");
        // The running turn's history is untouched (only the queued user
        // message was added by start_turn).
        assert_eq!(app.messages.len(), before.len() + 1);
    }

    #[test]
    fn provider_failure_hints_at_repair() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let _ = app.event_tx.send(AgentEvent::Failed {
            error: "provider stream error: {\"code\":400}".to_owned(),
            messages: vec![serde_json::json!({"role":"system","content":"s"})],
        });
        assert!(app.drain_agent_events());
        let last = app.entries.last().expect("a hint");
        assert_eq!(last.kind, EntryKind::System);
        assert!(last.text.contains("/repair"), "{}", last.text);
        // A non-provider failure gets no hint — /repair cannot fix those.
        let _ = app.event_tx.send(AgentEvent::Failed {
            error: "file reference warning".to_owned(),
            messages: vec![serde_json::json!({"role":"system","content":"s"})],
        });
        assert!(app.drain_agent_events());
        assert_eq!(app.entries.last().expect("an error").kind, EntryKind::Error);
    }

    #[test]
    fn thinking_command_toggles_and_takes_an_explicit_state() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        assert!(app.settings.ui.show_thinking);

        // Bare invocation flips it.
        assert!(app.slash_command("/thinking"));
        assert!(!app.settings.ui.show_thinking);
        assert!(app.slash_command("/thinking"));
        assert!(app.settings.ui.show_thinking);

        // Explicit states are idempotent, so a keybinding or script can set
        // rather than flip.
        assert!(app.slash_command("/thinking off"));
        assert!(!app.settings.ui.show_thinking);
        assert!(app.slash_command("/thinking off"));
        assert!(!app.settings.ui.show_thinking);
        assert!(app.slash_command("/thinking on"));
        assert!(app.settings.ui.show_thinking);
    }

    #[test]
    fn thinking_command_says_capture_continues_when_hidden() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.slash_command("/thinking off");
        let last = app.entries.last().expect("a notice");
        assert_eq!(last.kind, EntryKind::System);
        assert!(
            last.text.contains("still recorded"),
            "hiding must not read as disabling capture: {}",
            last.text
        );
    }

    #[test]
    fn thinking_command_rejects_an_unknown_argument_without_changing_anything() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let before = app.settings.ui.show_thinking;
        assert!(app.slash_command("/thinking maybe"));
        assert_eq!(app.settings.ui.show_thinking, before);
        assert_eq!(app.entries.last().expect("an error").kind, EntryKind::Error);
    }

    /// The palette is built from the same table the dispatcher matches on, so a
    /// command that exists must be discoverable.
    #[test]
    fn thinking_is_offered_by_the_command_palette() {
        assert!(
            SLASH_COMMANDS
                .iter()
                .any(|(command, _)| *command == "/thinking")
        );
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.input.insert_str("/think");
        let (items, _) = app.visible_completion().expect("a suggestion");
        assert!(items.iter().any(|(value, _)| value == "/thinking"));
    }

    #[test]
    fn providers_command_pins_orders_and_clears() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        assert!(app.slash_command("/providers Together, Anthropic"));
        let profile = app.settings.profiles.get("test").expect("profile");
        // Order is meaningful — it is a preference list, not a set.
        assert_eq!(profile.providers, vec!["Together", "Anthropic"]);

        // Whitespace separation works too, since both read naturally.
        assert!(app.slash_command("/providers DeepInfra Novita"));
        assert_eq!(
            app.settings.profiles["test"].providers,
            vec!["DeepInfra", "Novita"]
        );

        assert!(app.slash_command("/providers clear"));
        assert!(app.settings.profiles["test"].providers.is_empty());
    }

    #[test]
    fn strict_and_fallback_control_whether_anything_else_may_serve() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        assert!(
            app.settings.profiles["test"].allow_fallbacks,
            "on by default"
        );
        assert!(app.slash_command("/providers strict"));
        assert!(!app.settings.profiles["test"].allow_fallbacks);
        assert!(app.slash_command("/providers fallback"));
        assert!(app.settings.profiles["test"].allow_fallbacks);
    }

    #[test]
    fn providers_with_no_argument_reports_the_current_pin() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.slash_command("/providers");
        let text = &app.entries.last().expect("a notice").text;
        assert!(text.contains("No providers pinned"), "{text}");

        app.slash_command("/providers Together");
        app.slash_command("/providers");
        let text = &app.entries.last().expect("a notice").text;
        assert!(text.contains("Together"), "{text}");
    }

    #[test]
    fn config_rows_and_keys_agree() {
        // The panel numbers its rows by display position but looks settings up
        // in CONFIG_KEYS. If the two orders drift, the cursor sits on one row
        // and Enter edits another.
        let displayed = CONFIG_ROWS
            .iter()
            .filter_map(|row| match row {
                ConfigRow::Key(key) => Some(*key),
                ConfigRow::Heading(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(displayed, CONFIG_KEYS);
        // Every heading must precede at least one setting, or it renders as a
        // label with nothing under it.
        assert!(matches!(CONFIG_ROWS.first(), Some(ConfigRow::Heading(_))));
        for pair in CONFIG_ROWS.windows(2) {
            if let ConfigRow::Heading(title) = pair[0] {
                assert!(
                    matches!(pair[1], ConfigRow::Key(_)),
                    "heading {title} has no settings under it"
                );
            }
        }
    }

    #[test]
    fn transcript_wraps_to_an_exact_row_count() {
        // Wrapping happens here rather than in ratatui, so the row count is
        // authoritative: twelve columns minus the two-column gutter leaves ten
        // usable cells, so twenty-five characters take three rows.
        let entries = vec![Entry::new(EntryKind::Assistant, "a".repeat(25))];
        let rows = ui::transcript(&entries, 12, "•", None, true).lines.len();
        assert_eq!(rows, 3);
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
            assert!(rendered.contains("CONFIGURATION"));
            assert!(rendered.contains("Active profile"));

            app.config_panel = None;
            app.open_feedback();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let rendered = buffer_text(terminal.backend().buffer(), width, height);
            assert!(rendered.contains("FEEDBACK"));
            assert!(rendered.contains("Category"));
        }
    }

    #[test]
    fn transcript_renders_markdown_semantically() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.entries = vec![Entry::new(
            EntryKind::Assistant,
            "# Result\n\nUse **cargo test** and `cargo clippy`.\n\n```rust\nfn main() {}\n```\n\n| Check | State |\n|---|---|\n| tests | green |",
        )];
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), 80, 30);
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
        assert!(rendered.contains("USAGE"));
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
        assert_eq!(title, "FILES");
        assert!(items.iter().any(|(value, _)| value == "@src/main.rs"));

        assert!(app.accept_completion());
        assert_eq!(app.input.text(), "look at @src/main.rs ");
    }

    #[tokio::test]
    async fn exit_command_quits() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        assert!(app.slash_command("/exit"));
        assert!(app.quit);
    }

    #[test]
    fn completion_popup_navigates_and_inserts_the_highlighted_row() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        for ch in "/mod".chars() {
            let before = app.input.text();
            handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()),
            );
            app.sync_completion(&before);
        }
        let (items, _) = app.visible_completion().expect("command completion");
        assert!(items.len() > 1, "expected /mode and /model");
        assert_eq!(app.completion_index, 0);

        // Down moves the highlight rather than reaching for prompt history.
        let before = app.input.text();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        app.sync_completion(&before);
        assert_eq!(app.completion_index, 1);
        assert_eq!(
            app.input.text(),
            "/mod",
            "navigation must not edit the draft"
        );

        // Enter inserts what is highlighted, not the first match.
        let before = app.input.text();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        app.sync_completion(&before);
        assert_eq!(app.input.text(), format!("{} ", items[1].0));
    }

    #[test]
    fn editing_the_draft_resets_the_highlight_and_revives_a_dismissed_popup() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.input.insert_str("/mod");
        app.sync_completion("");
        app.completion_index = 1;

        // Esc dismisses without leaving insert mode or clearing the draft.
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert!(app.completion_dismissed);
        assert!(app.visible_completion().is_none());
        assert_eq!(app.input.text(), "/mod");

        // Typing brings it back, at the top of the fresh list.
        let before = app.input.text();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::empty()),
        );
        app.sync_completion(&before);
        assert!(!app.completion_dismissed);
        assert_eq!(app.completion_index, 0);
        assert!(app.visible_completion().is_some());
    }

    fn tool_entry(output: &str) -> Entry {
        Entry::tool(ToolCall {
            name: "run_command".into(),
            summary: "cargo test".into(),
            status: ToolStatus::Ok,
            output: tool_preview(output),
            full: retain_output(output),
            duration_ms: Some(120),
            expanded: false,
        })
    }

    #[test]
    fn normal_mode_walks_blocks_and_unfolds_the_selected_tool() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let long = (1..=40)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.entries = vec![
            Entry::new(EntryKind::User, "run the tests"),
            tool_entry(&long),
        ];
        app.mode = InputMode::Normal;

        // k from nothing selects the last block, not the first.
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty()),
        );
        assert_eq!(app.cursor, Some(1));
        assert!(!app.follow, "selecting stops follow-mode");

        let collapsed = ui::transcript(&app.entries, 60, "•", app.cursor, true)
            .lines
            .len();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::empty()),
        );
        assert!(
            app.entries[1].tool.as_ref().expect("tool").expanded,
            "o should unfold the selected tool"
        );
        let expanded = ui::transcript(&app.entries, 60, "•", app.cursor, true)
            .lines
            .len();
        assert!(
            expanded > collapsed,
            "unfolding must reveal rows: {collapsed} -> {expanded}"
        );

        // The preview caps at 8 lines; the full result must survive for the
        // unfolded view.
        let rendered = ui::transcript(&app.entries, 60, "•", app.cursor, true);
        let text: String = rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect();
        assert!(text.contains("line 40"), "full output should be reachable");

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::empty()),
        );
        assert!(!app.entries[1].tool.as_ref().expect("tool").expanded);
    }

    #[test]
    fn folding_is_offered_only_when_there_is_more_to_see() {
        let short = tool_entry("one line");
        let call = short.tool.as_ref().expect("tool");
        assert!(
            !call.has_more(),
            "a result the preview already shows in full is not foldable"
        );

        let long = (1..=40)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(tool_entry(&long).tool.as_ref().expect("tool").has_more());
    }

    #[test]
    fn a_selected_block_is_scrolled_into_view() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        for n in 0..40 {
            app.entries
                .push(Entry::new(EntryKind::User, format!("prompt {n}")));
        }
        app.mode = InputMode::Normal;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let bottom = app.scroll;

        // Walk to the very first block; the viewport has to follow it up.
        for _ in 0..60 {
            handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty()),
            );
        }
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.cursor, Some(0));
        assert!(app.scroll < bottom, "the view should have scrolled up");
        assert_eq!(app.scroll, 0, "the first block sits at the top");
    }

    #[test]
    fn clicking_the_transcript_selects_then_unfolds_a_tool_row() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let long = (1..=30)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.entries = vec![Entry::new(EntryKind::User, "run it"), tool_entry(&long)];
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        // Find where the tool block actually landed rather than assuming a row.
        let (rect, index) = app
            .hits
            .borrow()
            .transcript
            .iter()
            .copied()
            .find(|(_, index)| *index == 1)
            .expect("tool block should be clickable");
        assert_eq!(index, 1);

        handle_click(&mut app, rect.x + 2, rect.y);
        assert_eq!(app.cursor, Some(1), "first click selects");
        assert!(!app.entries[1].tool.as_ref().expect("tool").expanded);

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        handle_click(&mut app, rect.x + 2, rect.y);
        assert!(
            app.entries[1].tool.as_ref().expect("tool").expanded,
            "clicking the selected row unfolds it"
        );
    }

    #[test]
    fn clicking_a_suggestion_inserts_it() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.input.insert_str("/mod");
        app.sync_completion("");
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let (items, _) = app.visible_completion().expect("completion");
        let (rect, index) = app
            .hits
            .borrow()
            .completion
            .iter()
            .copied()
            .find(|(_, index)| *index == 1)
            .expect("second suggestion should be clickable");
        handle_click(&mut app, rect.x + 1, rect.y);
        assert_eq!(app.input.text(), format!("{} ", items[index].0));
    }

    #[test]
    fn a_click_on_an_overlay_does_not_reach_the_transcript_beneath() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.entries = vec![Entry::new(EntryKind::User, "hello")];
        app.config_panel = Some(ConfigPanel {
            selected: 0,
            editing: None,
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let (rect, _) = app
            .hits
            .borrow()
            .config
            .iter()
            .copied()
            .find(|(_, index)| *index == 2)
            .expect("config row");
        handle_click(&mut app, rect.x + 2, rect.y);
        assert_eq!(
            app.config_panel.as_ref().expect("panel").selected,
            2,
            "the click belongs to the panel on top"
        );
        assert_eq!(
            app.cursor, None,
            "the transcript must not have been touched"
        );
    }

    #[tokio::test]
    async fn esc_asks_a_running_turn_to_stop_before_killing_it() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.start_turn("check the tests".into(), "check the tests".into(), true);
        assert!(app.running.is_some());

        // First press is cooperative: the turn is asked to stop so it can
        // report the work it already did, rather than being killed and losing
        // every tool result from this turn.
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert!(app.cancel.load(Ordering::Relaxed), "stop was requested");
        assert!(
            app.running.is_some(),
            "the turn should still be finishing up"
        );
        assert!(app.status.contains("interrupting"));
    }

    #[tokio::test]
    async fn a_second_interrupt_escalates_and_settles_the_open_tool_row() {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        app.start_turn("check the tests".into(), "check the tests".into(), true);
        app.push_entry(Entry::tool(ToolCall {
            name: "run_command".into(),
            summary: "cargo test".into(),
            status: ToolStatus::Running,
            output: String::new(),
            full: String::new(),
            duration_ms: None,
            expanded: false,
        }));
        app.tool_started = Some(Instant::now());

        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        assert!(app.running.is_none(), "a second esc forces the stop");
        assert!(app.turn_started.is_none());
        // The aborted task never reports back, so the row must not keep
        // spinning for the rest of the session.
        let call = app
            .entries
            .iter()
            .rev()
            .find_map(|entry| entry.tool.as_ref())
            .expect("tool row");
        assert_eq!(call.status, ToolStatus::Failed);
        assert_eq!(call.output, "interrupted");
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
                aux_model: None,
                reasoning_effort: None,
                endpoint: None,
                providers: Vec::new(),
                allow_fallbacks: true,
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
            mode: None,
            trace_enabled: false,
            routing: Default::default(),
            web_search: crate::web::WebConfig::default(),
            endpoint: None,
            aux_model: None,
            reasoning_effort: None,
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
