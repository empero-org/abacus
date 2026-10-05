//! Live sharing of an interactive session with the account's browsers.
//!
//! The [`Bridge`] is owned by the terminal UI. It turns what the agent does
//! ([`AgentEvent`]s, prompts typed in the terminal, approvals and questions)
//! into protocol frames and hands them to a background [`link`] task that owns
//! the WebSocket. What browsers send comes back as [`Inbound`] events, which
//! the terminal handles through the same code paths as its own keyboard: the
//! terminal stays the only thing that executes anything, and a browser can
//! reach nothing — workspace, model, configuration — that has no frame type.

pub(crate) mod link;
pub mod protocol;
pub mod qr;

use crate::{
    agent::{AgentEvent, AgentMode, ApprovalDecision, DoneReason as TurnEnd},
    config::AbacusPaths,
    provider::TokenUsage,
    session::Session,
    sync::{SyncClient, SyncError},
    ui,
};
use link::{Connector, LinkConfig, LinkError, Stop};
use protocol::{
    Accepted, ApprovalFrame, ApprovalResolved, DeltaFrame, DeltaKind, DoneFrame, DoneReason, Entry, EntryFrame,
    EntryKind, ErrorFrame, Level, ModeFrame, NoticeFrame, Outbound, Pending, Phase, QuestionFrame, QuestionResolved,
    SnapshotHead, State, Status, ToolFrame, ToolInfo, ToolPhase, ToolState,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

pub use protocol::{AcceptResult, By, InputKind, SessionInfo, SnapshotReason};

/// Delivers link events to the terminal (in practice, onto its background
/// channel). Called from the link task.
pub type Notify = Arc<dyn Fn(Inbound) + Send + Sync>;

/// What the link reports to the terminal: connection changes, and browser
/// input to act on.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// The socket is open; a snapshot should follow.
    Connected {
        reconnect: bool,
    },
    /// How many browsers are watching.
    Peers {
        browsers: usize,
    },
    /// The connection is down; the link retries after `retry_in`.
    Disconnected {
        reason: String,
        retry_in: Duration,
    },
    /// The link gave up for good (signed out, sharing turned off, another
    /// terminal took the session over).
    Closed {
        reason: String,
    },
    /// The relay complained about something (rate limits, a bad frame).
    Warning {
        code: String,
        message: String,
    },
    Prompt {
        ref_id: String,
        text: String,
    },
    Answer {
        ref_id: String,
        question_id: String,
        selected: Vec<String>,
        custom: Option<String>,
    },
    Approve {
        ref_id: String,
        approval_id: String,
        decision: ApprovalDecision,
    },
    Interrupt {
        ref_id: String,
    },
    SnapshotRequested,
}

/// The connection as the footer badge shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// Uploading, enabling, or connecting for the first time.
    Connecting,
    Live,
    /// Lost after having been live; retrying.
    Reconnecting(String),
    /// Given up; the reason says why.
    Stopped(String),
}

/// What the terminal knows that a snapshot needs.
pub struct SnapshotView<'a> {
    pub session: SessionInfo,
    pub entries: &'a [ui::Entry],
    /// A turn is running, so the transcript's last block may be the one still
    /// streaming.
    pub live: bool,
    pub usage: &'a TokenUsage,
}

/// Frames queued for the link before the bridge starts dropping them and asks
/// for a resync instead. Snapshots are a few pages; a turn's frames drain in
/// milliseconds; this only fills while the network stalls.
const QUEUE: usize = 1024;

/// How long streamed text is held to coalesce into one `delta`. The relay
/// allows an agent 20 frames a second in all; ten for text leaves the rest
/// for tool and status frames while still reading as live.
const DELTA_INTERVAL: Duration = Duration::from_millis(100);

/// The fastest the reasoning-derived status label is re-sent.
const LABEL_INTERVAL: Duration = Duration::from_millis(500);

/// A snapshot carries the most recent transcript up to about this much text;
/// older entries are summarised in one line. A long session would otherwise
/// replay megabytes to a phone on every reconnect.
const SNAPSHOT_BUDGET: usize = 2 * 1024 * 1024;
const SNAPSHOT_MAX_ENTRIES: usize = 2_000;

const MAX_STATUS_LABEL: usize = 200;
const MAX_ERROR: usize = 8 * 1024;

