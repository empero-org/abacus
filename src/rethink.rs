//! Rethink: a bounded reflection pass over what a long turn actually did.
//!
//! After a turn with many actions — and, unconditionally, right before
//! rolling-summary compaction erases the verbatim evidence — the agent is
//! given one look back at the conversation with a restricted toolset:
//! `memory_record` / `memory_forget`, `papercut_record`, and
//! `working_notes_update`. If it finds durable knowledge (decisions taken,
//! things figured out, roadmap changes, goals accomplished), it records it;
//! if not, it does nothing. The reflection messages themselves are discarded
//! — only the side effects persist.
//!
//! Working notes live in a clearly delimited, abacus-managed block inside the
//! workspace's `AGENTS.md`. Only the block is ever rewritten; the user's own
//! content is untouched, byte for byte.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::memories::MemoryStore;
use crate::papercuts::PapercutStore;
use crate::provider::Provider;

/// A turn is "long" — worth reflecting on — from this many tool calls.
pub const LONG_TURN_TOOL_CALLS: usize = 6;
/// The reflection itself is bounded: one look, a batch of records, done.
const MAX_RETHINK_STEPS: usize = 2;

const NOTES_START: &str = "<!-- abacus:notes:start -->";
const NOTES_END: &str = "<!-- abacus:notes:end -->";
/// Ceiling on the managed block, so notes stay notes.
const MAX_NOTES_CHARS: usize = 4_000;

/// The extra tool available only during rethink.
fn working_notes_spec() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "working_notes_update",
            "description": "Replace the abacus-managed working-notes block in AGENTS.md — current direction, active constraints, roadmap changes. The block is injected into every future session's system prompt, so keep it short and current; it replaces wholesale, so include everything that should remain. User content outside the block is never touched.",
            "parameters": {
                "type": "object",
                "properties": {
                    "notes": {"type": "string", "description": "The full new content of the block (max ~4000 chars). Empty string clears it."}
                },
                "required": ["notes"]
            }
        }
    })
}

fn rethink_directive() -> String {
    "REFLECTION PASS. The turn above is finished; this is a look back before the \
     details fade. Review what actually happened: snags worked through, decisions \
     taken and why, facts about this codebase figured out the hard way, roadmap or \
     goal changes, [papercut] reminders that were right or wrong. Then:\n\
     - Record durable failure lessons with papercut_record (with distinctive tripwires).\n\
     - Record durable knowledge with memory_record; update or memory_forget stale ones.\n\
     - Update the working-notes block with working_notes_update if the project's \
     direction or constraints changed.\n\
     Only record what a future session would genuinely need — most turns need \
     nothing, and recording noise is worse than recording nothing. Do not run any \
     other action. Reply with a one-line summary of what you recorded, or the word \
     NOTHING."
        .to_owned()
}

/// Outcome summary for surfacing in the transcript.
pub struct RethinkOutcome {
    pub recorded: usize,
    pub summary: String,
}

