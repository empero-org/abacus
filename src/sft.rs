//! Supervised-fine-tuning traces.
//!
//! Every model call a session makes is appended to a JSONL file as one record:
//! the exact messages that went to the provider and the exact completion that
//! came back. Because the request is captured *after* the agent has layered on
//! the system prompt, the compaction summary, and the goal/task/mode context, a
//! record is a faithful sample of the task the model was actually given — which
//! is what makes the file usable for training a model to drive Abacus, rather
//! than just a log of what happened.
//!
//! What is deliberately kept:
//!
//! * **The full prompt**, including system messages and the rolling summary, so
//!   the compaction flow is learnable rather than invisible.
//! * **Reasoning**, where the provider exposes it separately from the answer.
//! * **Tool calls with their arguments**, and the tool results that followed —
//!   already present in the next record's messages, so a chain of records
//!   reconstructs the whole episode.
//!
//! One record per model call rather than one per session: a turn that makes
//! eight tool calls is eight training samples, each with its own correct
//! continuation, and a session that is never cleanly finished still leaves
//! usable data behind.
//!
//! `source` distinguishes a live capture from one rebuilt out of a saved
//! session by `abacus pull all`. Records written before the field existed
//! (schema version 1) are all live, since reconstruction did not exist then —
//! a reader may treat an absent `source` as `live`.
//!
//! Traces are plain files with no rotation. They contain your prompts, your
//! code, and your tool output, so they stay local and are trivially deletable —
//! `[trace] enabled = false` or the `/config` row turns them off.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};

/// Schema version, so a later reader can tell what it is looking at. Version 2
/// added `source`, distinguishing a live capture from one rebuilt after the
/// fact out of a saved session.
pub const TRACE_VERSION: u32 = 2;

/// One model call: what was asked, and what came back.
#[derive(Debug, Serialize)]
struct Record<'a> {
    version: u32,
    /// RFC 3339, UTC.
    timestamp: String,
    session: &'a str,
    model: &'a str,
    /// `live` when captured as the call happened, `session` when rebuilt from a
    /// saved transcript. A rebuilt sample is lower fidelity — see
    /// [`records_from_session`] — so a trainer can weight or drop them.
    source: &'a str,
    /// Which model call within the session, from 1.
    step: u64,
    /// Workflow mode in force for this call — AUTO, PLAN, or BUILD.
    mode: &'a str,
    /// Exactly what was sent, system messages and all.
    messages: &'a [Value],
    /// Tool schemas offered, by name. The full schemas are large and identical
    /// across a session; the names are what a sample needs to be interpretable.
    tools: Vec<&'a str>,
    completion: Completion<'a>,
}

#[derive(Debug, Serialize)]
struct Completion<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    content: &'a str,
    /// Chain-of-thought, when the provider reports it apart from the answer.
    #[serde(skip_serializing_if = "str::is_empty")]
    reasoning: &'a str,
    #[serde(skip_serializing_if = "<[Value]>::is_empty")]
    tool_calls: Vec<Value>,
    /// True when the user interrupted before the reply finished, so a trainer
    /// can drop truncated samples.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    cancelled: bool,
}

/// One model call, as the caller hands it over. A struct rather than a long
/// argument list so the call site names what each part is.
pub struct Sample<'a> {
    pub session: &'a str,
    pub model: &'a str,
    pub mode: &'a str,
    /// The request exactly as sent, system messages included.
    pub messages: &'a [Value],
    pub tools: &'a [Value],
    pub content: &'a str,
    pub reasoning: &'a str,
    pub tool_calls: &'a [crate::tools::ToolCall],
    pub cancelled: bool,
}

/// Appends records for one session.
///
/// Cloneable and internally locked so the agent task can write while the UI
/// reads. Every failure is swallowed after the first report: a training log is
/// never worth interrupting a session for.
#[derive(Clone)]
pub struct TraceWriter {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    file: File,
    path: PathBuf,
    step: u64,
    /// Set once a write fails, so a broken trace reports once and then stops
    /// trying rather than failing on every call.
    broken: bool,
}