/// The block currently streaming to the browsers.
struct OpenText {
    id: String,
    kind: DeltaKind,
    /// The block so far, up to the entry limit, for `entry{complete}` and for
    /// a snapshot taken mid-stream.
    text: String,
    clipped: bool,
    /// Streamed but not yet sent.
    pending: String,
}

struct OpenTool {
    entry_id: String,
    call_id: String,
    name: String,
    summary: String,
    started: Instant,
}

pub struct Bridge {
    session_id: String,
    frames: mpsc::Sender<Outbound>,
    stop: Option<oneshot::Sender<Stop>>,
    task: Option<JoinHandle<()>>,
    next_entry: u64,
    next_approval: u64,
    next_question: u64,
    next_call: u64,
    text: Option<OpenText>,
    tool: Option<OpenTool>,
    last_delta: Instant,
    status: Status,
    label_sent: Option<Instant>,
    approval: Option<ApprovalFrame>,
    question: Option<QuestionFrame>,
    turn_started: Option<Instant>,
    state: LinkState,
    connected_once: bool,
    browsers: usize,
    /// A frame was dropped because the queue was full; the next tick sends a
    /// snapshot so the browsers repair the gap.
    resync: bool,
}

impl Bridge {
    /// Share `session`: a background task uploads it, enables remote control,
    /// and keeps a socket to the relay open until [`Bridge::stop`].
    pub fn start(client: SyncClient, paths: AbacusPaths, session: Session, notify: Notify) -> Self {
        let session_id = session.id.to_string();
        Self::launch(session_id, SyncConnector { client, paths, session }, notify, LinkConfig::default())
    }

    pub(crate) fn launch<C: Connector>(session_id: String, connector: C, notify: Notify, config: LinkConfig) -> Self {
        let (frames, commands) = mpsc::channel(QUEUE);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(link::run(connector, commands, stopped, notify, config));
        Self { stop: Some(stop), task: Some(task), ..Self::new(session_id, frames) }
    }

    fn new(session_id: String, frames: mpsc::Sender<Outbound>) -> Self {
        Self {
            session_id,
            frames,
            stop: None,
            task: None,
            next_entry: 0,
            next_approval: 0,
            next_question: 0,
            next_call: 0,
            text: None,
            tool: None,
            last_delta: Instant::now(),
            status: Status { state: State::Idle, label: "ready".into(), since: now() },
            label_sent: None,
            approval: None,
            question: None,
            turn_started: None,
            state: LinkState::Connecting,
            connected_once: false,
            browsers: 0,
            resync: false,
        }
    }

    /// A bridge without a link, whose frames land in the returned receiver.
    #[cfg(test)]
    pub(crate) fn detached(session_id: &str) -> (Self, mpsc::Receiver<Outbound>) {
        let (frames, commands) = mpsc::channel(QUEUE);
        (Self::new(session_id.to_owned(), frames), commands)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn state(&self) -> &LinkState {
        &self.state
    }

    pub fn browsers(&self) -> usize {
        self.browsers
    }

    /// Whether the link is still trying; false once it gave up.
    pub fn is_active(&self) -> bool {
        !matches!(self.state, LinkState::Stopped(_))
    }

    /// Fold a link event into the connection state.
    pub fn observe(&mut self, event: &Inbound) {
        match event {
            Inbound::Connected { .. } => {
                self.state = LinkState::Live;
                self.connected_once = true;
            }
            Inbound::Peers { browsers } => self.browsers = *browsers,
            Inbound::Disconnected { reason, .. } => {
                self.browsers = 0;
                if self.connected_once {
                    self.state = LinkState::Reconnecting(reason.clone());
                }
            }
            Inbound::Closed { reason } => {
                self.browsers = 0;
                self.state = LinkState::Stopped(reason.clone());
            }
            _ => {}
        }
    }

    /// End sharing: flush what is queued, tell the browsers `notice`, close
    /// the socket, and with `disable` make the session undiscoverable. The
    /// handle completes when the link has finished all of that.
    pub fn stop(mut self, notice: &str, disable: bool) -> Option<JoinHandle<()>> {
        self.close_text();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(Stop { notice: notice.to_owned(), disable });
        }
        self.task.take()
    }

