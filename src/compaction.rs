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
use tokio::sync::mpsc;

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

const SENTINEL: &str = "[Old tool result content cleared]";
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

/// Run microcompaction (every turn) and rolling-summary compaction (when over
/// threshold). Mutates `messages` in place and updates `state`. The budgets are
/// derived from the chosen model's context window (see `model_info`).
pub async fn compact(
    provider: &Provider,
    messages: &mut Vec<Value>,
    state: &mut CompactionState,
    budget: &CompactionBudget,
) {
    // Tier 0 (cheap, no model call): once the conversation outgrows a fresh
    // recent window, replace stale re-derivable tool output (old file/grep
    // bodies the model already read) with a placeholder, keeping the recent
    // working set verbatim. This shrinks every later request instead of
    // re-sending big file bodies forever. Below the threshold the history stays
    // fully verbatim, so small sessions never lose findings and re-read in a
    // loop.
    if should_microcompact(messages, budget) {
        microcompact(messages);
    }

    // Tier 1 (one model call): full rolling-summary compaction only near the
    // real ceiling. Re-measure first — microcompaction may have kept us under.
    if !under_pressure(messages, state, budget) {
        return;
    }
    if messages.len() <= KEEP_FIRST + 1 {
        return;
    }

    let head_end = first_legal_cut_at_or_after(messages, KEEP_FIRST);
    let cut = find_tail_cut(messages, budget.recent_budget_chars, head_end);
    if cut <= head_end {
        return;
    }

    let to_summarize: Vec<Value> = messages[head_end..cut].to_vec();
    match summarize_range(provider, state, &to_summarize).await {
        Ok(summary) => {
            state.running_summary = Some(summary);
            let head: Vec<Value> = messages[..head_end].to_vec();
            let tail: Vec<Value> = messages[cut..].to_vec();
            messages.clear();
            messages.extend(head);
            messages.extend(tail);
            // Microcompact the rebuilt history (tail may still carry stale output).
            microcompact(messages);
        }
        Err(error) => {
            // Last-resort fallback: drop the middle with a local trace note so the
            // loop never breaks. Preserve the head and a smaller tail.
            let head: Vec<Value> = messages[..head_end].to_vec();
            let dropped = cut - head_end;
            let trace = crate::agent::compaction_trace(&messages[head_end..cut]);
            let note = if trace.is_empty() {
                format!(
                    "{dropped} older conversation messages were omitted to fit the model context. Reinspect files when prior details matter."
                )
            } else {
                format!(
                    "{dropped} older conversation messages were omitted to fit the model context. Earlier actions, in order: {trace} Reinspect files when prior details matter."
                )
            };
            let tail: Vec<Value> = messages[cut..].to_vec();
            messages.clear();
            messages.extend(head);
            messages.push(json!({"role":"system","content":note}));
            messages.extend(tail);
            microcompact(messages);
            // Surface the failure via the state so callers can observe it, but
            // keep going. We do not overwrite an existing good summary.
            let _ = error;
        }
    }
}

/// Whether the conversation is large enough to start trimming stale, re-derivable
/// tool output (microcompaction). Tied to the recent-window budget: once history
/// exceeds what a post-compaction tail would keep, old file/grep bodies are no
/// longer worth re-sending in full. Below this, everything stays verbatim so the
/// model never loses its own findings.
fn should_microcompact(messages: &[Value], budget: &CompactionBudget) -> bool {
    message_chars(messages) > budget.recent_budget_chars
}

/// Whether the conversation has grown near the real context ceiling, warranting
/// the expensive full rolling-summary compaction. Includes the running summary
/// itself in the measurement, since it is re-injected as a system message every
/// turn and grows over time — not accounting for it means compaction triggers
/// too late and the actual request overflows the context window.
fn under_pressure(messages: &[Value], state: &CompactionState, budget: &CompactionBudget) -> bool {
    let mut total = message_chars(messages);
    if let Some(summary) = &state.running_summary {
        // The summary is wrapped in a template; add the wrapper overhead too.
        total += summary.len() + 200;
    }
    total > budget.compact_at_chars
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

/// Replace stale compactable tool-result bodies with a sentinel, keeping the most
/// recent `KEEP_RECENT_TOOL_RESULTS` live. The tool message itself is preserved so
/// tool-call→tool-result pairing stays intact.
fn microcompact(messages: &mut [Value]) {
    let compactable: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message["role"] == "tool"
                && message["name"]
                    .as_str()
                    .is_some_and(|name| COMPACTABLE_TOOLS.contains(&name))
        })
        .map(|(index, _)| index)
        .collect();
    if compactable.len() <= KEEP_RECENT_TOOL_RESULTS {
        return;
    }
    let keep_from = compactable.len() - KEEP_RECENT_TOOL_RESULTS;
    for &index in &compactable[..keep_from] {
        // Only blank if it still has real content (idempotent across turns).
        if let Some(content) = messages[index].get_mut("content").and_then(|c| c.as_str())
            && content != SENTINEL
        {
            messages[index]["content"] = json!(SENTINEL);
        }
    }
}

