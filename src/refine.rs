//! `/refine`: the reflection pass that writes the continual harness.
//!
//! This replaces the older `rethink` pass. The trigger points are the same and
//! deliberately so — a long turn, and unconditionally just before rolling
//! compaction erases the verbatim evidence — because that is the moment the
//! trajectory still says what actually happened.
//!
//! What changed is the write. Rethink drove a restricted toolset over several
//! steps and whatever it recorded was final. Refine emits one JSON proposal of
//! create/update/delete edits, which [`crate::harness`] applies against
//! versioned entries with before/after snapshots — so every change carries the
//! evidence that motivated it and can be reverted.
//!
//! Two calls, not one. A cheap review gate decides whether the trajectory
//! contains anything worth keeping before the expensive planning call runs at
//! all; most turns have nothing, and rethink paid full price for them anyway.
//!
//! Both calls need parseable JSON in the final text, so both run without
//! reasoning regardless of the session's thinking level: a reasoning model
//! otherwise spends its output budget on visible thinking and returns no
//! final text, which reads as a parse failure for an otherwise fine call.

use std::sync::atomic::AtomicBool;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::harness::{HarnessStore, Lifetime, RefinementProposal, RefinementResult};
use crate::provider::Provider;

/// A turn is "long" — worth reflecting on — from this many tool calls.
pub const LONG_TURN_TOOL_CALLS: usize = 6;
/// How much trajectory the planning call sees, newest-last.
const PLAN_TRAJECTORY_CHARS: usize = 60_000;
/// The gate sees less; it is deciding whether, not what.
const REVIEW_TRAJECTORY_CHARS: usize = 24_000;

// Everything after the framing paragraph: the kinds, the rules, and the reply
// shape. Shared verbatim by the detached and in-context planning prompts so the
// two can never drift into asking for different JSON.
macro_rules! plan_rules {
    () => {
        "\
The three kinds:
- prompt: a narrow behavioural addendum. The base system prompt is immutable \
and must never be restated or rewritten here.
- memory: a durable fact, decision and its reason, or convention.
- subagent: a reusable delegation spec — purpose, instructions, when to use it.

Failure lessons do NOT go in `edits`. They go in the separate `papercuts` \
array, because they are recalled by matching a tripwire against future tool \
output rather than by sitting in the prompt. Recording one as a memory turns a \
trigger into noise. Add a papercut when the turn recovered from a non-obvious \
error or a repeated failure, with 1-6 distinctive tripwire substrings taken \
from the actual failure text — each at least 8 characters and never a generic \
phrase alone ('not found'), so include the identifier the error names.

Rules:
- Prefer few, small, evidence-backed edits. An empty edit list is a good answer.
- Create the smallest thing that fits: a recurring delegation role is a \
subagent spec, a durable fact is a memory, a narrow policy is a prompt note.
- Update or delete an entry the trajectory showed to be wrong or stale. \
Pruning is as valuable as adding.
- Use the exact id from the state overview for update and delete. For create, \
omit id and it is derived from the title.

Return JSON only, with this shape:
{
  \"summary\": \"one sentence\",
  \"rationale\": \"why the trajectory justifies these edits\",
  \"expected_outcome\": \"what should improve, and how it could be checked\",
  \"edits\": [
    {\"action\": \"create|update|delete\", \"kind\": \"prompt|memory|subagent\", \
\"id\": \"stable_id\", \"title\": \"...\", \"content\": \"...\", \
\"path\": \"optional grouping\", \"reason\": \"why this edit\"}
  ],
  \"papercuts\": [
    {\"title\": \"...\", \"description\": \"what went wrong\", \
\"fix\": \"the fix that worked, as an instruction\", \
\"tripwires\": [\"distinctive substring of the failure\"], \
\"references\": [\"optional paths or commands\"]}
  ]
}"
    };
}

const REVIEW_PROMPT: &str = "\
You are Abacus's refinement review gate. Decide whether this finished turn \
contains evidence worth writing into the agent's durable harness.