    /// [`Bridge::stop`] with `disable`, waited for at most `bound` — for the
    /// exit path, where the process is about to end.
    pub async fn shutdown(self, notice: &str, bound: Duration) {
        if let Some(task) = self.stop(notice, true) {
            let _ = tokio::time::timeout(bound, task).await;
        }
    }

    // -----------------------------------------------------------------------
    // Transcript
    // -----------------------------------------------------------------------

    /// A prompt entered in the terminal (or steering a running turn), so the
    /// browsers mirror what was typed.
    pub fn user_prompt(&mut self, text: &str) {
        self.close_text();
        let id = self.entry_id();
        self.emit(Outbound::Entry(EntryFrame {
            phase: Phase::Complete,
            entry: Entry::block(id, EntryKind::User, text),
        }));
    }

    /// A turn began.
    pub fn turn_started(&mut self) {
        self.turn_started = Some(Instant::now());
        self.set_status(State::Thinking, "thinking");
    }

    /// Mirror one agent event. Called before the terminal handles it, so it
    /// only borrows.
    pub fn on_event(&mut self, event: &AgentEvent, usage: &TokenUsage) {
        match event {
            AgentEvent::Delta(text) => self.stream(DeltaKind::Text, text),
            AgentEvent::Reasoning(piece) => self.stream(DeltaKind::Reasoning, piece),
            // The ids are assigned by `approval_opened`/`question_opened`,
            // once the terminal holds the request.
            AgentEvent::Approval(_) | AgentEvent::UserQuestion(_) => self.close_text(),
            AgentEvent::ToolStarted { name, summary } => self.tool_started(name, summary),
            AgentEvent::ToolFinished { name, output } => self.tool_finished(name, output),
            AgentEvent::ModeChanged { mode, reason } => self.mode_changed(*mode, reason),
            AgentEvent::Notice(text) => self.notice(text, Level::Info),
            AgentEvent::TraceFailed { error } => {
                self.notice(&format!("Trace capture stopped: {error}"), Level::Warning)
            }
            AgentEvent::Done { reason, .. } => {
                let reason = match reason {
                    TurnEnd::Complete => DoneReason::Complete,
                    TurnEnd::Interrupted => DoneReason::Interrupted,
                    TurnEnd::StepLimit => DoneReason::StepLimit,
                };
                self.turn_ended(reason, usage);
            }
            AgentEvent::Failed { error, .. } => {
                self.close_text();
                let message = protocol::clip_text(error, MAX_ERROR).0;
                self.emit(Outbound::Error(ErrorFrame { message, fatal: false }));
                self.turn_ended(DoneReason::Failed, usage);
            }
        }
    }

    /// The turn is over, however it ended — including a hard abort, which
    /// never reports `Done`. Settles everything still open so no browser is
    /// left with a spinning tool or an approval nobody can answer.
    pub fn turn_ended(&mut self, reason: DoneReason, usage: &TokenUsage) {
        self.close_text();
        if let Some(open) = self.tool.take() {
            let output = if reason == DoneReason::Interrupted { "interrupted" } else { "" };
            self.emit(Outbound::Tool(ToolFrame {
                entry_id: open.entry_id,
                phase: ToolPhase::Finish,
                call_id: open.call_id,
                name: open.name,
                summary: open.summary,
                status: ToolState::Failed,
                output: output.into(),
                output_clipped: false,
                duration_ms: Some(millis(open.started)),
            }));
        }
        if let Some(approval) = self.approval.take() {
            self.emit(Outbound::ApprovalResolved(ApprovalResolved {
                approval_id: approval.approval_id,
                decision: protocol::Resolution::Reject,
                by: By::Terminal,
            }));
        }
        if let Some(question) = self.question.take() {
            self.emit(Outbound::QuestionResolved(QuestionResolved {
                question_id: question.question_id,
                selected: Vec::new(),
                custom: None,
                by: By::Terminal,
            }));
        }
        let elapsed_ms = self.turn_started.take().map_or(0, millis);
        self.emit(Outbound::Done(DoneFrame { reason, elapsed_ms, usage: usage.into() }));
        let label = match reason {
            DoneReason::Complete => "ready",
            DoneReason::Interrupted => "interrupted",
            DoneReason::StepLimit => "step limit reached",
            DoneReason::Failed => "error",
        };
        self.set_status(State::Idle, label);
    }

    /// The terminal put something between streamed blocks (a notice, an
    /// error); the next streamed text starts a new block, as it does there.
    pub fn break_text(&mut self) {
        self.close_text();
    }