/// Run the reflection. `messages` is the finished conversation; nothing in it
/// is modified. Mutating the workspace notes is gated on `allow_notes` so a
/// PLAN-pinned session never writes a file.
pub async fn run(
    provider: &Provider,
    messages: &[Value],
    memories: &MemoryStore,
    papercuts: &PapercutStore,
    workspace: &Path,
    allow_notes: bool,
    cancel: &AtomicBool,
) -> Option<RethinkOutcome> {
    let mut specs = MemoryStore::tool_specs();
    specs.extend(PapercutStore::tool_specs());
    if allow_notes {
        specs.push(working_notes_spec());
    }
    let mut conversation = messages.to_vec();
    conversation.push(json!({"role": "user", "content": rethink_directive()}));

    let mut recorded = 0_usize;
    let mut summary = String::new();
    for _ in 0..MAX_RETHINK_STEPS {
        // Deltas are discarded: the reflection is bookkeeping, not output.
        let (deltas, _sink) = mpsc::unbounded_channel();
        let completion = match provider
            .complete(&conversation, &specs, deltas, cancel)
            .await
        {
            Ok(completion) => completion,
            Err(_) => break,
        };
        if completion.cancelled {
            break;
        }
        if !completion.content.is_empty() {
            summary = completion.content.trim().to_owned();
        }
        if completion.tool_calls.is_empty() {
            break;
        }
        conversation.push(crate::agent::assistant_reflection_message(&completion));
        // Every record in this batch landed, and the model already said what it
        // was recording? Then the next step exists only to repeat that summary
        // back — a second pass over the whole conversation for nothing. The
        // step is still taken whenever a record *failed*, which is the case
        // where seeing the results changes what the model does.
        let mut all_accepted = true;
        for call in &completion.tool_calls {
            let output = memories
                .execute(&call.name, &call.arguments)
                .or_else(|| papercuts.execute(&call.name, &call.arguments))
                .or_else(|| {
                    (call.name == "working_notes_update" && allow_notes).then(|| {
                        match notes_from_arguments(&call.arguments)
                            .and_then(|notes| update_working_notes(workspace, &notes))
                        {
                            Ok(()) => "Working notes updated.".to_owned(),
                            Err(error) => format!("Error: {error:#}"),
                        }
                    })
                })
                .unwrap_or_else(|| {
                    "Error: only memory, papercut, and working-notes tools are available here."
                        .to_owned()
                });
            if output.starts_with("Error:") {
                all_accepted = false;
            } else {
                recorded += 1;
            }
            conversation.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "name": call.name,
                "content": output
            }));
        }
        if all_accepted && !summary.is_empty() {
            break;
        }
    }

    if recorded == 0 {
        return None;
    }
    Some(RethinkOutcome {
        recorded,
        summary: if summary.is_empty() || summary.eq_ignore_ascii_case("nothing") {
            format!("{recorded} record(s) kept")
        } else {
            summary
        },
    })
}

fn notes_from_arguments(arguments: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Arguments {
        notes: String,
    }
    let arguments: Arguments =
        serde_json::from_str(arguments).context("invalid working_notes_update arguments")?;
    Ok(arguments.notes)
}

/// Rewrite only the abacus-managed block of `AGENTS.md`, creating the file
/// (or appending the block) when absent. Everything outside the markers is
/// preserved byte for byte.
pub fn update_working_notes(workspace: &Path, notes: &str) -> Result<()> {
    let notes = notes.trim();
    if notes.chars().count() > MAX_NOTES_CHARS {
        bail!("working notes exceed {MAX_NOTES_CHARS} characters — keep them summary-sized");
    }
    let path = workspace.join("AGENTS.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    let block = if notes.is_empty() {
        format!("{NOTES_START}\n{NOTES_END}")
    } else {
        format!("{NOTES_START}\n## Working notes (maintained by Abacus)\n\n{notes}\n{NOTES_END}")
    };

    let updated = match (existing.find(NOTES_START), existing.find(NOTES_END)) {
        (Some(start), Some(end)) if end >= start => {
            let after = end + NOTES_END.len();
            format!("{}{}{}", &existing[..start], block, &existing[after..])
        }
        // Markers missing or mangled: append a fresh block rather than guess
        // at intent inside the user's content.
        _ => {
            if existing.is_empty() {
                format!("{block}\n")
            } else {
                let separator = if existing.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                format!("{existing}{separator}{block}\n")
            }
        }
    };
    crate::config::atomic_write(&path, updated.as_bytes(), false)
        .context("write AGENTS.md working notes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_block_is_created_and_replaced_without_touching_user_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "# My rules\n\nAlways run fmt.\n").unwrap();

        update_working_notes(dir.path(), "Current focus: the importer.").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.starts_with("# My rules\n\nAlways run fmt.\n"),
            "{content}"
        );
        assert!(content.contains("Current focus: the importer."));

        // Replacing rewrites only the block.
        update_working_notes(dir.path(), "Current focus: the exporter.").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("the exporter"));
        assert!(!content.contains("the importer"), "{content}");
        assert_eq!(content.matches(NOTES_START).count(), 1);
        assert!(content.contains("Always run fmt."));
    }

    #[test]
    fn notes_created_when_agents_md_absent_and_cleared_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        update_working_notes(dir.path(), "note one").unwrap();
        let path = dir.path().join("AGENTS.md");
        assert!(std::fs::read_to_string(&path).unwrap().contains("note one"));

        update_working_notes(dir.path(), "").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("note one"));
        assert!(content.contains(NOTES_START), "markers stay for next time");
    }

    #[test]
    fn oversized_notes_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(update_working_notes(dir.path(), &"x".repeat(5_000)).is_err());
    }
}