impl TraceWriter {
    /// Open (creating if needed) the trace for `session` under `directory`.
    pub fn open(directory: &Path, session: &str) -> Result<Self> {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        let path = directory.join(format!("{session}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;
        // Resuming a session continues its trace, so numbering picks up where
        // the file left off instead of restarting and colliding.
        let step = std::fs::read_to_string(&path)
            .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count() as u64)
            .unwrap_or(0);
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                file,
                path,
                step,
                broken: false,
            })),
        })
    }

    pub fn path(&self) -> PathBuf {
        self.lock().path.clone()
    }

    /// Records written so far, for the diagnostics panel.
    pub fn steps(&self) -> u64 {
        self.lock().step
    }

    /// Append one model call. Returns the error only the first time, so a
    /// caller can surface it once; later failures are silent.
    pub fn record(&self, sample: Sample<'_>) -> Result<()> {
        let Sample {
            session,
            model,
            mode,
            messages,
            tools,
            content,
            reasoning,
            tool_calls,
            cancelled,
        } = sample;
        let mut inner = self.lock();
        if inner.broken {
            return Ok(());
        }
        inner.step += 1;
        let record = Record {
            version: TRACE_VERSION,
            timestamp: chrono::Utc::now().to_rfc3339(),
            session,
            model,
            source: "live",
            step: inner.step,
            mode,
            messages,
            tools: tools
                .iter()
                .filter_map(|tool| {
                    tool.pointer("/function/name")
                        .or_else(|| tool.get("name"))
                        .and_then(Value::as_str)
                })
                .collect(),
            completion: Completion {
                content,
                reasoning,
                tool_calls: tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id,
                            "name": call.name,
                            // Left as the raw string the model emitted: a
                            // trainer needs the bytes that were produced, not a
                            // re-encoding of them.
                            "arguments": call.arguments,
                        })
                    })
                    .collect(),
                cancelled,
            },
        };
        let mut line = serde_json::to_vec(&record).context("could not encode trace record")?;
        line.push(b'\n');
        match inner
            .file
            .write_all(&line)
            .and_then(|()| inner.file.flush())
        {
            Ok(()) => Ok(()),
            Err(error) => {
                inner.broken = true;
                Err(error).with_context(|| format!("could not write {}", inner.path.display()))
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Rebuild trace records from a saved session transcript.
///
/// A session file stores the *conversation*, not the requests that produced it,
/// so a rebuilt record is necessarily thinner than a live capture: there is no
/// reasoning (never persisted), no list of tools that were offered, and no
/// record of which mode each call ran under. What it does have is the prompt as
/// the model saw it and the completion it produced, which is the substance of a
/// training sample.
///
/// One record per assistant message: everything before it is the prompt, and it
/// is the correct continuation — the same shape a live trace produces.
pub fn records_from_session(session: &Value) -> Vec<Value> {
    let Some(messages) = session["messages"].as_array() else {
        return Vec::new();
    };
    let id = session["id"].as_str().unwrap_or("unknown");
    let model = session["model"].as_str().unwrap_or("unknown");
    let timestamp = session["updated_at"].as_str().unwrap_or_default();

    let mut records = Vec::new();
    let mut step = 0u64;
    for (index, message) in messages.iter().enumerate() {
        if message["role"] != "assistant" {
            continue;
        }
        // An assistant message with neither text nor a call teaches nothing.
        let content = message["content"].as_str().unwrap_or_default();
        let calls = message["tool_calls"].as_array();
        if content.trim().is_empty() && calls.is_none_or(|calls| calls.is_empty()) {
            continue;
        }
        step += 1;
        let tool_calls = calls
            .map(|calls| {
                calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call["id"],
                            "name": call.pointer("/function/name"),
                            "arguments": call.pointer("/function/arguments"),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut completion = json!({});
        if !content.is_empty() {
            completion["content"] = json!(content);
        }
        if !tool_calls.is_empty() {
            completion["tool_calls"] = json!(tool_calls);
        }
        records.push(json!({
            "version": TRACE_VERSION,
            "timestamp": timestamp,
            "session": id,
            "model": model,
            "source": "session",
            "step": step,
            "mode": "UNKNOWN",
            "messages": &messages[..index],
            "tools": [],
            "completion": completion,
        }));
    }
    records
}

/// Rebuild traces for every saved session under `sessions_root`, skipping any
/// whose id already has a live trace — a live capture is strictly better, so it
/// is never replaced by a reconstruction.
pub fn pull_sessions(
    sessions_root: &Path,
    destination: &Path,
    already: &std::collections::HashSet<String>,
) -> Result<Vec<PullEntry>> {
    if !sessions_root.exists() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(destination)
        .with_context(|| format!("could not create {}", destination.display()))?;

    let mut pulled = Vec::new();
    // Sessions live one directory deep, keyed by workspace.
    let mut stack = vec![sessions_root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Ok(contents) = std::fs::read(&path) else {
                continue;
            };
            let Ok(session) = serde_json::from_slice::<Value>(&contents) else {
                continue;
            };
            let Some(id) = session["id"].as_str() else {
                continue;
            };
            let name = format!("{id}.jsonl");
            if already.contains(&name) {
                continue;
            }
            let records = records_from_session(&session);
            if records.is_empty() {
                pulled.push(PullEntry {
                    name,
                    records: 0,
                    bytes: 0,
                    outcome: Pulled::Empty,
                });
                continue;
            }
            let mut body = Vec::new();
            for record in &records {
                body.extend_from_slice(&serde_json::to_vec(record)?);
                body.push(b'\n');
            }
            let into = destination.join(&name);
            let outcome = match std::fs::read(&into) {
                Ok(existing) if existing == body => Pulled::Unchanged,
                Ok(_) => Pulled::Updated,
                Err(_) => Pulled::Copied,
            };
            if outcome != Pulled::Unchanged {
                std::fs::write(&into, &body)
                    .with_context(|| format!("could not write {}", into.display()))?;
            }
            pulled.push(PullEntry {
                name,
                records: records.len(),
                bytes: body.len() as u64,
                outcome,
            });
        }
    }
    pulled.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pulled)
}

/// What a `pull` did to one trace file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pulled {
    Copied,
    /// The destination already held an identical file.
    Unchanged,
    /// Refreshed a destination that differed — traces only ever grow, so the
    /// source is the fuller version.
    Updated,
    /// A session that recorded nothing; not useful as training data.
    Empty,
}

/// One trace considered by a `pull`.
#[derive(Debug, Clone)]
pub struct PullEntry {
    pub name: String,
    pub records: usize,
    pub bytes: u64,
    pub outcome: Pulled,
}

/// Copy every trace from `source` into `destination`, leaving the originals
/// exactly where they are.
///
/// Copies rather than moves on purpose: the traces directory is the running
/// record of this machine's sessions, and a live session may be appending to a
/// file while this runs. Taking them away would break capture and lose the
/// history; taking a copy costs only disk.
///
/// A destination file that already matches is left alone, and one that differs
/// is refreshed — traces are append-only, so the source is a superset of an
/// earlier copy. Files recording nothing are skipped.
pub fn pull(source: &Path, destination: &Path) -> Result<Vec<PullEntry>> {
    if !source.exists() {
        return Ok(Vec::new());
    }
    let source = source
        .canonicalize()
        .with_context(|| format!("could not resolve {}", source.display()))?;
    std::fs::create_dir_all(destination)
        .with_context(|| format!("could not create {}", destination.display()))?;
    let target = destination
        .canonicalize()
        .with_context(|| format!("could not resolve {}", destination.display()))?;
    if target == source {
        anyhow::bail!(
            "that is where the traces already live ({}) — run this somewhere else",
            source.display()
        );
    }

    let mut pulled = Vec::new();
    for entry in std::fs::read_dir(&source)
        .with_context(|| format!("could not read {}", source.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let contents = match std::fs::read(&path) {
            Ok(contents) => contents,
            // A file that vanished or is unreadable is reported as empty rather
            // than aborting a pull that has already copied others.
            Err(_) => continue,
        };
        let records = contents
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .count();
        let bytes = contents.len() as u64;
        if records == 0 {
            pulled.push(PullEntry {
                name: name.to_owned(),
                records,
                bytes,
                outcome: Pulled::Empty,
            });
            continue;
        }
        let into = target.join(name);
        let outcome = match std::fs::read(&into) {
            Ok(existing) if existing == contents => Pulled::Unchanged,
            Ok(_) => Pulled::Updated,
            Err(_) => Pulled::Copied,
        };
        if outcome != Pulled::Unchanged {
            std::fs::write(&into, &contents)
                .with_context(|| format!("could not write {}", into.display()))?;
        }
        pulled.push(PullEntry {
            name: name.to_owned(),
            records,
            bytes,
            outcome,
        });
    }
    pulled.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pulled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCall;
    use tempfile::tempdir;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn write_one(writer: &TraceWriter, content: &str) {
        let messages = [
            json!({"role":"system","content":"be careful"}),
            json!({"role":"user","content":"fix the parser"}),
        ];
        let tools = [json!({"type":"function","function":{"name":"read_file"}})];
        let calls = [call("read_file", r#"{"path":"src/parser.rs"}"#)];
        writer
            .record(Sample {
                session: "session-1",
                model: "test-model",
                mode: "BUILD",
                messages: &messages,
                tools: &tools,
                content,
                reasoning: "let me check the file first",
                tool_calls: &calls,
                cancelled: false,
            })
            .unwrap();
    }

    /// A saved transcript becomes one sample per assistant message: everything
    /// before it is the prompt, it is the continuation.
    #[test]
    fn a_session_rebuilds_into_one_record_per_model_call() {
        let session = json!({
            "id": "abc",
            "model": "test-model",
            "updated_at": "2026-07-31T00:00:00Z",
            "messages": [
                {"role":"system","content":"be careful"},
                {"role":"user","content":"read it"},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}
                ]},
                {"role":"tool","tool_call_id":"c1","name":"read_file","content":"fn main() {}"},
                {"role":"assistant","content":"It is a main function."}
            ]
        });
        let records = records_from_session(&session);
        assert_eq!(records.len(), 2);

        // First sample: prompt is everything before the call.
        assert_eq!(records[0]["step"], 1);
        assert_eq!(records[0]["source"], "session");
        assert_eq!(records[0]["messages"].as_array().unwrap().len(), 2);
        assert_eq!(
            records[0]["completion"]["tool_calls"][0]["name"],
            "read_file"
        );

        // Second: the tool result is now part of the prompt.
        assert_eq!(records[1]["messages"].as_array().unwrap().len(), 4);
        assert_eq!(
            records[1]["completion"]["content"],
            "It is a main function."
        );
        // Honest about what a reconstruction cannot know.
        assert_eq!(records[1]["mode"], "UNKNOWN");
        assert!(records[1]["completion"].get("reasoning").is_none());
    }

    #[test]
    fn an_assistant_message_with_nothing_in_it_is_not_a_sample() {
        let session = json!({
            "id": "abc",
            "model": "m",
            "messages": [
                {"role":"user","content":"hi"},
                {"role":"assistant","content":""},
                {"role":"assistant","content":"real answer"}
            ]
        });
        assert_eq!(records_from_session(&session).len(), 1);
    }

    /// A live capture carries reasoning and the tool list; a reconstruction
    /// cannot. Where both exist for a session, the live one must win.
    #[test]
    fn a_live_trace_is_never_replaced_by_a_reconstruction() {
        let home = tempdir().unwrap();
        let sessions = home.path().join("sessions/workspace-1");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("kept.json"),
            serde_json::to_vec(&json!({
                "id": "session-1",
                "model": "m",
                "messages": [
                    {"role":"user","content":"hi"},
                    {"role":"assistant","content":"rebuilt"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let out = tempdir().unwrap();
        let traces = home.path().join("traces");
        let writer = TraceWriter::open(&traces, "session-1").unwrap();
        write_one(&writer, "live");
        let live = pull(&traces, out.path()).unwrap();
        let captured: std::collections::HashSet<String> =
            live.iter().map(|entry| entry.name.clone()).collect();

        let rebuilt = pull_sessions(&home.path().join("sessions"), out.path(), &captured).unwrap();
        assert!(
            rebuilt.is_empty(),
            "the session already had a live trace: {rebuilt:?}"
        );
        let kept = std::fs::read_to_string(out.path().join("session-1.jsonl")).unwrap();
        assert!(kept.contains("\"source\":\"live\""));
    }

    #[test]
    fn sessions_without_a_trace_are_rebuilt() {
        let home = tempdir().unwrap();
        let sessions = home.path().join("sessions/workspace-1");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("old.json"),
            serde_json::to_vec(&json!({
                "id": "older",
                "model": "m",
                "messages": [
                    {"role":"user","content":"hi"},
                    {"role":"assistant","content":"an answer"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let out = tempdir().unwrap();
        let rebuilt = pull_sessions(
            &home.path().join("sessions"),
            out.path(),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].records, 1);
        let body = std::fs::read_to_string(out.path().join("older.jsonl")).unwrap();
        assert!(body.contains("\"source\":\"session\""));
        // Originals untouched, exactly as for a plain pull.
        assert!(sessions.join("old.json").exists());
    }

    /// The originals are the machine's running record and a live session may be
    /// appending to one. Pulling must never take them away.
    #[test]
    fn pull_copies_and_leaves_the_originals_in_place() {
        let home = tempdir().unwrap();
        let out = tempdir().unwrap();
        let writer = TraceWriter::open(home.path(), "session-1").unwrap();
        write_one(&writer, "first");
        let original = writer.path();

        let pulled = pull(home.path(), out.path()).unwrap();
        assert_eq!(pulled.len(), 1);
        assert_eq!(pulled[0].outcome, Pulled::Copied);
        assert_eq!(pulled[0].records, 1);

        assert!(original.exists(), "the source must survive a pull");
        let copy = out.path().join("session-1.jsonl");
        assert!(copy.exists());
        assert_eq!(
            std::fs::read(&original).unwrap(),
            std::fs::read(&copy).unwrap()
        );
    }

    /// Pulling twice is not an error and does not rewrite what already matches;
    /// a source that has grown since refreshes the copy.
    #[test]
    fn pulling_again_is_idempotent_until_the_source_grows() {
        let home = tempdir().unwrap();
        let out = tempdir().unwrap();
        let writer = TraceWriter::open(home.path(), "session-1").unwrap();
        write_one(&writer, "first");
        pull(home.path(), out.path()).unwrap();

        let again = pull(home.path(), out.path()).unwrap();
        assert_eq!(again[0].outcome, Pulled::Unchanged);

        write_one(&writer, "second");
        let grown = pull(home.path(), out.path()).unwrap();
        assert_eq!(grown[0].outcome, Pulled::Updated);
        assert_eq!(grown[0].records, 2);
        let copy = std::fs::read_to_string(out.path().join("session-1.jsonl")).unwrap();
        assert_eq!(copy.lines().filter(|l| !l.trim().is_empty()).count(), 2);
    }

    #[test]
    fn pull_skips_empty_traces_and_other_files() {
        let home = tempdir().unwrap();
        let out = tempdir().unwrap();
        // A session that recorded nothing, and a file that is not a trace.
        std::fs::write(home.path().join("empty.jsonl"), "").unwrap();
        std::fs::write(home.path().join("notes.txt"), "ignore me").unwrap();
        let writer = TraceWriter::open(home.path(), "real").unwrap();
        write_one(&writer, "content");

        let pulled = pull(home.path(), out.path()).unwrap();
        let names: Vec<&str> = pulled.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["empty.jsonl", "real.jsonl"]);
        assert_eq!(pulled[0].outcome, Pulled::Empty);
        assert!(
            !out.path().join("empty.jsonl").exists(),
            "an empty trace is not training data"
        );
        assert!(!out.path().join("notes.txt").exists());
        assert!(out.path().join("real.jsonl").exists());
    }

    /// Pulling into the traces directory itself would be a self-copy; refuse
    /// rather than do something surprising.
    #[test]
    fn pulling_into_the_source_is_refused() {
        let home = tempdir().unwrap();
        let writer = TraceWriter::open(home.path(), "session-1").unwrap();
        write_one(&writer, "first");
        assert!(pull(home.path(), home.path()).is_err());
    }

    #[test]
    fn pulling_with_nothing_recorded_is_not_an_error() {
        let home = tempdir().unwrap();
        let out = tempdir().unwrap();
        assert!(
            pull(&home.path().join("missing"), out.path())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_record_keeps_the_whole_prompt_and_the_reasoning() {
        let dir = tempdir().unwrap();
        let writer = TraceWriter::open(dir.path(), "session-1").unwrap();
        write_one(&writer, "reading it now");

        let text = std::fs::read_to_string(writer.path()).unwrap();
        let record: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(record["version"], TRACE_VERSION);
        assert_eq!(
            record["source"], "live",
            "a live capture must say so, to be told apart from a reconstruction"
        );
        assert_eq!(record["step"], 1);
        assert_eq!(record["mode"], "BUILD");
        // The system message is the point: without it a sample cannot teach the
        // model what it was actually asked to do.
        assert_eq!(record["messages"][0]["role"], "system");
        assert_eq!(record["messages"][0]["content"], "be careful");
        assert_eq!(record["tools"][0], "read_file");
        assert_eq!(
            record["completion"]["reasoning"],
            "let me check the file first"
        );
        assert_eq!(record["completion"]["tool_calls"][0]["name"], "read_file");
        assert_eq!(
            record["completion"]["tool_calls"][0]["arguments"], r#"{"path":"src/parser.rs"}"#,
            "arguments must survive as the model emitted them"
        );
    }

    #[test]
    fn one_line_per_model_call() {
        let dir = tempdir().unwrap();
        let writer = TraceWriter::open(dir.path(), "session-1").unwrap();
        write_one(&writer, "first");
        write_one(&writer, "second");
        let text = std::fs::read_to_string(writer.path()).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2);
        // Every line has to parse on its own — that is the whole contract of
        // JSONL, and a half-written record would break a training run.
        for (index, line) in lines.iter().enumerate() {
            let record: Value = serde_json::from_str(line).unwrap();
            assert_eq!(record["step"], index as u64 + 1);
        }
    }

    /// Resuming a session appends to its trace rather than restarting the
    /// numbering, so steps stay unique across the whole episode.
    #[test]
    fn reopening_continues_the_numbering() {
        let dir = tempdir().unwrap();
        let writer = TraceWriter::open(dir.path(), "session-1").unwrap();
        write_one(&writer, "first");
        drop(writer);

        let resumed = TraceWriter::open(dir.path(), "session-1").unwrap();
        assert_eq!(resumed.steps(), 1);
        write_one(&resumed, "second");
        let text = std::fs::read_to_string(resumed.path()).unwrap();
        let steps: Vec<u64> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["step"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(steps, vec![1, 2]);
    }

    /// Empty optional fields are omitted so a plain text turn does not carry
    /// three empty keys, and `cancelled` marks a truncated sample.
    #[test]
    fn empty_fields_are_omitted_and_cancellation_is_marked() {
        let dir = tempdir().unwrap();
        let writer = TraceWriter::open(dir.path(), "session-1").unwrap();
        let messages = [json!({"role":"user","content":"hi"})];
        writer
            .record(Sample {
                session: "session-1",
                model: "test-model",
                mode: "PLAN",
                messages: &messages,
                tools: &[],
                content: "hello",
                reasoning: "",
                tool_calls: &[],
                cancelled: true,
            })
            .unwrap();
        let text = std::fs::read_to_string(writer.path()).unwrap();
        let record: Value = serde_json::from_str(text.trim()).unwrap();
        assert!(record["completion"].get("reasoning").is_none());
        assert!(record["completion"].get("tool_calls").is_none());
        assert_eq!(record["completion"]["cancelled"], true);
        assert_eq!(record["completion"]["content"], "hello");
    }
}