    /// The footer's live label while the model thinks, mirrored at most every
    /// half second.
    pub fn thinking_label(&mut self, label: &str) {
        if self.status.state != State::Thinking || label.trim().is_empty() || self.status.label == label {
            return;
        }
        if self.label_sent.is_some_and(|sent| sent.elapsed() < LABEL_INTERVAL) {
            return;
        }
        self.set_status(State::Thinking, label);
    }

    pub fn mode_changed(&mut self, mode: AgentMode, reason: &str) {
        let mode = mode.label().to_ascii_lowercase();
        self.emit(Outbound::Mode(ModeFrame { mode, reason: protocol::clip_text(reason, MAX_ERROR).0 }));
    }

    pub fn notice(&mut self, text: &str, level: Level) {
        self.close_text();
        self.emit(Outbound::Notice(NoticeFrame::new(text, level)));
    }

    /// An approval dialog opened; returns its id for the terminal to keep.
    pub fn approval_opened(&mut self, tool: &str, summary: &str, details: &str) -> String {
        self.close_text();
        self.next_approval += 1;
        let id = format!("a{}", self.next_approval);
        let frame = ApprovalFrame::new(id.clone(), tool, summary, details);
        self.approval = Some(frame.clone());
        self.emit(Outbound::Approval(frame));
        self.set_status(State::WaitingApproval, &format!("approval needed: {tool}"));
        id
    }

    pub fn approval_resolved(&mut self, id: &str, decision: ApprovalDecision, by: By) {
        if self.approval.as_ref().is_some_and(|approval| approval.approval_id == id) {
            self.approval = None;
        }
        self.emit(Outbound::ApprovalResolved(ApprovalResolved {
            approval_id: id.to_owned(),
            decision: decision.into(),
            by,
        }));
        self.set_status(State::Thinking, "thinking");
    }

    /// A question dialog opened; returns its id for the terminal to keep.
    pub fn question_opened(&mut self, header: &str, text: &str, options: &[String], multi: bool) -> String {
        self.close_text();
        self.next_question += 1;
        let id = format!("q{}", self.next_question);
        let frame = QuestionFrame::new(id.clone(), header, text, options, multi);
        self.question = Some(frame.clone());
        self.emit(Outbound::Question(frame));
        let label = if header.trim().is_empty() { "waiting for an answer" } else { header };
        self.set_status(State::WaitingAnswer, label);
        id
    }

    pub fn question_resolved(&mut self, id: &str, selected: &[String], custom: Option<&str>, by: By) {
        if self.question.as_ref().is_some_and(|question| question.question_id == id) {
            self.question = None;
        }
        self.emit(Outbound::QuestionResolved(QuestionResolved {
            question_id: id.to_owned(),
            selected: selected.to_vec(),
            custom: custom.map(str::to_owned),
            by,
        }));
        self.set_status(State::Thinking, "thinking");
    }

    /// Tell the browsers what became of their input.
    pub fn accepted(&mut self, ref_id: &str, kind: InputKind, result: AcceptResult, reason: Option<&str>) {
        self.emit(Outbound::Accepted(Accepted {
            ref_id: ref_id.to_owned(),
            kind,
            result,
            reason: reason.map(str::to_owned),
        }));
    }

