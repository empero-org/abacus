//! The live remote wire format (protocol v1, SPEC "Live remote protocol"):
//! frame envelopes, every payload in both directions, the clipping rules that
//! keep frames under the relay's size cap, and snapshot paging.
//!
//! Everything here is pure data — no sockets, no clocks beyond timestamps — so
//! the whole contract can be tested against the SPEC's JSON examples.

use crate::agent::ApprovalDecision;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const VERSION: u64 = 1;

/// The relay closes a socket (`4413`) that sends a larger frame.
pub const MAX_FRAME_BYTES: usize = 128 * 1024;

/// What the agent aims for, leaving headroom under [`MAX_FRAME_BYTES`] for
/// the envelope and for JSON escaping the estimates do not see.
pub const TARGET_FRAME_BYTES: usize = 96 * 1024;

pub const MAX_ENTRY_TEXT: usize = 64 * 1024;
pub const MAX_TOOL_OUTPUT: usize = 16 * 1024;
const TOOL_OUTPUT_TAIL: usize = 4 * 1024;
pub const MAX_APPROVAL_DETAILS: usize = 32 * 1024;
pub const MAX_DELTA: usize = 8 * 1024;
pub const MAX_PROMPT: usize = 32 * 1024;

/// Bounds for fields the SPEC leaves open; generous for people, small enough
/// that a runaway model cannot build a frame the relay refuses.
const MAX_LABEL: usize = 512;
const MAX_NOTICE: usize = 8 * 1024;
const MAX_QUESTION: usize = 16 * 1024;
const MAX_OPTION: usize = 2 * 1024;

const CLIPPED_MARKER: &str = "\n… [clipped]";
const ELISION: &str = "\n…\n";

/// Room the envelope (`v`, `id`, `seq`, `type`) takes around a payload:
/// `{"id":"<36>","payload":…,"seq":<20 digits>,"type":"snapshot","v":1}` is
/// 107 bytes; the rest is slack.
const ENVELOPE_BYTES: usize = 128;

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    System,
    Error,
    Rule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolState {
    Running,
    Ok,
    Failed,
}

/// The tool half of an [`Entry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub call_id: String,
    pub name: String,
    pub summary: String,
    pub status: ToolState,
    pub output: String,
    pub output_clipped: bool,
    pub duration_ms: Option<u64>,
}

/// One transcript block as a browser renders it. `id` is stable from the
/// snapshot that introduced it until the next snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub kind: EntryKind,
    pub text: String,
    pub clipped: bool,
    pub tool: Option<ToolInfo>,
}

