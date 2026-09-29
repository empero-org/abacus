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

use std::sync::atomic::AtomicBool;

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

/// Tools whose results are large and re-derivable from disk, so their old
/// output is safe to placeholder.
const COMPACTABLE_TOOLS: &[&str] = &[
    "read_file",
    "read_files",
    "grep",
    "glob",
    "list_files",
    "run_command",
    "git_diff",
    "git_show",
    "git_blame",
    "git_log",
];

/// Persisted rolling-summary state, carried across compactions and session resume.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompactionState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_summary: Option<String>,
}

impl CompactionState {
    pub fn new(running_summary: Option<String>) -> Self {
        Self { running_summary }
    }

    pub fn snapshot(&self) -> Option<String> {
        self.running_summary.clone()
    }

    /// Context injected as a system message every turn so the model retains the
    /// long-arc state of the goal/loop across compactions.
    pub fn prompt_context(&self) -> String {
        match &self.running_summary {
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

/// Whether the next `compact` call is likely to run rolling-summary
/// compaction — the tier that erases verbatim history. Exposed so the agent
/// can run its reflection pass first, while the evidence still exists.
/// Microcompaction inside `compact` may still relieve the pressure, in which
/// case the reflection simply ran a little early.
pub fn needs_summary(
    messages: &[Value],
    state: &CompactionState,
    budget: &CompactionBudget,
) -> bool {
    under_pressure(messages, state, budget) && messages.len() > KEEP_FIRST + 1
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
    state: &mut CompactionState,
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
    if cut <= head_end {
        return;
    }

    let to_summarize: Vec<Value> = messages[head_end..cut].to_vec();
    let note = match summarize(summariser, state, messages, &to_summarize, budget, cancel).await {
        Ok(summary) => {
            // Backstop: a model that ignores the compression directive must not
            // be able to reinstate the growth loop, so an oversized summary is
            // cut structurally. The tail is kept because the most recent state
            // is what the next turn needs.
            state.running_summary = Some(bound_summary(summary, budget.summary_budget_chars));
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
                to_summarize.len()
            )}))
        }
    };
    // The middle gives way to the note, when there is one. What is left may
    // still carry stale output.
    messages.splice(head_end..cut, note);
    microcompact(messages, budget);
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
    let mut total = message_chars(messages);
    if let Some(summary) = &state.running_summary {
        // The summary is wrapped in a template; add the wrapper overhead too.
        total += summary.len() + 200;
    }
    total > budget.compact_at_chars
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
            message["role"] == "tool"
                && message["name"].as_str().is_some_and(|name| COMPACTABLE_TOOLS.contains(&name))
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
            (call["id"].as_str() == Some(id))
                .then(|| call.pointer("/function/arguments")?.as_str())
                .flatten()
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
    state: &CompactionState,
    conversation: &[Value],
    range: &[Value],
    budget: &CompactionBudget,
    cancel: &AtomicBool,
) -> Result<String, String> {
    if summariser.in_context {
        match summarize_in_context(summariser, state, conversation, budget, cancel).await {
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
    summarize_range(summariser.provider, state, range, budget, cancel).await
}

/// Summarise by appending the instruction to the live conversation.
///
/// The request is the conversation's own message list plus one trailing user
/// turn, and it carries the same tools, so everything up to the last cache
/// breakpoint is a cache read. The model is addressed in the second person
/// about its own context, because that is what it is looking at.
async fn summarize_in_context(
    summariser: &Summariser<'_>,
    state: &CompactionState,
    conversation: &[Value],
    budget: &CompactionBudget,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let mut directive = String::from(IN_CONTEXT_PROMPT);
    if let Some(prior) =
        state.running_summary.as_deref().filter(|summary| !summary.trim().is_empty())
    {
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
    state: &CompactionState,
    range: &[Value],
    budget: &CompactionBudget,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let prompt = json!({"role":"system","content": SUMMARY_PROMPT});
    let prior = state.running_summary.as_deref().filter(|s| !s.trim().is_empty());
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
            messages.push(
                json!({"role":"user","content":format!("Existing summary so far:\n{summary}")}),
            );
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
    fn tool_calls(
        messages: &mut Vec<Value>,
        count: usize,
        tool: &str,
        result: impl Fn(usize) -> String,
    ) {
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
        let budget = CompactionBudget {
            compact_at_chars: 100_000,
            recent_budget_chars: 30_000,
            summary_budget_chars: 4_000,
        };
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        tool_calls(&mut messages, 20, "read_file", |i| format!("finding {i}"));
        assert!(!should_microcompact(&messages, &budget));
        // Mirror compact()'s policy: under threshold, nothing is blanked.
        if should_microcompact(&messages, &budget) {
            microcompact(&mut messages, &CompactionBudget::default());
        }
        assert!(
            !messages
                .iter()
                .any(|m| m["content"].as_str().is_some_and(|c| c.starts_with(SENTINEL_PREFIX))),
            "tool results must survive on a small context"
        );
    }

    #[test]
    fn large_contexts_trim_stale_tool_output_before_summarizing() {
        // Above the recent-window budget but below the ceiling: microcompaction
        // trims old bodies (token savings) without invoking the summarizer.
        let budget = CompactionBudget {
            compact_at_chars: 1_000_000,
            recent_budget_chars: 1_000,
            summary_budget_chars: 4_000,
        };
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        tool_calls(&mut messages, 20, "read_file", |_| "x".repeat(200));
        assert!(should_microcompact(&messages, &budget));
        assert!(!under_pressure(&messages, &CompactionState::default(), &budget));
        microcompact(&mut messages, &CompactionBudget::default());
        let live = messages
            .iter()
            .filter(|m| {
                m["role"] == "tool"
                    && m["content"].as_str().is_some_and(|c| !c.starts_with(SENTINEL_PREFIX))
            })
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
            .filter(|m| {
                m["role"] == "tool"
                    && m["content"].as_str().is_some_and(|c| c.starts_with("edited"))
            })
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
        assert!(
            shrunk.contains("src/module_0.rs"),
            "the file must still be identifiable: {shrunk}"
        );
    }

    #[test]
    fn shrinking_is_idempotent_across_turns() {
        let once = shrink_tool_result("grep", "todo", "src/a.rs:1: todo\nsrc/b.rs:2: todo");
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        tool_calls(&mut messages, 20, "grep", |_| once.clone());
        microcompact(&mut messages, &CompactionBudget::default());
        // Nothing should have been wrapped a second time.
        assert!(
            !messages.iter().any(|m| m["content"]
                .as_str()
                .is_some_and(|c| c.matches(SENTINEL_PREFIX).count() > 1)),
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
        let budget = CompactionBudget {
            compact_at_chars: 100_000,
            recent_budget_chars: 20_000,
            summary_budget_chars: 4_000,
        };
        microcompact(&mut messages, &budget);
        let live: usize = messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .filter(|m| m["content"].as_str().is_some_and(|c| !c.starts_with(SENTINEL_PREFIX)))
            .map(message_chars_one)
            .sum();
        assert!(
            live <= budget.recent_budget_chars / 2,
            "the live tail should fit its byte budget, got {live}"
        );
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
        let mut messages = vec![
            json!({"role":"system","content":"rules"}),
            json!({"role":"user","content":"fix the importer"}),
        ];
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
        let cut = find_tail_cut(&messages, 0, head_end)
            .min(last_user_index(&messages).unwrap_or(messages.len()));
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
        let omitted =
            stripped.iter().filter(|m| m["content"].as_str() == Some(TOOL_BODY_OMITTED)).count();
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
}
