//! Live sharing of an interactive session with the account's browsers.
//!
//! The [`Bridge`] is owned by the terminal UI. It turns what the agent does
//! ([`AgentEvent`]s, prompts typed in the terminal, approvals and questions)
//! into protocol frames and hands them to a background [`link`] task that owns
//! the WebSocket. What browsers send comes back as [`Inbound`] events, which
//! the terminal handles through the same code paths as its own keyboard: the
//! terminal stays the only thing that executes anything, and a browser can
//! reach nothing — workspace, model, configuration — that has no frame type.
//!
//! # Block ids
//!
//! A browser keeps a row's state (expanded, scrolled to) by its entry id, and
//! a snapshot replaces its transcript wholesale, so the same block must have
//! the same id live and in every snapshot:
//!
//! - A block of the terminal's transcript is `e<n>`, `n` being the number the
//!   transcript gave the entry ([`ui::Entry::id`]). Numbers count up and are
//!   never reused, so a row survives other entries being merged away or
//!   rewound, and a new block can never take over an old one's id. A tool's
//!   call is `t<n>` alike. An entry the transcript never numbered (only in
//!   tests) is `p<position>`.
//! - A block with no entry of the terminal's — reasoning the terminal hides —
//!   is `x<k>`, counted by the bridge; it is in a snapshot only while it is
//!   still streaming.
//! - What became of an approval or a question is a system row, `resolved-<id>`
//!   or `answered-<id>`, which the bridge keeps because the terminal's
//!   transcript has no entry for it. The line saying earlier entries were left
//!   out of a snapshot is always `earlier`.

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

/// How many approval and question outcomes a snapshot can replay; the oldest
/// go first.
const MAX_NOTES: usize = 1_000;

/// The longest an approval's target or an answer is quoted in the row that
/// records it.
const MAX_NOTE_QUOTE: usize = 120;

/// The line a snapshot starts with when it leaves earlier entries out.
const OMITTED_ID: &str = "earlier";

/// The block currently streaming to the browsers.
struct OpenText {
    id: String,
    /// The terminal entry it is written into, if it has one.
    entry: Option<u64>,
    kind: DeltaKind,
    /// The block so far, up to the entry limit, for `entry{complete}` and for
    /// a snapshot taken mid-stream.
    text: String,
    clipped: bool,
    /// Streamed but not yet sent.
    pending: String,
}

/// A block that ended: what a continuation of its entry starts from.
struct Finished {
    entry: u64,
    kind: DeltaKind,
    text: String,
    clipped: bool,
}

struct OpenTool {
    entry_id: String,
    call_id: String,
    name: String,
    summary: String,
    started: Instant,
}

/// How an approval or a question ended, as the row browsers show for it. The
/// terminal's transcript has no entry for either — they are dialogs there — so
/// the bridge keeps the row for the snapshots a resyncing browser asks for.
struct Note {
    /// `resolved-<approval id>` or `answered-<question id>`.
    id: String,
    /// The number the terminal's next transcript entry was going to get when
    /// it happened. The row goes before the first entry numbered at or above
    /// it, which keeps its place whatever is merged or rewound after.
    before: u64,
    text: String,
}

impl Note {
    fn entry(&self) -> Entry {
        Entry::block(self.id.clone(), EntryKind::System, &self.text)
    }
}

pub struct Bridge {
    session_id: String,
    frames: mpsc::Sender<Outbound>,
    stop: Option<oneshot::Sender<Stop>>,
    task: Option<JoinHandle<()>>,
    next_approval: u64,
    next_question: u64,
    /// Counts the blocks that have no entry of the terminal's (`x<k>`).
    next_loose: u64,
    /// The newest block that ended, while the terminal may yet write on into
    /// its entry (see `stream`).
    finished: Option<Finished>,
    notes: Vec<Note>,
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
            next_approval: 0,
            next_question: 0,
            next_loose: 0,
            finished: None,
            notes: Vec::new(),
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
    /// browsers mirror what was typed. `entry` is the transcript entry that
    /// holds it.
    pub fn user_prompt(&mut self, entry: u64, text: &str) {
        self.close_text();
        self.emit(Outbound::Entry(EntryFrame {
            phase: Phase::Complete,
            entry: Entry::block(entry_id(entry), EntryKind::User, text),
        }));
    }

