use crate::{
    activity::ActivityReporter,
    agent::{
        AgentEvent, AgentMode, ApprovalDecision, ApprovalRequest, DoneReason, TurnOptions,
        UserQuestionRequest, compact_messages, initial_messages, message_chars, run_turn,
    },
    config::{Config, Credentials, PermissionMode, ProviderProtocol, SETTINGS_VERSION, Settings},
    context::expand_file_references,
    diff::{DiffDocument, DiffLineKind},
    input::{InputBuffer, InputMode},
    provider::Provider,
    ralph::{RalphLoop, RalphStatus},
    services::AgentServices,
    session::{Session, SessionState, SessionStore, SessionUsage},
    theme::{
        ThemeMode, border, danger, inverse, muted, primary, rail, secondary, success, surface,
        text, warning,
    },
    ui::{self, Entry, EntryKind, ToolCall, ToolStatus, bold, emphasis, fg},
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
use futures_util::{SinkExt, StreamExt};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, Stdout},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};
use unicode_width::UnicodeWidthStr;

mod commands;
mod draw;
mod hub;
mod keys;
mod settings;
#[cfg(test)]
mod tests;
mod usage;

use self::{draw::*, hub::*, keys::*, settings::*, usage::*};

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/goal", "Set or manage a persistent goal"),
    ("/loop", "Start or inspect a Ralph loop"),
    ("/cancel-loop", "Cancel the active Ralph loop"),
    ("/swarm", "Delegate an objective to parallel subagents"),
    ("/config", "Change live settings"),
    ("/theme", "Switch dark, light, or auto theme"),
    ("/feedback", "Send product feedback"),
    ("/remote", "Share this session through Abacus Sync"),
    ("/mode", "Set auto, plan, or build mode"),
    ("/plan", "Toggle plan pin"),
    ("/thinking", "Show or hide the model's reasoning"),
    ("/effort", "Set reasoning effort: minimal, low, medium, high, xhigh, max, auto"),
    ("/btw", "Note a side question without derailing the running turn"),
    ("/model", "Inspect or switch model"),
    ("/models", "Browse every model and assign the roles they serve"),
    ("/profile", "List, switch, rename, or delete a provider profile"),
    ("/providers", "Pin which upstream providers may serve the model"),
    ("/usage", "View local usage and activity"),
    ("/sessions", "Browse saved sessions"),
    ("/new", "Start a new session"),
    ("/fork", "Fork the session — conversation continues in a new one"),
    ("/compact", "Compact conversation context"),
    ("/repair", "Fix corrupted session history"),
    ("/papercuts", "List or delete recorded lessons"),
    ("/memories", "List or delete stored memories"),
    ("/harness", "Inspect the continual harness, its log, and revert"),
    ("/refine", "Update the harness from this conversation now"),
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
    hub_scope: Vec<(Rect, usize)>,
    hub_body: Vec<(Rect, usize)>,
}

impl Hits {
    fn clear(&mut self) {
        self.completion.clear();
        self.config.clear();
        self.picker.clear();
        self.transcript.clear();
        self.hub_scope.clear();
        self.hub_body.clear();
    }
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
                let raw = self.options[index].split(" — ").next().unwrap_or(&self.options[index]);
                selected_labels.push(raw.to_owned());
            }
        }
        let custom_text = self.custom.text();
        let custom = if custom_text.trim().is_empty() { None } else { Some(custom_text) };
        crate::agent::UserAnswer { selected_labels, custom_text: custom }
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
    /// Inline prompt on top of the list (profile rename / delete confirm).
    prompt: Option<PickerPrompt>,
}

enum PickerPrompt {
    Rename { id: String, input: InputBuffer },
    ConfirmDelete { id: String },
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
    Theme,
    Glyphs,
    VimMode,
    ShowThinking,
    TokenRate,
    Animations,
    Tooltips,
    DraftReplies,
    TokenCompression,
    OneStream,
    CheckUpdates,
    SafetyModel,
    TraceLogging,
    MaxSteps,
    ToolOutputLimit,
    ProjectTrust,
    SearchEnabled,
    SearchBackend,
    SearchInstanceUrl,
    SearchApiKeyEnv,
    SearchSharedInstance,
    FeedbackEnabled,
    FeedbackDiagnostics,
    FeedbackEndpoint,
    AdvancedToml,
}

const fn typed(key: ConfigKey, label: &'static str, help: &'static str) -> Setting {
    Setting { key, label, typed: true, help }
}

const fn toggled(key: ConfigKey, label: &'static str, help: &'static str) -> Setting {
    Setting { key, label, typed: false, help }
}

fn settings() -> impl Iterator<Item = &'static Setting> {
    SETTINGS.iter().flat_map(|(_, rows)| rows.iter())
}

/// The setting for `key`, and where the cursor has to be to select it.
fn setting(key: ConfigKey) -> (usize, &'static Setting) {
    settings().enumerate().find(|(_, setting)| setting.key == key).expect("every key has a row")
}

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

/// Work finished off the UI thread, posted back to it over one channel.
enum Background {
    /// A predicted next message for the empty composer.
    Draft(Option<String>),
    /// A profile's model list, for the `/models` hub.
    Catalog {
        profile: String,
        result: Result<Vec<crate::model_info::ModelCard>, String>,
    },
    Feedback(Result<crate::feedback::FeedbackReceipt, String>),
    /// The outcome of a manual `/refine`.
    Refined(String),
    Services(Result<Box<AgentServices>, String>),
    Remote(Remote),
    /// A newer release exists.
    Update(crate::update::Available),
}

/// What the `/remote` connection reports.
enum Remote {
    Status(String),
    Prompt(String),
    Interrupt,
    Closed(String),
}