    /// Send the whole transcript, paged. Entry ids are fresh except for the
    /// block still streaming, which keeps the id its deltas use.
    pub fn send_snapshot(&mut self, view: SnapshotView<'_>, reason: SnapshotReason) {
        self.flush_delta();
        self.resync = false;
        let open_text = self.text.as_ref().map(|open| (open.id.clone(), open.kind));
        let open_tool = self.tool.as_ref().map(|open| (open.entry_id.clone(), open.call_id.clone()));
        let start = window_start(view.entries);
        let mut entries = Vec::with_capacity(view.entries.len() - start + 2);
        if start > 0 {
            let id = self.entry_id();
            let text = format!(
                "{start} earlier entr{} not shown live; the full transcript is under Sessions.",
                if start == 1 { "y is" } else { "ies are" }
            );
            entries.push(Entry::block(id, EntryKind::System, &text));
        }
        let mut text_placed = false;
        let last = view.entries.len().saturating_sub(1);
        for (index, entry) in view.entries.iter().enumerate().skip(start) {
            let tail = view.live && index == last;
            let streaming = open_text.as_ref().filter(|(_, kind)| tail && matches_stream(entry.kind, *kind));
            let running = open_tool
                .as_ref()
                .filter(|_| tail && entry.tool.as_ref().is_some_and(|call| call.status == ui::ToolStatus::Running));
            let (id, call_id) = match (streaming, running) {
                (Some((id, _)), _) => {
                    text_placed = true;
                    (id.clone(), None)
                }
                (None, Some((id, call_id))) => (id.clone(), Some(call_id.clone())),
                (None, None) => (self.entry_id(), None),
            };
            let call_id = match (entry.kind, call_id) {
                (ui::EntryKind::Tool, None) => Some(self.call_id()),
                (_, call_id) => call_id,
            };
            entries.push(convert(entry, id, call_id));
        }
        // Reasoning streams to the browsers even when the terminal hides it,
        // so the block may exist only here.
        if !text_placed && let Some(open) = &self.text {
            let kind = if open.kind == DeltaKind::Text { EntryKind::Assistant } else { EntryKind::Reasoning };
            let mut entry = Entry::block(open.id.clone(), kind, &open.text);
            entry.clipped |= open.clipped;
            entries.push(entry);
        }
        let head = SnapshotHead {
            snapshot_id: uuid::Uuid::new_v4().to_string(),
            reason,
            session: view.session,
            status: self.status.clone(),
            pending: Pending { approval: self.approval.clone(), question: self.question.clone() },
            usage: view.usage.into(),
        };
        for page in protocol::paginate(head, entries, protocol::TARGET_FRAME_BYTES) {
            self.push(Outbound::Snapshot(Box::new(page)));
        }
    }

    /// Called every event-loop tick: send coalesced text that has waited
    /// long enough.
    pub fn flush(&mut self) {
        if self.last_delta.elapsed() >= DELTA_INTERVAL {
            self.flush_delta();
        }
    }

    /// Whether frames were dropped and a snapshot should repair the gap.
    /// Holds off while the queue is still nearly full.
    pub fn needs_resync(&self) -> bool {
        self.resync && self.frames.capacity() > QUEUE / 2
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    fn stream(&mut self, kind: DeltaKind, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.text.as_ref().is_some_and(|open| open.kind != kind) {
            self.close_text();
        }
        if self.text.is_none() {
            let id = self.entry_id();
            let entry_kind = if kind == DeltaKind::Text { EntryKind::Assistant } else { EntryKind::Reasoning };
            let entry = Entry { id: id.clone(), kind: entry_kind, text: String::new(), clipped: false, tool: None };
            self.emit(Outbound::Entry(EntryFrame { phase: Phase::Start, entry }));
            self.text = Some(OpenText { id, kind, text: String::new(), clipped: false, pending: String::new() });
            if self.status.state != State::Thinking {
                self.set_status(State::Thinking, "thinking");
            }
        }
        let Some(open) = self.text.as_mut() else {
            return;
        };
        open.pending.push_str(text);
        let room = protocol::MAX_ENTRY_TEXT.saturating_sub(open.text.len());
        if text.len() <= room {
            open.text.push_str(text);
        } else {
            open.text.push_str(crate::text::prefix(text, room));
            open.clipped = true;
        }
        if open.pending.len() >= protocol::MAX_DELTA {
            self.flush_delta();
        }
    }

    fn flush_delta(&mut self) {
        let Some(open) = self.text.as_mut() else {
            return;
        };
        if open.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut open.pending);
        let (entry_id, kind) = (open.id.clone(), open.kind);
        let mut rest = pending.as_str();
        while !rest.is_empty() {
            let chunk = crate::text::prefix(rest, protocol::MAX_DELTA);
            // A first character wider than the limit cannot happen (a char
            // is at most four bytes), but never loop on an empty chunk.
            let chunk = if chunk.is_empty() { rest } else { chunk };
            self.push(Outbound::Delta(DeltaFrame { entry_id: entry_id.clone(), kind, text: chunk.to_owned() }));
            rest = &rest[chunk.len()..];
        }
        self.last_delta = Instant::now();
    }