    /// A turn began.
    pub fn turn_started(&mut self) {
        self.turn_started = Some(Instant::now());
        self.set_status(State::Thinking, "thinking");
    }

    /// Mirror one agent event. Called before the terminal handles it, so it
    /// only borrows. `entry` is the transcript entry the terminal is about to
    /// write the event into or create for it — text and reasoning that carry
    /// on an entry, a tool call — and `None` for an event it keeps no entry
    /// for (reasoning it hides) or none at all.
    pub fn on_event(&mut self, event: &AgentEvent, usage: &TokenUsage, entry: Option<u64>) {
        match event {
            AgentEvent::Delta(text) => self.stream(DeltaKind::Text, text, entry),
            AgentEvent::Reasoning(piece) => self.stream(DeltaKind::Reasoning, piece, entry),
            // The ids are assigned by `approval_opened`/`question_opened`,
            // once the terminal holds the request.
            AgentEvent::Approval(_) | AgentEvent::UserQuestion(_) => self.close_text(),
            AgentEvent::ToolStarted { name, summary } => self.tool_started(name, summary, entry),
            AgentEvent::ToolFinished { name, output } => self.tool_finished(name, output, entry),
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
        // An approval or a question still open was never decided. `done`
        // clears both in the browsers; a resolution frame would make them
        // read it as a decision ("Denied in the terminal") nobody made.
        self.approval = None;
        self.question = None;
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

    /// An approval was decided — and only then: one that is still open when
    /// the turn ends is dropped by [`Bridge::turn_ended`] without a word.
    /// `next_entry` is the number the terminal's next transcript entry will
    /// get, which is where the row recording the decision belongs.
    pub fn approval_resolved(&mut self, id: &str, decision: ApprovalDecision, by: By, next_entry: u64) {
        let open = self.approval.take_if(|approval| approval.approval_id == id);
        let text = approval_note(open.as_ref(), decision, by);
        self.note(format!("resolved-{id}"), next_entry, text);
        self.emit(Outbound::ApprovalResolved(ApprovalResolved {
            approval_id: id.to_owned(),
            decision: decision.into(),
            by,
        }));
        self.set_status(State::Thinking, "thinking");
    }

    /// Why no open approval has this id, for the refusal's `reason`.
    pub fn approval_miss(&self, id: &str) -> &'static str {
        if self.noted(&format!("resolved-{id}")) { "approval already decided" } else { "no open approval with that id" }
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

    /// A question was answered; see [`Bridge::approval_resolved`].
    pub fn question_resolved(&mut self, id: &str, selected: &[String], custom: Option<&str>, by: By, next_entry: u64) {
        let open = self.question.take_if(|question| question.question_id == id);
        let text = answer_note(open.as_ref(), selected, custom, by);
        self.note(format!("answered-{id}"), next_entry, text);
        self.emit(Outbound::QuestionResolved(QuestionResolved {
            question_id: id.to_owned(),
            selected: selected.to_vec(),
            custom: custom.map(str::to_owned),
            by,
        }));
        self.set_status(State::Thinking, "thinking");
    }

    /// Why no open question has this id, for the refusal's `reason`.
    pub fn question_miss(&self, id: &str) -> &'static str {
        if self.noted(&format!("answered-{id}")) {
            "question already answered"
        } else {
            "no open question with that id"
        }
    }

    /// The terminal dropped its transcript from the entry numbered `first`
    /// on (a rewind, or a different transcript altogether with `0`): what
    /// became of approvals and questions in the part that went is no longer
    /// part of the conversation.
    pub fn entries_dropped(&mut self, first: u64) {
        self.notes.retain(|note| note.before <= first);
    }

    /// Tell the browsers what became of their input. A refusal always says why,
    /// in words the browser shows as they are.
    pub fn accepted(&mut self, ref_id: &str, kind: InputKind, result: AcceptResult, reason: Option<&str>) {
        let reason = match (result, reason) {
            (_, Some(reason)) => Some(reason.to_owned()),
            (AcceptResult::Rejected, None) => Some("the terminal could not act on that".to_owned()),
            _ => None,
        };
        self.emit(Outbound::Accepted(Accepted { ref_id: ref_id.to_owned(), kind, result, reason }));
    }

    /// Send the whole transcript, paged. Every block has the id it has live
    /// (see the module docs), so a browser keeps what it knows of a row across
    /// the resync, and the rows recording approvals and questions come back in
    /// the places they were first seen.
    pub fn send_snapshot(&mut self, view: SnapshotView<'_>, reason: SnapshotReason) {
        self.flush_delta();
        self.resync = false;
        let start = window_start(view.entries);
        let mut entries = Vec::with_capacity(view.entries.len() - start + self.notes.len() + 2);
        if start > 0 {
            let text = format!(
                "{start} earlier entr{} not shown live; the full transcript is under Sessions.",
                if start == 1 { "y is" } else { "ies are" }
            );
            entries.push(Entry::block(OMITTED_ID.to_owned(), EntryKind::System, &text));
        }
        // Rows from before the window went out with the entries around them.
        let mut notes = self
            .notes
            .iter()
            .filter(|note| start == 0 || view.entries.get(start).is_none_or(|first| note.before > first.id))
            .peekable();
        for (index, entry) in view.entries.iter().enumerate().skip(start) {
            while let Some(note) = notes.next_if(|note| entry.id != 0 && note.before <= entry.id) {
                entries.push(note.entry());
            }
            entries.push(convert(entry, index));
        }
        entries.extend(notes.map(Note::entry));
        // Reasoning streams to the browsers even when the terminal hides it,
        // so the block may exist only here.
        if let Some(open) = &self.text
            && !entries.iter().any(|entry| entry.id == open.id)
        {
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

    fn stream(&mut self, kind: DeltaKind, text: &str, entry: Option<u64>) {
        if text.is_empty() {
            return;
        }
        // A different kind, or a different entry than the block was written
        // into (the terminal began one the bridge was not told of): a new block.
        if self.text.as_ref().is_some_and(|open| open.kind != kind || open.entry != entry) {
            self.close_text();
        }
        if self.text.is_none() {
            // The terminal writes on into an entry the bridge ended when
            // something it does not show came between (reasoning it hides): the
            // browsers' row goes on too, instead of a second one with the same
            // text in a snapshot.
            let resumed = self.finished.take_if(|done| Some(done.entry) == entry && done.kind == kind);
            let (id, text, clipped) = match resumed {
                Some(done) => (entry_id(done.entry), done.text, done.clipped),
                None => {
                    let id = self.block_id(entry);
                    let entry_kind = if kind == DeltaKind::Text { EntryKind::Assistant } else { EntryKind::Reasoning };
                    let block =
                        Entry { id: id.clone(), kind: entry_kind, text: String::new(), clipped: false, tool: None };
                    self.emit(Outbound::Entry(EntryFrame { phase: Phase::Start, entry: block }));
                    (id, String::new(), false)
                }
            };
            self.text = Some(OpenText { id, entry, kind, text, clipped, pending: String::new() });
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
        if let Some(entry) = open.entry {
            self.finished = Some(Finished { entry, kind: open.kind, text: open.text, clipped: open.clipped });
        }
    }

    fn tool_started(&mut self, name: &str, summary: &str, entry: Option<u64>) {
        self.close_text();
        // Tools run one after another; one still open never reported back.
        if let Some(open) = self.tool.take() {
            self.emit(Outbound::Tool(finish_frame(open, ToolState::Ok, String::new(), false)));
        }
        let entry_id = self.block_id(entry);
        let call_id = call_id(&entry_id);
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

    fn tool_finished(&mut self, name: &str, output: &str, entry: Option<u64>) {
        self.close_text();
        // One that began before sharing did: the entry it is in names it.
        let open = match self.tool.take() {
            Some(open) => open,
            None => {
                let entry_id = self.block_id(entry);
                OpenTool {
                    call_id: call_id(&entry_id),
                    entry_id,
                    name: name.to_owned(),
                    summary: String::new(),
                    started: Instant::now(),
                }
            }
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

    /// The id of a block written into terminal entry `entry`, or one of the
    /// bridge's own when the terminal keeps none.
    fn block_id(&mut self, entry: Option<u64>) -> String {
        entry.map(entry_id).unwrap_or_else(|| {
            self.next_loose += 1;
            format!("x{}", self.next_loose)
        })
    }

    /// Remember how an approval or question ended, replacing any earlier note
    /// of the same id.
    fn note(&mut self, id: String, before: u64, text: String) {
        self.notes.retain(|note| note.id != id);
        self.notes.push(Note { id, before, text });
        let excess = self.notes.len().saturating_sub(MAX_NOTES);
        self.notes.drain(..excess);
    }

    fn noted(&self, id: &str) -> bool {
        self.notes.iter().any(|note| note.id == id)
    }
}

/// The id of the browsers' block for the terminal transcript entry numbered
/// `entry` (see the module docs).
fn entry_id(entry: u64) -> String {
    format!("e{entry}")
}

/// A tool call's id: its entry's, in the `t` namespace.
fn call_id(entry_id: &str) -> String {
    format!("t{}", entry_id.strip_prefix('e').unwrap_or(entry_id))
}

/// Where a decision was made, as the rows record it.
fn made(by: By) -> &'static str {
    match by {
        By::Terminal => "in the terminal",
        By::Browser => "from phone",
    }
}

/// `text` on one line, short enough to quote in a row.
fn quote(text: &str) -> String {
    crate::text::clip(&crate::text::squeeze(text), MAX_NOTE_QUOTE, "…")
}

/// "Allowed write_file notes.txt · from phone": what was decided about which
/// call, and by whom.
fn approval_note(approval: Option<&ApprovalFrame>, decision: ApprovalDecision, by: By) -> String {
    let subject = match approval {
        Some(approval) if approval.summary.trim().is_empty() || approval.summary == approval.tool => {
            approval.tool.clone()
        }
        Some(approval) => format!("{} {}", approval.tool, quote(&approval.summary)),
        None => "the request".to_owned(),
    };
    let outcome = match decision {
        ApprovalDecision::Once => format!("Allowed {subject}"),
        ApprovalDecision::Always => format!("Allowed {subject} for this session"),
        ApprovalDecision::Reject => format!("Denied {subject}"),
    };
    format!("{outcome} · {}", made(by))
}

/// `Answered "Yes" · in the terminal`. An option answers with what it says
/// rather than its label (the labels may be bare numbers), and typed text
/// takes the place of the options, as it does for the browsers' own row.
fn answer_note(question: Option<&QuestionFrame>, selected: &[String], custom: Option<&str>, by: By) -> String {
    let said = match custom.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => quote(text),
        None => {
            let said: Vec<&str> = selected
                .iter()
                .map(|label| {
                    let option = question.and_then(|question| question.options.iter().find(|o| &o.label == label));
                    option.map(|option| option.description.as_str()).filter(|text| !text.is_empty()).unwrap_or(label)
                })
                .collect();
            quote(&said.join(", "))
        }
    };
    if said.is_empty() {
        format!("Skipped the question · {}", made(by))
    } else {
        format!("Answered \"{said}\" · {}", made(by))
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

/// A terminal transcript block as a browser entry; `index` is its place in
/// the transcript, which names an entry the transcript never numbered.
fn convert(entry: &ui::Entry, index: usize) -> Entry {
    let id = if entry.id == 0 { format!("p{index}") } else { entry_id(entry.id) };
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
    let call_id = call_id(&id);
    Entry {
        id,
        kind,
        text: String::new(),
        clipped: false,
        tool: Some(ToolInfo {
            call_id,
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
        // A ticket for a session whose sharing was turned off elsewhere
        // (409 `remote_not_enabled`). Read as a sync conflict, it would say
        // the session "changed on the server".
        SyncError::Conflict(_) | SyncError::Deleted(_) => {
            LinkError::Fatal("sharing was turned off for this session".into())
        }
        other => LinkError::Fatal(other.to_string()),
    }
}

#[cfg(test)]
mod tests;