Say yes when the turn produced something a future turn would genuinely need: a \
convention discovered, a decision and its reason, an architecture fact learned \
the hard way, a correction from the user that should stick, or a harness entry \
now shown to be wrong. Say no for routine work, one-off details, transient tool \
output, and anything the agent merely guessed at. Most turns are a no, and a \
no costs nothing — recording noise is worse than recording nothing.

Return JSON only:
{\"should_refine\": true|false, \"rationale\": \"one short sentence\", \
\"instructions\": \"optional focus for the refiner\"}";

/// The gate, asked from inside the conversation it is judging.
const IN_CONTEXT_REVIEW_PROMPT: &str = "\
Step out of the task for one answer. Looking back at the turn you have just finished, decide \
whether it produced evidence worth writing into your durable harness — the state you carry \
into future sessions.\n\n\
Say yes when the turn produced something a future turn would genuinely need: a convention \
discovered, a decision and its reason, an architecture fact learned the hard way, a correction \
from the user that should stick, or a harness entry now shown to be wrong. Say no for routine \
work, one-off details, transient tool output, and anything you merely guessed at. Most turns \
are a no, and a no costs nothing — recording noise is worse than recording nothing.\n\n\
Do not call a tool. Return JSON only:\n\
{\"should_refine\": true|false, \"rationale\": \"one short sentence\", \
\"instructions\": \"optional focus for the refiner\"}";

/// The planner, asked from inside the conversation it is learning from. Shares
/// [`PLAN_RULES`] with the detached prompt; only the framing differs, because
/// here the evidence is the context rather than a rendering of it.
const IN_CONTEXT_PLAN_PROMPT: &str = concat!(
    "\
Step out of the task for one answer. From the turn you have just finished, emit precise edits \
to your own reusable state. This is like compaction, except you are not summarising the \
conversation — you are updating what you carry into future work. Your current state is listed \
below; the evidence is the conversation you are in. Do not call a tool.\n",
    plan_rules!()
);

const PLAN_PROMPT: &str = concat!(
    "\
You are Abacus's continual-harness refiner. From the finished trajectory, emit \
precise edits to the agent's reusable state. This is like compaction, except \
you are not summarising the conversation — you are updating what the agent \
carries into future work.\n",
    plan_rules!()
);

/// What a finished refinement did, for the transcript.
pub struct RefineOutcome {
    pub applied: usize,
    /// Failure lessons recorded alongside the harness edits.
    pub papercuts: usize,
    pub summary: String,
    pub result: RefinementResult,
}

/// Who reflects, and whether it can do so from inside the live context.
///
/// The detached form renders the finished turn into a `<trajectory>` block and
/// sends it under its own system prompt — a prompt that shares no prefix with
/// the conversation it describes, so both refinement calls are full-price cache
/// misses over content the provider already has cached verbatim.
///
/// The in-context form appends the question to the conversation instead. The
/// trajectory is not rendered at all: the model is looking at the real thing,
/// which is both cheaper and strictly more evidence than a summary of it.
/// Available only when the reflector is the conversation's own model, since
/// another model has a different cache and nothing to hit.
pub struct Reflector<'a> {
    pub provider: &'a Provider,
    pub in_context: bool,
    /// The conversation's tool definitions, sent unchanged on an in-context call
    /// so the cached prefix still matches. See [`crate::compaction::Summariser`].
    pub tools: &'a [Value],
}

impl<'a> Reflector<'a> {
    /// A reflector that builds its own prompt, with no cache to reuse.
    pub fn detached(provider: &'a Provider) -> Self {
        Self {
            provider,
            in_context: false,
            tools: &[],
        }
    }