struct App {
    config: Config,
    settings: Settings,
    credentials: Credentials,
    provider: Provider,
    /// The auxiliary-model provider for secondary calls (drafts here; the
    /// agent loop builds its own for refine/tether/classification).
    aux_provider: Provider,
    messages: Vec<Value>,
    session: Option<Session>,
    session_store: Option<SessionStore>,
    services: Arc<AgentServices>,
    /// What the conversation carries between turns.
    state: SessionState,
    papercuts: crate::papercuts::PapercutStore,
    hive: crate::hive::HiveHandle,
    /// Mode-discipline counts behind the escalating reminder.
    modes: crate::modes::ModeCoach,
    /// Whether Abacus is holding the mouse. Holding it enables wheel scrolling
    /// and clickable rows but takes click-drag away from the terminal on
    /// terminals without a Shift-drag bypass, which is how you select and copy
    /// text — so it is releasable with F2.
    mouse_captured: bool,
    /// Ctrl+P: the subagent detail overlay.
    hive_overlay: bool,
    hive_scroll: u16,
    ralph_loop: Option<RalphLoop>,
    entries: Vec<Entry>,
    input: InputBuffer,
    mode: InputMode,
    running: Option<JoinHandle<()>>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    event_rx: mpsc::UnboundedReceiver<AgentEvent>,
    background_tx: mpsc::UnboundedSender<Background>,
    background_rx: mpsc::UnboundedReceiver<Background>,
    approval: Option<PendingApproval>,
    approval_scroll: u16,
    approval_horizontal: u16,
    question: Option<PendingUserQuestion>,
    picker: Option<Picker>,
    usage_panel: Option<UsagePanel>,
    config_panel: Option<ConfigPanel>,
    raw_config: Option<RawConfigEditor>,
    feedback_form: Option<FeedbackForm>,
    model_hub: Option<crate::model_hub::ModelHub>,
    /// Body rows the last frame had room for. Keyboard paging needs the
    /// measure the renderer arrived at, and only the renderer knows it.
    hub_rows: std::cell::Cell<usize>,
    /// Model catalogs by profile id, fetched on first visit and kept for the
    /// session so reopening the hub is instant.
    catalogs: HashMap<String, crate::model_hub::Catalog>,
    /// Where the pointer last was, so a row under it can paint a hover band.
    /// Cleared on any keystroke: once you are back on the keyboard, a stale
    /// band beside the cursor is two highlights saying different things.
    pointer: Option<(u16, u16)>,
    remote_task: Option<JoinHandle<()>>,
    remote_outbound: Option<mpsc::UnboundedSender<String>>,
    sync_idle_since: Option<Instant>,
    last_auto_push: Option<Instant>,
    last_board_version: u64,
    reload_services: bool,
    services_reloading: bool,
    /// A manual refinement is in flight; a second would race it for the store.
    refining: bool,
    /// Safety verdicts, kept for the session rather than the turn.
    safety: crate::safety::SafetyCache,
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
    show_help: bool,
    normal_prefix: Option<char>,
    agent_mode: AgentMode,
    resolved_agent_mode: Option<AgentMode>,
    /// Shared session token ledger; reused when the provider is rebuilt on a
    /// model switch so the running totals survive.
    tokens: Arc<crate::provider::TokenLedger>,
    session_initial_active_secs: u64,
    started: Instant,
    last_ctrl_c: Option<Instant>,
    quit: bool,
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
    // first frame so the palette matches the surrounding terminal. A named
    // theme that fails to load falls back and reports itself once the screen
    // exists to report it on.
    let (theme, theme_error) = crate::theme::resolve(&settings.ui.theme, &config.paths.themes_dir);
    crate::theme::set_active(theme);
    crate::ui::set_glyphs(settings.ui.glyphs);
    // A recovery file names the session that was interrupted. Resume THAT
    // session so recovered output lands where the turn was running, instead of
    // opening a fresh empty screen and pinning the text to a throwaway session.
    let mut session = session;
    if session.is_none()
        && let Some(recovery_id) = crate::recovery::peek_session(&config.paths.recovery_file)
        && let Some(store) = &session_store
        && let Ok(recovered) = store.load(&recovery_id)
    {
        session = Some(recovered);
    }
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
    let activity_session = session_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let activity_model = config.model.clone();
    if let Some(reporter) = &reporter {
        reporter.report_start(&activity_session, &activity_model).await;
    }
    crate::recovery::arm(config.paths.recovery_file.clone(), session_id.clone());
    enable_raw_mode()?;
    TERMINAL_CLAIMED.store(true, Ordering::SeqCst);
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        // Capture the mouse from the first frame so the wheel scrolls the
        // transcript and rows are clickable. Terminals that support Shift-drag
        // bypass (iTerm2, kitty, WezTerm, Alacritty, Ghostty) still select text
        // while captured; everywhere else F2 hands the mouse back for
        // drag-select, and PgUp/PgDn scroll regardless.
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
    KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
    let restore = TerminalRestore;
    install_terminal_guards();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let workspace = config.workspace.clone();
    let mut app =
        App::new(config, settings, credentials, session, session_store, services.clone())?;
    if let Some(error) = theme_error {
        app.fail(format!("{error}\nFalling back to the built-in theme."));
    }
    if crate::sync::is_configured(&app.credentials) {
        let workspace = app.config.workspace.clone();
        let paths_sync = app.config.paths.clone();
        tokio::spawn(async move {
            let _ = crate::sync::pull_workspace(&paths_sync, &workspace).await;
        });
    }
    // Heartbeat the open session so the dashboard shows live tokens and so a
    // session that is killed (terminal closed) drops off "active" instead of
    // lingering. The shared token counter survives model switches.
    let heartbeat = reporter
        .as_ref()
        .map(|reporter| reporter.heartbeat(activity_session.clone(), app.tokens.clone()));
    // Before the first frame: a reply an earlier run died in the middle of is
    // handed back at the top of the transcript.
    app.surface_recovered_reply();
    let result = event_loop(&mut terminal, &mut app).await;
    app.persist_session();
    if let Some(handle) = heartbeat {
        handle.abort();
    }
    let end_services = app.services.clone();
    let end_session_id = app.session.as_ref().map(|session| session.id.to_string());
    let tokens_used = app.provider.tokens_used();
    let duration_secs = app.started.elapsed().as_secs();
    // Restore the user's terminal BEFORE any network sync or lifecycle hook.
    // Those are best-effort and can stall on DNS/TLS/server timeouts; keeping
    // raw mode active while awaiting them makes `/exit` look like a total
    // terminal freeze and prevents the user from recovering their shell.
    drop(terminal);
    drop(restore);
    // After the terminal is back: how to get back here. Without this the only
    // ways in are `/sessions`-style archaeology or remembering the UUID.
    if let Some(id) = &end_session_id {
        eprintln!(
            "\nSession saved — resume with: abacus --resume {id}  (or `abacus -c` for the latest in this workspace)"
        );
    }
    if crate::sync::is_configured(&app.credentials)
        && tokio::time::timeout(
            Duration::from_secs(3),
            crate::sync::push_all_updated_local(&app.config.paths),
        )
        .await
        .is_err()
    {
        eprintln!("Session sync is still pending; it will retry next time Abacus opens.");
    }
    let status = if result.is_ok() { "completed" } else { "failed" };
    let hook_result = tokio::time::timeout(
        Duration::from_secs(3),
        end_services.run_hooks(
            "session_end",
            end_session_id.as_deref(),
            &json!({"workspace":workspace,"mode":"tui","status":status}),
        ),
    )
    .await
    .unwrap_or_else(|_| Ok(Vec::new()));
    if let Some(reporter) = &reporter {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            reporter.report_end(&activity_session, tokens_used, duration_secs),
        )
        .await;
    }
    result?;
    hook_result?;
    // A termination signal asked for a graceful exit; the session is now
    // persisted and synced, so leave with the shell-conventional code.
    if PENDING_SIGNAL.load(Ordering::SeqCst) != 0 {
        GRACEFUL_EXIT.store(true, Ordering::SeqCst);
        std::process::exit(PENDING_SIGNAL.swap(0, Ordering::SeqCst));
    }
    Ok(())
}

