//! Tiered context compaction for long-running loops.
//!
//! Design grounded in surveyed open-source coding agents (OpenHands
//! `LLMSummarizingCondenser`, LangMem `RunningSummary`, Cline/Claude Code
//! structured summary prompts, Goose progressive-overflow fallback, SWE-agent
//! observation masking). Two tiers:
//!
//! - **Microcompaction (no LLM):** replace stale compactable tool-result bodies
//!   with a sentinel, keeping a hot tail of recent results live. This is the
//!   cheap lever for an agent that re-reads files each turn — old `read_file` /
//!   `grep` / `run_command` output becomes a one-line placeholder while the
//!   recent working set stays fully visible.
//! - **Rolling-summary compaction (one LLM call):** when the context crosses the
//!   threshold, keep a verbatim head (system + original user task) and a verbatim
//!   recent tail, summarize the dropped middle into a persisted *running summary*
//!   that is extended (not regenerated) on each compaction, and re-inject that
//!   summary as memory every turn. Cut boundaries respect tool-call→tool-result
//!   groups so a call is never orphaned from its result. If the summarizer call
//!   itself overflows, tool-result bodies are stripped from the middle outward
//!   (Goose) and a drop-only trace fallback (OpenHands hard-reset spirit) keeps
//!   the loop alive.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use std::borrow::Cow;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::agent::{message_chars, message_chars_one};
use crate::model_info::CompactionBudget;
use crate::provider::Provider;

/// Head messages preserved verbatim (system prompt + original user task).
const KEEP_FIRST: usize = 2;
/// Hot tail of compactable tool results kept verbatim by microcompaction. Sized
/// to comfortably cover an active investigation's working set so the model does
/// not lose a finding it still needs and re-read it.
const KEEP_RECENT_TOOL_RESULTS: usize = 12;
/// Progressive middle-out tool-body stripping on summarizer overflow (Goose).
const OVERFLOW_STRIP_PERCENTS: &[u32] = &[0, 10, 20, 50, 100];

/// Marks a shrunk tool result. Matched as a prefix so the check stays true
/// however much identifying detail the placeholder carries.
const SENTINEL_PREFIX: &str = "[compacted:";
const TOOL_BODY_OMITTED: &str = "[tool output omitted for summarization]";

/// The field that makes a system message a marker of where compaction cut
/// the history (see [`Folded`]).
const MARKER_FIELD: &str = "compacted";

/// Tools whose results are large and re-derivable from disk, so their old
/// output is safe to placeholder.
const COMPACTABLE_TOOLS: &[&str] = &[
    "read_file", "read_files", "grep", "glob", "list_files", "run_command", "git_diff", "git_show", "git_blame",
    "git_log",
];

/// Rolling-summary state, carried across compactions and session resume.
///
/// Shared, cloneable handle following the `GoalState` pattern: a turn writes
/// the summary into the same state the front end holds, so the next turn
/// starts from it. It used to be a plain value copied into each turn, so a
/// summary written mid-turn was gone when the turn ended — and the next turn,
/// finding no summary, marked the summary layer cleared while the history it
/// stood in for stayed dropped.
///
/// Saved as `{"running_summary": …}`, the shape sessions have always had.
#[derive(Debug, Clone, Default)]
pub struct CompactionState {
    summary: Arc<RwLock<Option<String>>>,
}

#[derive(Serialize, Deserialize)]
struct SavedCompaction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    running_summary: Option<String>,
}

impl Serialize for CompactionState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SavedCompaction { running_summary: self.snapshot() }.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CompactionState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        SavedCompaction::deserialize(deserializer).map(|saved| Self::new(saved.running_summary))
    }
}

impl CompactionState {
    pub fn new(running_summary: Option<String>) -> Self {
        Self { summary: Arc::new(RwLock::new(running_summary)) }
    }

    pub fn snapshot(&self) -> Option<String> {
        self.summary.read().expect("compaction lock").clone()
    }

    /// A separate state holding the same summary — for a session file, which
    /// must not change behind its back when the live state moves on.
    pub fn detached(&self) -> Self {
        Self::new(self.snapshot())
    }

    pub fn set(&self, summary: String) {
        *self.summary.write().expect("compaction lock") = Some(summary);
    }

    /// Characters the summary adds to every request: the summary is wrapped in
    /// a template, so the wrapper's overhead counts too.
    fn weight(&self) -> usize {
        self.summary.read().expect("compaction lock").as_ref().map_or(0, |summary| summary.len() + 200)
    }

    /// Context injected as a system message every turn so the model retains the
    /// long-arc state of the goal across compactions.
    pub fn prompt_context(&self) -> String {
        match self.snapshot() {
            Some(summary) if !summary.trim().is_empty() => format!(
                "<compaction_summary>\n{summary}\n</compaction_summary>\n\
                 This is a rolling summary of earlier conversation that was compacted to fit the context \
                 window. Treat it as accurate memory of prior work; re-read files from disk when you need \
                 their current contents."
            ),
            _ => String::new(),
        }
    }
}

/// What compaction folded away at one place in the history.
///
/// A session file holds only what the model sees, so a transcript rebuilt
/// from a compacted one ran from the opening straight to the last exchange.
/// Where compaction cuts into the conversation it leaves a marker — a system
/// message with no text that carries this — and the front ends show it as a
/// divider. The model never reads it: requests are built without it (see
/// [`without_markers`]), so it costs no tokens, moves no cached prefix, and
/// never reaches `merge_system_messages`.
///
/// Every compaction cuts at the end of the verbatim head, where the last one
/// left its marker, so that marker is folded with the rest and what it
/// counted carries into the new one: one divider for the whole gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folded {
    /// The person's messages, the agent's, and tool results.
    pub messages: usize,
    /// About how many tokens of context they took up.
    pub tokens: usize,
    /// Whether the rolling summary stands in for them. False when the last
    /// compaction could not write one and dropped them, leaving the model
    /// only a trace of what was done.
    pub summarised: bool,
}

impl Folded {
    /// What `message` marks, if it is a marker.
    pub fn marked_by(message: &Value) -> Option<Self> {
        if !is_marker(message) {
            return None;
        }
        serde_json::from_value(message[MARKER_FIELD].clone()).ok()
    }

    /// What folding `range` away folds, with what the markers in it already
    /// counted. Other system messages — context layers, a fallback note —
    /// are scaffolding, not conversation.
    fn of(range: &[Value], summarised: bool) -> Self {
        let mut folded = Self { messages: 0, tokens: 0, summarised };
        let mut chars = 0;
        for message in range {
            if let Some(earlier) = Self::marked_by(message) {
                folded.messages += earlier.messages;
                folded.tokens += earlier.tokens;
            } else if message["role"] != "system" {
                folded.messages += 1;
                chars += message_chars_one(message);
            }
        }
        folded.tokens += chars / crate::model_info::CHARS_PER_TOKEN;
        folded
    }

    /// The marker that stands where the history was cut, if any of the
    /// conversation was: a fold of context layers alone leaves nothing for a
    /// divider to stand for.
    fn marker(self) -> Option<Value> {
        (self.messages > 0).then(|| json!({"role": "system", "content": "", MARKER_FIELD: self}))
    }
}