    /// Ask for one JSON answer, in the live context when that is possible.
    async fn ask(
        &self,
        system: &str,
        in_context_prompt: &str,
        detail: &str,
        conversation: &[Value],
        cancel: &AtomicBool,
    ) -> Option<String> {
        if self.in_context {
            let mut messages = conversation.to_vec();
            messages.push(json!({
                "role": "user",
                "content": format!("{in_context_prompt}\n\n{detail}"),
            }));
            if let Some(reply) = complete(self.provider, &messages, self.tools, cancel).await {
                return Some(reply);
            }
            // Fall through: a refinement is optional, but if the cheap call
            // fails the detached prompt is still worth one attempt.
        }
        let messages = vec![
            json!({"role": "system", "content": system}),
            json!({"role": "user", "content": detail}),
        ];
        complete(self.provider, &messages, &[], cancel).await
    }
}

/// Ask the gate whether this trajectory is worth refining.
pub async fn should_refine(
    reflector: &Reflector<'_>,
    messages: &[Value],
    harness: &HarnessStore,
    cancel: &AtomicBool,
) -> Option<(bool, Option<String>)> {
    let detail = if reflector.in_context {
        format!("<harness_state>\n{}\n</harness_state>", harness.overview())
    } else {
        format!(
            "<harness_state>\n{}\n</harness_state>\n\n<trajectory>\n{}\n</trajectory>",
            harness.overview(),
            trajectory(messages, REVIEW_TRAJECTORY_CHARS)
        )
    };
    let reply = reflector
        .ask(
            REVIEW_PROMPT,
            IN_CONTEXT_REVIEW_PROMPT,
            &detail,
            messages,
            cancel,
        )
        .await?;
    let value = extract_json(&reply).ok()?;
    Some((
        value.get("should_refine").and_then(Value::as_bool) == Some(true),
        value
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|text| !text.trim().is_empty()),
    ))
}

/// Plan and apply a refinement. `lifetime` decides whether the edits persist
/// beyond this session.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    reflector: &Reflector<'_>,
    messages: &[Value],
    harness: &HarnessStore,
    papercuts: &crate::papercuts::PapercutStore,
    lifetime: Lifetime,
    instructions: Option<&str>,
    cancel: &AtomicBool,
) -> Option<RefineOutcome> {
    // Captured before the planning call, which takes seconds. Anything that
    // writes the shared state in that window invalidates the edits that
    // touched it, rather than being silently overwritten.
    let baseline = harness.baseline(lifetime);

    let mut detail = if reflector.in_context {
        format!("<harness_state>\n{}\n</harness_state>", harness.overview())
    } else {
        format!(
            "<harness_state>\n{}\n</harness_state>\n\n<trajectory>\n{}\n</trajectory>",
            harness.overview(),
            trajectory(messages, PLAN_TRAJECTORY_CHARS)
        )
    };
    if let Some(instructions) = instructions {
        detail.push_str(&format!("\n\n<focus>\n{instructions}\n</focus>"));
    }
    detail.push_str(
        "\n\nReturn only the JSON object. If nothing is justified, return an empty edits array.",
    );

    let reply = reflector
        .ask(
            PLAN_PROMPT,
            IN_CONTEXT_PLAN_PROMPT,
            &detail,
            messages,
            cancel,
        )
        .await?;
    let (proposal, drafts) = parse_reply(&reply).ok()?;

    // Papercuts go through the ordinary tool path, so they inherit its
    // validation — the generic-tripwire blocklist above all, which is what
    // stops a lesson from matching every future tool result.
    let mut recorded = 0_usize;
    for draft in &drafts {
        if let Some(reply) = papercuts.execute("papercut_record", &draft.to_string())
            && !reply.starts_with("Error:")
        {
            recorded += 1;
        }
    }

    if proposal.edits.is_empty() && recorded == 0 {
        return None;
    }
    let result = harness.apply(&proposal, lifetime, None, Some(&baseline));
    let applied = result.applied_count();
    if applied == 0 && recorded == 0 {
        return None;
    }
    Some(RefineOutcome {
        applied,
        papercuts: recorded,
        summary: if result.summary.trim().is_empty() {
            format!("{applied} harness edit(s)")
        } else {
            result.summary.clone()
        },
        result,
    })
}

