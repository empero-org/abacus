//! Tethering: drift control against the session's intent.
//!
//! After the first prompt is answered, a quick model call snapshots the
//! session's INTENT — what the user is trying to achieve and under which
//! constraints. The snapshot refreshes before every rolling-summary
//! compaction (the user may have redirected, and compaction erases the
//! evidence), and rides the session so resume keeps it.
//!
//! Every ~35 model steps a quick check runs: the intent plus a *compact*
//! history — assistant text, its recorded thinking, and tool-call names, but
//! never tool outputs — and the question "is this still serving the
//! intent?". Thinking is included deliberately: drift shows up in the
//! reasoning before it shows up in the actions. An off-track verdict
//! comes with a course correction written by the checking call itself, which
//! is then injected as a system layer into the next few requests and
//! surfaced to the user as a notice.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::provider::Provider;

/// How many model steps between drift checks.
pub const CHECK_EVERY_STEPS: usize = 35;
/// Token Compression keeps drift protection, but samples it less often.
pub const CHECK_EVERY_STEPS_COMPRESSED: usize = 70;
/// A correction stays in the system layer for this many requests, then
/// clears — one nudge, visible long enough to land, not a permanent nag.
const CORRECTION_REQUESTS: u8 = 3;
/// Ceiling on the compact history handed to the intent and drift calls.
const COMPACT_HISTORY_CHARS: usize = 6_000;
/// Of that, the share reserved for user prompts. A build phase emits assistant
/// lines fast enough to flush every user turn out of a plain tail window, which
/// left the drift check rating a session against no user input at all — it saw
/// only the agent talking to itself. Unused share falls through to activity.
const USER_HISTORY_SHARE: usize = COMPACT_HISTORY_CHARS * 2 / 3;
/// Each assistant excerpt in the compact history is clipped to this.
const EXCERPT_CHARS: usize = 240;

#[derive(Default)]
struct Inner {
    intent: Option<String>,
    steps_since_check: usize,
    /// Active course correction and how many more requests it rides.
    correction: Option<(String, u8)>,
}

/// Shared, cloneable handle following the `GoalState` pattern.
#[derive(Clone, Default)]
pub struct TetherState {
    inner: Arc<RwLock<Inner>>,
}

impl TetherState {
    pub fn new(intent: Option<String>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                intent,
                ..Inner::default()
            })),
        }
    }

    pub fn intent(&self) -> Option<String> {
        self.inner.read().expect("tether lock").intent.clone()
    }

    pub fn set_intent(&self, intent: String) {
        self.inner.write().expect("tether lock").intent = Some(intent);
    }

    /// Count a model step; true when a drift check is due.
    pub fn step_and_check_due(&self, interval: usize) -> bool {
        let mut inner = self.inner.write().expect("tether lock");
        if inner.intent.is_none() {
            return false;
        }
        inner.steps_since_check += 1;
        if inner.steps_since_check >= interval.max(1) {
            inner.steps_since_check = 0;
            return true;
        }
        false
    }

    pub fn set_correction(&self, correction: String) {
        self.inner.write().expect("tether lock").correction =
            Some((correction, CORRECTION_REQUESTS));
    }

    /// The system-layer text for the next request, if a correction is active.
    /// Each call burns one of the correction's remaining appearances.
    pub fn correction_layer(&self) -> Option<String> {
        let mut inner = self.inner.write().expect("tether lock");
        let (text, remaining) = inner.correction.take()?;
        let layer = format!(
            "COURSE CORRECTION (session tether): {text}\nSession intent: {}",
            inner.intent.as_deref().unwrap_or("(unset)")
        );
        if remaining > 1 {
            inner.correction = Some((text, remaining - 1));
        }
        Some(layer)
    }
}