    fn close_text(&mut self) {
        self.flush_delta();
        let Some(open) = self.text.take() else {
            return;
        };
        let kind = if open.kind == DeltaKind::Text { EntryKind::Assistant } else { EntryKind::Reasoning };
        let mut entry = Entry::block(open.id, kind, &open.text);
        entry.clipped |= open.clipped;
        self.push(Outbound::Entry(EntryFrame { phase: Phase::Complete, entry }));
    }

    fn tool_started(&mut self, name: &str, summary: &str) {
        self.close_text();
        // Tools run one after another; one still open never reported back.
        if let Some(open) = self.tool.take() {
            self.emit(Outbound::Tool(finish_frame(open, ToolState::Ok, String::new(), false)));
        }
        let entry_id = self.entry_id();
        let call_id = self.call_id();
        let summary = protocol::clip_text(summary, 512).0;
        self.emit(Outbound::Tool(ToolFrame {
            entry_id: entry_id.clone(),
            phase: ToolPhase::Start,
            call_id: call_id.clone(),
            name: name.to_owned(),
            summary: summary.clone(),
            status: ToolState::Running,
            output: String::new(),
            output_clipped: false,
            duration_ms: None,
        }));
        self.tool = Some(OpenTool { entry_id, call_id, name: name.to_owned(), summary, started: Instant::now() });
        self.set_status(State::Tool, &format!("running {name}"));
    }

    fn tool_finished(&mut self, name: &str, output: &str) {
        self.close_text();
        let open = match self.tool.take() {
            Some(open) => open,
            None => OpenTool {
                entry_id: self.entry_id(),
                call_id: self.call_id(),
                name: name.to_owned(),
                summary: String::new(),
                started: Instant::now(),
            },
        };
        let status = if tool_failed(output) { ToolState::Failed } else { ToolState::Ok };
        let (output, clipped) = protocol::clip_tool_output(output);
        self.emit(Outbound::Tool(finish_frame(open, status, output, clipped)));
        self.set_status(State::Thinking, "thinking");
    }

    fn set_status(&mut self, state: State, label: &str) {
        let label = protocol::clip_text(label, MAX_STATUS_LABEL).0;
        if self.status.state == state && self.status.label == label {
            return;
        }
        if self.status.state != state {
            self.status.since = now();
        }
        self.status.state = state;
        self.status.label = label;
        self.label_sent = Some(Instant::now());
        let status = self.status.clone();
        self.emit(Outbound::Status(status));
    }

    /// Queue a frame after any text still waiting, so order is preserved.
    fn emit(&mut self, frame: Outbound) {
        self.flush_delta();
        self.push(frame);
    }

    fn push(&mut self, frame: Outbound) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.frames.try_send(frame) {
            self.resync = true;
        }
    }

    fn entry_id(&mut self) -> String {
        self.next_entry += 1;
        format!("e{}", self.next_entry)
    }

    fn call_id(&mut self) -> String {
        self.next_call += 1;
        format!("t{}", self.next_call)
    }
}

fn finish_frame(open: OpenTool, status: ToolState, output: String, output_clipped: bool) -> ToolFrame {
    ToolFrame {
        entry_id: open.entry_id,
        phase: ToolPhase::Finish,
        call_id: open.call_id,
        name: open.name,
        summary: open.summary,
        status,
        output,
        output_clipped,
        duration_ms: Some(millis(open.started)),
    }
}

/// The agent reports failures as ordinary tool output; the outcome is read
/// back out of the text, by the same rule the terminal and the server's
/// transcript renderer use.
fn tool_failed(output: &str) -> bool {
    let head = output.trim_start();
    head.starts_with("Error:") || head.starts_with("error:") || head.starts_with("User rejected")
}

fn matches_stream(kind: ui::EntryKind, stream: DeltaKind) -> bool {
    matches!(
        (kind, stream),
        (ui::EntryKind::Assistant, DeltaKind::Text) | (ui::EntryKind::Thinking, DeltaKind::Reasoning)
    )
}

/// Index of the first entry a snapshot includes: as many recent entries as
/// fit [`SNAPSHOT_BUDGET`] and [`SNAPSHOT_MAX_ENTRIES`].
fn window_start(entries: &[ui::Entry]) -> usize {
    let mut size = 0;
    for (taken, (index, entry)) in entries.iter().enumerate().rev().enumerate() {
        let output = entry.tool.as_ref().map_or(0, |call| call.full.len().max(call.output.len()));
        size += entry.text.len().min(protocol::MAX_ENTRY_TEXT) + output.min(protocol::MAX_TOOL_OUTPUT) + 256;
        if size > SNAPSHOT_BUDGET || taken >= SNAPSHOT_MAX_ENTRIES {
            return index + 1;
        }
    }
    0
}

