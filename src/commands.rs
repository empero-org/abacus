//! The slash commands the terminal and the app server both answer: what Abacus
//! remembers, the goal, what the agent can reach, history repair, and usage.
//! Each returns what to say; the front end decides how to show it and what
//! else to do (start a turn, stop one).

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, anyhow};
use chrono::{Duration as ChronoDuration, Local, NaiveDate};
use serde_json::Value;

use crate::goal::GoalState;
use crate::memories::MemoryStore;
use crate::services::AgentServices;
use crate::session::SessionUsage;

/// The row a 1-based `number` names in a listing.
pub fn numbered<'a, T>(rows: &'a [T], number: &str) -> Option<&'a T> {
    rows.get(number.trim().parse::<usize>().ok()?.saturating_sub(1))
}

/// `/memory`: what Abacus remembers in this workspace, numbered, and
/// `forget m<n>` to remove one.
pub fn memory(memories: &MemoryStore, argument: &str) -> Result<String> {
    let argument = argument.trim();
    let (verb, rest) =
        argument.split_once(char::is_whitespace).map_or((argument, ""), |(verb, rest)| (verb, rest.trim()));
    // One ordering for display and forgetting alike, or the numbers shown
    // would target different entries.
    let listed = memories.snapshot();
    match verb {
        "" => {}
        "forget" | "delete" => {
            let reference = rest.to_ascii_lowercase();
            if let Some(memory) = numbered(&listed, reference.strip_prefix('m').unwrap_or(&reference))
                && memories.remove(memory.id)
            {
                return Ok(format!("Memory \"{}\" forgotten.", memory.title));
            }
            return Err(anyhow!("Usage: /memory forget m<number> — numbers from /memory"));
        }
        _ => return Err(anyhow!("Usage: /memory [forget m1]")),
    }
    if listed.is_empty() {
        return Ok("No memories here yet. Abacus records facts and decisions worth keeping as it works.".to_owned());
    }
    let mut lines = vec![format!("Memories ({}), newest first:", listed.len())];
    lines.extend(listed.iter().enumerate().map(|(index, memory)| {
        format!("  m{}. {} — {}", index + 1, memory.title, crate::ui::truncate(&memory.body, 120))
    }));
    lines.push("/memory forget m1 removes one.".to_owned());
    Ok(lines.join("\n"))
}

/// What `/goal` did, beyond what it says.
pub struct GoalReply {
    pub said: Result<String>,
    /// A turn to start on the goal: what the transcript shows, and the prompt.
    pub start: Option<(String, String)>,
    /// The goal was paused, so a running turn should stop.
    pub paused: bool,
}