/// The conversation reduced to what steering needs: user prompts, assistant
/// text and thinking (clipped), and tool-call names with their argument
/// summaries — never tool outputs. Most recent kept when the budget bites.
pub fn compact_history(messages: &[Value]) -> String {
    // (is_user, text): what the user asked is the evidence both callers judge
    // against, so it is budgeted separately from what the agent has been doing.
    let mut lines: Vec<(bool, String)> = Vec::new();
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("user") => {
                if let Some(text) = message.get("content").and_then(Value::as_str) {
                    lines.push((true, format!("user: {}", clip(text, EXCERPT_CHARS))));
                }
            }
            Some("assistant") => {
                let mut line = String::from("assistant:");
                // The thinking first: intentions drift before actions do.
                if let Some(thinking) = message.get("reasoning_content").and_then(Value::as_str)
                    && !thinking.trim().is_empty()
                {
                    line.push_str(&format!(" (thinking: {})", clip(thinking, EXCERPT_CHARS)));
                }
                if let Some(text) = message.get("content").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    line.push(' ');
                    line.push_str(&clip(text, EXCERPT_CHARS));
                }
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                            let arguments = call
                                .pointer("/function/arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            line.push_str(&format!(" [{} {}]", name, clip(arguments, 80)));
                        }
                    }
                }
                if line != "assistant:" {
                    lines.push((false, line));
                }
            }
            _ => {}
        }
    }
    // Two passes, newest first. User prompts claim their reserved share before
    // any activity is kept, so a redirect five minutes ago survives an hour of
    // tool calls; recent activity then fills whatever is left.
    let mut keep = vec![false; lines.len()];
    let mut total = 0_usize;
    for (index, (is_user, line)) in lines.iter().enumerate().rev() {
        if !is_user || total + line.len() + 1 > USER_HISTORY_SHARE {
            continue;
        }
        total += line.len() + 1;
        keep[index] = true;
    }
    for (index, (is_user, line)) in lines.iter().enumerate().rev() {
        if *is_user || total + line.len() + 1 > COMPACT_HISTORY_CHARS {
            continue;
        }
        total += line.len() + 1;
        keep[index] = true;
    }
    // Mark elisions: a reader that cannot see a gap reads what is left as the
    // whole conversation, which is the failure this function just had.
    let mut kept: Vec<&str> = Vec::new();
    let mut skipped = false;
    for (index, (_, line)) in lines.iter().enumerate() {
        if keep[index] {
            if skipped && !kept.is_empty() {
                kept.push("…");
            }
            skipped = false;
            kept.push(line.as_str());
        } else {
            skipped = true;
        }
    }
    kept.join("\n")
}

fn clip(text: &str, limit: usize) -> String {
    let trimmed = text.trim().replace('\n', " ");
    if trimmed.chars().count() <= limit {
        return trimmed;
    }
    let clipped: String = trimmed.chars().take(limit).collect();
    format!("{clipped}…")
}

async fn quick_call(
    provider: &Provider,
    conversation: Vec<Value>,
    cancel: &AtomicBool,
) -> Option<String> {
    let (deltas, _sink) = mpsc::unbounded_channel();
    let completion = provider
        .complete(&conversation, &[], deltas, cancel)
        .await
        .ok()?;
    if completion.cancelled || completion.content.trim().is_empty() {
        return None;
    }
    Some(completion.content.trim().to_owned())
}

/// Snapshot (or refresh) the session intent from the conversation so far.
pub async fn capture_intent(
    provider: &Provider,
    messages: &[Value],
    previous: Option<&str>,
    cancel: &AtomicBool,
) -> Option<String> {
    let directive = match previous {
        Some(previous) => format!(
            "Previous intent snapshot:\n{previous}\n\nUpdate the snapshot if the \
             conversation shows the user has redirected or narrowed the goal; \
             otherwise restate it. Reply with only the intent, 2-4 sentences."
        ),
        None => "State this session's INTENT: what the user is trying to achieve, \
                 and any constraints they set. Reply with only the intent, 2-4 \
                 sentences."
            .to_owned(),
    };
    let conversation = vec![
        serde_json::json!({"role": "system", "content": "You summarise coding-agent sessions precisely."}),
        serde_json::json!({"role": "user", "content": format!("Conversation so far:\n{}\n\n{directive}", compact_history(messages))}),
    ];
    quick_call(provider, conversation, cancel).await
}

/// The plan the user has already agreed to: the active goal and task list.
/// Empty when neither exists.
pub fn agreed_plan(goal: &crate::goal::GoalState, tasks: &crate::task::TaskList) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(goal) = goal
        .snapshot()
        .filter(|goal| goal.status == crate::goal::GoalStatus::Active)
    {
        parts.push(format!("Active goal: {}", goal.objective));
    }
    let items = tasks.snapshot();
    if !items.is_empty() {
        let rendered = items
            .iter()
            .map(|task| format!("- [{}] {}", if task.done { 'x' } else { ' ' }, task.text))
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!("Task list:\n{rendered}"));
    }
    parts.join("\n")
}