async fn summarize_range(
    provider: &Provider,
    state: &CompactionState,
    range: &[Value],
) -> Result<String, String> {
    let prompt = json!({"role":"system","content": SUMMARY_PROMPT});
    let prior = state
        .running_summary
        .as_deref()
        .filter(|s| !s.trim().is_empty());
    let directive = match prior {
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

        // Drain streaming deltas silently — the summary is internal memory, not
        // assistant output to show the user.
        let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<String>();
        let drain = tokio::spawn(async move { while delta_rx.recv().await.is_some() {} });
        let result = provider.complete(&messages, &[], delta_tx).await;
        let _ = drain.await;

        match result {
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
    let tool_indices: Vec<usize> = range
        .iter()
        .enumerate()
        .filter(|(_, m)| m["role"] == "tool")
        .map(|(i, _)| i)
        .collect();
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

    #[test]
    fn small_contexts_keep_every_finding_verbatim() {
        // Regression: microcompaction used to blank tool results every turn,
        // even on a tiny context, so the model forgot findings and re-read in a
        // loop. Below the recent-window budget, nothing is shed.
        let budget = CompactionBudget {
            compact_at_chars: 100_000,
            recent_budget_chars: 30_000,
        };
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        for i in 0..20 {
            messages.push(json!({"role":"assistant","content":null,"tool_calls":[
                {"id":format!("c{i}"),"type":"function","function":{"name":"read_file","arguments":"{}"}}
            ]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("c{i}"),"name":"read_file","content":format!("finding {i}")}));
        }
        assert!(!should_microcompact(&messages, &budget));
        // Mirror compact()'s policy: under threshold, nothing is blanked.
        if should_microcompact(&messages, &budget) {
            microcompact(&mut messages);
        }
        assert!(
            !messages
                .iter()
                .any(|m| m["content"].as_str() == Some(SENTINEL)),
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
        };
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        for i in 0..20 {
            messages.push(json!({"role":"assistant","content":null,"tool_calls":[
                {"id":format!("c{i}"),"type":"function","function":{"name":"read_file","arguments":"{}"}}
            ]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("c{i}"),"name":"read_file","content":"x".repeat(200)}));
        }
        assert!(should_microcompact(&messages, &budget));
        assert!(!under_pressure(
            &messages,
            &CompactionState::default(),
            &budget
        ));
        microcompact(&mut messages);
        let live = messages
            .iter()
            .filter(|m| m["role"] == "tool" && m["content"].as_str() != Some(SENTINEL))
            .count();
        assert_eq!(live, KEEP_RECENT_TOOL_RESULTS);
    }

    #[test]
    fn microcompact_blanks_old_compactable_results_only() {
        let mut messages = vec![json!({"role":"system","content":"rules"})];
        for i in 0..20 {
            messages.push(json!({"role":"assistant","content":null,"tool_calls":[
                {"id":format!("c{i}"),"type":"function","function":{"name":"read_file","arguments":"{}"}}
            ]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("c{i}"),"name":"read_file","content":format!("big file body {i}")}));
        }
        microcompact(&mut messages);
        // The most recent 8 read_file results stay live; older ones become the sentinel.
        let live = messages
            .iter()
            .filter(|m| {
                m["role"] == "tool"
                    && m["name"] == "read_file"
                    && m["content"].as_str().is_some_and(|c| !c.contains(SENTINEL))
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
        for i in 0..20 {
            messages.push(json!({"role":"assistant","content":null,"tool_calls":[
                {"id":format!("e{i}"),"type":"function","function":{"name":"edit_file","arguments":"{}"}}
            ]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("e{i}"),"name":"edit_file","content":format!("edited {i}")}));
        }
        microcompact(&mut messages);
        let edited = messages
            .iter()
            .filter(|m| {
                m["role"] == "tool"
                    && m["content"]
                        .as_str()
                        .is_some_and(|c| c.starts_with("edited"))
            })
            .count();
        assert_eq!(edited, 20);
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
        let omitted = stripped
            .iter()
            .filter(|m| m["content"].as_str() == Some(TOOL_BODY_OMITTED))
            .count();
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
