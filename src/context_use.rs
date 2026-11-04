//! What fills the context: the window split by kind — the system prompt, the
//! project's instructions, the tools offered, the layers Abacus adds, and the
//! conversation itself — as a front end shows it beside its meter.
//!
//! Each part is estimated from its characters, then scaled to the count the
//! meter shows, which is the provider's when it reported one: its tokenizer is
//! the one that matters, and it applies to every part alike.

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;
use serde_json::Value;

use crate::model_info::CHARS_PER_TOKEN;

/// Where the system prompt's project instructions start (see `system_prompt`).
const INSTRUCTIONS: &str = "\n\nProject instructions from AGENTS.md:\n";

/// One kind of thing in the context.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Part {
    pub key: &'static str,
    pub label: &'static str,
    pub tokens: u64,
    /// How many there are, where that says something: tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
}

/// The kinds, in the order a front end lists them.
const KINDS: &[(&str, &str)] = &[
    ("messages", "Messages"),
    ("system", "System prompt"),
    ("instructions", "Project instructions"),
    ("tools", "Built-in tools"),
    ("mcp", "MCP tools"),
    ("app", "App tools"),
    ("extensions", "Skills and plugins"),
    ("memory", "Memories"),
    ("summary", "Compaction summary"),
    ("session", "Session state"),
];

/// The context in `messages`, with `tools` offered: `mcp` and `app` name the
/// tools that come from MCP servers and from the front end. `total` is what
/// the parts should add up to, 0 to leave them as estimated.
pub fn breakdown(
    messages: &[Value],
    tools: &[Value],
    mcp: &HashSet<String>,
    app: &HashSet<String>,
    total: u64,
) -> Vec<Part> {
    let mut chars: BTreeMap<&str, usize> = BTreeMap::new();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, message) in messages.iter().enumerate() {
        if crate::compaction::is_marker(message) {
            continue;
        }
        let text = message["content"].as_str().unwrap_or_default();
        let kind = match message["role"].as_str() {
            Some("system") if index == 0 => {
                let (prompt, instructions) = text.split_once(INSTRUCTIONS).unwrap_or((text, ""));
                *chars.entry("system").or_default() += prompt.len();
                *chars.entry("instructions").or_default() += instructions.len();
                continue;
            }
            Some("system") => layer_kind(text),
            _ => "messages",
        };
        *chars.entry(kind).or_default() += crate::agent::message_chars_one(message);
    }
    for spec in tools {
        let name = spec["function"]["name"].as_str().unwrap_or_default();
        let kind = if mcp.contains(name) {
            "mcp"
        } else if app.contains(name) {
            "app"
        } else {
            "tools"
        };
        *chars.entry(kind).or_default() += spec.to_string().len();
        *counts.entry(kind).or_default() += 1;
    }

    let estimated: usize = chars.values().sum::<usize>() / CHARS_PER_TOKEN;
    // The meter's count, spread over the parts in proportion.
    let scale = if total > 0 && estimated > 0 { total as f64 / estimated as f64 } else { 1.0 };
    KINDS
        .iter()
        .filter_map(|&(key, label)| {
            let tokens = (chars.get(key).copied().unwrap_or(0) / CHARS_PER_TOKEN) as f64 * scale;
            let count = counts.get(key).copied();
            (tokens >= 1.0 || count.is_some()).then(|| Part { key, label, tokens: tokens.round() as u64, count })
        })
        .collect()
}

/// Which kind a context layer Abacus appended is, by its name.
fn layer_kind(text: &str) -> &'static str {
    let name = text
        .strip_prefix(crate::agent::SESSION_CONTEXT_TAG)
        .and_then(|rest| rest.strip_prefix(" layer=\""))
        .and_then(|rest| rest.split('"').next());
    match name {
        Some("extensions") => "extensions",
        Some("memory") => "memory",
        Some("summary") => "summary",
        Some(_) => "session",
        // A note compaction left in place of what it dropped.
        None => "messages",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> Value {
        json!({"type": "function", "function": {"name": name, "description": "x".repeat(380)}})
    }

    #[test]
    fn splits_the_context_by_kind() {
        let messages = vec![
            json!({"role": "system", "content": format!("{}{INSTRUCTIONS}{}", "p".repeat(400), "i".repeat(800))}),
            json!({"role": "user", "content": "u".repeat(4_000)}),
            json!({"role": "system", "content": format!("<session_context layer=\"memory\">\n{}\n</session_context>", "m".repeat(1_200))}),
            json!({"role": "system", "content": format!("<session_context layer=\"permissions\">\n{}\n</session_context>", "a".repeat(400))}),
            json!({"role": "assistant", "content": "a".repeat(2_000)}),
            json!({"role": "system", "content": "", "compacted": {"messages": 4, "tokens": 9_000, "summarised": true}}),
        ];
        let tools = [tool("read_file"), tool("grep"), tool("github_search"), tool("browser_navigate")];
        let mcp = HashSet::from(["github_search".to_owned()]);
        let app = HashSet::from(["browser_navigate".to_owned()]);
        let parts = breakdown(&messages, &tools, &mcp, &app, 0);
        let shape: Vec<_> = parts.iter().map(|part| (part.key, part.count)).collect();
        assert_eq!(
            shape,
            [
                ("messages", None),
                ("system", None),
                ("instructions", None),
                ("tools", Some(2)),
                ("mcp", Some(1)),
                ("app", Some(1)),
                ("memory", None),
                ("session", None),
            ]
        );
        let tokens = |key: &str| parts.iter().find(|part| part.key == key).map_or(0, |part| part.tokens);
        assert!(tokens("messages") >= 1_500, "{parts:?}");
        assert_eq!(tokens("system"), 100);
        assert_eq!(tokens("instructions"), 200);
        assert!(tokens("tools") > tokens("mcp"));
    }

    #[test]
    fn spreads_the_meters_count_over_the_parts() {
        let messages = vec![
            json!({"role": "system", "content": "s".repeat(4_000)}),
            json!({"role": "user", "content": "u".repeat(4_000)}),
        ];
        let parts = breakdown(&messages, &[], &HashSet::new(), &HashSet::new(), 3_000);
        let total: u64 = parts.iter().map(|part| part.tokens).sum();
        assert!((2_990..=3_010).contains(&total), "{parts:?}");
    }
}