/// Whether `message` is a marker for the transcript rather than part of the
/// conversation: one compaction left, or a turn's edits (see
/// [`crate::checkpoint::TurnEdits::marker`]).
pub fn is_marker(message: &Value) -> bool {
    message["role"] == "system"
        && (message.get(MARKER_FIELD).is_some() || message.get(crate::checkpoint::MARKER_FIELD).is_some())
}

/// The history as the model reads it: without the markers compaction leaves
/// for the transcript.
pub fn without_markers(messages: &[Value]) -> Cow<'_, [Value]> {
    if !messages.iter().any(is_marker) {
        return Cow::Borrowed(messages);
    }
    Cow::Owned(messages.iter().filter(|message| !is_marker(message)).cloned().collect())
}

/// Who summarises, and whether it can do so from inside the live context.
///
/// An out-of-context summarisation builds a fresh prompt — its own system
/// message, then the slice of history being summarised — which shares no prefix
/// with the conversation's own requests and is therefore a total prompt-cache
/// miss: the most expensive call of the session, paid at the exact moment the
/// session is longest. An in-context summarisation instead *appends* the
/// instruction to the conversation as it stands, so every token before it is a
/// cache read at a fraction of the price, and the model summarises from the
/// evidence it already has rather than from a re-serialised copy of it.
///
/// It is only available when the summariser is the model running the
/// conversation — a different model has a different cache, and nothing to hit.
pub struct Summariser<'a> {
    pub provider: &'a Provider,
    /// True when `provider` is the conversation's own model.
    pub in_context: bool,
    /// The tool definitions the conversation's requests carry, sent unchanged on
    /// an in-context call. They sit ahead of the system prompt and the history
    /// in the cached prefix, so omitting them — which a summarisation call has
    /// no other reason to include — would move the prefix and miss everything
    /// behind it.
    pub tools: &'a [Value],
}

impl<'a> Summariser<'a> {
    /// An out-of-context summariser: a fresh prompt, no cache to reuse.
    pub fn detached(provider: &'a Provider) -> Self {
        Self { provider, in_context: false, tools: &[] }
    }
}

/// Run microcompaction (every turn) and rolling-summary compaction (when over
/// threshold). Mutates `messages` in place and updates `state`. The budgets are
/// derived from the chosen model's context window (see `model_info`).
pub async fn compact(
    summariser: &Summariser<'_>,
    messages: &mut Vec<Value>,
    state: &CompactionState,
    budget: &CompactionBudget,
    cancel: &AtomicBool,
) {
    // Tier 0 (cheap, no model call): once the conversation outgrows a fresh
    // recent window, replace stale re-derivable tool output (old file/grep
    // bodies the model already read) with a placeholder, keeping the recent
    // working set verbatim. This shrinks every later request instead of
    // re-sending big file bodies forever. Below the threshold the history stays
    // fully verbatim, so small sessions never lose findings and re-read in a
    // loop.
    if should_microcompact(messages, budget) {
        microcompact(messages, budget);
    }

    // Tier 1 (one model call): full rolling-summary compaction only near the
    // real ceiling. Re-measure first — microcompaction may have kept us under.
    if !under_pressure(messages, state, budget) {
        return;
    }
    if messages.len() <= KEEP_FIRST + 1 {
        return;
    }
    // Still on the opening user turn: only microcompact. Summarising this
    // turn drops the tool results and the next request is just the original
    // prompt again — the model then restarts the same work every step.
    if opening_turn_still_live(messages) {
        return;
    }

    let head_end = first_legal_cut_at_or_after(messages, KEEP_FIRST);
    // Never cut into the current user turn, even when the recent-window
    // budget is tiny — that tail is what the next model call has to see.
    let cut = find_tail_cut(messages, budget.recent_budget_chars, head_end)
        .min(last_user_index(messages).unwrap_or(messages.len()));
    // The marker the last compaction left is nothing to summarise.
    if cut <= head_end || messages[head_end..cut].iter().all(is_marker) {
        return;
    }

    let to_summarize: Vec<Value> = messages[head_end..cut].to_vec();
    let prior = state.snapshot();
    let note = match summarize(summariser, prior.as_deref(), messages, &to_summarize, budget, cancel).await {
        Ok(summary) => {
            // Backstop: a model that ignores the compression directive must not
            // be able to reinstate the growth loop, so an oversized summary is
            // cut structurally. The tail is kept because the most recent state
            // is what the next turn needs.
            state.set(bound_summary(summary, budget.summary_budget_chars));
            None
        }
        // Last-resort fallback: drop the middle with a local trace note so the
        // loop never breaks. An existing good summary is left as it is.
        Err(_) => {
            let trace = crate::agent::compaction_trace(&to_summarize);
            let actions = match trace.as_str() {
                "" => String::new(),
                trace => format!(" Earlier actions, in order: {trace}"),
            };
            Some(json!({"role":"system","content":format!(
                "{} older conversation messages were omitted to fit the model context.{actions} \
                 Reinspect files when prior details matter.",
                to_summarize.iter().filter(|message| !is_marker(message)).count()
            )}))
        }
    };
    // The middle gives way to the marker the transcript shows, and to the
    // note, when there is one. What is left may still carry stale output.
    let marker = Folded::of(&to_summarize, note.is_none()).marker();
    messages.splice(head_end..cut, marker.into_iter().chain(note));
    microcompact(messages, budget);
}

/// How long a finished turn waits, with nobody typing, before idle compaction
/// writes its summary: 90% of the five minutes a provider keeps a prompt cached
/// (Anthropic's default lifetime, and about OpenAI's), so the summary call
/// still reads the conversation from the cache. Written any later, the summary
/// costs what the next message would have, and saves nothing.
pub const IDLE_AFTER: Duration = Duration::from_secs(270);

/// The smallest context idle compaction touches, in tokens. Below it, sending
/// the history again uncached is cheap, and verbatim history is worth more than
/// the saving.
const IDLE_MIN_TOKENS: usize = 100_000;

/// Whether a quiet session is worth compacting before the person comes back.
///
/// The bar is the smaller of `IDLE_MIN_TOKENS` and half the compaction trigger:
/// on a small window, a context that far along is heading for a compaction
/// anyway, and doing it now spares the person the wait.
pub fn idle_worthwhile(messages: &[Value], state: &CompactionState, budget: &CompactionBudget) -> bool {
    let bar = idle_bar(budget);
    bar > 0 && message_chars(messages) + state.weight() >= bar && idle_cut(messages, budget).is_some()
}

/// How much context, in characters, makes a quiet session worth compacting.
fn idle_bar(budget: &CompactionBudget) -> usize {
    (IDLE_MIN_TOKENS * crate::model_info::CHARS_PER_TOKEN).min(budget.compact_at_chars / 2)
}

/// Where compaction steps in, in tokens of context, for a meter to mark: past
/// `trim` old tool output is trimmed, past `idle` a session left alone is
/// summarised (none when idle compaction is off), and past `summary` the
/// history is summarised before the next request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Marks {
    pub trim: usize,
    pub idle: Option<usize>,
    pub summary: usize,
}