/// Whether the terminal is currently handed over to the TUI, so a restore that
/// runs twice (or before setup) does nothing.
static TERMINAL_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Whether the kitty keyboard flags were pushed and still need popping.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

/// A termination signal's shell exit code (128+n), set by the guard task so
/// the event loop can finish the session — persist, sync, hint — before
/// exiting with that code.
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Set once the graceful exit path has taken over; the guard task's hard-exit
/// backstop checks this so it never fires after a clean shutdown.
static GRACEFUL_EXIT: AtomicBool = AtomicBool::new(false);

/// Give the terminal back exactly as it was found.
///
/// This used to live only in `Drop`, which covers a normal return and an
/// unwinding panic but *not* a signal — and a signal is how a TUI usually
/// dies: SIGHUP when an SSH session closes, SIGTERM when someone else on the
/// box ends it. Neither unwinds, so the terminal was left in raw mode with the
/// alternate screen still active. Every line printed afterwards then advanced
/// without a carriage return, marching one column right per row: a diagonal
/// staircase of shell prompts across a blank screen.
///
/// Idempotent, so `Drop`, the panic hook and a signal handler can all call it.
fn restore_terminal() {
    if !TERMINAL_CLAIMED.swap(false, Ordering::SeqCst) {
        return;
    }
    if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
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

/// Cover the two exits `Drop` cannot see: a panic, whose message would
/// otherwise print *into* the raw-mode alternate screen where it is both
/// illegible and invisible, and a termination signal.
///
/// The panic hook restores unconditionally rather than trying to tell a render
/// panic from one in a background task. A panic is a bug either way, and the
/// failure mode this picks — the message legible on a normal screen — beats
/// the alternative of a wrecked terminal that outlives the process.
fn install_terminal_guards() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
        // After the panic message, so the pointer to the recovered text is the
        // last thing on screen rather than buried above a backtrace.
        if let Some(path) = crate::recovery::flush() {
            eprintln!("\nThe reply in progress was saved to {} — it is not lost.", path.display());
        }
    }));
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // 128 + signal number, the shell's convention for a signalled exit.
        for (kind, code) in [(SignalKind::hangup(), 129), (SignalKind::terminate(), 143)] {
            let Ok(mut stream) = signal(kind) else {
                continue;
            };
            tokio::spawn(async move {
                stream.recv().await;
                // Hand control to the event loop, which persists the session
                // and syncs before exiting. The delay below is only a backstop
                // for a loop that is wedged and cannot notice.
                PENDING_SIGNAL.store(code, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(10)).await;
                if !GRACEFUL_EXIT.load(Ordering::SeqCst) {
                    restore_terminal();
                    if let Some(path) = crate::recovery::flush() {
                        eprintln!(
                            "Interrupted mid-reply; the partial text is in {}",
                            path.display()
                        );
                    }
                    std::process::exit(code);
                }
            });
        }
    }
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        restore_terminal();
    }
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut dirty = true;
    while !app.quit {
        dirty |= app.drain_agent_events();
        dirty |= app.drain_background();
        app.maybe_idle_sync();
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
                    if app.sync_idle_since.is_some() {
                        app.sync_idle_since = Some(Instant::now());
                    }
                    let before = app.input.text();
                    app.pointer = None;
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
                            app.pointer = Some((mouse.column, mouse.row));
                            handle_click(app, mouse.column, mouse.row)
                        }
                        MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                            app.pointer = Some((mouse.column, mouse.row));
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
                    } else if let Some(input) =
                        app.picker.as_mut().and_then(|picker| match picker.prompt.as_mut() {
                            Some(PickerPrompt::Rename { input, .. }) => Some(input),
                            _ => None,
                        })
                    {
                        input.insert_str(&text);
                    } else if let Some((_, input)) =
                        app.config_panel.as_mut().and_then(|panel| panel.editing.as_mut())
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
        dirty |= app.board_changed();
        if app.pending_signal() {
            app.interrupt();
            return Ok(());
        }
    }
    app.interrupt();
    Ok(())
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
        if path.is_absolute() { path } else { workspace.join(path) }
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_owned()),
        // Detached HEAD: the short object id is the useful thing to show.
        None => head.get(..7).map(str::to_owned),
    }
}

/// Columns held back for the scrollbar: one blank gap, one track.
const SCROLLBAR_COLUMNS: u16 = 2;

/// Content is centred and capped at this width on wide terminals.
const CONTENT_COLUMNS: u16 = 112;

/// Push the session, open the remote socket, and relay in both directions
/// until the server hangs up: replies out, browser prompts and interrupts in.
async fn serve_remote(
    client: crate::sync::SyncClient,
    session: Session,
    trace: std::path::PathBuf,
    snapshot: Vec<Value>,
    mut replies: mpsc::UnboundedReceiver<String>,
    events: &mpsc::UnboundedSender<Background>,
) -> Result<()> {
    use tokio_tungstenite::tungstenite::Message;
    let trace = std::fs::read(trace).unwrap_or_default();
    client.push(&session, &trace, true).await.context("sync failed")?;
    let socket_url = client.enable_remote(&session.id.to_string()).await?;
    let (socket, _) = tokio_tungstenite::connect_async(&socket_url).await?;
    let (mut sink, mut stream) = socket.split();
    let mut seq = 0_u64;
    let mut frame = |kind: &str, payload: Value| {
        seq += 1;
        let id = uuid::Uuid::new_v4().to_string();
        let frame = json!({"v": 1, "type": kind, "id": id, "seq": seq, "payload": payload});
        Message::Text(frame.to_string().into())
    };
    sink.send(frame("snapshot", json!({"entries": snapshot}))).await?;
    let status = "remote enabled — open the server /remote page".to_owned();
    let _ = events.send(Background::Remote(Remote::Status(status)));
    loop {
        tokio::select! {
            Some(text) = replies.recv() => {
                sink.send(frame("entry", json!({"kind": "assistant", "text": text}))).await?;
            }
            message = stream.next() => {
                let Some(message) = message else { break };
                let message = message?;
                let Some(incoming) = message.to_text().ok().and_then(|text| serde_json::from_str::<Value>(text).ok()) else {
                    continue;
                };
                match incoming["type"].as_str().unwrap_or_default() {
                    "prompt" => {
                        if let Some(prompt) = incoming.pointer("/payload/text").and_then(Value::as_str) {
                            let _ = events.send(Background::Remote(Remote::Prompt(prompt.to_owned())));
                        }
                    }
                    "interrupt" => {
                        let _ = events.send(Background::Remote(Remote::Interrupt));
                    }
                    "ping" => sink.send(frame("pong", json!({}))).await?,
                    _ => {}
                }
            }
        }
    }
    bail!("remote disconnected")
}