/// Run the drift check. `Some(correction)` means off track.
pub async fn check_drift(
    provider: &Provider,
    intent: &str,
    plan: &str,
    messages: &[Value],
    cancel: &AtomicBool,
) -> Option<String> {
    // An agreed plan is the strongest evidence of what the user wants, and it
    // postdates the intent snapshot. Without it the check flagged a running
    // build — with an approved 9-task list open — as drift from the greeting
    // that opened the session.
    let plan = if plan.trim().is_empty() {
        "(none recorded)".to_owned()
    } else {
        plan.to_owned()
    };
    let conversation = vec![
        serde_json::json!({"role": "system", "content": "You audit a coding-agent session against its intent. Be strict about drift, tolerant of legitimate sub-work (tests, refactors, debugging serve the intent)."}),
        serde_json::json!({"role": "user", "content": format!(
            "Session intent:\n{intent}\n\nPlan the user has agreed to:\n{plan}\n\n\
             Recent activity (assistant turns and tool calls; the history may be \
             elided, marked …):\n{}\n\n\
             Is the recent activity still serving the intent? Work that advances \
             the agreed plan is ON_TRACK even when the intent snapshot is older \
             and narrower than the plan — the snapshot is a summary taken earlier, \
             not a limit on what the user may since have asked for. Only call \
             OFF_TRACK for activity that serves neither. Reply with exactly \
             ON_TRACK, or OFF_TRACK: followed by one short paragraph addressed to \
             the agent telling it specifically what to stop and what to return to.",
            compact_history(messages)
        )}),
    ];
    let reply = quick_call(provider, conversation, cancel).await?;
    parse_verdict(&reply)
}