pub fn marks(budget: &CompactionBudget, idle: bool) -> Marks {
    let tokens = |chars: usize| chars / crate::model_info::CHARS_PER_TOKEN;
    Marks {
        trim: tokens(budget.recent_budget_chars),
        idle: idle.then(|| tokens(idle_bar(budget))).filter(|&at| at > 0),
        summary: tokens(budget.compact_at_chars),
    }
}

/// The range idle compaction folds into the summary: everything between the
/// verbatim head and a recent tail.
///
/// The tail is a third of the window a compaction under pressure keeps. That
/// one is the working set of a turn still in flight; here the turn is over,
/// and what the next message most likely refers to is the last exchange. The
/// last reply stays whatever its size — it is what the person was reading when
/// they left.
fn idle_cut(messages: &[Value], budget: &CompactionBudget) -> Option<(usize, usize)> {
    let head_end = first_legal_cut_at_or_after(messages, KEEP_FIRST);
    let last = messages.iter().rposition(|message| message["role"] != "tool")?;
    let cut = find_tail_cut(messages, budget.recent_budget_chars / 3, head_end).min(last);
    // A range holding only the last compaction's marker has nothing to fold.
    (cut > head_end && !messages[head_end..cut].iter().all(is_marker)).then_some((head_end, cut))
}

/// The rolling summary, written ahead of time because the session went quiet.
///
/// Returns the shorter history and the summary that goes with it, and leaves
/// `state` alone: the person may come back while it is being written, so the
/// front end commits the pair only if nothing moved in the meantime. `None`
/// when the session is not worth it or summarising failed — unlike compaction
/// under pressure, nothing forces a drop, so a failure keeps the history whole.
pub async fn compact_idle(
    summariser: &Summariser<'_>,
    messages: &[Value],
    state: &CompactionState,
    budget: &CompactionBudget,
    cancel: &AtomicBool,
) -> Option<(Vec<Value>, String)> {
    if !idle_worthwhile(messages, state, budget) {
        return None;
    }
    let (head_end, cut) = idle_cut(messages, budget)?;
    let prior = state.snapshot();
    let summary =
        summarize(summariser, prior.as_deref(), messages, &messages[head_end..cut], budget, cancel).await.ok()?;
    let mut compacted = messages.to_vec();
    compacted.splice(head_end..cut, Folded::of(&messages[head_end..cut], true).marker());
    microcompact(&mut compacted, budget);
    Some((compacted, bound_summary(summary, budget.summary_budget_chars)))
}

/// Cap a summary at `budget` chars, keeping the end.
///
/// Only a backstop — the summariser is asked to compress first. Truncation
/// loses information, so it keeps the tail, where the current state lives, and
/// says plainly that earlier detail was cut.
fn bound_summary(summary: String, budget: usize) -> String {
    if summary.len() <= budget || budget == 0 {
        return summary;
    }
    let mut start = summary.len() - budget;
    while start < summary.len() && !summary.is_char_boundary(start) {
        start += 1;
    }
    // Resume at a line break so the kept text does not start mid-sentence.
    let tail = match summary[start..].find('\n') {
        Some(offset) => &summary[start + offset + 1..],
        None => &summary[start..],
    };
    format!("[earlier summary detail truncated to fit the context budget]\n{tail}")
}

/// Whether the conversation is large enough to start trimming stale, re-derivable
/// tool output (microcompaction). Tied to the recent-window budget: once history
/// exceeds what a post-compaction tail would keep, old file/grep bodies are no
/// longer worth re-sending in full. Below this, everything stays verbatim so the
/// model never loses its own findings.
fn should_microcompact(messages: &[Value], budget: &CompactionBudget) -> bool {
    // A zero recent window is a broken budget, not a signal to blank every
    // tool result. Leave the transcript alone until the numbers are real.
    budget.recent_budget_chars > 0 && message_chars(messages) > budget.recent_budget_chars
}

/// Whether the conversation has grown near the real context ceiling, warranting
/// the expensive full rolling-summary compaction. Includes the running summary
/// itself in the measurement, since it is re-injected as a system message every
/// turn and grows over time — not accounting for it means compaction triggers
/// too late and the actual request overflows the context window.
fn under_pressure(messages: &[Value], state: &CompactionState, budget: &CompactionBudget) -> bool {
    if budget.compact_at_chars == 0 {
        return false;
    }
    message_chars(messages) + state.weight() > budget.compact_at_chars
}

/// Index of the most recent `user` message, if any.
fn last_user_index(messages: &[Value]) -> Option<usize> {
    messages.iter().rposition(|message| message["role"] == "user")
}

/// True while the only real user prompt is the opening one (the pair
/// `KEEP_FIRST` protects). Later user turns are fair game for the summariser.
fn opening_turn_still_live(messages: &[Value]) -> bool {
    last_user_index(messages).is_none_or(|index| index < KEEP_FIRST)
}

/// Smallest index `>= start` that is a legal cut (never splits a tool-call group).
fn first_legal_cut_at_or_after(messages: &[Value], start: usize) -> usize {
    let mut i = start.min(messages.len());
    while i < messages.len() && messages[i]["role"] == "tool" {
        i += 1;
    }
    i
}

/// Find the smallest legal cut `>= head_end` whose tail fits `budget` chars —
/// i.e. the largest verbatim recent window we can keep. Returns `messages.len()`
/// if nothing fits, meaning no compaction is possible this round.
fn find_tail_cut(messages: &[Value], budget: usize, head_end: usize) -> usize {
    let total = message_chars(messages);
    let mut prefix = 0usize;
    for i in 0..=messages.len() {
        let is_legal = i == messages.len() || messages[i]["role"] != "tool";
        if is_legal && i >= head_end && total - prefix <= budget {
            return i;
        }
        if i < messages.len() {
            prefix += message_chars_one(&messages[i]);
        }
    }
    messages.len()
}

/// Shrink stale compactable tool results, keeping a hot tail live. The tool
/// message itself is preserved so tool-call→tool-result pairing stays intact.
///
/// The tail is bounded two ways: by count *and* by size. Count alone was not
/// enough — twelve `read_file` results on large files can exceed the entire
/// recent budget, so the cheap lever could run every turn and still leave the
/// tail bloated.
fn microcompact(messages: &mut [Value], budget: &CompactionBudget) {
    let compactable: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message["role"] == "tool" && message["name"].as_str().is_some_and(|name| COMPACTABLE_TOOLS.contains(&name))
        })
        .map(|(index, _)| index)
        .collect();

    // Walk backwards from the newest, keeping results until either limit trips.
    let size_budget = budget.recent_budget_chars / 2;
    let mut kept_chars = 0usize;
    let mut keep_from = compactable.len();
    for (position, &index) in compactable.iter().enumerate().rev() {
        // Everything after `keep_from` has already been accepted, so its length
        // is the running count — no separate counter to keep in step.
        let kept = compactable.len() - keep_from;
        let size = message_chars_one(&messages[index]);
        if kept >= KEEP_RECENT_TOOL_RESULTS || kept_chars + size > size_budget {
            break;
        }
        kept_chars += size;
        keep_from = position;
    }

    for &index in &compactable[..keep_from] {
        let Some(content) = messages[index].get("content").and_then(Value::as_str) else {
            continue;
        };
        // Idempotent across turns: an already-shrunk result is left alone.
        if content.starts_with(SENTINEL_PREFIX) {
            continue;
        }
        let name = messages[index]["name"].as_str().unwrap_or("tool").to_owned();
        let subject = call_subject(messages, index);
        messages[index]["content"] = json!(shrink_tool_result(&name, &subject, content));
    }
}