/// A terminal transcript block as a browser entry.
fn convert(entry: &ui::Entry, id: String, call_id: Option<String>) -> Entry {
    let kind = match entry.kind {
        ui::EntryKind::User => EntryKind::User,
        ui::EntryKind::Thinking => EntryKind::Reasoning,
        ui::EntryKind::Assistant => EntryKind::Assistant,
        ui::EntryKind::Tool => EntryKind::Tool,
        ui::EntryKind::System => EntryKind::System,
        ui::EntryKind::Error => EntryKind::Error,
        ui::EntryKind::Rule => EntryKind::Rule,
    };
    let Some(call) = entry.tool.as_ref().filter(|_| kind == EntryKind::Tool) else {
        // Terminal notices are the only place a pairing link is ever shown.
        let text = if matches!(kind, EntryKind::System | EntryKind::Error) {
            redact_pairing(&entry.text)
        } else {
            entry.text.clone()
        };
        return Entry::block(id, kind, &text);
    };
    let full = if call.full.is_empty() { &call.output } else { &call.full };
    let (output, output_clipped) = protocol::clip_tool_output(full);
    let status = match call.status {
        ui::ToolStatus::Running => ToolState::Running,
        ui::ToolStatus::Ok => ToolState::Ok,
        ui::ToolStatus::Failed => ToolState::Failed,
    };
    Entry {
        id,
        kind,
        text: String::new(),
        clipped: false,
        tool: Some(ToolInfo {
            call_id: call_id.unwrap_or_default(),
            name: call.name.clone(),
            summary: protocol::clip_text(&call.summary, 512).0,
            status,
            output,
            output_clipped,
            duration_ms: call.duration_ms,
        }),
    }
}

/// `text` with the token of any pairing link (`…/pair#t=<token>`) replaced:
/// a pairing link signs a device in, so it never leaves the terminal.
pub fn redact_pairing(text: &str) -> String {
    const MARK: &str = "#t=";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(MARK) {
        out.push_str(&rest[..at + MARK.len()]);
        rest = &rest[at + MARK.len()..];
        let end = rest.find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')).unwrap_or(rest.len());
        if end > 0 {
            out.push('…');
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn millis(since: Instant) -> u64 {
    since.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// The real [`Connector`]: the sync server's REST API.
struct SyncConnector {
    client: SyncClient,
    paths: AbacusPaths,
    session: Session,
}

impl Connector for SyncConnector {
    async fn prepare(&self) -> Result<(), LinkError> {
        // The server can only share a session it holds, so upload first —
        // through the sync engine, never forcing over another device's copy.
        // A refused upload still leaves that copy there to share.
        let uploaded = crate::sync::push_session(&self.paths, &self.session).await;
        match self.client.enable_remote(&self.session.id.to_string()).await {
            Ok(()) => Ok(()),
            Err(SyncError::Gone) if uploaded.is_err() => Err(LinkError::Retry(format!(
                "could not upload the session: {:#}",
                uploaded.err().map(|error| format!("{error:#}")).unwrap_or_default()
            ))),
            Err(error) => Err(classify(error)),
        }
    }

    async fn socket_url(&self) -> Result<String, LinkError> {
        let ticket = self.client.agent_ticket(&self.session.id.to_string()).await.map_err(classify)?;
        self.client.agent_socket_url(&ticket).map_err(|error| LinkError::Fatal(format!("{error:#}")))
    }

    async fn disable(&self) {
        let _ = self.client.disable_remote(&self.session.id.to_string()).await;
    }
}

fn classify(error: SyncError) -> LinkError {
    match error {
        SyncError::Transient(detail) => LinkError::Retry(format!("sync server unavailable: {detail}")),
        SyncError::Unauthorized => LinkError::Fatal("this device is signed out; run `abacus sync login`".into()),
        SyncError::Gone => LinkError::Fatal("the server does not offer live sharing for this session".into()),
        SyncError::Rejected { status: 409, .. } => LinkError::Fatal("sharing was turned off for this session".into()),
        other => LinkError::Fatal(other.to_string()),
    }
}

#[cfg(test)]
mod tests;