/// The slice of a tool result kept for expansion, bounded so one enormous
/// result cannot grow the session's footprint without limit.
fn retain_output(output: &str) -> String {
    crate::text::clip_bytes(output, ui::MAX_RETAINED_OUTPUT, "\n… truncated")
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
    crate::text::clip_bytes(&preview, 1_200, "…")
}

/// Record a result on a tool row: the outcome read out of it, the collapsed
/// preview, and the text kept for expansion.
fn settle(call: &mut ToolCall, output: &str, duration_ms: Option<u64>) {
    call.status = if tool_failed(output) { ToolStatus::Failed } else { ToolStatus::Ok };
    call.output = tool_preview(output);
    call.full = retain_output(output);
    call.duration_ms = duration_ms;
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
                content.split("\n\n<attached_file path=\"").next().unwrap_or(content).to_owned(),
            )),
            "assistant" => entries.push(Entry::new(EntryKind::Assistant, content.to_owned())),
            // A restored session has no timings — the durations were never
            // persisted — but the outcome is still readable from the output, so
            // resumed tool rows keep their pass/fail colouring.
            "tool" => {
                let mut call = ToolCall::running(message["name"].as_str().unwrap_or("tool"), "");
                settle(&mut call, content, None);
                entries.push(Entry::tool(call));
            }
            _ => {}
        }
    }
    entries
}

/// The name a session goes by in the harness: its id, or a fresh one for a
/// conversation that has not been saved yet.
fn session_key(session: Option<&Session>) -> String {
    session.map_or_else(|| uuid::Uuid::new_v4().to_string(), |session| session.id.to_string())
}