async fn complete(
    provider: &Provider,
    messages: &[Value],
    tools: &[Value],
    cancel: &AtomicBool,
) -> Option<String> {
    // Deltas are discarded: a refinement is bookkeeping, not output.
    let (deltas, _sink) = mpsc::unbounded_channel();
    let completion = provider
        .complete(messages, tools, deltas, cancel)
        .await
        .ok()?;
    if completion.cancelled || completion.content.trim().is_empty() {
        return None;
    }
    Some(completion.content)
}

/// A compact view of the finished turn: what the model said and which tools it
/// ran, without the tool bodies. The evidence for "what did this turn learn" is
/// in the narrative and the sequence, and including outputs would blow the
/// budget on the one thing compaction is already discarding.
fn trajectory(messages: &[Value], budget: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or_default();
        match role {
            "user" | "assistant" => {
                if let Some(content) = message["content"].as_str()
                    && !content.trim().is_empty()
                {
                    lines.push(format!("{role}: {}", content.trim()));
                }
                if let Some(calls) = message["tool_calls"].as_array() {
                    let names: Vec<&str> = calls
                        .iter()
                        .filter_map(|call| call["function"]["name"].as_str())
                        .collect();
                    if !names.is_empty() {
                        lines.push(format!("assistant ran: {}", names.join(", ")));
                    }
                }
            }
            "tool" => {
                // Only whether it failed; the body is deliberately dropped.
                let content = message["content"].as_str().unwrap_or_default();
                if crate::agent::tool_result_failed(content) {
                    lines.push(format!(
                        "tool {} FAILED: {}",
                        message["name"].as_str().unwrap_or("?"),
                        content.chars().take(200).collect::<String>()
                    ));
                }
            }
            _ => {}
        }
    }
    // Newest-last: the tail of a turn is where its conclusions are.
    let mut joined = lines.join("\n");
    if joined.chars().count() > budget {
        let skip = joined.chars().count() - budget;
        joined = joined.chars().skip(skip).collect();
    }
    joined
}

/// Split a reply into the harness proposal and any papercut drafts.
///
/// The drafts are left as raw JSON so they can be handed straight to
/// `papercut_record`; re-typing them here would mean re-implementing that
/// tool's validation, and the two would drift.
fn parse_reply(reply: &str) -> Result<(RefinementProposal, Vec<Value>)> {
    let mut value = extract_json(reply)?;
    let drafts = value
        .as_object_mut()
        .and_then(|object| object.remove("papercuts"))
        .and_then(|papercuts| papercuts.as_array().cloned())
        .unwrap_or_default();
    Ok((serde_json::from_value(value)?, drafts))
}

/// Pull a JSON object out of a reply that may be fenced or wrapped in prose.
///
/// A reply cut off by an exhausted output budget and a merely malformed one
/// both fail to parse, and the parser error describes the fragment rather than
/// the cause — so truncation is named explicitly.
pub(crate) fn extract_json(reply: &str) -> Result<Value> {
    let trimmed = reply.trim();
    let candidate = if let Some(fenced) = trimmed
        .split_once("```")
        .and_then(|(_, rest)| rest.split_once("```"))
        .map(|(inside, _)| inside)
    {
        inside_fence(fenced)
    } else {
        trimmed
    };
    if let Ok(value) = serde_json::from_str::<Value>(candidate) {
        return Ok(value);
    }
    if let (Some(start), Some(end)) = (candidate.find('{'), candidate.rfind('}'))
        && end > start
        && let Ok(value) = serde_json::from_str::<Value>(&candidate[start..=end])
    {
        return Ok(value);
    }
    if is_incomplete(candidate) {
        bail!("the refiner's reply was cut off before its JSON closed");
    }
    bail!("the refiner did not return a JSON object")
}

fn inside_fence(block: &str) -> &str {
    block.strip_prefix("json").unwrap_or(block).trim()
}