/// `/goal`: show the goal, or `<objective>`, `edit <objective>`, `pause`,
/// `resume`, `done`, `clear`.
pub fn goal(goal: &GoalState, argument: &str) -> GoalReply {
    let argument = argument.trim();
    let said = |said: Result<String>| GoalReply { said, start: None, paused: false };
    match argument {
        "" => said(Ok(goal
            .snapshot()
            .map(|goal| {
                format!(
                    "Goal · {:?}\n{}{}",
                    goal.status,
                    goal.objective,
                    goal.note.map(|note| format!("\n\nLatest update: {note}")).unwrap_or_default()
                )
            })
            .unwrap_or_else(|| "No goal is set. Use /goal <objective>.".to_owned()))),
        "pause" => {
            let result = goal.pause().map(|_| "Goal paused. Use /goal resume when ready.".to_owned());
            let paused = result.is_ok();
            GoalReply { said: result, start: None, paused }
        }
        "resume" => match goal.resume() {
            Ok(goal) => GoalReply {
                said: Ok("Goal resumed.".to_owned()),
                start: Some(("Resume goal".to_owned(), goal.objective)),
                paused: false,
            },
            Err(error) => said(Err(error)),
        },
        "clear" => said(goal.set(None).map(|()| "Goal cleared.".to_owned())),
        "done" | "complete" => said(Ok(goal
            .execute("goal_update", r#"{"status":"complete"}"#)
            .unwrap_or_else(|| "Error: no goal is set".to_owned()))),
        _ => {
            if let Some(objective) = argument.strip_prefix("edit ") {
                return said(goal.edit(objective).map(|goal| format!("Goal updated: {}", goal.objective)));
            }
            match goal.create(argument) {
                Ok(goal) => GoalReply {
                    said: Ok(format!("Goal set: {}", goal.objective)),
                    start: Some((goal.objective.clone(), goal.objective)),
                    paused: false,
                },
                Err(error) => said(Err(error)),
            }
        }
    }
}

/// `/tools`: everything the agent can reach — its tools, then what extensions
/// add: skills, plugin commands, MCP tools.
pub fn extensions(services: &AgentServices) -> String {
    let mut names: Vec<String> =
        services.tool_specs().iter().filter_map(|spec| spec["function"]["name"].as_str().map(str::to_owned)).collect();
    names.extend(["goal_update", "spawn_subagents"].map(str::to_owned));
    let mut sections = vec![format!("Tools: {}", names.join(", "))];
    let skills: Vec<String> = services
        .skills
        .read()
        .expect("skill registry lock")
        .list()
        .map(|skill| format!("  /{}  {}", skill.name, skill.description))
        .collect();
    if !skills.is_empty() {
        sections.push(format!("Skills (run one with /<name>):\n{}", skills.join("\n")));
    }
    let plugins: Vec<String> = services
        .plugins
        .list()
        .map(|plugin| format!("  {} {}  {}", plugin.name, plugin.version, plugin.description))
        .collect();
    if !plugins.is_empty() {
        sections.push(format!("Plugins:\n{}", plugins.join("\n")));
    }
    let mcp: Vec<String> =
        services.mcp.tools().map(|tool| format!("  {}  {}", tool.exposed_name, tool.description)).collect();
    if !mcp.is_empty() {
        sections.push(format!("MCP tools:\n{}", mcp.join("\n")));
    }
    sections.join("\n\n")
}

/// `/repair`: mend history a strict provider keeps rejecting (an interrupted
/// or failed turn can leave it so). Returns what to say, and whether the
/// history changed and needs saving.
pub fn repair(messages: &mut Vec<Value>) -> (String, bool) {
    let fixes = crate::session::repair_messages(messages);
    if fixes.is_empty() {
        return ("No corruption found: every tool call parses and has a result.".to_owned(), false);
    }
    (format!("Repaired the session history: {}.", fixes.join("; ")), true)
}

/// What the usage records add up to.
#[derive(Default)]
pub struct UsageStats {
    pub sessions: usize,
    pub total_tokens: u64,
    pub tokens_estimated: bool,
    pub favorite_model: Option<String>,
    pub active_days: usize,
    pub most_active_day: Option<NaiveDate>,
    pub longest_session: u64,
    pub longest_streak: usize,
    pub current_streak: usize,
}

pub fn usage_stats(records: &[&SessionUsage], today: NaiveDate) -> UsageStats {
    let mut stats = UsageStats { sessions: records.len(), ..UsageStats::default() };
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
    stats.favorite_model =
        models.into_iter().max_by_key(|(_, (sessions, tokens))| (*tokens, *sessions)).map(|(model, _)| model);
    stats.most_active_day = daily.into_iter().max_by_key(|(_, activity)| *activity).map(|(date, _)| date);

    let mut sorted_dates = dates.into_iter().collect::<Vec<_>>();
    sorted_dates.sort_unstable();
    let mut run = 0;
    let mut previous = None;
    for date in &sorted_dates {
        run = if previous.is_some_and(|value| *date == value + ChronoDuration::days(1)) { run + 1 } else { 1 };
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

/// Sessions, tokens and time per model, most tokens first.
pub fn usage_by_model(records: &[&SessionUsage]) -> Vec<(String, usize, u64, u64)> {
    let mut models = HashMap::<String, (usize, u64, u64)>::new();
    for record in records {
        let usage = models.entry(record.model.clone()).or_default();
        usage.0 += 1;
        usage.1 = usage.1.saturating_add(record.tokens_used);
        usage.2 = usage.2.saturating_add(record.active_secs);
    }
    let mut models: Vec<_> =
        models.into_iter().map(|(model, (sessions, tokens, secs))| (model, sessions, tokens, secs)).collect();
    models.sort_by_key(|(_, sessions, tokens, _)| std::cmp::Reverse((*tokens, *sessions)));
    models
}

pub fn usage_date(record: &SessionUsage) -> NaiveDate {
    record.created_at.with_timezone(&Local).date_naive()
}

pub fn format_duration(seconds: u64) -> String {
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

/// `/usage` as text, for a front end without the terminal's panel: totals
/// for all time and the last week, then the models, `current` marked.
pub fn usage_report(records: &[SessionUsage], current: &str, today: NaiveDate) -> String {
    let all: Vec<&SessionUsage> = records.iter().collect();
    if all.is_empty() {
        return "No usage recorded yet.".to_owned();
    }
    let week: Vec<&SessionUsage> =
        all.iter().copied().filter(|record| usage_date(record) > today - ChronoDuration::days(7)).collect();
    let tokens = |stats: &UsageStats| {
        format!("{}{}", if stats.tokens_estimated { "~" } else { "" }, crate::ui::format_count(stats.total_tokens))
    };
    let stats = usage_stats(&all, today);
    let recent = usage_stats(&week, today);
    let row = |label: &str, value: String| format!("  {label:<18}{value}");
    let mut lines = vec![
        "Usage, all time".to_owned(),
        row("Sessions", stats.sessions.to_string()),
        row("Total tokens", tokens(&stats)),
        row("Favorite model", stats.favorite_model.clone().unwrap_or_else(|| "—".to_owned())),
        row("Active days", stats.active_days.to_string()),
        row(
            "Most active day",
            stats.most_active_day.map_or_else(|| "—".to_owned(), |date| date.format("%b %-d, %Y").to_string()),
        ),
        row("Longest session", format_duration(stats.longest_session)),
        row("Streak", format!("{} days now, {} at most", stats.current_streak, stats.longest_streak)),
        String::new(),
        "Last 7 days".to_owned(),
        row("Sessions", recent.sessions.to_string()),
        row("Total tokens", tokens(&recent)),
        String::new(),
        format!("  {:<34}{:>9}{:>10}  Time", "Model", "Sessions", "Tokens"),
    ];
    for (model, sessions, tokens, secs) in usage_by_model(&all).into_iter().take(12) {
        let marker = if model == current { "●" } else { " " };
        let name = crate::text::clip(&crate::text::flat(&model), 31, "…");
        lines.push(format!(
            "{marker} {name:<34}{sessions:>9}{:>10}  {}",
            crate::ui::format_count(tokens),
            format_duration(secs)
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn record(model: &str, day: u32, tokens: u64) -> SessionUsage {
        let at = Utc.with_ymd_and_hms(2026, 10, day, 12, 0, 0).unwrap();
        SessionUsage {
            id: uuid::Uuid::new_v4(),
            model: model.to_owned(),
            created_at: at,
            updated_at: at,
            message_count: 4,
            tokens_used: tokens,
            tokens_estimated: false,
            active_secs: 600,
        }
    }

    #[test]
    fn usage_report_totals_and_ranks_models() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 28).unwrap();
        let records = [record("a/small", 1, 1_000), record("b/large", 27, 50_000), record("b/large", 28, 25_000)];
        let report = usage_report(&records, "b/large", today);
        assert!(report.contains("Sessions          3"), "{report}");
        assert!(report.contains("Favorite model    b/large"), "{report}");
        let large = report.lines().position(|line| line.contains("b/large") && line.starts_with('●'));
        let small = report.lines().position(|line| line.contains("a/small"));
        assert!(large.is_some() && large < small, "the current, busier model first and marked:\n{report}");
        assert_eq!(usage_report(&[], "x", today), "No usage recorded yet.");
    }

    #[test]
    fn repair_says_when_there_was_nothing_to_fix() {
        let mut messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let (said, changed) = repair(&mut messages);
        assert!(!changed);
        assert!(said.starts_with("No corruption found"));
    }
}