fn toggle(flag: &mut bool) {
    *flag = !*flag;
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
                    ..Default::default()
                },
            );
        }
        settings.default_profile = config.profile.clone();
        settings.agent.max_steps = config.max_steps;
        settings.agent.tool_output_limit = config.tool_output_limit;
        if config.yes {
            settings.ui.permission_mode = PermissionMode::AlwaysApprove;
        }
        let initial_tokens = session.as_ref().map(|session| session.tokens_used).unwrap_or(0);
        let session_initial_active_secs =
            session.as_ref().map(|session| session.active_secs).unwrap_or(0);
        let tokens = Arc::new(crate::provider::TokenLedger::new(initial_tokens));
        let provider = Provider::with_tokens(&config, tokens.clone())?;
        let aux_provider = provider.for_role(config.aux_model.as_deref());
        let hive = crate::hive::HiveHandle::load(config.paths.hive_file.clone());
        let modes = crate::modes::ModeCoach::load(config.paths.modes_file.clone());
        let ralph_loop = session.as_ref().and_then(|session| session.ralph_loop.clone());
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
            entries.push(Entry::new(EntryKind::Error, format!("Extension warning: {diagnostic}")));
        }
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (background_tx, background_rx) = mpsc::unbounded_channel();
        // Detached, so a slow or unreachable GitHub never delays the first
        // frame, and silent on failure — being offline is not a problem worth
        // reporting.
        // `try_current` rather than `spawn`: the app is also constructed in
        // synchronous tests, where there is no reactor to spawn onto.
        if settings.ui.check_updates
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let cache = config.paths.update_file.clone();
            let updates = background_tx.clone();
            runtime.spawn(async move {
                if let Ok(Some(available)) =
                    crate::update::check(&cache, env!("CARGO_PKG_VERSION")).await
                {
                    let _ = updates.send(Background::Update(available));
                }
            });
        }
        let yes = config.yes;
        let branch = git_branch(&config.workspace);
        let papercuts = crate::papercuts::PapercutStore::load(
            config.paths.papercuts_file.clone(),
            &config.workspace,
        );
        let state = SessionState::open(&config, session.as_ref(), session_key(session.as_ref()));
        if let Some(store) = &session_store
            && let Ok(summaries) = store.list()
        {
            for summary in summaries {
                // Still titled "New session" with at most the system prompt:
                // a screen that was opened but never used.
                if summary.title == "New session"
                    && summary.message_count <= 1
                    && let Ok(session) = store.load(&summary.id.to_string())
                    && crate::sync::is_placeholder(&session)
                {
                    let _ = std::fs::remove_file(store.path_for(summary.id));
                    let _ = std::fs::remove_file(
                        config.paths.traces_dir.join(format!("{}.jsonl", summary.id)),
                    );
                }
            }
        }
        let resume_from_id: Option<String> = if session.is_some() {
            None
        } else if let Some(store) = &session_store
            && let Ok(summaries) = store.list()
            && summaries
                .iter()
                .any(|summary| summary.message_count > 1 || summary.title != "New session")
        {
            store.latest().ok().map(|session| session.id.to_string())
        } else {
            None
        };
        if let Some(resume_id) = &resume_from_id {
            entries.push(Entry::new(
                EntryKind::System,
                format!("Continue your last session? Press F12 or /resume {resume_id}."),
            ));
        }
        Ok(Self {
            config,
            settings,
            credentials,
            provider,
            aux_provider,
            messages,
            session,
            session_store: session_store.clone(),
            services,
            state,
            papercuts,
            hive,
            modes,
            mouse_captured: true,
            hive_overlay: false,
            hive_scroll: 0,
            ralph_loop,
            entries,
            input: InputBuffer::new(),
            mode: InputMode::Insert,
            running: None,
            event_tx,
            event_rx,
            background_tx,
            background_rx,
            approval: None,
            approval_scroll: 0,
            approval_horizontal: 0,
            question: None,
            picker: None,
            usage_panel: None,
            config_panel: None,
            raw_config: None,
            feedback_form: None,
            model_hub: None,
            hub_rows: std::cell::Cell::new(1),
            catalogs: HashMap::new(),
            pointer: None,
            remote_task: None,
            remote_outbound: None,
            sync_idle_since: None,
            last_auto_push: None,
            last_board_version: 0,
            reload_services: false,
            services_reloading: false,
            refining: false,
            safety: crate::safety::SafetyCache::default(),
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
        // Any new block closes an open stream. Streaming appends to the *last*
        // entry, so anything pushed mid-turn — a steering message, a notice, a
        // side note — would otherwise swallow the tokens that came after it:
        // the user's own card ending in the model's reasoning. The stream
        // handlers set these flags again after pushing their own block, so
        // resetting here only affects blocks that came from somewhere else.
        self.receiving_delta = false;
        self.receiving_thinking = false;
        self.entries_rev = self.entries_rev.wrapping_add(1);
        self.entries.push(entry);
    }

    /// Tell the user something, in the transcript, and bring it into view.
    fn say(&mut self, text: impl Into<String>) {
        self.push_entry(Entry::new(EntryKind::System, text));
        self.follow = true;
    }

    /// Report a problem in the transcript and bring it into view.
    fn fail(&mut self, text: impl Into<String>) {
        self.push_entry(Entry::new(EntryKind::Error, text));
        self.follow = true;
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
        let current_fits = self.entries[count - 1].tool.as_ref().is_some_and(explorable);
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
                if target.full.is_empty() { "(no output)" } else { &target.full }
            );
            target.summary = format!("{} {}", explore_verb(&target.name), target.summary);
            target.name = "explored".to_owned();
            // The group header is the content; per-call previews live behind
            // the fold.
            target.output = String::new();
        }
        target.summary = ui::truncate(
            &format!("{} · {} {}", target.summary, explore_verb(&current.name), current.summary),
            200,
        );
        target.full.push_str(&format!(
            "\n\n── {} {} ──\n{}",
            current.name,
            current.summary,
            if current.full.is_empty() { "(no output)" } else { &current.full }
        ));
        let kept = crate::text::prefix(&target.full, ui::MAX_RETAINED_OUTPUT).len();
        target.full.truncate(kept);
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
        if !self.settings.ui.draft_replies
            || self.config.token_compression
            || !self.input.is_empty()
            || self.running.is_some()
        {
            return;
        }
        if let Some(task) = self.draft_task.take() {
            task.abort();
        }
        // The next-message recommendation is a secondary call — use the aux
        // model so a heavy main model does not pay for a throwaway guess.
        let provider = self.aux_provider.clone();
        let messages = self.messages.clone();
        let sender = self.background_tx.clone();
        self.draft_task = Some(tokio::spawn(async move {
            let draft = crate::agent::draft_reply(&provider, &messages).await;
            let _ = sender.send(Background::Draft(draft));
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

    fn drain_agent_events(&mut self) -> bool {
        let mut changed = false;
        loop {
            let event = match self.event_rx.try_recv() {
                Ok(event) => event,
                Err(mpsc::error::TryRecvError::Empty) => break,
                // The turn task is gone without a final event — a panic in a
                // background spawn is the usual way. Left alone, `running`
                // would stay Some forever: the spinner would spin, every new
                // prompt would be parked as "steering", and a finished
                // subagent's report would never trigger its turn.
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    if self.running.take().is_some() {
                        self.fail(
                            "The turn ended unexpectedly (the agent task exited). \
                             Any partial output was saved to the recovery file.",
                        );
                        changed = true;
                    }
                    break;
                }
            };
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
                    // Mirrored where a panic or a signal can still reach it.
                    crate::recovery::record_answer(&delta);
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
                    let required = self.credentials.sync.is_some();
                    self.fail(if required {
                        format!("Sync trace failed — session sync is paused: {error}")
                    } else {
                        format!("Training trace disabled — {error}")
                    });
                }
                AgentEvent::Notice(notice) => {
                    self.say(notice);
                }
                AgentEvent::Reasoning(piece) => {
                    self.turn_output_chars = self.turn_output_chars.saturating_add(piece.len());
                    // The status header follows the reasoning even when the
                    // reasoning itself is hidden — it is the footer's job to
                    // say what the model is doing either way.
                    self.turn_reasoning.push_str(&piece);
                    crate::recovery::record_thinking(&piece);
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
                    self.status = format!("running {name}");
                    self.push_entry(Entry::tool(ToolCall::running(name, summary)));
                }
                AgentEvent::ToolFinished { name, output } => {
                    self.receiving_delta = false;
                    // The full tool result (not the preview) lands in the
                    // messages array; estimate its JSON size for the live ctx %.
                    self.ctx_chars = self.ctx_chars.saturating_add(output.len() + name.len() + 80);
                    let duration_ms = self
                        .tool_started
                        .take()
                        .map(|started| started.elapsed().as_millis() as u64);
                    // Settle the row the matching `ToolStarted` opened, keeping
                    // the argument summary it already shows rather than
                    // replacing the row wholesale.
                    if self.open_tool().is_none() {
                        self.push_entry(Entry::tool(ToolCall::running(name, "")));
                    }
                    if let Some(call) = self.open_tool() {
                        settle(call, &output, duration_ms);
                    }
                    self.group_exploration();
                    self.status = "thinking".to_owned();
                }
                AgentEvent::ModeChanged { mode, reason } => {
                    self.resolved_agent_mode = Some(mode);
                    self.say(format!("{} mode — {reason}", mode.label()));
                    self.status = format!("{} mode", mode.label().to_ascii_lowercase());
                }
                AgentEvent::Done { messages, reason } => {
                    let assistant_output = crate::text::last_reply(&messages).to_owned();
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
                    if let Some(remote) = &self.remote_outbound {
                        let _ = remote.send(assistant_output.clone());
                    }
                    if !continue_loop {
                        self.sync_idle_since = Some(Instant::now());
                    }
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
                            self.say("Interrupted.");
                            self.status = "interrupted".to_owned();
                        }
                        DoneReason::StepLimit => {
                            self.say(format!(
                                "Stopped after {} steps — the step limit for one turn. \
                                     Send another message to continue, or raise \
                                     `Maximum agent steps` in /config.",
                                self.config.max_steps
                            ));
                            self.status = "step limit reached".to_owned();
                        }
                    }
                    self.close_turn();
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
                    self.close_turn();
                    self.messages = messages;
                    self.ctx_chars = message_chars(&self.messages);
                    self.last_outcome = Some(TurnOutcome::Failed);
                    self.approval = None;
                    // Provider rejections are very often not transient: an
                    // interrupted turn leaves history that strict providers
                    // refuse on every retry. Point at the way out.
                    let provider_rejection = error.contains("provider stream error")
                        || error.contains("provider returned");
                    if let Some(remote) = &self.remote_outbound {
                        let _ = remote.send(format!("Error: {error}"));
                    }
                    self.fail(error);
                    if provider_rejection {
                        self.say(
                            "If this error repeats, the session history may be corrupted — \
                             run /repair to check and fix it.",
                        );
                    }
                    self.status = "error".to_owned();
                    if let Some(state) = &mut self.ralph_loop {
                        let _ = state.pause();
                    }
                    self.persist_session();
                    self.sync_idle_since = Some(Instant::now());
                    self.follow = true;
                }
            }
        }
        // Watchdog: a turn task that ended WITHOUT a Done/Failed event (a panic
        // in a spawned tool task, most often) leaves `running` stuck. The
        // channel never disconnects because the App holds a sender, so the
        // finished handle with an empty event queue is the only signal.
        if let Some(handle) = &self.running
            && handle.is_finished()
            && matches!(self.event_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty))
        {
            self.close_turn();
            self.fail(
                "The turn ended unexpectedly. Partial output was kept — \
                 send a message to continue.",
            );
            self.status = "turn ended unexpectedly".to_owned();
            self.persist_session();
            self.sync_idle_since = Some(Instant::now());
            changed = true;
        }
        changed
    }

    /// The turn is over, however it ended: it reported back, so there is no
    /// half-finished reply to recover, and nothing of it is still in flight.
    fn close_turn(&mut self) {
        crate::recovery::clear();
        self.running = None;
        self.turn_started = None;
        self.tool_started = None;
        self.resolved_agent_mode = None;
        self.receiving_delta = false;
        self.receiving_thinking = false;
    }

    /// A termination signal arrived; the loop should wind down gracefully.
    /// Called on the UI thread, which notices within one poll tick (~150ms).
    fn pending_signal(&self) -> bool {
        PENDING_SIGNAL.load(Ordering::SeqCst) != 0
    }

    /// True when the worker board changed since the last draw. The strip must
    /// update while no turn is running — during that window nothing else marks
    /// the frame dirty, so background swarms looked frozen between keystrokes.
    fn board_changed(&mut self) -> bool {
        let version = self.hive.board.version();
        std::mem::replace(&mut self.last_board_version, version) != version
    }

    fn set_approval(&mut self, request: ApprovalRequest) {
        self.overlay_hidden = false;
        self.status = format!("approval needed: {}", request.tool);
        let diff = DiffDocument::parse(&request.details);
        self.approval = Some(PendingApproval {
            tool: request.tool,
            summary: request.summary,
            details: request.details,
            view: if diff.is_some() { ApprovalView::Unified } else { ApprovalView::Raw },
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
                let prompt = self.input.take();
                self.record_history(prompt.trim());
                self.steer(prompt.trim().to_owned());
            }
            return;
        }
        let prompt = self.input.take();
        let prompt = prompt.trim().to_owned();
        self.record_history(&prompt);
        self.submit_prompt(prompt);
    }

    /// Hand a message to the running turn, which picks it up after its
    /// current tool call. Steering, not queueing: a correction that waits for
    /// the whole turn to end arrives too late to change what it was correcting.
    fn steer(&mut self, prompt: String) {
        self.push_entry(Entry::new(EntryKind::User, prompt.clone()));
        self.state.injections.push(crate::agent::Injection::UserMessage(prompt));
        self.follow = true;
        self.status = "steering · delivered after the current step".to_owned();
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
            let skills = self.services.skills.read().expect("skill registry lock");
            skills.get(name).map(|_| skills.invocation(name, argument)).or_else(|| {
                self.services
                    .plugins
                    .command(name)
                    .map(|plugin_command| Ok(plugin_command.prompt.replace("{{args}}", argument)))
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
        let Some(entry_index) =
            self.entries.iter().rposition(|entry| entry.kind == EntryKind::User)
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
        self.ctx_chars = self.ctx_chars.saturating_add(crate::agent::message_chars_one(&message));
        self.messages.push(message);
        self.persist_session();
        // The session now exists (first prompt creates it), so recovery can
        // name it — the startup arm only knew the id for resumed sessions.
        if let Some(session) = &self.session {
            crate::recovery::set_session(Some(session.id.to_string()));
        }
        self.sync_idle_since = None;
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
            safety: self.safety.clone(),
            safety_uses_main: self.settings.ui.safety_uses_main,
            trace: self.trace.clone(),
            cancel: self.cancel.clone(),
            mode: agent_mode,
            allow_mutations,
            session_id: self.session.as_ref().map(|session| session.id.to_string()),
            papercuts: self.papercuts.clone(),
            hive: self.hive.clone(),
            modes: self.modes.clone(),
            ..self.state.turn(&self.config, self.services.clone())
        };
        self.running = Some(tokio::spawn(async move {
            run_turn(provider, messages, options, events).await;
        }));
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
        let Some(session) = &mut self.session else {
            return;
        };
        session.update_messages(self.messages.clone());
        self.state.save(session);
        session.ralph_loop = self.ralph_loop.clone();
        session.tokens_used = self.provider.tokens_used();
        session.active_secs =
            self.session_initial_active_secs.saturating_add(self.started.elapsed().as_secs());
        // A fresh screen that has never received a prompt is not a real session
        // yet — keep it out of storage and out of the trace set. The first
        // `start_turn` persists immediately, before calling the model.
        if crate::sync::is_placeholder(session) {
            return;
        }
        // The trace is keyed by session id, so it can only be opened once the
        // session exists — which is here, on the first real persist.
        if self.trace.is_none() && (self.config.trace_enabled || self.credentials.sync.is_some()) {
            match crate::sft::TraceWriter::open(
                &self.config.paths.traces_dir,
                &session.id.to_string(),
            ) {
                Ok(writer) => self.trace = Some(writer),
                Err(error) => self.status = format!("training trace disabled: {error:#}"),
            }
        }
        if let Err(error) = store.save(session) {
            self.status = format!("session save failed: {error}");
        }
    }

    fn maybe_idle_sync(&mut self) {
        if self.running.is_some() || !crate::sync::is_configured(&self.credentials) {
            return;
        }
        let Some(idle_since) = self.sync_idle_since else {
            return;
        };
        if idle_since.elapsed() < crate::sync::AUTO_PUSH_IDLE {
            return;
        }
        if self.last_auto_push.is_some_and(|pushed| pushed.elapsed() < crate::sync::AUTO_PUSH_IDLE)
        {
            return;
        }
        self.last_auto_push = Some(Instant::now());
        self.sync_idle_since = None;
        if let Some(session) = &self.session {
            crate::sync::spawn_session_sync(&self.config.paths, session);
        }
    }

    /// Point the app at `session`, or at a blank conversation, with the state
    /// that travels with one. Steering and worker reports already in flight
    /// stay queued: they belong to the process, not to a session.
    fn adopt(&mut self, session: Option<Session>) {
        let injections = self.state.injections.clone();
        let state =
            SessionState::open(&self.config, session.as_ref(), session_key(session.as_ref()));
        self.state = SessionState { injections, ..state };
        self.messages = session
            .as_ref()
            .map_or_else(|| initial_messages(&self.config.workspace), |s| s.messages.clone());
        self.ctx_chars = message_chars(&self.messages);
        self.set_entries(entries_from_messages(&self.messages));
        self.ralph_loop = session.as_ref().and_then(|session| session.ralph_loop.clone());
        self.tokens.store_total(session.as_ref().map_or(0, |session| session.tokens_used));
        self.session_initial_active_secs = session.as_ref().map_or(0, |s| s.active_secs);
        self.started = Instant::now();
        self.session = session; // `None` is recreated lazily on the first send
        self.scroll = 0;
    }

    fn new_session(&mut self) {
        self.persist_session();
        self.adopt(None);
        self.say("New session.");
    }

    /// Reopen the conversation as a new session — a fork. The original session
    /// is left on disk untouched; the fork takes over the transcript with
    /// "(fork)" in its title so `/sessions` shows where it came from. What is
    /// in the composer stays put: fork first, then edit and send it in the new
    /// branch.
    fn fork_session(&mut self) {
        if self.running.is_some() {
            self.status = "finish or interrupt the turn before forking".to_owned();
            return;
        }
        if self.session.is_none() {
            self.status = "nothing to fork yet — send a message first".to_owned();
            return;
        }
        self.persist_session(); // save the original before switching away from it
        let fork = {
            let Some(store) = &self.session_store else {
                self.status = "sessions are disabled".to_owned();
                return;
            };
            let Some(current) = &self.session else {
                self.status = "nothing to fork yet — send a message first".to_owned();
                return;
            };
            let mut fork = match store.create(
                self.config.profile.clone(),
                self.config.model.clone(),
                self.messages.clone(),
            ) {
                Ok(fork) => fork,
                Err(error) => {
                    self.status = format!("fork failed: {error}");
                    return;
                }
            };
            fork.title = format!("(fork) {}", current.title).chars().take(100).collect();
            // A fork is a branch of the work, so the session state that shapes
            // it carries over; time and token accounting start fresh.
            fork.goal = current.goal.clone();
            fork.ralph_loop = current.ralph_loop.clone();
            fork.tasks = current.tasks.clone();
            fork.compaction = current.compaction.clone();
            fork.intent = current.intent.clone();
            if let Err(error) = store.save(&fork) {
                self.status = format!("fork failed: {error}");
                return;
            }
            fork
        };
        self.session = Some(fork);
        // Traces are keyed by session id; drop the writer so the next persist
        // reopens it for the fork instead of appending to the original's trace.
        self.trace = None;
        self.tokens.store_total(0);
        self.session_initial_active_secs = 0;
        self.started = Instant::now();
        self.say(
            "Session forked — the conversation continues in a new session; the \
             original is still saved.",
        );
        self.follow = true;
        self.status = "session forked — the original is untouched".to_owned();
    }

    fn list_sessions(&mut self) {
        let Some(store) = &self.session_store else {
            self.status = "sessions are disabled".to_owned();
            return;
        };
        match store.list() {
            Ok(sessions) if sessions.is_empty() => {
                self.say("No saved sessions for this workspace.")
            }
            Ok(sessions) => {
                self.picker = Some(Picker {
                    title: "sessions".to_owned(),
                    action: PickerAction::ResumeSession,
                    prompt: None,
                    items: sessions
                        .into_iter()
                        .take(50)
                        .map(|session| {
                            (
                                format!(
                                    "{}  {}  {}",
                                    &session.id.to_string()[..8],
                                    session.updated_at.with_timezone(&Local).format("%m-%d %H:%M"),
                                    session.title
                                ),
                                session.id.to_string(),
                            )
                        })
                        .collect(),
                    selected: 0,
                });
            }
            Err(error) => self.fail(format!("Could not list sessions: {error}")),
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
                let resumed =
                    format!("Resumed {} ({})", session.title, &session.id.to_string()[..8]);
                self.adopt(Some(session));
                self.say(resumed);
                self.status = "ready".to_owned();
            }
            Err(error) => self.fail(format!("Could not resume session: {error}")),
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

    fn open_feedback(&mut self) {
        if !self.settings.feedback.enabled {
            self.fail("Feedback is disabled. Enable it in /config.");
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
            diagnostics: if include_diagnostics { self.services.diagnostics() } else { Vec::new() },
            session_id: self.session.as_ref().map(|session| session.id.to_string()),
            workspace: self.config.workspace_name().to_owned(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
        };
        let endpoint = self.settings.feedback.endpoint.clone();
        let sender = self.background_tx.clone();
        tokio::spawn(async move {
            let result = match crate::feedback::FeedbackClient::new(&endpoint) {
                Ok(client) => client.submit(&payload).await.map_err(|error| format!("{error:#}")),
                Err(error) => Err(format!("{error:#}")),
            };
            let _ = sender.send(Background::Feedback(result));
        });
    }

    /// `/remote`: share this session through Abacus Sync, or stop sharing it.
    fn toggle_remote(&mut self) {
        self.persist_session();
        let Some(session) = self.session.clone() else {
            self.status = "send a message before enabling remote".to_owned();
            return;
        };
        let client = crate::sync::configured_client(&self.config.paths);
        if let Some(task) = self.remote_task.take() {
            task.abort();
            self.remote_outbound = None;
            if let Ok(client) = client {
                tokio::spawn(async move {
                    let _ = client.disable_remote(&session.id.to_string()).await;
                });
            }
            self.status = "remote disabled".to_owned();
            return;
        }
        let Ok(client) = client else {
            self.status = "run `abacus sync login` before /remote".to_owned();
            return;
        };
        let (outbound, replies) = mpsc::unbounded_channel();
        self.remote_outbound = Some(outbound);
        self.config.trace_enabled = true;
        self.persist_session();
        self.status = "enabling remote".to_owned();
        // The transcript so far, so the browser opens on the history rather
        // than on a blank page.
        let snapshot = self
            .entries
            .iter()
            .map(|entry| {
                let kind = match entry.kind {
                    EntryKind::User => "user",
                    EntryKind::Assistant => "assistant",
                    EntryKind::Tool => "tool",
                    EntryKind::Thinking => "thinking",
                    EntryKind::System | EntryKind::Rule => "system",
                    EntryKind::Error => "error",
                };
                json!({"kind": kind, "text": entry.text, "tool": entry.tool})
            })
            .collect();
        let trace = self.config.paths.traces_dir.join(format!("{}.jsonl", session.id));
        let events = self.background_tx.clone();
        self.remote_task = Some(tokio::spawn(async move {
            let served = serve_remote(client, session, trace, snapshot, replies, &events).await;
            if let Err(error) = served {
                let _ = events.send(Background::Remote(Remote::Closed(format!("{error:#}"))));
            }
        }));
    }

    fn start_services_reload(&mut self) {
        if !self.reload_services || self.services_reloading || self.running.is_some() {
            return;
        }
        self.reload_services = false;
        self.services_reloading = true;
        self.status = "reloading extensions".to_owned();
        let (workspace, paths) = (self.config.workspace.clone(), self.config.paths.clone());
        let settings = self.settings.clone();
        let events = self.background_tx.clone();
        tokio::spawn(async move {
            let result = AgentServices::discover(&workspace, &paths, &settings)
                .await
                .map(Box::new)
                .map_err(|error| format!("{error:#}"));
            let _ = events.send(Background::Services(result));
        });
    }

    /// Hand back a reply an earlier run died in the middle of. Shown once —
    /// `take` removes the file, so the transcript and the session now hold
    /// the only copy, which is where the user and the model will look.
    fn surface_recovered_reply(&mut self) {
        let Some(content) = crate::recovery::take(&self.config.paths.recovery_file) else {
            return;
        };
        self.say("A previous run stopped mid-reply. This is how far the model got:");
        self.push_entry(Entry::new(EntryKind::Assistant, content.trim()));
        self.messages.push(json!({"role": "assistant", "content": content.trim()}));
        self.persist_session();
    }

    /// Apply everything background tasks have posted since the last frame.
    fn drain_background(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.background_rx.try_recv() {
            changed = true;
            match event {
                // Discard a draft that arrived after the user started typing.
                Background::Draft(draft) => {
                    self.draft = draft.filter(|_| self.input.is_empty() && self.running.is_none());
                }
                Background::Catalog { profile, result } => {
                    use crate::model_hub::Catalog;
                    let state = result.map_or_else(Catalog::Failed, Catalog::Ready);
                    self.catalogs.insert(profile, state);
                }
                // A notice, not a prompt: nothing is downloaded or blocked.
                Background::Update(available) => self.say(available.message()),
                Background::Refined(message) => {
                    self.refining = false;
                    self.say(message);
                }
                Background::Feedback(Ok(receipt)) => {
                    self.feedback_form = None;
                    let reference =
                        receipt.id.map(|id| format!(" Reference: {id}.")).unwrap_or_default();
                    self.say(format!("Thank you — your feedback was sent.{reference}"));
                    self.status = "feedback sent".to_owned();
                }
                Background::Feedback(Err(error)) => {
                    if let Some(form) = &mut self.feedback_form {
                        form.sending = false;
                        form.error = Some(format!(
                            "Could not send feedback: {error}\nThe endpoint is a placeholder until the Empero API is available."
                        ));
                    }
                }
                Background::Services(result) => {
                    self.services_reloading = false;
                    match result {
                        Ok(services) => {
                            self.services = Arc::new(*services);
                            self.status = "configuration active".to_owned();
                        }
                        Err(error) => {
                            self.fail(format!(
                                "Configuration saved, but extensions could not reload: {error}"
                            ));
                            self.status = "extension reload failed".to_owned();
                        }
                    }
                }
                Background::Remote(event) => self.remote_event(event),
            }
        }
        changed
    }

    fn remote_event(&mut self, event: Remote) {
        match event {
            Remote::Status(status) => self.status = status,
            Remote::Closed(reason) => {
                self.status = reason;
                self.remote_task = None;
                self.remote_outbound = None;
            }
            Remote::Interrupt => {
                if let Some(handle) = self.running.take() {
                    handle.abort();
                    self.status = "interrupted via remote".to_owned();
                }
            }
            Remote::Prompt(prompt) => {
                // The browser echoing the last reply back must not start a loop.
                let echoed = self
                    .entries
                    .iter()
                    .rev()
                    .find(|entry| entry.kind == EntryKind::Assistant)
                    .is_some_and(|entry| entry.text.trim() == prompt.trim());
                if echoed {
                } else if self.running.is_some() {
                    self.steer(prompt);
                } else {
                    self.start_turn(prompt.clone(), prompt, true);
                }
            }
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
            let elapsed = self.tool_started.map(|started| started.elapsed().as_millis() as u64);
            if let Some(call) = self.open_tool().filter(|call| call.status == ToolStatus::Running) {
                call.status = ToolStatus::Failed;
                call.output = "interrupted".to_owned();
                call.duration_ms = elapsed;
            }
            self.say("Interrupted.");
            self.status = "interrupted".to_owned();
            self.last_outcome = Some(TurnOutcome::Interrupted);
            self.follow = true;
        }
        self.turn_started = None;
        self.tool_started = None;
        self.persist_session();
    }

    /// F12: pick up the most recent session that was actually used, or offer
    /// the list when there is none.
    fn resume_latest(&mut self) {
        let used =
            self.session_store.as_ref().and_then(|store| store.list().ok()).and_then(|sessions| {
                sessions
                    .into_iter()
                    .filter(|session| session.message_count > 1 || session.title != "New session")
                    .max_by_key(|session| session.updated_at)
            });
        match used {
            Some(session) => self.resume_session(&session.id.to_string()),
            None => self.list_sessions(),
        }
    }

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

    /// A background subagent that finished after its turn ended still has a
    /// report to deliver. Start a turn to hand it over, the same way a running
    /// turn would have picked it up between tool calls.
    fn deliver_pending_injections(&mut self) -> bool {
        if self.running.is_some() || self.state.injections.is_empty() {
            return false;
        }
        let pending = self.state.injections.drain();
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
                        self.say(format!("Side note not delivered — the turn ended first: {note}"));
                        delivered = true;
                    }
                    crate::agent::Injection::SubagentReport(_) => {}
                }
                continue;
            };
            self.say("A background subagent finished.");
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