/// `Some(correction)` for an off-track verdict, `None` for on-track or an
/// unparseable reply — an auditor that can't follow the format is not given
/// steering power.
pub fn parse_verdict(reply: &str) -> Option<String> {
    let trimmed = reply.trim();
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("ON_TRACK") || upper.starts_with("ON TRACK") {
        return None;
    }
    for prefix in ["OFF_TRACK", "OFF TRACK"] {
        if upper.starts_with(prefix) {
            let rest = trimmed[prefix.len()..]
                .trim_start_matches([':', '-', ' '])
                .trim();
            if !rest.is_empty() {
                return Some(rest.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compact_history_keeps_calls_and_drops_tool_outputs() {
        let messages = vec![
            json!({"role":"system","content":"rules"}),
            json!({"role":"user","content":"fix the importer"}),
            json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"a","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"importer.rs\"}"}}
            ]}),
            json!({"role":"tool","tool_call_id":"a","content":"SECRET GIANT FILE BODY"}),
            json!({"role":"assistant","content":"The importer drops rows with null keys.",
                   "reasoning_content":"maybe I should refactor everything while I am here"}),
        ];
        let compact = compact_history(&messages);
        assert!(compact.contains("user: fix the importer"));
        assert!(compact.contains("[read_file"));
        assert!(compact.contains("drops rows"));
        assert!(!compact.contains("SECRET GIANT FILE BODY"), "{compact}");
        // Recorded thinking is part of the analysis: drift shows there first.
        assert!(
            compact.contains("thinking: maybe I should refactor everything"),
            "{compact}"
        );
    }

    /// The reported failure: a long build phase flushed every user prompt out
    /// of the window, so the drift check judged the session with no idea what
    /// had ever been asked for — and flagged agreed work as drift.
    #[test]
    fn a_long_build_phase_cannot_flush_the_user_out_of_the_window() {
        let mut messages = vec![
            json!({"role":"user","content":"hi! use empero.org as the theme guide"}),
            json!({"role":"assistant","content":"Sure — what would you like to build?"}),
            json!({"role":"user","content":"build the full SaaS backend: auth, plans, billing, admin"}),
        ];
        for index in 0..400 {
            messages.push(json!({
                "role":"assistant",
                "content": format!("writing module {index} {}", "x".repeat(200))
            }));
        }
        let compact = compact_history(&messages);
        assert!(
            compact.contains("build the full SaaS backend"),
            "the actual request must survive: {compact}"
        );
        assert!(
            compact.contains("empero.org"),
            "so must the opening prompt: {compact}"
        );
        // Recent activity still gets its share.
        assert!(compact.contains("writing module 399"), "{compact}");
        // And the gap is visible, so nothing reads the remainder as the whole.
        assert!(compact.contains('…'), "{compact}");
        assert!(compact.len() <= COMPACT_HISTORY_CHARS + 300);
    }

    /// User prompts claim their share first, but never the whole window — a
    /// chatty user must not blind the check to what the agent is doing.
    #[test]
    fn activity_keeps_its_share_when_the_user_talks_a_lot() {
        let mut messages = Vec::new();
        for index in 0..400 {
            messages
                .push(json!({"role":"user","content":format!("note {index} {}", "u".repeat(200))}));
        }
        for index in 0..50 {
            messages.push(
                json!({"role":"assistant","content":format!("doing {index} {}", "a".repeat(200))}),
            );
        }
        let compact = compact_history(&messages);
        assert!(
            compact.contains("doing 49"),
            "recent activity survives: {compact}"
        );
        assert!(
            compact.contains("note 399"),
            "newest prompts survive: {compact}"
        );
        assert!(compact.len() <= COMPACT_HISTORY_CHARS + 300);
    }

    #[test]
    fn the_agreed_plan_is_rendered_for_the_drift_check() {
        let goal = crate::goal::GoalState::default();
        let tasks = crate::task::TaskList::default();
        assert_eq!(
            agreed_plan(&goal, &tasks),
            "",
            "nothing agreed, nothing said"
        );

        tasks.execute("task_create", r#"{"tasks":["scaffold repo","add auth"]}"#);
        let plan = agreed_plan(&goal, &tasks);
        assert!(plan.contains("scaffold repo"), "{plan}");
        assert!(plan.contains("[ ] add auth"), "{plan}");
    }

    #[test]
    fn compact_history_keeps_the_tail_under_budget() {
        let mut messages = Vec::new();
        for index in 0..200 {
            messages.push(
                json!({"role":"assistant","content":format!("step {index} {}", "x".repeat(200))}),
            );
        }
        let compact = compact_history(&messages);
        assert!(compact.len() <= COMPACT_HISTORY_CHARS + 300);
        assert!(compact.contains("step 199"), "newest activity survives");
        assert!(!compact.contains("step 0 "), "oldest is dropped");
    }

    #[test]
    fn verdict_parsing_is_strict() {
        assert_eq!(parse_verdict("ON_TRACK"), None);
        assert_eq!(parse_verdict("on track — all good"), None);
        assert_eq!(
            parse_verdict("OFF_TRACK: stop refactoring the CLI and return to the importer bug."),
            Some("stop refactoring the CLI and return to the importer bug.".to_owned())
        );
        assert_eq!(
            parse_verdict("off track - go back to the tests"),
            Some("go back to the tests".to_owned())
        );
        // An empty correction or free-form rambling earns no steering power.
        assert_eq!(parse_verdict("OFF_TRACK:"), None);
        assert_eq!(parse_verdict("The agent seems busy."), None);
    }

    #[test]
    fn correction_rides_a_bounded_number_of_requests() {
        let tether = TetherState::new(Some("ship the importer".into()));
        tether.set_correction("return to the importer".into());
        let mut seen = 0;
        while tether.correction_layer().is_some() {
            seen += 1;
            assert!(seen <= 10, "correction must clear");
        }
        assert_eq!(seen as u8, super::CORRECTION_REQUESTS);
        // The layer carries both the correction and the intent.
        tether.set_correction("focus".into());
        let layer = tether.correction_layer().unwrap();
        assert!(layer.contains("COURSE CORRECTION"));
        assert!(layer.contains("ship the importer"));
    }

    #[test]
    fn steps_only_count_once_intent_exists() {
        let tether = TetherState::default();
        for _ in 0..100 {
            assert!(
                !tether.step_and_check_due(CHECK_EVERY_STEPS),
                "no intent, no checks"
            );
        }
        tether.set_intent("do the thing".into());
        let mut due = 0;
        for _ in 0..(CHECK_EVERY_STEPS * 2) {
            if tether.step_and_check_due(CHECK_EVERY_STEPS) {
                due += 1;
            }
        }
        assert_eq!(due, 2, "one check per {CHECK_EVERY_STEPS} steps");
    }
}