/// The path or query a tool result came from, recovered from the tool call that
/// produced it.
///
/// A single `read_file` result is line-numbered content with no path in it, so
/// the preview alone cannot say which file it was — and that is exactly the
/// detail whose loss makes the model read it again. The matching call is found
/// by `tool_call_id` in an earlier assistant message.
fn call_subject(messages: &[Value], result: usize) -> String {
    let Some(id) = messages[result]["tool_call_id"].as_str() else {
        return String::new();
    };
    let arguments = messages[..result].iter().rev().find_map(|message| {
        message["tool_calls"].as_array()?.iter().find_map(|call| {
            (call["id"].as_str() == Some(id)).then(|| call.pointer("/function/arguments")?.as_str()).flatten()
        })
    });
    let Some(parsed) = arguments.and_then(|raw| serde_json::from_str::<Value>(raw).ok()) else {
        return String::new();
    };
    for key in ["path", "paths", "pattern", "query", "command"] {
        match &parsed[key] {
            Value::String(value) => return crate::text::clip(&crate::text::flat(value), 80, "…"),
            Value::Array(values) => {
                let joined = values.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
                if !joined.is_empty() {
                    return crate::text::clip(&crate::text::flat(&joined), 80, "…");
                }
            }
            _ => {}
        }
    }
    String::new()
}

/// Replace a stale tool result with a placeholder that still says what it was.
///
/// Blanking the body outright removed every trace of which file or query the
/// result came from, so the model re-read it — the exact loop microcompaction
/// exists to prevent. Compactable tools all lead with their subject (the path
/// header, the first match, the command's first output line), so keeping the
/// opening lines preserves the identifying detail for a fixed small cost.
fn shrink_tool_result(name: &str, subject: &str, content: &str) -> String {
    const PREVIEW_LINES: usize = 3;
    const PREVIEW_CHARS: usize = 240;
    let mut preview = String::new();
    for line in content.lines().take(PREVIEW_LINES) {
        if preview.len() + line.len() > PREVIEW_CHARS {
            break;
        }
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push_str(line);
    }
    let header = if subject.is_empty() {
        format!("{SENTINEL_PREFIX} older {name} result cleared")
    } else {
        format!("{SENTINEL_PREFIX} older {name} result for {subject} cleared")
    };
    if preview.trim().is_empty() {
        return format!("{header}]");
    }
    format!("{header}, began:\n{preview}\n…]")
}

/// Produce the next running summary, preferring the in-context call.
///
/// The in-context attempt is one request; if it comes back empty or errors, the
/// detached ladder below runs exactly as it did before, so the cheap path can
/// never cost the session its compaction.
async fn summarize(
    summariser: &Summariser<'_>,
    prior: Option<&str>,
    conversation: &[Value],
    range: &[Value],
    budget: &CompactionBudget,
    cancel: &AtomicBool,
) -> Result<String, String> {
    if summariser.in_context {
        match summarize_in_context(summariser, prior, conversation, budget, cancel).await {
            Ok(summary) => return Ok(summary),
            // Falling through is the point: an in-context call can fail for
            // reasons the detached one will not (a request one turn's worth
            // larger than the last, a model that answers with a tool call
            // despite being told not to), and compaction must still happen.
            Err(_) if cancel.load(std::sync::atomic::Ordering::Relaxed) => {
                return Err("cancelled".to_owned());
            }
            Err(_) => {}
        }
    }
    summarize_range(summariser.provider, prior, range, budget, cancel).await
}

/// Summarise by appending the instruction to the live conversation.
///
/// The request is the conversation's own message list plus one trailing user
/// turn, and it carries the same tools, so everything up to the last cache
/// breakpoint is a cache read. The model is addressed in the second person
/// about its own context, because that is what it is looking at.
async fn summarize_in_context(
    summariser: &Summariser<'_>,
    prior: Option<&str>,
    conversation: &[Value],
    budget: &CompactionBudget,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let mut directive = String::from(IN_CONTEXT_PROMPT);
    if let Some(prior) = prior.filter(|summary| !summary.trim().is_empty()) {
        directive.push_str(&format!(
            "\n\nEarlier conversation was already compacted into this summary. Fold everything \
             from it that still matters into the new one; do not lose a decision or constraint it \
             records:\n{prior}"
        ));
    }
    directive.push_str(&format!(
        "\n\nKeep the summary under {} characters. Respond with the summary text only — no tool \
         calls, no preamble.",
        budget.summary_budget_chars
    ));

    let mut messages = conversation.to_vec();
    messages.push(json!({"role": "user", "content": directive}));

    let result = summariser.provider.ask(&messages, summariser.tools, cancel).await;
    let completion = result.map_err(|error| format!("{error:#}"))?;
    if completion.cancelled {
        return Err("cancelled".to_owned());
    }
    // A model that answered with a tool call instead of a summary has not
    // summarised anything; the detached path asks again without tools.
    if !completion.tool_calls.is_empty() {
        return Err("summariser called a tool instead of answering".to_owned());
    }
    let summary = strip_analysis(&completion.content).trim().to_owned();
    if summary.is_empty() {
        return Err("in-context summarisation returned no text".to_owned());
    }
    Ok(summary)
}