impl Entry {
    /// A text entry, clipped to [`MAX_ENTRY_TEXT`].
    pub fn block(id: String, kind: EntryKind, text: &str) -> Self {
        let (text, clipped) = clip_text(text, MAX_ENTRY_TEXT);
        Self { id, kind, text, clipped, tool: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Idle,
    Thinking,
    Tool,
    WaitingApproval,
    WaitingAnswer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub state: State,
    pub label: String,
    /// When `state` was entered, RFC 3339 UTC.
    pub since: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

impl From<&crate::provider::TokenUsage> for Usage {
    fn from(usage: &crate::provider::TokenUsage) -> Self {
        Self {
            input: usage.input,
            output: usage.output,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            total: usage.total,
        }
    }
}

/// Who settled an approval or a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum By {
    Terminal,
    Browser,
}

/// An approval outcome as the wire spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Once,
    Always,
    Reject,
}

impl From<ApprovalDecision> for Resolution {
    fn from(decision: ApprovalDecision) -> Self {
        match decision {
            ApprovalDecision::Once => Self::Once,
            ApprovalDecision::Always => Self::Always,
            ApprovalDecision::Reject => Self::Reject,
        }
    }
}

/// Browser input kinds, the ones `accepted` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Prompt,
    Answer,
    Approve,
    Interrupt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptResult {
    /// A turn started or was steered.
    Queued,
    /// An approval or answer was applied.
    Applied,
    /// Nothing matched (no such pending id, invalid input).
    Rejected,
}

// ---------------------------------------------------------------------------
// Agent → browser payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotReason {
    Connect,
    Requested,
    Reconnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageInfo {
    pub index: usize,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub model: String,
    pub mode: String,
    pub app_version: String,
    pub started_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub approval: Option<ApprovalFrame>,
    pub question: Option<QuestionFrame>,
}

/// One page of a snapshot. The session, status, pending and usage fields ride
/// on page 0 only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotPage {
    pub snapshot_id: String,
    pub reason: SnapshotReason,
    pub page: PageInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Start,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryFrame {
    pub phase: Phase,
    pub entry: Entry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    Text,
    Reasoning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaFrame {
    pub entry_id: String,
    pub kind: DeltaKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPhase {
    Start,
    Finish,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFrame {
    pub entry_id: String,
    pub phase: ToolPhase,
    pub call_id: String,
    pub name: String,
    pub summary: String,
    pub status: ToolState,
    pub output: String,
    pub output_clipped: bool,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Diff,
    Command,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalFrame {
    pub approval_id: String,
    pub tool: String,
    pub summary: String,
    pub kind: ApprovalKind,
    pub details: String,
    pub details_clipped: bool,
}

impl ApprovalFrame {
    pub fn new(approval_id: String, tool: &str, summary: &str, details: &str) -> Self {
        let kind = if crate::diff::DiffDocument::parse(details).is_some() {
            ApprovalKind::Diff
        } else if tool == "run_command" {
            ApprovalKind::Command
        } else {
            ApprovalKind::Other
        };
        let (details, details_clipped) = clip_head_json(details, MAX_APPROVAL_DETAILS);
        Self {
            approval_id,
            tool: clip_text(tool, MAX_LABEL).0,
            summary: clip_text(summary, MAX_LABEL).0,
            kind,
            details,
            details_clipped,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalResolved {
    pub approval_id: String,
    pub decision: Resolution,
    pub by: By,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionFrame {
    pub question_id: String,
    pub header: String,
    pub text: String,
    pub options: Vec<QuestionOption>,
    pub multi: bool,
}

impl QuestionFrame {
    /// `options` arrive as the terminal displays them, `label — description`.
    pub fn new(question_id: String, header: &str, text: &str, options: &[String], multi: bool) -> Self {
        let options = options
            .iter()
            .map(|option| {
                let (label, description) = option.split_once(" — ").unwrap_or((option.as_str(), ""));
                QuestionOption {
                    label: clip_text(label, MAX_LABEL).0,
                    description: clip_text(description, MAX_OPTION).0,
                }
            })
            .collect();
        Self {
            question_id,
            header: clip_text(header, MAX_LABEL).0,
            text: clip_text(text, MAX_QUESTION).0,
            options,
            multi,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionResolved {
    pub question_id: String,
    pub selected: Vec<String>,
    pub custom: Option<String>,
    pub by: By,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeFrame {
    pub mode: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeFrame {
    pub text: String,
    pub level: Level,
}

impl NoticeFrame {
    pub fn new(text: &str, level: Level) -> Self {
        Self { text: clip_text(text, MAX_NOTICE).0, level }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorFrame {
    pub message: String,
    pub fatal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoneReason {
    Complete,
    Interrupted,
    StepLimit,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneFrame {
    pub reason: DoneReason,
    pub elapsed_ms: u64,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    pub ref_id: String,
    pub kind: InputKind,
    pub result: AcceptResult,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub ts: Value,
}

/// Every frame the agent sends. Serialises as `{"type":…,"payload":…}`; the
/// link adds `v`, `id` and `seq` when it puts the frame on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum Outbound {
    /// Boxed: a page with its head is several times larger than any other
    /// frame, and every queued frame would otherwise pay for it.
    Snapshot(Box<SnapshotPage>),
    Entry(EntryFrame),
    Delta(DeltaFrame),
    Tool(ToolFrame),
    Status(Status),
    Approval(ApprovalFrame),
    ApprovalResolved(ApprovalResolved),
    Question(QuestionFrame),
    QuestionResolved(QuestionResolved),
    Mode(ModeFrame),
    Notice(NoticeFrame),
    Error(ErrorFrame),
    Done(DoneFrame),
    Accepted(Accepted),
    /// Answer to a browser's `ping`.
    Pong(Ping),
    /// The agent's own heartbeat; the relay answers it.
    Ping(Ping),
}

/// `frame` as the JSON text that goes on the wire.
pub fn encode(frame: &Outbound, id: &str, seq: u64) -> String {
    let mut value = serde_json::to_value(frame).unwrap_or_else(|_| json!({"type": "notice", "payload": {}}));
    if let Value::Object(map) = &mut value {
        map.insert("v".into(), json!(VERSION));
        map.insert("id".into(), json!(id));
        map.insert("seq".into(), json!(seq));
    }
    value.to_string()
}

// ---------------------------------------------------------------------------
// Browser / server → agent
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Always,
}

impl Decision {
    /// `allow` approves once, `always` for the session (the terminal's `a`),
    /// `deny` rejects — exactly the terminal's three answers.
    pub fn approval(self) -> ApprovalDecision {
        match self {
            Self::Allow => ApprovalDecision::Once,
            Self::Always => ApprovalDecision::Always,
            Self::Deny => ApprovalDecision::Reject,
        }
    }
}

/// Input a browser may send. Nothing here can reach workspace, model, trust or
/// configuration: there is no frame for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Prompt { text: String },
    Answer { question_id: String, selected: Vec<String>, custom: Option<String> },
    Approve { approval_id: String, decision: Decision },
    Interrupt,
    RequestSnapshot,
}

/// A parsed frame from the relay.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Hello {
        heartbeat_s: Option<u64>,
        idle_timeout_s: Option<u64>,
        browsers: Option<usize>,
    },
    PeerState {
        role: String,
        online: bool,
        browsers: Option<usize>,
    },
    /// A browser's input, with the frame id it is deduplicated and answered by.
    Input {
        id: String,
        input: Input,
    },
    /// Well-formed browser input the agent refuses (too long, malformed).
    Invalid {
        id: String,
        kind: InputKind,
        reason: String,
    },
    Ping {
        ts: Value,
    },
    ServerError {
        code: String,
        message: String,
    },
    /// `ack`, `pong`, and anything unknown: nothing to do.
    Ignored,
}

#[derive(Deserialize)]
struct RawFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    payload: Value,
}

/// Parse one text frame. Never fails: what cannot be understood is ignored,
/// so a newer relay can add frame types without breaking older agents.
pub fn parse_incoming(text: &str) -> Incoming {
    let Ok(frame) = serde_json::from_str::<RawFrame>(text) else {
        return Incoming::Ignored;
    };
    let payload = &frame.payload;
    let id = frame.id.unwrap_or_default();
    let string = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_owned);
    let count = |key: &str| payload.get(key).and_then(Value::as_u64).map(|value| value as usize);
    match frame.kind.as_str() {
        "hello" => Incoming::Hello {
            heartbeat_s: payload.get("heartbeat_interval_s").and_then(Value::as_u64),
            idle_timeout_s: payload.get("idle_timeout_s").and_then(Value::as_u64),
            browsers: payload.pointer("/peers/browsers").and_then(Value::as_u64).map(|value| value as usize),
        },
        "peer_state" => Incoming::PeerState {
            role: string("role").unwrap_or_default(),
            online: payload.get("online").and_then(Value::as_bool).unwrap_or(false),
            browsers: count("browsers"),
        },
        "ping" => Incoming::Ping { ts: payload.get("ts").cloned().unwrap_or(Value::Null) },
        "error" => Incoming::ServerError {
            code: string("code").unwrap_or_default(),
            message: string("message").unwrap_or_default(),
        },
        "prompt" | "answer" | "approve" | "interrupt" | "request_snapshot" if !id.is_empty() => {
            parse_input(&frame.kind, id, payload)
        }
        _ => Incoming::Ignored,
    }
}

fn parse_input(kind: &str, id: String, payload: &Value) -> Incoming {
    let invalid = |kind: InputKind, reason: &str| Incoming::Invalid { id: id.clone(), kind, reason: reason.to_owned() };
    let input = match kind {
        "prompt" => {
            let Some(text) = payload.get("text").and_then(Value::as_str) else {
                return invalid(InputKind::Prompt, "missing text");
            };
            if text.trim().is_empty() {
                return invalid(InputKind::Prompt, "empty prompt");
            }
            if text.len() > MAX_PROMPT {
                return invalid(InputKind::Prompt, "prompt is longer than 32 KiB");
            }
            Input::Prompt { text: text.to_owned() }
        }
        "answer" => {
            let Some(question_id) = payload.get("question_id").and_then(Value::as_str) else {
                return invalid(InputKind::Answer, "missing question_id");
            };
            let selected = match payload.get("selected") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => {
                    let labels: Option<Vec<String>> =
                        items.iter().map(|item| item.as_str().map(str::to_owned)).collect();
                    match labels {
                        Some(labels) if labels.len() <= 64 => labels,
                        _ => return invalid(InputKind::Answer, "selected must be a short list of labels"),
                    }
                }
                Some(_) => return invalid(InputKind::Answer, "selected must be a list"),
            };
            let custom = payload.get("custom").and_then(Value::as_str).map(str::to_owned);
            if custom.as_ref().is_some_and(|custom| custom.len() > MAX_PROMPT) {
                return invalid(InputKind::Answer, "answer is longer than 32 KiB");
            }
            Input::Answer { question_id: question_id.to_owned(), selected, custom }
        }
        "approve" => {
            let Some(approval_id) = payload.get("approval_id").and_then(Value::as_str) else {
                return invalid(InputKind::Approve, "missing approval_id");
            };
            let decision = match payload.get("decision").and_then(Value::as_str) {
                Some("allow") => Decision::Allow,
                Some("deny") => Decision::Deny,
                Some("always") => Decision::Always,
                _ => return invalid(InputKind::Approve, "decision must be allow, deny or always"),
            };
            Input::Approve { approval_id: approval_id.to_owned(), decision }
        }
        "interrupt" => Input::Interrupt,
        _ => Input::RequestSnapshot,
    };
    Incoming::Input { id, input }
}

// ---------------------------------------------------------------------------
// Clipping
// ---------------------------------------------------------------------------

/// `text` held to `limit` bytes, the marker included, and whether it was cut.
pub fn clip_text(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    let keep = crate::text::prefix(text, limit.saturating_sub(CLIPPED_MARKER.len()));
    (format!("{keep}{CLIPPED_MARKER}"), true)
}

/// The head of `text` within `limit` bytes, unmarked (approval details keep
/// their own shape, the flag says it was cut).
pub fn clip_head(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    (crate::text::prefix(text, limit).to_owned(), true)
}

/// [`clip_head`], cut further while the text's JSON form — where every
/// control character takes six bytes — is well over `limit`. Keeps a frame
/// carrying it under the relay's cap whatever the text contains.
pub fn clip_head_json(text: &str, limit: usize) -> (String, bool) {
    let (mut clipped, mut cut) = clip_head(text, limit);
    let ceiling = limit + limit / 4;
    let mut keep = clipped.len();
    while json_len(&clipped) > ceiling && keep > 0 {
        keep /= 2;
        clipped = crate::text::prefix(text, keep).to_owned();
        cut = true;
    }
    (clipped, cut)
}

/// The first `head` and last `tail` bytes of `text` with an elision between,
/// or `text` itself when it fits. Tool output keeps its tail because that is
/// where a failing command prints why.
pub fn clip_head_tail(text: &str, head: usize, tail: usize) -> (String, bool) {
    if text.len() <= head + tail {
        return (text.to_owned(), false);
    }
    let start = crate::text::prefix(text, head);
    let mut from = text.len() - tail;
    while !text.is_char_boundary(from) {
        from += 1;
    }
    (format!("{start}{ELISION}{}", &text[from..]), true)
}

/// Tool output within [`MAX_TOOL_OUTPUT`], elision included.
pub fn clip_tool_output(text: &str) -> (String, bool) {
    clip_output_to(text, MAX_TOOL_OUTPUT)
}

fn clip_output_to(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    let budget = limit.saturating_sub(ELISION.len());
    let tail = TOOL_OUTPUT_TAIL.min(budget / 3);
    clip_head_tail(text, budget - tail, tail)
}

// ---------------------------------------------------------------------------
// Snapshot paging
// ---------------------------------------------------------------------------

/// What page 0 of a snapshot carries besides entries.
#[derive(Debug, Clone)]
pub struct SnapshotHead {
    pub snapshot_id: String,
    pub reason: SnapshotReason,
    pub session: SessionInfo,
    pub status: Status,
    pub pending: Pending,
    pub usage: Usage,
}

/// Pack `entries` into snapshot pages whose frames stay within `max_bytes`,
/// in order. An entry too large for a page of its own is clipped to fit.
pub fn paginate(head: SnapshotHead, entries: Vec<Entry>, max_bytes: usize) -> Vec<SnapshotPage> {
    // Page numbers are measured at their widest so the real ones never
    // overflow the budget.
    let widest = PageInfo { index: 999_999, count: 999_999 };
    let first = SnapshotPage {
        snapshot_id: head.snapshot_id.clone(),
        reason: head.reason,
        page: widest,
        session: Some(head.session),
        status: Some(head.status),
        pending: Some(head.pending),
        usage: Some(head.usage),
        entries: Vec::new(),
    };
    let later = SnapshotPage { session: None, status: None, pending: None, usage: None, ..first.clone() };
    let first_overhead = json_len(&Outbound::Snapshot(Box::new(first.clone()))) + ENVELOPE_BYTES;
    let later_overhead = json_len(&Outbound::Snapshot(Box::new(later.clone()))) + ENVELOPE_BYTES;
    // A fresh later page always has room for a fitted entry; page 0 may not
    // when it carries a large pending approval, and then stays entry-less.
    let room = max_bytes.saturating_sub(later_overhead + 1).max(1024);

    let mut pages: Vec<Vec<Entry>> = vec![Vec::new()];
    let mut used = first_overhead;
    for entry in entries {
        let entry = fit_entry(entry, room);
        let size = json_len(&entry) + 1;
        let current = pages.last().expect("one page exists");
        if used + size > max_bytes && (!current.is_empty() || pages.len() == 1) {
            pages.push(Vec::new());
            used = later_overhead;
        }
        used += size;
        pages.last_mut().expect("one page exists").push(entry);
    }
    let count = pages.len();
    pages
        .into_iter()
        .enumerate()
        .map(|(index, entries)| {
            let template = if index == 0 { &first } else { &later };
            SnapshotPage { page: PageInfo { index, count }, entries, ..template.clone() }
        })
        .collect()
}

/// `entry` clipped until its JSON fits in `room` bytes. Escaping can make
/// JSON several times longer than the text, so the cut is found by halving
/// rather than computed.
fn fit_entry(entry: Entry, room: usize) -> Entry {
    if json_len(&entry) <= room {
        return entry;
    }
    let text = entry.text.clone();
    let output = entry.tool.as_ref().map(|tool| tool.output.clone()).unwrap_or_default();
    let mut limit = text.len().max(output.len());
    let mut fitted = entry;
    while limit > 64 {
        limit /= 2;
        let (clipped_text, text_cut) = clip_text(&text, limit);
        fitted.text = clipped_text;
        fitted.clipped |= text_cut;
        if let Some(tool) = &mut fitted.tool {
            let (clipped_output, output_cut) = clip_output_to(&output, limit);
            tool.output = clipped_output;
            tool.output_clipped |= output_cut;
        }
        if json_len(&fitted) <= room {
            break;
        }
    }
    fitted
}

fn json_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head() -> SnapshotHead {
        SnapshotHead {
            snapshot_id: "0b7c3b3e-8d1e-4a49-9c3b-111111111111".into(),
            reason: SnapshotReason::Connect,
            session: SessionInfo {
                id: "s1".into(),
                title: "Fix parser".into(),
                workspace: "/home/me/proj".into(),
                model: "empero/model-x".into(),
                mode: "auto".into(),
                app_version: "0.6.4".into(),
                started_at: "2026-10-04T12:00:00Z".into(),
            },
            status: Status { state: State::Thinking, label: "Checking the parser".into(), since: "t".into() },
            pending: Pending::default(),
            usage: Usage::default(),
        }
    }

    fn roundtrip(frame: Outbound, expected: Value) {
        let wire: Value = serde_json::from_str(&encode(&frame, "abc", 7)).unwrap();
        assert_eq!(wire["v"], 1);
        assert_eq!(wire["id"], "abc");
        assert_eq!(wire["seq"], 7);
        let mut shape = wire.clone();
        for key in ["v", "id", "seq"] {
            shape.as_object_mut().unwrap().remove(key);
        }
        assert_eq!(shape, expected, "{frame:?}");
        let back: Outbound = serde_json::from_value(shape).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn outbound_frames_match_the_spec_shapes() {
        let tool = ToolInfo {
            call_id: "call_1".into(),
            name: "edit_file".into(),
            summary: "src/x.rs".into(),
            status: ToolState::Running,
            output: String::new(),
            output_clipped: false,
            duration_ms: None,
        };
        roundtrip(
            Outbound::Entry(EntryFrame {
                phase: Phase::Start,
                entry: Entry {
                    id: "e12".into(),
                    kind: EntryKind::Tool,
                    text: String::new(),
                    clipped: false,
                    tool: Some(tool),
                },
            }),
            json!({"type":"entry","payload":{"phase":"start","entry":{"id":"e12","kind":"tool","text":"","clipped":false,
                "tool":{"call_id":"call_1","name":"edit_file","summary":"src/x.rs","status":"running","output":"",
                        "output_clipped":false,"duration_ms":null}}}}),
        );
        roundtrip(
            Outbound::Delta(DeltaFrame { entry_id: "e13".into(), kind: DeltaKind::Reasoning, text: "…".into() }),
            json!({"type":"delta","payload":{"entry_id":"e13","kind":"reasoning","text":"…"}}),
        );
        roundtrip(
            Outbound::Tool(ToolFrame {
                entry_id: "e14".into(),
                phase: ToolPhase::Finish,
                call_id: "call_1".into(),
                name: "run_command".into(),
                summary: "cargo test".into(),
                status: ToolState::Ok,
                output: "ok".into(),
                output_clipped: true,
                duration_ms: Some(4120),
            }),
            json!({"type":"tool","payload":{"entry_id":"e14","phase":"finish","call_id":"call_1","name":"run_command",
                "summary":"cargo test","status":"ok","output":"ok","output_clipped":true,"duration_ms":4120}}),
        );
        roundtrip(
            Outbound::Status(Status {
                state: State::WaitingApproval,
                label: "running cargo test".into(),
                since: "t".into(),
            }),
            json!({"type":"status","payload":{"state":"waiting_approval","label":"running cargo test","since":"t"}}),
        );
        roundtrip(
            Outbound::Approval(ApprovalFrame::new("a3".into(), "edit_file", "src/x.rs", "plain details")),
            json!({"type":"approval","payload":{"approval_id":"a3","tool":"edit_file","summary":"src/x.rs","kind":"other",
                "details":"plain details","details_clipped":false}}),
        );
        roundtrip(
            Outbound::ApprovalResolved(ApprovalResolved {
                approval_id: "a3".into(),
                decision: Resolution::Always,
                by: By::Browser,
            }),
            json!({"type":"approval_resolved","payload":{"approval_id":"a3","decision":"always","by":"browser"}}),
        );
        roundtrip(
            Outbound::Question(QuestionFrame::new(
                "q2".into(),
                "Pick a strategy",
                "Which…?",
                &["1 — Rewrite".into(), "2 — Patch".into()],
                false,
            )),
            json!({"type":"question","payload":{"question_id":"q2","header":"Pick a strategy","text":"Which…?",
                "options":[{"label":"1","description":"Rewrite"},{"label":"2","description":"Patch"}],"multi":false}}),
        );
        roundtrip(
            Outbound::QuestionResolved(QuestionResolved {
                question_id: "q2".into(),
                selected: vec!["1".into()],
                custom: None,
                by: By::Browser,
            }),
            json!({"type":"question_resolved","payload":{"question_id":"q2","selected":["1"],"custom":null,"by":"browser"}}),
        );
        roundtrip(
            Outbound::Mode(ModeFrame { mode: "plan".into(), reason: "why".into() }),
            json!({"type":"mode","payload":{"mode":"plan","reason":"why"}}),
        );
        roundtrip(
            Outbound::Notice(NoticeFrame::new("hi", Level::Warning)),
            json!({"type":"notice","payload":{"text":"hi","level":"warning"}}),
        );
        roundtrip(
            Outbound::Error(ErrorFrame { message: "provider returned 500".into(), fatal: false }),
            json!({"type":"error","payload":{"message":"provider returned 500","fatal":false}}),
        );
        roundtrip(
            Outbound::Done(DoneFrame {
                reason: DoneReason::StepLimit,
                elapsed_ms: 51230,
                usage: Usage { input: 1, output: 2, cache_read: 3, cache_write: 4, total: 5 },
            }),
            json!({"type":"done","payload":{"reason":"step_limit","elapsed_ms":51230,
                "usage":{"input":1,"output":2,"cache_read":3,"cache_write":4,"total":5}}}),
        );
        roundtrip(
            Outbound::Accepted(Accepted {
                ref_id: "b1".into(),
                kind: InputKind::Approve,
                result: AcceptResult::Rejected,
                reason: Some("no open approval".into()),
            }),
            json!({"type":"accepted","payload":{"ref_id":"b1","kind":"approve","result":"rejected",
                "reason":"no open approval"}}),
        );
        roundtrip(Outbound::Pong(Ping { ts: json!("x") }), json!({"type":"pong","payload":{"ts":"x"}}));
    }

    #[test]
    fn snapshot_page_zero_carries_the_head_and_later_pages_do_not() {
        let entries =
            (0..40).map(|n| Entry::block(format!("e{n}"), EntryKind::Assistant, &"x".repeat(8_000))).collect();
        let pages = paginate(head(), entries, TARGET_FRAME_BYTES);
        assert!(pages.len() > 1);
        let first: Value = serde_json::to_value(Outbound::Snapshot(Box::new(pages[0].clone()))).unwrap();
        assert_eq!(first["payload"]["session"]["title"], "Fix parser");
        assert_eq!(first["payload"]["status"]["state"], "thinking");
        assert_eq!(first["payload"]["pending"], json!({"approval": null, "question": null}));
        assert!(first["payload"]["usage"].is_object());
        let second: Value = serde_json::to_value(Outbound::Snapshot(Box::new(pages[1].clone()))).unwrap();
        for key in ["session", "status", "pending", "usage"] {
            assert!(second["payload"].get(key).is_none(), "{key} on page 1");
        }
    }

    #[test]
    fn snapshot_pages_stay_under_budget_and_keep_order() {
        let mut entries = Vec::new();
        for n in 0..120 {
            let text = match n % 4 {
                0 => "short".to_owned(),
                1 => "y".repeat(30_000),
                // Control characters escape to six bytes each: a 64 KiB text
                // of them is ~384 KiB of JSON and must be cut to fit alone.
                2 => "\u{1}".repeat(MAX_ENTRY_TEXT - 100),
                _ => "\"quoted\" ".repeat(5_000),
            };
            entries.push(Entry::block(format!("e{n}"), EntryKind::Assistant, &text));
        }
        let pages = paginate(head(), entries, TARGET_FRAME_BYTES);
        let mut ids = Vec::new();
        for (index, page) in pages.iter().enumerate() {
            assert_eq!(page.page, PageInfo { index, count: pages.len() });
            let wire = encode(&Outbound::Snapshot(Box::new(page.clone())), &uuid::Uuid::new_v4().to_string(), u64::MAX);
            assert!(wire.len() <= TARGET_FRAME_BYTES, "page {index} is {} bytes", wire.len());
            ids.extend(page.entries.iter().map(|entry| entry.id.clone()));
        }
        let expected: Vec<String> = (0..120).map(|n| format!("e{n}")).collect();
        assert_eq!(ids, expected);
        let escaped = pages.iter().flat_map(|page| &page.entries).find(|entry| entry.id == "e2").unwrap();
        assert!(escaped.clipped);
    }

    #[test]
    fn an_empty_snapshot_is_one_page() {
        let pages = paginate(head(), Vec::new(), TARGET_FRAME_BYTES);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].page, PageInfo { index: 0, count: 1 });
        assert!(pages[0].session.is_some());
    }

    #[test]
    fn a_large_pending_approval_still_leaves_every_page_in_budget() {
        let mut head = head();
        head.pending.approval = Some(ApprovalFrame::new("a1".into(), "edit_file", "x", &"\u{2}".repeat(40_000)));
        let entries = vec![Entry::block("e1".into(), EntryKind::Assistant, &"z".repeat(60_000))];
        let pages = paginate(head, entries, TARGET_FRAME_BYTES);
        for page in &pages {
            assert!(encode(&Outbound::Snapshot(Box::new(page.clone())), "x", 1).len() <= TARGET_FRAME_BYTES);
        }
        assert_eq!(pages.iter().map(|page| page.entries.len()).sum::<usize>(), 1);
    }

    #[test]
    fn clipping_respects_every_limit() {
        let (text, clipped) = clip_text(&"é".repeat(MAX_ENTRY_TEXT), MAX_ENTRY_TEXT);
        assert!(clipped && text.len() <= MAX_ENTRY_TEXT && text.ends_with("[clipped]"));
        assert_eq!(clip_text("short", MAX_ENTRY_TEXT), ("short".to_owned(), false));

        let output = format!("{}MIDDLE{}", "h".repeat(20_000), "t".repeat(20_000));
        let (clipped_output, cut) = clip_tool_output(&output);
        assert!(cut && clipped_output.len() <= MAX_TOOL_OUTPUT, "{}", clipped_output.len());
        assert!(clipped_output.starts_with("hhh") && clipped_output.ends_with("ttt"));
        assert!(!clipped_output.contains("MIDDLE"));
        let tail = clipped_output.rsplit("\n…\n").next().unwrap();
        assert_eq!(tail.len(), 4 * 1024);

        // Multi-byte characters never split.
        let (wide, _) = clip_tool_output(&"漢".repeat(10_000));
        assert!(wide.len() <= MAX_TOOL_OUTPUT);

        let approval = ApprovalFrame::new("a".into(), "write_file", "x", &"d".repeat(MAX_APPROVAL_DETAILS + 1));
        assert!(approval.details_clipped && approval.details.len() == MAX_APPROVAL_DETAILS);
        // Binary-looking details escape six bytes a character; they are cut
        // until the JSON fits.
        let binary = ApprovalFrame::new("a".into(), "write_file", "x", &"\u{0}".repeat(MAX_APPROVAL_DETAILS));
        assert!(binary.details_clipped);
        assert!(json_len(&binary.details) <= MAX_APPROVAL_DETAILS + MAX_APPROVAL_DETAILS / 4);
    }

    #[test]
    fn approval_kinds_are_read_from_the_details() {
        let diff = "--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1 +1 @@\n-old\n+new\n";
        assert_eq!(ApprovalFrame::new("a".into(), "edit_file", "x", diff).kind, ApprovalKind::Diff);
        assert_eq!(ApprovalFrame::new("a".into(), "run_command", "ls", "$ ls").kind, ApprovalKind::Command);
        assert_eq!(ApprovalFrame::new("a".into(), "move_file", "a → b", "Move a").kind, ApprovalKind::Other);
    }

    #[test]
    fn browser_frames_from_the_spec_parse() {
        let parse = |text: &str| parse_incoming(text);
        assert_eq!(
            parse(r#"{"v":1,"type":"prompt","id":"b1","seq":1,"payload":{"text":"Fix the parser"}}"#),
            Incoming::Input { id: "b1".into(), input: Input::Prompt { text: "Fix the parser".into() } }
        );
        assert_eq!(
            parse(
                r#"{"v":1,"type":"answer","id":"b2","seq":2,"payload":{"question_id":"q2","selected":["1"],"custom":null}}"#
            ),
            Incoming::Input {
                id: "b2".into(),
                input: Input::Answer { question_id: "q2".into(), selected: vec!["1".into()], custom: None },
            }
        );
        for (word, decision) in [("allow", Decision::Allow), ("deny", Decision::Deny), ("always", Decision::Always)] {
            let text = format!(
                r#"{{"v":1,"type":"approve","id":"b3","seq":3,"payload":{{"approval_id":"a3","decision":"{word}"}}}}"#
            );
            assert_eq!(
                parse(&text),
                Incoming::Input { id: "b3".into(), input: Input::Approve { approval_id: "a3".into(), decision } }
            );
        }
        assert_eq!(Decision::Allow.approval(), ApprovalDecision::Once);
        assert_eq!(Decision::Always.approval(), ApprovalDecision::Always);
        assert_eq!(Decision::Deny.approval(), ApprovalDecision::Reject);
        assert_eq!(
            parse(r#"{"v":1,"type":"interrupt","id":"b4","seq":4,"payload":{}}"#),
            Incoming::Input { id: "b4".into(), input: Input::Interrupt }
        );
        assert_eq!(
            parse(r#"{"v":1,"type":"request_snapshot","id":"b5","seq":5,"payload":{}}"#),
            Incoming::Input { id: "b5".into(), input: Input::RequestSnapshot }
        );
        assert_eq!(
            parse(r#"{"v":1,"type":"ping","id":"b6","seq":6,"payload":{"ts":"2026-10-04T12:00:00Z"}}"#),
            Incoming::Ping { ts: json!("2026-10-04T12:00:00Z") }
        );
    }

    #[test]
    fn server_frames_from_the_spec_parse() {
        assert_eq!(
            parse_incoming(
                r#"{"type":"hello","id":"server","seq":0,"payload":{"role":"agent","session_id":"s","server_time":"t",
                "heartbeat_interval_s":25,"idle_timeout_s":75,"max_frame_bytes":131072,"peers":{"agent":true,"browsers":1}}}"#
            ),
            Incoming::Hello { heartbeat_s: Some(25), idle_timeout_s: Some(75), browsers: Some(1) }
        );
        assert_eq!(
            parse_incoming(
                r#"{"type":"peer_state","id":"server","seq":0,"payload":{"role":"browser","online":true,"browsers":2}}"#
            ),
            Incoming::PeerState { role: "browser".into(), online: true, browsers: Some(2) }
        );
        assert_eq!(
            parse_incoming(
                r#"{"type":"error","id":"server","seq":0,"payload":{"code":"rate_limited","message":"slow"}}"#
            ),
            Incoming::ServerError { code: "rate_limited".into(), message: "slow".into() }
        );
        assert_eq!(parse_incoming(r#"{"type":"ack","id":"b1","seq":1,"payload":{}}"#), Incoming::Ignored);
        assert_eq!(parse_incoming(r#"{"type":"pong","id":"server","seq":0,"payload":{"ts":"x"}}"#), Incoming::Ignored);
        assert_eq!(parse_incoming("not json"), Incoming::Ignored);
        assert_eq!(parse_incoming(r#"{"type":"set_model","id":"b9","payload":{"model":"x"}}"#), Incoming::Ignored);
    }

    #[test]
    fn invalid_browser_input_is_refused_with_a_reason() {
        let long = "p".repeat(MAX_PROMPT + 1);
        let frame = json!({"v":1,"type":"prompt","id":"b1","seq":1,"payload":{"text":long}}).to_string();
        assert!(matches!(parse_incoming(&frame), Incoming::Invalid { kind: InputKind::Prompt, .. }));
        let frame = r#"{"v":1,"type":"approve","id":"b2","seq":2,"payload":{"approval_id":"a1","decision":"yolo"}}"#;
        assert!(matches!(parse_incoming(frame), Incoming::Invalid { kind: InputKind::Approve, .. }));
        let frame = r#"{"v":1,"type":"prompt","id":"b3","seq":3,"payload":{"text":"   "}}"#;
        assert!(matches!(parse_incoming(frame), Incoming::Invalid { kind: InputKind::Prompt, .. }));
        // Input without an id cannot be answered or deduplicated.
        assert_eq!(parse_incoming(r#"{"v":1,"type":"prompt","payload":{"text":"hi"}}"#), Incoming::Ignored);
    }
}