/// Whether a candidate ends mid-value: an unterminated string or unclosed
/// braces. A complete-but-malformed reply is balanced, so this distinguishes
/// "ran out of budget" from "wrote nonsense".
fn is_incomplete(candidate: &str) -> bool {
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escaped = false;
    for character in candidate.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            match character {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            _ => {}
        }
    }
    in_string || depth > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{EditAction, EntryKind};

    fn parse_proposal(reply: &str) -> Result<RefinementProposal> {
        parse_reply(reply).map(|(proposal, _)| proposal)
    }

    #[test]
    fn parses_a_bare_json_object() {
        let proposal = parse_proposal(
            r#"{"summary":"s","rationale":"r","expected_outcome":"e",
                "edits":[{"action":"create","kind":"memory","title":"t","content":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(proposal.edits.len(), 1);
        assert_eq!(proposal.edits[0].action, EditAction::Create);
        assert_eq!(proposal.edits[0].kind, EntryKind::Memory);
    }

    #[test]
    fn parses_a_fenced_object_and_one_wrapped_in_prose() {
        let fenced = "Here you go:\n```json\n{\"summary\":\"s\",\"rationale\":\"r\",\
                      \"expected_outcome\":\"e\",\"edits\":[]}\n```\nhope that helps";
        assert!(parse_proposal(fenced).unwrap().edits.is_empty());

        let prose = "I think: {\"summary\":\"s\",\"rationale\":\"r\",\
                     \"expected_outcome\":\"e\",\"edits\":[]} — done";
        assert!(parse_proposal(prose).unwrap().edits.is_empty());
    }

    #[test]
    fn a_truncated_reply_is_named_as_truncation_not_malformed_json() {
        // The failure mode when the output budget runs out mid-proposal, which
        // is a different problem from the model writing nonsense.
        let cut =
            r#"{"summary":"s","rationale":"r","expected_outcome":"e","edits":[{"action":"cre"#;
        let error = parse_proposal(cut).unwrap_err().to_string();
        assert!(error.contains("cut off"), "{error}");

        let nonsense = "I would rather not.";
        let error = parse_proposal(nonsense).unwrap_err().to_string();
        assert!(error.contains("did not return a JSON object"), "{error}");
    }

    #[test]
    fn an_unknown_kind_is_rejected_rather_than_coerced() {
        let reply = r#"{"summary":"s","rationale":"r","expected_outcome":"e",
            "edits":[{"action":"create","kind":"skill","title":"t","content":"c"}]}"#;
        // Abacus has three kinds; a skill is a real file on disk, not an entry.
        assert!(parse_proposal(reply).is_err());
    }

    #[test]
    fn trajectory_keeps_narrative_and_failures_but_drops_tool_bodies() {
        let messages = vec![
            json!({"role": "user", "content": "fix the importer"}),
            json!({"role": "assistant", "content": "Looking now.",
                   "tool_calls": [{"function": {"name": "read_file"}}]}),
            json!({"role": "tool", "name": "read_file", "content": "x".repeat(50_000)}),
            json!({"role": "tool", "name": "run_command",
                   "content": "exit: 1\nerror: DATABASE_URL must be set"}),
            json!({"role": "assistant", "content": "Needed an env var."}),
        ];
        let rendered = trajectory(&messages, 10_000);
        assert!(rendered.contains("fix the importer"));
        assert!(rendered.contains("assistant ran: read_file"));
        assert!(rendered.contains("Needed an env var."));
        // A failure is evidence; a successful read's body is not.
        assert!(rendered.contains("DATABASE_URL"));
        assert!(!rendered.contains(&"x".repeat(100)));
    }

    #[test]
    fn trajectory_keeps_the_tail_when_it_overflows() {
        let messages: Vec<Value> = (0..200)
            .map(|index| json!({"role": "assistant", "content": format!("step {index}")}))
            .collect();
        let rendered = trajectory(&messages, 200);
        // Conclusions live at the end of a turn, so the tail is what survives.
        assert!(rendered.contains("step 199"), "{rendered}");
        assert!(!rendered.contains("step 0\n"));
        assert!(rendered.chars().count() <= 200);
    }
}