async fn summarize_range(
    provider: &Provider,
    prior: Option<&str>,
    range: &[Value],
    budget: &CompactionBudget,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let prompt = json!({"role":"system","content": SUMMARY_PROMPT});
    let prior = prior.filter(|s| !s.trim().is_empty());
    // Past its budget the summary has to be condensed rather than extended.
    // Extending unconditionally is what let it grow until it alone kept the
    // context over threshold, firing a summariser call every single turn.
    let over_budget = prior.is_some_and(|summary| summary.len() > budget.summary_budget_chars);
    let directive = match prior {
        Some(summary) if over_budget => format!(
            "This is the summary of the conversation so far:\n{summary}\n\n\
             Rewrite this summary to incorporate the new messages above, compressed to under \
             {} characters. It has grown too large. Keep current state, open questions, and pending \
             work in full; condense or drop work that is finished and no longer referenced. Never \
             drop a decision that still constrains the task.",
            budget.summary_budget_chars
        ),
        Some(summary) => format!(
            "This is the summary of the conversation so far:\n{summary}\n\n\
             Extend this summary by taking into account the new messages above. Do not lose any fact \
             from the existing summary."
        ),
        None => "Create the initial summary of the conversation above.".to_owned(),
    };

    for &strip_percent in OVERFLOW_STRIP_PERCENTS {
        let mut messages = Vec::with_capacity(range.len() + 4);
        messages.push(prompt.clone());
        if let Some(summary) = prior {
            messages.push(json!({"role":"user","content":format!("Existing summary so far:\n{summary}")}));
            messages.push(json!({"role":"assistant","content":"Understood. I will extend it."}));
        }
        if strip_percent == 0 {
            messages.extend_from_slice(range);
        } else {
            messages.extend(strip_tool_bodies(range, strip_percent));
        }
        messages.push(json!({"role":"user","content": directive}));

        match provider.ask(&messages, &[], cancel).await {
            Ok(completion) => {
                let cleaned = strip_analysis(&completion.content);
                let trimmed = cleaned.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_owned());
                }
                // Empty summary — try harder stripping on the next loop iteration.
            }
            Err(error) => {
                // Likely context overflow; progress to a higher strip percentage.
                let last = *OVERFLOW_STRIP_PERCENTS.last().unwrap();
                if strip_percent == last {
                    return Err(format!("{error:#}"));
                }
            }
        }
    }
    Err("summarization produced no usable summary".to_owned())
}

/// Return a copy of `range` with the given percentage of tool-result bodies
/// blanked from the middle outward (symmetric, Goose-style).
fn strip_tool_bodies(range: &[Value], strip_percent: u32) -> Vec<Value> {
    let tool_indices: Vec<usize> =
        range.iter().enumerate().filter(|(_, m)| m["role"] == "tool").map(|(i, _)| i).collect();
    let mut out: Vec<Value> = range.to_vec();
    if tool_indices.is_empty() || strip_percent == 0 {
        return out;
    }
    let count = tool_indices.len();
    let num_to_remove = (((count * strip_percent as usize) / 100).max(1)).min(count);
    // Middle-out removal order: center, then alternate left/right.
    let mid = count / 2;
    let mut order: Vec<usize> = Vec::with_capacity(count);
    order.push(tool_indices[mid]);
    let mut left = mid as isize - 1;
    let mut right = mid + 1;
    while left >= 0 || right < count {
        if right < count {
            order.push(tool_indices[right]);
            right += 1;
        }
        if left >= 0 {
            order.push(tool_indices[left as usize]);
            left -= 1;
        }
    }
    for index in order.into_iter().take(num_to_remove) {
        out[index]["content"] = json!(TOOL_BODY_OMITTED);
    }
    out
}

/// Remove `<analysis>...</analysis>` scratchpad blocks (inclusive). If a block is
/// opened but never closed, drop from the opener to the end.
fn strip_analysis(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<analysis>") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "<analysis>".len()..];
        match after.find("</analysis>") {
            Some(end) => {
                rest = &after[end + "</analysis>".len()..];
            }
            None => {
                // Unclosed scratchpad — discard the remainder.
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The in-context compaction instruction, appended to the conversation itself.
///
/// Written in the second person and in the present tense, because the model is
/// looking at the context it is being asked to replace — there is no
/// "conversation above" to describe from outside. Short on purpose: the
/// structure a detached summariser has to be told about (roles, ordering, what
/// a tool result was) is visible to this one.
const IN_CONTEXT_PROMPT: &str = "\
Your context is full, so this conversation is about to be replaced by your summary of it. \
Write the summary you need to continue seamlessly: the user's requests and constraints \
(quote the precise ones), what has been done and learned, files changed, the current state, \
and the exact next steps. Be dense and complete.\n\n\
Write it as notes to yourself, not as a report to the user. Prefer specifics over \
characterisations: exact paths, identifiers, commands, numbers, and error text. Anything you \
omit is gone — but a file's contents can be re-read from disk, so spend the space on what \
cannot be recovered that way: decisions and their reasons, what was ruled out, what failed \
and how, and what you were about to do next.";

const SUMMARY_PROMPT: &str = "\
You are maintaining a context-aware state summary for a long-running coding agent.\n\
Your summary is the agent's memory across many compactions, so it must stay coherent\n\
with the original goal even after dozens of iterations.\n\n\
CRITICAL: Respond with TEXT ONLY. Do NOT call any tools. Do NOT emit anything other\n\
than the summary. Tool calls will be rejected and will waste this turn.\n\n\
First, wrap your analysis in <analysis> tags to organize your thoughts. The analysis\n\
block is a scratchpad and will be stripped before storage, so use it freely. In your\n\
analysis, chronologically review: every user request and intent, the approach taken,\n\
key decisions and why, technical concepts, code patterns, file names, load-bearing\n\
code snippets and function signatures, file edits, errors encountered and how they\n\
were fixed, and any user feedback or corrections. Pay special attention to the most\n\
recent user message — it indicates the current intent.\n\n\
Then produce a summary with EXACTLY these sections, in order:\n\n\
1. Primary Request and Intent\n\
   The original goal and all explicit user requests, in detail. Preserve the user's\n\
   exact phrasing for the most recent request.\n\n\
2. Key Technical Concepts\n\
   Technologies, frameworks, libraries, and architecture relevant to the task.\n\n\
3. Files and Code Sections\n\
   Enumerate files examined, modified, or created. For each: the path (relative to\n\
   the working directory), what it contains, and why it matters. Include FULL code\n\
   snippets only for snippets that are load-bearing (a function being debugged, a\n\
   signature being implemented against). Do NOT paste entire files — reference paths\n\
   and summarize contents.\n\n\
4. Errors and Fixes\n\
   Every error encountered, its cause, and how it was fixed. Note user feedback.\n\n\
5. Problem Solving\n\
   Problems solved and any ongoing troubleshooting, including dead ends explored.\n\n\
6. All User Messages\n\
   List ALL user messages that are not tool results, oldest to newest, paraphrased\n\
   briefly except the most recent which is quoted.\n\n\
7. Pending Tasks\n\
   Work explicitly requested but not yet done. Preserve any task IDs verbatim.\n\n\
8. Current Work\n\
   What was being worked on immediately before this summary request — the exact\n\
   state of the in-flight change.\n\n\
9. Required Files\n\
   The files most likely needed to continue, most important first, one per line\n\
   prefixed with \"- \" (e.g. \"- src/main.rs\"). Re-read these from disk when needed.\n\n\
10. Next Step\n\
    The single next action, DIRECTLY in line with the user's most recent explicit\n\
    request. Include a direct quote from the most recent conversation showing\n\
    exactly what task was in progress.\n\n\
Rules:\n\
- If the input includes a prior summary, EXTEND it — do not discard earlier facts.\n\
  Carry forward completed work, preserved task IDs, and the original goal verbatim.\n\
- Preserve exact task IDs, file paths, and error messages.\n\
- Distinguish clearly between work COMPLETED and work PENDING. Do not relabel\n\
  finished work as pending.\n\
- Capture key user requirements and goals; skip details irrelevant to the task.\n\
- Be concise but lossless about decisions, errors, and current state. Drop raw tool\n\
  output and file bodies (paths + what was learned is enough).";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Append `count` calls to `tool`, each with the result `result` gives it.
    fn tool_calls(messages: &mut Vec<Value>, count: usize, tool: &str, result: impl Fn(usize) -> String) {
        for i in 0..count {
            messages.push(json!({"role":"assistant","content":null,"tool_calls":[
                {"id":format!("c{i}"),"type":"function","function":{"name":tool,"arguments":"{}"}}
            ]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("c{i}"),"name":tool,"content":result(i)}));
        }
    }

    #[test]
    fn small_contexts_keep_every_finding_verbatim() {
        // Regression: microcompaction used to blank tool results every turn,
        // even on a tiny context, so the model forgot findings and re-read in a
        // loop. Below the recent-window budget, nothing is shed.
        let budget =
            CompactionBudget { compact_at_chars: 100_000, recent_budget_chars: 30_000, summary_budget_chars: 4_000 };
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        tool_calls(&mut messages, 20, "read_file", |i| format!("finding {i}"));
        assert!(!should_microcompact(&messages, &budget));
        // Mirror compact()'s policy: under threshold, nothing is blanked.
        if should_microcompact(&messages, &budget) {
            microcompact(&mut messages, &CompactionBudget::default());
        }
        assert!(
            !messages.iter().any(|m| m["content"].as_str().is_some_and(|c| c.starts_with(SENTINEL_PREFIX))),
            "tool results must survive on a small context"
        );
    }

    #[test]
    fn large_contexts_trim_stale_tool_output_before_summarizing() {
        // Above the recent-window budget but below the ceiling: microcompaction
        // trims old bodies (token savings) without invoking the summarizer.
        let budget =
            CompactionBudget { compact_at_chars: 1_000_000, recent_budget_chars: 1_000, summary_budget_chars: 4_000 };
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        tool_calls(&mut messages, 20, "read_file", |_| "x".repeat(200));
        assert!(should_microcompact(&messages, &budget));
        assert!(!under_pressure(&messages, &CompactionState::default(), &budget));
        microcompact(&mut messages, &CompactionBudget::default());
        let live = messages
            .iter()
            .filter(|m| m["role"] == "tool" && m["content"].as_str().is_some_and(|c| !c.starts_with(SENTINEL_PREFIX)))
            .count();
        assert_eq!(live, KEEP_RECENT_TOOL_RESULTS);
    }

    #[test]
    fn microcompact_blanks_old_compactable_results_only() {
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        tool_calls(&mut messages, 20, "read_file", |i| format!("big file body {i}"));
        microcompact(&mut messages, &CompactionBudget::default());
        // The most recent 8 read_file results stay live; older ones become the sentinel.
        let live = messages
            .iter()
            .filter(|m| {
                m["role"] == "tool"
                    && m["name"] == "read_file"
                    && m["content"].as_str().is_some_and(|c| !c.starts_with(SENTINEL_PREFIX))
            })
            .count();
        assert_eq!(live, KEEP_RECENT_TOOL_RESULTS);
        // Sentinel tool messages are preserved (pairing intact), not removed.
        let tools = messages.iter().filter(|m| m["role"] == "tool").count();
        assert_eq!(tools, 20);
    }

    #[test]
    fn microcompact_leaves_non_compactable_tools_alone() {
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        tool_calls(&mut messages, 20, "edit_file", |i| format!("edited {i}"));
        microcompact(&mut messages, &CompactionBudget::default());
        let edited = messages
            .iter()
            .filter(|m| m["role"] == "tool" && m["content"].as_str().is_some_and(|c| c.starts_with("edited")))
            .count();
        assert_eq!(edited, 20);
    }

    /// Blanking a result outright erased which file it came from, so the model
    /// re-read it — the loop microcompaction exists to prevent.
    #[test]
    fn a_shrunk_result_still_says_what_it_was() {
        let content = "    1 | fn parse() {\n    2 |     todo!()\n";
        let shrunk = shrink_tool_result("read_file", "src/parser.rs", content);
        assert!(shrunk.starts_with(SENTINEL_PREFIX));
        assert!(shrunk.contains("src/parser.rs"), "the path must survive: {shrunk}");
        assert!(shrunk.contains("read_file"), "the tool must survive");
        assert!(shrunk.len() < content.len() + 120, "the placeholder must stay small");
    }

    /// A single `read_file` result is line-numbered content with no path in it,
    /// so the preview alone cannot identify the file. The path has to come from
    /// the call that produced the result.
    #[test]
    fn the_path_is_recovered_from_the_tool_call() {
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        for i in 0..20 {
            messages.push(json!({"role":"assistant","content":null,"tool_calls":[
                {"id":format!("c{i}"),"type":"function","function":{
                    "name":"read_file",
                    "arguments":format!("{{\"path\":\"src/module_{i}.rs\"}}")
                }}
            ]}));
            messages.push(json!({
                "role":"tool","tool_call_id":format!("c{i}"),"name":"read_file",
                "content":format!("    1 | fn thing_{i}() {{}}")
            }));
        }
        microcompact(&mut messages, &CompactionBudget::default());
        let shrunk = messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .find(|c| c.starts_with(SENTINEL_PREFIX))
            .expect("something should have been shrunk");
        assert!(shrunk.contains("src/module_0.rs"), "the file must still be identifiable: {shrunk}");
    }

    #[test]
    fn shrinking_is_idempotent_across_turns() {
        let once = shrink_tool_result("grep", "todo", "src/a.rs:1: todo\nsrc/b.rs:2: todo");
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        tool_calls(&mut messages, 20, "grep", |_| once.clone());
        microcompact(&mut messages, &CompactionBudget::default());
        // Nothing should have been wrapped a second time.
        assert!(
            !messages.iter().any(|m| m["content"].as_str().is_some_and(|c| c.matches(SENTINEL_PREFIX).count() > 1)),
            "an already-shrunk result must be left alone"
        );
    }

    /// Twelve large results can exceed the whole recent budget, so the count
    /// limit alone left the tail bloated.
    #[test]
    fn the_hot_tail_is_bounded_by_size_as_well_as_count() {
        let big = "x".repeat(5_000);
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        tool_calls(&mut messages, 12, "read_file", |_| big.clone());
        let budget =
            CompactionBudget { compact_at_chars: 100_000, recent_budget_chars: 20_000, summary_budget_chars: 4_000 };
        microcompact(&mut messages, &budget);
        let live: usize = messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .filter(|m| m["content"].as_str().is_some_and(|c| !c.starts_with(SENTINEL_PREFIX)))
            .map(message_chars_one)
            .sum();
        assert!(live <= budget.recent_budget_chars / 2, "the live tail should fit its byte budget, got {live}");
        // The count limit alone would have kept all twelve.
        assert!(live < 12 * 5_000);
    }

    /// The summary is counted by `under_pressure`, so an unbounded one keeps
    /// the context over threshold on its own and fires a summariser call every
    /// turn, growing it further.
    #[test]
    fn an_oversized_summary_is_cut_back() {
        let long = (0..400).map(|n| format!("fact {n}")).collect::<Vec<_>>().join("\n");
        let bounded = bound_summary(long.clone(), 500);
        assert!(bounded.len() <= 500 + 80, "got {} chars", bounded.len());
        assert!(bounded.contains("truncated"), "the loss is stated");
        // The tail is what survives — the most recent state is what the next
        // turn needs.
        assert!(bounded.contains("fact 399"));
        assert!(!bounded.contains("fact 0\n"));

        // A summary within budget is returned untouched.
        assert_eq!(bound_summary("short".to_owned(), 500), "short");
    }

    #[test]
    fn the_opening_turn_is_not_summarised_away() {
        // system + original user + assistant/tool pairs: last user is still
        // the opening prompt. Rolling-summary must refuse, or the next
        // request is just that prompt again.
        let mut messages =
            vec![json!({"role":"system","content":"rules"}), json!({"role":"user","content":"fix the importer"})];
        tool_calls(&mut messages, 8, "read_file", |i| format!("finding {i}"));
        assert!(opening_turn_still_live(&messages));
        assert_eq!(last_user_index(&messages), Some(1));

        messages.push(json!({"role":"user","content":"also run the tests"}));
        assert!(!opening_turn_still_live(&messages));
        assert_eq!(last_user_index(&messages), Some(messages.len() - 1));
    }

    #[test]
    fn a_zero_recent_budget_still_keeps_the_current_user_turn() {
        // Defense in depth if the budget arithmetic ever goes to zero again:
        // the cut is clamped to the last user message so the current turn
        // is not dropped.
        let messages = vec![
            json!({"role":"system","content":"rules"}),
            json!({"role":"user","content":"first"}),
            json!({"role":"assistant","content":"working"}),
            json!({"role":"user","content":"second"}),
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"c1","type":"function","function":{"name":"read_file","arguments":"{}"}}
            ]}),
            json!({"role":"tool","tool_call_id":"c1","name":"read_file","content":"body"}),
        ];
        let head_end = first_legal_cut_at_or_after(&messages, KEEP_FIRST);
        let cut = find_tail_cut(&messages, 0, head_end).min(last_user_index(&messages).unwrap_or(messages.len()));
        assert_eq!(cut, 3, "current user turn must survive a zero recent budget");
        assert!(cut > head_end);
    }

    #[test]
    fn find_tail_cut_respects_tool_group_boundaries() {
        // system, user, assistant(tool_call), tool(result), assistant(text)
        let messages = vec![
            json!({"role":"system","content":"rules"}),
            json!({"role":"user","content":"do thing"}),
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"c1","type":"function","function":{"name":"read_file","arguments":"{}"}}
            ]}),
            json!({"role":"tool","tool_call_id":"c1","name":"read_file","content":"body"}),
            json!({"role":"assistant","content":"done"}),
        ];
        // Tiny budget forces the tail to shrink; the cut must never land on the
        // tool message (index 3) — it must jump to index 4.
        let cut = find_tail_cut(&messages, 30, 2);
        assert_ne!(cut, 3, "cut must not orphan the tool result from its call");
    }

    #[test]
    fn strip_analysis_removes_scratchpad() {
        let text = "prefix\n<analysis>secret thoughts\nmore</analysis>\nreal summary";
        assert_eq!(strip_analysis(text), "prefix\n\nreal summary");
        // Unclosed block: drop to end.
        assert_eq!(strip_analysis("a<analysis>stuff"), "a");
        // No block: unchanged.
        assert_eq!(strip_analysis("just summary"), "just summary");
    }

    #[test]
    fn strip_tool_bodies_removes_from_middle_out() {
        let range: Vec<Value> = (0..5)
            .map(|i| json!({"role":"tool","tool_call_id":format!("t{i}"),"name":"read_file","content":format!("body{i}")}))
            .collect();
        let stripped = strip_tool_bodies(&range, 40); // 2 of 5 removed
        let omitted = stripped.iter().filter(|m| m["content"].as_str() == Some(TOOL_BODY_OMITTED)).count();
        assert_eq!(omitted, 2);
        // Middle-out: index 2 (center) must be among the removed.
        assert_eq!(stripped[2]["content"].as_str(), Some(TOOL_BODY_OMITTED));
    }

    #[test]
    fn compaction_state_prompt_context_round_trip() {
        let state = CompactionState::new(Some("did X".to_owned()));
        assert!(state.prompt_context().contains("did X"));
        assert!(CompactionState::default().prompt_context().is_empty());
    }

    /// A turn gets a clone of the session's state. When the clone was a copy,
    /// the summary a turn wrote was gone when it ended.
    #[test]
    fn a_clone_shares_the_summary_and_a_detached_copy_does_not() {
        let state = CompactionState::default();
        let turn = state.clone();
        let saved = state.detached();
        turn.set("did X".to_owned());
        assert_eq!(state.snapshot().as_deref(), Some("did X"));
        assert_eq!(saved.snapshot(), None);
    }

    #[test]
    fn the_saved_shape_is_unchanged() {
        let state = CompactionState::new(Some("did X".to_owned()));
        assert_eq!(serde_json::to_value(&state).unwrap(), json!({"running_summary": "did X"}));
        assert_eq!(serde_json::to_value(CompactionState::default()).unwrap(), json!({}));
        let loaded: CompactionState = serde_json::from_value(json!({"running_summary": "did Y"})).unwrap();
        assert_eq!(loaded.snapshot().as_deref(), Some("did Y"));
        let empty: CompactionState = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty.snapshot(), None);
    }

    /// system, opening prompt, a long middle of tool work, then the last
    /// exchange.
    fn long_session(body: usize) -> Vec<Value> {
        let mut messages =
            vec![json!({"role":"system","content":"rules"}), json!({"role":"user","content":"fix the importer"})];
        tool_calls(&mut messages, 10, "edit_file", |_| "y".repeat(body));
        messages.push(json!({"role":"user","content":"and the tests?"}));
        messages.push(json!({"role":"assistant","content":"They pass."}));
        messages
    }

    #[test]
    fn the_marks_fall_where_compaction_steps_in() {
        let budget = CompactionBudget::default();
        let marks = marks(&budget, true);
        assert_eq!(marks.summary, budget.compact_at_chars / crate::model_info::CHARS_PER_TOKEN);
        assert!(marks.trim < marks.summary, "{marks:?}");
        let idle = marks.idle.expect("idle compaction is on");
        assert!(idle <= marks.summary / 2 && idle <= IDLE_MIN_TOKENS, "{marks:?}");
        assert_eq!(super::marks(&budget, false).idle, None, "nothing to mark when it is off");
    }

    #[test]
    fn idle_compaction_waits_for_a_context_worth_it() {
        let budget = CompactionBudget {
            compact_at_chars: 2_000_000,
            recent_budget_chars: 600_000,
            summary_budget_chars: 300_000,
        };
        let state = CompactionState::default();
        // Under the 100k-token bar: resending it is cheap.
        assert!(!idle_worthwhile(&long_session(30_000), &state, &budget));
        assert!(idle_worthwhile(&long_session(45_000), &state, &budget));
        // The summary is sent with every request, so it counts.
        let carried = CompactionState::new(Some("z".repeat(200_000)));
        assert!(idle_worthwhile(&long_session(30_000), &carried, &budget));

        // On a small window the bar is half the compaction trigger.
        let small =
            CompactionBudget { compact_at_chars: 100_000, recent_budget_chars: 30_000, summary_budget_chars: 15_000 };
        assert!(!idle_worthwhile(&long_session(4_000), &state, &small));
        assert!(idle_worthwhile(&long_session(6_000), &state, &small));
    }

    #[test]
    fn the_idle_cut_keeps_the_head_and_the_last_exchange() {
        let budget =
            CompactionBudget { compact_at_chars: 100_000, recent_budget_chars: 30_000, summary_budget_chars: 15_000 };
        // Each tool result outgrows the 10k tail on its own.
        let messages = long_session(12_000);
        let (head_end, cut) = idle_cut(&messages, &budget).unwrap();
        assert_eq!(head_end, KEEP_FIRST, "the system prompt and opening prompt stay");
        assert_eq!(messages[cut]["content"], "and the tests?", "the last exchange stays whole");
        assert_ne!(messages[cut]["role"], "tool", "a result never loses its call");
    }

    /// The tail budget is small, but the reply the person was reading when
    /// they left is kept whatever its size.
    #[test]
    fn the_idle_cut_keeps_a_last_reply_larger_than_the_tail() {
        let budget =
            CompactionBudget { compact_at_chars: 100_000, recent_budget_chars: 30_000, summary_budget_chars: 15_000 };
        let mut messages = long_session(6_000);
        let last = messages.len() - 1;
        messages[last]["content"] = json!("r".repeat(50_000));
        let (_, cut) = idle_cut(&messages, &budget).unwrap();
        assert!(cut <= last, "the last reply is never summarised away");
    }

    #[test]
    fn a_short_session_has_nothing_to_fold() {
        let budget =
            CompactionBudget { compact_at_chars: 100_000, recent_budget_chars: 30_000, summary_budget_chars: 15_000 };
        let messages =
            vec![json!({"role":"system","content":"rules"}), json!({"role":"user","content":"x".repeat(80_000)})];
        assert_eq!(idle_cut(&messages, &budget), None);
        assert!(!idle_worthwhile(&messages, &CompactionState::default(), &budget));
    }

    /// One divider for the whole gap: a marker already in the range carries
    /// what it counted into the new one. Context layers and notes are
    /// scaffolding, not conversation, so they are not counted.
    #[test]
    fn a_marker_counts_what_it_folds_and_what_the_last_one_did() {
        let mut range = vec![
            Folded { messages: 40, tokens: 90_000, summarised: false }.marker().unwrap(),
            json!({"role":"system","content":"<session_context layer=\"memory\">\nnotes\n</session_context>"}),
            json!({"role":"user","content":"and the tests?"}),
        ];
        tool_calls(&mut range, 2, "read_file", |_| "x".repeat(4_000));
        let folded = Folded::of(&range, true);
        assert_eq!(folded.messages, 40 + 5);
        let chars: usize = range[2..].iter().map(message_chars_one).sum();
        assert_eq!(folded.tokens, 90_000 + chars / crate::model_info::CHARS_PER_TOKEN);
        assert!(folded.summarised, "the new summary stands in for what the last compaction dropped");
        assert_eq!(Folded::marked_by(&folded.marker().unwrap()), Some(folded));
        assert_eq!(Folded::marked_by(&range[1]), None);
    }

    /// A compaction can fold nothing but context layers, which the next
    /// request sends again anyway. No conversation went, so no divider says
    /// "0 messages" were summarised.
    #[test]
    fn folding_only_context_layers_leaves_no_marker() {
        let layer =
            json!({"role":"system","content":"<session_context layer=\"permissions\">\nAUTO\n</session_context>"});
        assert_eq!(Folded::of(&[layer], true).marker(), None);
    }

    /// The marker is the transcript's: the model reads the history without
    /// it, and the context measured for compaction leaves it out.
    #[test]
    fn the_model_reads_the_history_without_its_markers() {
        let plain = vec![
            json!({"role":"system","content":"rules"}),
            json!({"role":"user","content":"fix the importer"}),
            json!({"role":"assistant","content":"done"}),
        ];
        assert!(matches!(without_markers(&plain), Cow::Borrowed(_)));
        let mut marked = plain.clone();
        marked.insert(2, Folded { messages: 3, tokens: 1_000, summarised: true }.marker().unwrap());
        assert_eq!(without_markers(&marked).as_ref(), plain.as_slice());
        assert_eq!(message_chars(&marked), message_chars(&plain));
        // Only a system message carrying the field is one.
        assert!(!is_marker(&json!({"role":"user","content":"hi","compacted":{"messages":1}})));
        assert!(!is_marker(&json!({"role":"system","content":"rules"})));
    }

    /// Once idle compaction has folded all but the last reply away, its
    /// marker is all there is to fold: another quiet spell has nothing to
    /// summarise.
    #[test]
    fn a_lone_marker_is_not_folded_again_when_idle() {
        let budget =
            CompactionBudget { compact_at_chars: 100_000, recent_budget_chars: 30_000, summary_budget_chars: 15_000 };
        let messages = vec![
            json!({"role":"system","content":"rules"}),
            json!({"role":"user","content":"fix the importer"}),
            Folded { messages: 22, tokens: 30_000, summarised: true }.marker().unwrap(),
            json!({"role":"assistant","content":"r".repeat(60_000)}),
        ];
        assert_eq!(idle_cut(&messages, &budget), None);
        assert!(!idle_worthwhile(&messages, &CompactionState::default(), &budget));
    }

    /// Under pressure likewise: a turn that began right after the marker
    /// leaves the summariser nothing to read, so nothing is asked of it and
    /// the history stays as it is.
    #[tokio::test]
    async fn a_lone_marker_is_not_summarised_again_under_pressure() {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            no_session: true,
            ..crate::config::Config::for_endpoint(
                directory.path().to_path_buf(),
                "http://127.0.0.1:9/v1".into(),
                "test-model".into(),
                crate::config::AbacusPaths::under(directory.path().join("home")),
            )
        };
        let provider = Provider::new(&config).unwrap();
        let mut messages = vec![
            json!({"role":"system","content":"rules"}),
            json!({"role":"user","content":"fix the importer"}),
            Folded { messages: 12, tokens: 40_000, summarised: true }.marker().unwrap(),
            json!({"role":"user","content":"now the exporter"}),
        ];
        tool_calls(&mut messages, 3, "edit_file", |_| "y".repeat(6_000));
        let before = messages.clone();
        let state = CompactionState::new(Some("did the importer".to_owned()));
        let budget =
            CompactionBudget { compact_at_chars: 10_000, recent_budget_chars: 3_000, summary_budget_chars: 1_000 };
        compact(&Summariser::detached(&provider), &mut messages, &state, &budget, &AtomicBool::new(false)).await;
        assert_eq!(messages, before);
        assert_eq!(state.snapshot().as_deref(), Some("did the importer"));
    }
}
