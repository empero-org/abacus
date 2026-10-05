use super::*;
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};

fn usage() -> TokenUsage {
    TokenUsage { total: 30, input: 20, output: 10, cache_read: 5, cache_write: 0 }
}

/// Every queued frame as JSON (`{"type":…,"payload":…}`).
fn drain(frames: &mut mpsc::Receiver<Outbound>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(frame) = frames.try_recv() {
        out.push(serde_json::to_value(frame).unwrap());
    }
    out
}

fn types(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .map(|frame| {
            let kind = frame["type"].as_str().unwrap();
            match (kind, frame["payload"]["phase"].as_str(), frame["payload"]["state"].as_str()) {
                (_, Some(phase), _) => format!("{kind}:{phase}"),
                ("status", _, Some(state)) => format!("status:{state}"),
                _ => kind.to_owned(),
            }
        })
        .collect()
}

fn session_info() -> SessionInfo {
    SessionInfo {
        id: "s1".into(),
        title: "Fix parser".into(),
        workspace: "/w".into(),
        model: "m".into(),
        mode: "auto".into(),
        app_version: "0.6.4".into(),
        started_at: "2026-10-04T12:00:00Z".into(),
    }
}

fn done(reason: TurnEnd) -> AgentEvent {
    AgentEvent::Done { messages: Vec::new(), reason }
}

#[test]
fn a_turn_maps_to_the_documented_frame_sequence() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.turn_started();
    // The terminal's transcript numbers the reply 1 and the tool row 2.
    bridge.on_event(&AgentEvent::Delta("Hel".into()), &usage(), Some(1));
    bridge.on_event(&AgentEvent::Delta("lo".into()), &usage(), Some(1));
    bridge.on_event(
        &AgentEvent::ToolStarted { name: "run_command".into(), summary: "cargo test".into() },
        &usage(),
        Some(2),
    );
    bridge.on_event(
        &AgentEvent::ToolFinished { name: "run_command".into(), output: "ok: 3 passed".into() },
        &usage(),
        Some(2),
    );
    bridge.on_event(&done(TurnEnd::Complete), &usage(), None);
    let frames = drain(&mut frames);
    assert_eq!(
        types(&frames),
        [
            "status:thinking", "entry:start", "delta", "entry:complete", "tool:start", "status:tool", "tool:finish",
            "status:thinking", "done", "status:idle",
        ]
    );
    // The two deltas were coalesced into one frame, on the block's id: the
    // number the terminal gave the entry.
    let entry_id = frames[1]["payload"]["entry"]["id"].clone();
    assert_eq!(entry_id, "e1");
    assert_eq!(frames[1]["payload"]["entry"]["kind"], "assistant");
    assert_eq!(frames[2]["payload"], json!({"entry_id": entry_id, "kind": "text", "text": "Hello"}));
    assert_eq!(frames[3]["payload"]["entry"]["text"], "Hello");
    // Tool start and finish share an entry and a call id.
    assert_eq!(frames[4]["payload"]["entry_id"], frames[6]["payload"]["entry_id"]);
    assert_ne!(frames[4]["payload"]["entry_id"], entry_id);
    assert_eq!(frames[4]["payload"]["entry_id"], "e2");
    assert_eq!(frames[4]["payload"]["call_id"], "t2");
    assert_eq!(frames[5]["payload"]["label"], "running run_command");
    assert_eq!(frames[6]["payload"]["status"], "ok");
    assert_eq!(frames[6]["payload"]["output"], "ok: 3 passed");
    assert!(frames[6]["payload"]["duration_ms"].is_u64());
    assert_eq!(frames[8]["payload"]["reason"], "complete");
    assert_eq!(frames[8]["payload"]["usage"]["input"], 20);
    assert_eq!(frames[9]["payload"]["label"], "ready");
}

#[test]
fn reasoning_and_text_stream_as_separate_blocks() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.on_event(&AgentEvent::Reasoning("**Checking** things".into()), &usage(), Some(1));
    bridge.on_event(&AgentEvent::Delta("Answer".into()), &usage(), Some(2));
    bridge.on_event(&done(TurnEnd::Complete), &usage(), None);
    let frames = drain(&mut frames);
    let entries: Vec<(String, String)> = frames
        .iter()
        .filter(|frame| frame["type"] == "entry")
        .map(|frame| {
            (
                frame["payload"]["phase"].as_str().unwrap().to_owned(),
                frame["payload"]["entry"]["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        entries,
        [
            ("start".to_owned(), "reasoning".to_owned()),
            ("complete".to_owned(), "reasoning".to_owned()),
            ("start".to_owned(), "assistant".to_owned()),
            ("complete".to_owned(), "assistant".to_owned()),
        ]
    );
    let delta = frames.iter().find(|frame| frame["type"] == "delta").unwrap();
    assert_eq!(delta["payload"]["kind"], "reasoning");
}

#[test]
fn long_text_is_sent_in_bounded_deltas_and_clipped_on_completion() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    let big = "é".repeat(50_000); // 100 KB, two bytes a character
    bridge.on_event(&AgentEvent::Delta(big.clone()), &usage(), Some(1));
    bridge.on_event(&done(TurnEnd::Complete), &usage(), None);
    let frames = drain(&mut frames);
    let deltas: Vec<&str> = frames
        .iter()
        .filter(|frame| frame["type"] == "delta")
        .map(|frame| frame["payload"]["text"].as_str().unwrap())
        .collect();
    assert!(deltas.len() >= 13);
    assert!(deltas.iter().all(|text| text.len() <= protocol::MAX_DELTA));
    assert_eq!(deltas.concat(), big, "every byte streams, in order");
    let complete = frames.iter().find(|frame| frame["payload"]["phase"] == "complete").unwrap();
    assert!(complete["payload"]["entry"]["text"].as_str().unwrap().len() <= protocol::MAX_ENTRY_TEXT);
    assert_eq!(complete["payload"]["entry"]["clipped"], true);
}

#[test]
fn flush_sends_coalesced_text_after_the_interval() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.on_event(&AgentEvent::Delta("a".into()), &usage(), Some(1));
    bridge.on_event(&AgentEvent::Delta("b".into()), &usage(), Some(1));
    assert!(drain(&mut frames).iter().all(|frame| frame["type"] != "delta"), "held while coalescing");
    std::thread::sleep(DELTA_INTERVAL);
    bridge.flush();
    let frames = drain(&mut frames);
    assert_eq!(types(&frames), ["delta"]);
    assert_eq!(frames[0]["payload"]["text"], "ab");
}

#[test]
fn approvals_and_questions_round_trip_with_their_ids() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.turn_started();
    bridge.on_event(&AgentEvent::Delta("Let me edit".into()), &usage(), Some(1));
    let approval = bridge.approval_opened("write_file", "notes.txt", "Write notes.txt\n+hello");
    assert_eq!(approval, "a1");
    bridge.approval_resolved(&approval, ApprovalDecision::Always, By::Browser, 2);
    let question = bridge.question_opened("Pick", "Which one?", &["1 — Rewrite".into(), "2".into()], false);
    assert_eq!(question, "q1");
    bridge.question_resolved(&question, &["1".into()], None, By::Terminal, 2);
    let frames = drain(&mut frames);
    assert_eq!(
        types(&frames),
        [
            "status:thinking", "entry:start", "delta", "entry:complete", "approval", "status:waiting_approval",
            "approval_resolved", "status:thinking", "question", "status:waiting_answer", "question_resolved",
            "status:thinking",
        ]
    );
    assert_eq!(frames[4]["payload"]["approval_id"], "a1");
    assert_eq!(frames[4]["payload"]["kind"], "other");
    assert_eq!(frames[6]["payload"], json!({"approval_id": "a1", "decision": "always", "by": "browser"}));
    assert_eq!(frames[8]["payload"]["options"][0], json!({"label": "1", "description": "Rewrite"}));
    assert_eq!(frames[8]["payload"]["options"][1], json!({"label": "2", "description": ""}));
    assert_eq!(
        frames[10]["payload"],
        json!({"question_id": "q1", "selected": ["1"], "custom": null, "by": "terminal"})
    );
}

#[test]
fn an_aborted_turn_settles_everything_still_open() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.turn_started();
    bridge.on_event(&AgentEvent::ToolStarted { name: "write_file".into(), summary: "x".into() }, &usage(), Some(1));
    bridge.approval_opened("write_file", "x", "details");
    drain(&mut frames);
    bridge.turn_ended(DoneReason::Interrupted, &usage());
    let frames = drain(&mut frames);
    // The approval nobody answered is not "denied": `done` closes it in the
    // browsers, and a resolution frame would show up as a decision.
    assert_eq!(types(&frames), ["tool:finish", "done", "status:idle"]);
    assert_eq!(frames[0]["payload"]["status"], "failed");
    assert_eq!(frames[0]["payload"]["output"], "interrupted");
    assert_eq!(frames[1]["payload"]["reason"], "interrupted");
    assert_eq!(frames[2]["payload"]["label"], "interrupted");
}

#[test]
fn a_turn_that_ends_with_a_dialog_open_sends_no_decision_for_it() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.turn_started();
    let approval = bridge.approval_opened("run_command", "rm -rf target", "$ rm -rf target");
    bridge.on_event(&done(TurnEnd::Complete), &usage(), None);
    let ended = drain(&mut frames);
    assert!(!types(&ended).iter().any(|kind| kind.ends_with("_resolved")), "{:?}", types(&ended));
    assert_eq!(types(&ended).last().map(String::as_str), Some("status:idle"));

    // A question the same way, whether the turn completed or failed.
    bridge.turn_started();
    bridge.question_opened("Pick", "Which?", &["A".into()], false);
    bridge.on_event(&AgentEvent::Failed { error: "boom".into(), messages: Vec::new() }, &usage(), None);
    let failed = drain(&mut frames);
    assert!(!types(&failed).iter().any(|kind| kind.ends_with("_resolved")), "{:?}", types(&failed));

    // Nothing is pending afterwards, and no row claims a decision.
    bridge.send_snapshot(
        SnapshotView { session: session_info(), entries: &[], usage: &usage() },
        SnapshotReason::Requested,
    );
    let sent = drain(&mut frames);
    let snapshot = &sent[0]["payload"];
    assert_eq!(snapshot["pending"], json!({"approval": null, "question": null}));
    assert_eq!(snapshot["entries"], json!([]));
    assert!(!bridge.noted(&format!("resolved-{approval}")));
}

#[test]
fn a_failed_turn_reports_the_error_then_done() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.turn_started();
    bridge.on_event(&AgentEvent::Delta("partial".into()), &usage(), Some(1));
    bridge.on_event(
        &AgentEvent::Failed { error: "provider returned 500".into(), messages: Vec::new() },
        &usage(),
        None,
    );
    let frames = drain(&mut frames);
    assert_eq!(
        types(&frames),
        ["status:thinking", "entry:start", "delta", "entry:complete", "error", "done", "status:idle"]
    );
    assert_eq!(frames[4]["payload"], json!({"message": "provider returned 500", "fatal": false}));
    assert_eq!(frames[5]["payload"]["reason"], "failed");
    assert_eq!(frames[6]["payload"]["label"], "error");
}

#[test]
fn notices_modes_and_prompts_are_mirrored() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.user_prompt(1, "Fix the parser");
    bridge.on_event(&AgentEvent::ModeChanged { mode: AgentMode::Plan, reason: "read first".into() }, &usage(), None);
    bridge.on_event(&AgentEvent::Notice("cut short".into()), &usage(), None);
    bridge.on_event(&AgentEvent::TraceFailed { error: "disk full".into() }, &usage(), None);
    let frames = drain(&mut frames);
    assert_eq!(types(&frames), ["entry:complete", "mode", "notice", "notice"]);
    assert_eq!(frames[0]["payload"]["entry"]["kind"], "user");
    assert_eq!(frames[0]["payload"]["entry"]["id"], "e1");
    assert_eq!(frames[0]["payload"]["entry"]["text"], "Fix the parser");
    assert_eq!(frames[1]["payload"], json!({"mode": "plan", "reason": "read first"}));
    assert_eq!(frames[2]["payload"]["level"], "info");
    assert_eq!(frames[3]["payload"]["level"], "warning");
}

#[test]
fn the_thinking_label_is_rate_limited_and_only_while_thinking() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.thinking_label("Checking the parser");
    assert!(drain(&mut frames).is_empty(), "idle: no label");
    bridge.turn_started();
    drain(&mut frames);
    bridge.thinking_label("Checking the parser");
    assert!(drain(&mut frames).is_empty(), "within 500 ms of the last status");
    std::thread::sleep(LABEL_INTERVAL);
    bridge.thinking_label("Checking the parser");
    let frames = drain(&mut frames);
    assert_eq!(types(&frames), ["status:thinking"]);
    assert_eq!(frames[0]["payload"]["label"], "Checking the parser");
}

/// Transcript entries numbered the way the terminal numbers them: 1, 2, ….
fn numbered(entries: Vec<ui::Entry>) -> Vec<ui::Entry> {
    entries
        .into_iter()
        .enumerate()
        .map(|(index, mut entry)| {
            entry.id = index as u64 + 1;
            entry
        })
        .collect()
}

fn view<'a>(entries: &'a [ui::Entry], usage: &'a TokenUsage) -> SnapshotView<'a> {
    SnapshotView { session: session_info(), entries, usage }
}

/// The entries of the snapshot just sent, across its pages.
fn snapshot_entries(frames: &mut mpsc::Receiver<Outbound>) -> Vec<Value> {
    let pages: Vec<Value> = drain(frames).into_iter().filter(|frame| frame["type"] == "snapshot").collect();
    pages.iter().flat_map(|page| page["payload"]["entries"].as_array().unwrap().clone()).collect()
}

fn ids(entries: &[Value]) -> Vec<&str> {
    entries.iter().map(|entry| entry["id"].as_str().unwrap()).collect()
}

#[test]
fn a_snapshot_keeps_the_streaming_block_id_and_redacts_pairing_links() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.turn_started();
    bridge.on_event(&AgentEvent::Delta("Working on".into()), &usage(), Some(4));
    let approval = bridge.approval_opened("edit_file", "src/x.rs", "details");
    // The terminal hides reasoning, so it has no entry for it.
    bridge.on_event(&AgentEvent::Reasoning("hidden thoughts".into()), &usage(), None);
    let streamed = drain(&mut frames);
    let reasoning_id = streamed
        .iter()
        .rev()
        .find(|frame| frame["type"] == "entry" && frame["payload"]["entry"]["kind"] == "reasoning")
        .unwrap()["payload"]["entry"]["id"]
        .clone();
    assert_eq!(reasoning_id, "x1", "a block of the bridge's own");

    let mut tool = ui::ToolCall::running("run_command", "ls");
    tool.status = ui::ToolStatus::Ok;
    tool.full = "a\nb".into();
    let entries = numbered(vec![
        ui::Entry::new(ui::EntryKind::User, "hello"),
        ui::Entry::new(ui::EntryKind::System, "Pairing link: https://x.test/pair#t=SECRET-token_1 (single use)"),
        ui::Entry::tool(tool),
        ui::Entry::new(ui::EntryKind::Assistant, "Working on"),
    ]);
    bridge.send_snapshot(view(&entries, &usage()), SnapshotReason::Requested);
    // Text still being coalesced goes out first, so the snapshot is current.
    let pages: Vec<Value> = drain(&mut frames).into_iter().filter(|frame| frame["type"] == "snapshot").collect();
    assert_eq!(pages.len(), 1);
    let payload = &pages[0]["payload"];
    assert_eq!(payload["reason"], "requested");
    assert_eq!(payload["page"], json!({"index": 0, "count": 1}));
    assert_eq!(payload["session"]["title"], "Fix parser");
    assert_eq!(payload["status"]["state"], "thinking");
    assert_eq!(payload["pending"]["approval"]["approval_id"], approval);
    assert_eq!(payload["usage"]["total"], 30);
    let entries = payload["entries"].as_array().unwrap();
    let kinds: Vec<&str> = entries.iter().map(|entry| entry["kind"].as_str().unwrap()).collect();
    // The hidden reasoning block is still streaming, so it is appended with
    // the id its deltas use.
    assert_eq!(kinds, ["user", "system", "tool", "assistant", "reasoning"]);
    assert_eq!(ids(entries), ["e1", "e2", "e3", "e4", "x1"]);
    assert_eq!(entries[4]["text"], "hidden thoughts");
    let system = entries[1]["text"].as_str().unwrap();
    assert!(!system.contains("SECRET") && system.contains("#t=…"), "{system}");
    assert_eq!(entries[2]["tool"]["output"], "a\nb");
    assert_eq!(entries[2]["tool"]["status"], "ok");
    assert_eq!(entries[2]["tool"]["call_id"], "t3");

    // Deltas after the snapshot continue the same block.
    bridge.on_event(&AgentEvent::Reasoning(" more".into()), &usage(), None);
    bridge.break_text();
    let after = drain(&mut frames);
    let delta = after.iter().find(|frame| frame["type"] == "delta").unwrap();
    assert_eq!(delta["payload"]["entry_id"], reasoning_id);
}

#[test]
fn a_snapshot_names_the_open_text_block_by_its_terminal_entry() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.on_event(&AgentEvent::Delta("Hi".into()), &usage(), Some(1));
    let started = drain(&mut frames);
    let id = started.iter().find(|frame| frame["type"] == "entry").unwrap()["payload"]["entry"]["id"].clone();
    let entries = numbered(vec![ui::Entry::new(ui::EntryKind::Assistant, "Hi")]);
    bridge.send_snapshot(view(&entries, &usage()), SnapshotReason::Connect);
    let entries = snapshot_entries(&mut frames);
    assert_eq!(entries.len(), 1, "the streaming block is that entry, not a second row");
    assert_eq!(entries[0]["id"], id);
}

#[test]
fn ids_are_the_same_live_and_in_every_snapshot() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    let mut tool = ui::ToolCall::running("read_file", "a.rs");
    tool.status = ui::ToolStatus::Ok;
    let mut entries = numbered(vec![
        ui::Entry::new(ui::EntryKind::User, "look"),
        ui::Entry::tool(tool),
        ui::Entry::new(ui::EntryKind::Assistant, "Looked."),
    ]);
    bridge.send_snapshot(view(&entries, &usage()), SnapshotReason::Connect);
    let first = snapshot_entries(&mut frames);
    assert_eq!(ids(&first), ["e1", "e2", "e3"]);

    // The same transcript again: nothing is renamed.
    bridge.send_snapshot(view(&entries, &usage()), SnapshotReason::Requested);
    assert_eq!(snapshot_entries(&mut frames), first);

    // Something new arrives live under the id the next snapshot gives it...
    bridge.user_prompt(4, "again");
    bridge.on_event(&AgentEvent::Delta("Sure".into()), &usage(), Some(5));
    bridge.on_event(&AgentEvent::ToolStarted { name: "grep".into(), summary: "x".into() }, &usage(), Some(6));
    let live = drain(&mut frames);
    let live_ids: Vec<&str> = live
        .iter()
        .filter_map(|frame| {
            let payload = &frame["payload"];
            match frame["type"].as_str()? {
                "entry" => payload["entry"]["id"].as_str(),
                "tool" => payload["entry_id"].as_str(),
                _ => None,
            }
        })
        .collect();
    assert_eq!(live_ids, ["e4", "e5", "e5", "e6"], "start and complete name the entry alike");

    // ...and the entries before it keep theirs, whatever the terminal did to
    // the ones in between (here: the tool row was merged away, as the
    // terminal does with read-only runs, so number 2 is gone).
    entries.push(ui::Entry::new(ui::EntryKind::User, "again"));
    entries.last_mut().unwrap().id = 4;
    entries.push(ui::Entry::new(ui::EntryKind::Assistant, "Sure"));
    entries.last_mut().unwrap().id = 5;
    entries.remove(1);
    bridge.send_snapshot(view(&entries, &usage()), SnapshotReason::Requested);
    let later = snapshot_entries(&mut frames);
    assert_eq!(ids(&later), ["e1", "e3", "e4", "e5"]);
    assert_eq!(later[0], first[0]);
    assert_eq!(later[1], first[2]);
}

#[test]
fn text_continued_after_hidden_reasoning_goes_on_in_the_same_row() {
    // The terminal keeps appending to its one assistant entry across reasoning
    // it hides. The browsers' row goes on too: a second `entry start` under
    // its id would read as the finished row beginning again, and a row of its
    // own would show the same text twice after a resync.
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.on_event(&AgentEvent::Delta("Before ".into()), &usage(), Some(1));
    bridge.on_event(&AgentEvent::Reasoning("thinking".into()), &usage(), None);
    bridge.on_event(&AgentEvent::Delta("after".into()), &usage(), Some(1));
    // A snapshot taken now holds the entry once, with all of its text.
    let entries = numbered(vec![ui::Entry::new(ui::EntryKind::Assistant, "Before after")]);
    bridge.send_snapshot(view(&entries, &usage()), SnapshotReason::Requested);
    bridge.on_event(&done(TurnEnd::Complete), &usage(), None);
    let frames = drain(&mut frames);
    let starts: Vec<&str> = frames
        .iter()
        .filter(|frame| frame["type"] == "entry" && frame["payload"]["phase"] == "start")
        .map(|frame| frame["payload"]["entry"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(starts, ["e1", "x1"]);
    let deltas: Vec<(&str, &str)> = frames
        .iter()
        .filter(|frame| frame["type"] == "delta")
        .map(|frame| (frame["payload"]["entry_id"].as_str().unwrap(), frame["payload"]["text"].as_str().unwrap()))
        .collect();
    assert_eq!(deltas, [("e1", "Before "), ("x1", "thinking"), ("e1", "after")]);
    let snapshot = frames.iter().find(|frame| frame["type"] == "snapshot").unwrap();
    assert_eq!(ids(snapshot["payload"]["entries"].as_array().unwrap()), ["e1"]);
    let completes: Vec<(&str, &str)> = frames
        .iter()
        .filter(|frame| frame["type"] == "entry" && frame["payload"]["phase"] == "complete")
        .map(|frame| {
            (frame["payload"]["entry"]["id"].as_str().unwrap(), frame["payload"]["entry"]["text"].as_str().unwrap())
        })
        .collect();
    assert_eq!(completes, [("e1", "Before "), ("x1", "thinking"), ("e1", "Before after")], "the last carries it all");
}

#[test]
fn a_resync_replays_decisions_as_the_rows_live_viewers_saw() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.turn_started();
    bridge.on_event(&AgentEvent::Delta("I will write it.".into()), &usage(), Some(2));
    // The terminal's transcript: the prompt (1) and the reply (2); the tool
    // row (3) comes once the approval is settled.
    let approval = bridge.approval_opened("write_file", "notes.txt", "details");
    bridge.approval_resolved(&approval, ApprovalDecision::Once, By::Browser, 3);
    bridge.on_event(
        &AgentEvent::ToolStarted { name: "write_file".into(), summary: "notes.txt".into() },
        &usage(),
        Some(3),
    );
    bridge.on_event(&AgentEvent::ToolFinished { name: "write_file".into(), output: "ok".into() }, &usage(), Some(3));
    let question = bridge.question_opened("Pick", "Which?", &["1 — Rewrite".into(), "2 — Patch".into()], false);
    bridge.question_resolved(&question, &["2".into()], None, By::Terminal, 4);
    bridge.on_event(&AgentEvent::Delta("Done.".into()), &usage(), Some(4));
    drain(&mut frames);

    let mut tool = ui::ToolCall::running("write_file", "notes.txt");
    tool.status = ui::ToolStatus::Ok;
    let entries = numbered(vec![
        ui::Entry::new(ui::EntryKind::User, "write notes"),
        ui::Entry::new(ui::EntryKind::Assistant, "I will write it."),
        ui::Entry::tool(tool),
        ui::Entry::new(ui::EntryKind::Assistant, "Done."),
    ]);
    bridge.send_snapshot(view(&entries, &usage()), SnapshotReason::Requested);
    let first = snapshot_entries(&mut frames);
    assert_eq!(ids(&first), ["e1", "e2", "resolved-a1", "e3", "answered-q1", "e4"]);
    assert_eq!(first[2]["kind"], "system");
    assert_eq!(first[2]["text"], "Allowed write_file notes.txt · from phone");
    assert_eq!(first[4]["kind"], "system");
    assert_eq!(first[4]["text"], "Answered \"Patch\" · in the terminal");

    // Asked again, the same rows come back: none is added or doubled.
    bridge.send_snapshot(view(&entries, &usage()), SnapshotReason::Requested);
    assert_eq!(snapshot_entries(&mut frames), first);
}

#[test]
fn decision_rows_say_what_was_decided_and_by_whom() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    let note = |bridge: &mut Bridge, frames: &mut mpsc::Receiver<Outbound>, id: &str| {
        bridge.send_snapshot(view(&[], &usage()), SnapshotReason::Requested);
        let entries = snapshot_entries(frames);
        entries.iter().find(|entry| entry["id"] == id).unwrap()["text"].as_str().unwrap().to_owned()
    };
    let always = bridge.approval_opened("edit_file", "src/x.rs", "diff");
    bridge.approval_resolved(&always, ApprovalDecision::Always, By::Terminal, 1);
    assert_eq!(
        note(&mut bridge, &mut frames, "resolved-a1"),
        "Allowed edit_file src/x.rs for this session · in the terminal"
    );
    let denied = bridge.approval_opened("run_command", "rm -rf   target\nnow", "$ rm");
    bridge.approval_resolved(&denied, ApprovalDecision::Reject, By::Browser, 1);
    assert_eq!(note(&mut bridge, &mut frames, "resolved-a2"), "Denied run_command rm -rf target now · from phone");

    // The summary is not repeated when it is the tool's own name.
    let bare = bridge.approval_opened("spawn_subagents", "spawn_subagents", "x");
    bridge.approval_resolved(&bare, ApprovalDecision::Once, By::Terminal, 1);
    assert_eq!(note(&mut bridge, &mut frames, "resolved-a3"), "Allowed spawn_subagents · in the terminal");

    // An option answers with what it says; typed text is quoted instead; a
    // label the question never offered stands for itself.
    let options = ["Yes — go on".to_owned(), "No".to_owned()];
    let first = bridge.question_opened("Ok?", "Continue?", &options, true);
    bridge.question_resolved(&first, &["Yes".into(), "No".into(), "Maybe".into()], None, By::Browser, 1);
    assert_eq!(note(&mut bridge, &mut frames, "answered-q1"), "Answered \"go on, No, Maybe\" · from phone");
    let second = bridge.question_opened("Ok?", "Continue?", &options, false);
    bridge.question_resolved(&second, &["No".into()], Some("  only the\ntests "), By::Terminal, 1);
    assert_eq!(note(&mut bridge, &mut frames, "answered-q2"), "Answered \"only the tests\" · in the terminal");
    let third = bridge.question_opened("Ok?", "Continue?", &options, false);
    bridge.question_resolved(&third, &[], None, By::Terminal, 1);
    assert_eq!(note(&mut bridge, &mut frames, "answered-q3"), "Skipped the question · in the terminal");
}

#[test]
fn decisions_rows_leave_with_the_part_of_the_conversation_they_belong_to() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    let first = bridge.approval_opened("write_file", "a", "d");
    bridge.approval_resolved(&first, ApprovalDecision::Once, By::Terminal, 2);
    let second = bridge.approval_opened("write_file", "b", "d");
    bridge.approval_resolved(&second, ApprovalDecision::Once, By::Terminal, 6);
    // Rewinding to the prompt numbered 5 takes the second decision with it.
    bridge.entries_dropped(5);
    let entries =
        numbered(vec![ui::Entry::new(ui::EntryKind::User, "one"), ui::Entry::new(ui::EntryKind::User, "two")]);
    bridge.send_snapshot(view(&entries, &usage()), SnapshotReason::Requested);
    assert_eq!(ids(&snapshot_entries(&mut frames)), ["e1", "resolved-a1", "e2"]);
    bridge.entries_dropped(0);
    bridge.send_snapshot(view(&entries, &usage()), SnapshotReason::Requested);
    assert_eq!(ids(&snapshot_entries(&mut frames)), ["e1", "e2"]);
}

#[test]
fn a_refusal_never_goes_out_without_a_reason() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.accepted("b1", InputKind::Prompt, AcceptResult::Rejected, None);
    bridge.accepted("b2", InputKind::Prompt, AcceptResult::Rejected, Some("prompt is empty"));
    bridge.accepted("b3", InputKind::Prompt, AcceptResult::Queued, None);
    let frames = drain(&mut frames);
    assert_eq!(frames[0]["payload"]["reason"], "the terminal could not act on that");
    assert_eq!(frames[1]["payload"]["reason"], "prompt is empty");
    assert_eq!(frames[2]["payload"]["reason"], Value::Null);
}

#[test]
fn a_refusal_can_tell_a_decided_dialog_from_one_that_never_was() {
    let (mut bridge, _frames) = Bridge::detached("s1");
    let approval = bridge.approval_opened("write_file", "a", "d");
    let question = bridge.question_opened("Q", "Which?", &[], false);
    assert_eq!(bridge.approval_miss("a99"), "no open approval with that id");
    assert_eq!(bridge.question_miss("q99"), "no open question with that id");
    bridge.approval_resolved(&approval, ApprovalDecision::Once, By::Browser, 1);
    bridge.question_resolved(&question, &[], Some("x"), By::Browser, 1);
    assert_eq!(bridge.approval_miss(&approval), "approval already decided");
    assert_eq!(bridge.question_miss(&question), "question already answered");
}

#[test]
fn long_transcripts_snapshot_their_recent_end_in_bounded_pages() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    let entries: Vec<ui::Entry> =
        (0..3_000).map(|n| ui::Entry::new(ui::EntryKind::Assistant, format!("{n}: {}", "w".repeat(2_000)))).collect();
    bridge.send_snapshot(view(&entries, &usage()), SnapshotReason::Connect);
    let pages = drain(&mut frames);
    assert!(pages.len() > 1);
    let mut all = Vec::new();
    for (index, page) in pages.iter().enumerate() {
        assert_eq!(page["payload"]["page"], json!({"index": index, "count": pages.len()}));
        let wire = protocol::encode(&serde_json::from_value(page.clone()).unwrap(), "x", 1);
        assert!(wire.len() <= protocol::TARGET_FRAME_BYTES);
        all.extend(page["payload"]["entries"].as_array().unwrap().iter().cloned());
    }
    assert_eq!(all[0]["kind"], "system", "a leading line says what was left out");
    assert!(all[0]["text"].as_str().unwrap().contains("earlier entries"));
    assert!(all.last().unwrap()["text"].as_str().unwrap().starts_with("2999: "));
    assert!(all.len() < 3_000);
}

#[test]
fn a_full_queue_asks_for_a_resync_instead_of_blocking() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    for n in 0..(QUEUE + 10) {
        bridge.notice(&format!("n{n}"), Level::Info);
    }
    assert!(bridge.resync);
    assert!(!bridge.needs_resync(), "not while the queue is still full");
    drain(&mut frames);
    assert!(bridge.needs_resync());
    bridge.send_snapshot(view(&[], &usage()), SnapshotReason::Requested);
    assert!(!bridge.needs_resync());
}

#[test]
fn link_events_drive_the_badge_state() {
    let (mut bridge, _frames) = Bridge::detached("s1");
    assert_eq!(bridge.state(), &LinkState::Connecting);
    bridge.observe(&Inbound::Disconnected { reason: "offline".into(), retry_in: Duration::from_secs(1) });
    assert_eq!(bridge.state(), &LinkState::Connecting, "never connected yet");
    bridge.observe(&Inbound::Connected { reconnect: false });
    bridge.observe(&Inbound::Peers { browsers: 2 });
    assert_eq!((bridge.state(), bridge.browsers()), (&LinkState::Live, 2));
    bridge.observe(&Inbound::Disconnected { reason: "lost".into(), retry_in: Duration::from_secs(1) });
    assert_eq!((bridge.state(), bridge.browsers()), (&LinkState::Reconnecting("lost".into()), 0));
    bridge.observe(&Inbound::Closed { reason: "signed out".into() });
    assert!(!bridge.is_active());
}

#[test]
fn pairing_tokens_are_redacted_wherever_they_appear() {
    assert_eq!(redact_pairing("open https://a.test/pair#t=abc_DEF-1 now"), "open https://a.test/pair#t=… now");
    assert_eq!(redact_pairing("two #t=x and #t=y."), "two #t=… and #t=….");
    assert_eq!(redact_pairing("no token #t= here"), "no token #t= here");
    assert_eq!(redact_pairing("plain"), "plain");
}

// ---------------------------------------------------------------------------
// The link against an in-process relay
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FakeRelay {
    address: SocketAddr,
    tickets: Arc<AtomicUsize>,
    prepared: Arc<AtomicUsize>,
    disabled: Arc<AtomicBool>,
}

impl FakeRelay {
    fn new(address: SocketAddr) -> Self {
        Self {
            address,
            tickets: Arc::new(AtomicUsize::new(0)),
            prepared: Arc::new(AtomicUsize::new(0)),
            disabled: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Connector for FakeRelay {
    async fn prepare(&self) -> Result<(), LinkError> {
        self.prepared.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn socket_url(&self) -> Result<String, LinkError> {
        let ticket = self.tickets.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(format!("ws://{}/v1/remote/agent/s1?ticket=t{ticket}", self.address))
    }

    async fn disable(&self) {
        self.disabled.store(true, Ordering::SeqCst);
    }
}

fn fast() -> LinkConfig {
    LinkConfig {
        heartbeat: Duration::from_secs(30),
        idle_timeout: Duration::from_secs(60),
        backoff_base: Duration::from_millis(20),
        backoff_cap: Duration::from_millis(100),
        connect_timeout: Duration::from_secs(5),
        send_timeout: Duration::from_secs(5),
        stable_after: Duration::from_secs(60),
        disable_timeout: Duration::from_secs(1),
    }
}

type Peer = WebSocketStream<TcpStream>;

/// Accept the agent's next connection; returns it with the request URI.
#[allow(clippy::result_large_err)] // the handshake callback's signature is tungstenite's
async fn accept(listener: &TcpListener) -> (Peer, String) {
    let (stream, _) = timeout(Duration::from_secs(5), listener.accept()).await.expect("agent connects").unwrap();
    let uri = Arc::new(Mutex::new(String::new()));
    let seen = uri.clone();
    let socket = accept_hdr_async(stream, move |request: &Request, response: Response| {
        *seen.lock().unwrap() = request.uri().to_string();
        Ok::<_, ErrorResponse>(response)
    })
    .await
    .unwrap();
    let uri = uri.lock().unwrap().clone();
    (socket, uri)
}

async fn say(peer: &mut Peer, frame: Value) {
    peer.send(Message::Text(frame.to_string().into())).await.unwrap();
}

async fn hear(peer: &mut Peer) -> Value {
    loop {
        let message = timeout(Duration::from_secs(5), peer.next()).await.expect("a frame").unwrap().unwrap();
        if let Message::Text(text) = message {
            return serde_json::from_str(text.as_str()).unwrap();
        }
    }
}

async fn event(events: &mut mpsc::UnboundedReceiver<Inbound>) -> Inbound {
    timeout(Duration::from_secs(5), events.recv()).await.expect("a link event").unwrap()
}

fn hello(browsers: usize) -> Value {
    json!({"v":1,"type":"hello","id":"server","seq":0,"payload":{"role":"agent","session_id":"s1",
        "server_time":"2026-10-04T12:00:00Z","heartbeat_interval_s":25,"idle_timeout_s":75,
        "max_frame_bytes":131072,"peers":{"agent":true,"browsers":browsers}}})
}

fn launch(relay: &FakeRelay) -> (Bridge, mpsc::UnboundedReceiver<Inbound>) {
    let (sender, events) = mpsc::unbounded_channel();
    let notify: Notify = Arc::new(move |event| {
        let _ = sender.send(event);
    });
    (Bridge::launch("s1".into(), relay.clone(), notify, fast()), events)
}

use futures_util::{SinkExt, StreamExt};

#[tokio::test]
async fn the_link_relays_a_turn_reconnects_with_a_fresh_ticket_and_yields_to_a_replacement() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = FakeRelay::new(listener.local_addr().unwrap());
    let (mut bridge, mut events) = launch(&relay);
    let transcript = vec![ui::Entry::new(ui::EntryKind::User, "earlier prompt")];

    // hello → snapshot
    let (mut peer, uri) = accept(&listener).await;
    assert!(uri.ends_with("/v1/remote/agent/s1?ticket=t1"), "{uri}");
    let connected = event(&mut events).await;
    assert_eq!(connected, Inbound::Connected { reconnect: false });
    bridge.observe(&connected);
    say(&mut peer, hello(1)).await;
    let peers = event(&mut events).await;
    assert_eq!(peers, Inbound::Peers { browsers: 1 });
    bridge.observe(&peers);
    bridge.send_snapshot(view(&transcript, &usage()), SnapshotReason::Connect);
    let snapshot = hear(&mut peer).await;
    assert_eq!(
        (snapshot["type"].as_str(), snapshot["v"].as_u64(), snapshot["seq"].as_u64()),
        (Some("snapshot"), Some(1), Some(1))
    );
    assert_eq!(snapshot["payload"]["reason"], "connect");
    assert_eq!(snapshot["payload"]["entries"][0]["text"], "earlier prompt");

    // prompt (sent twice, as a browser retry would) → one event
    let prompt = json!({"v":1,"type":"prompt","id":"b1","seq":1,"payload":{"text":"write a note"}});
    say(&mut peer, prompt.clone()).await;
    say(&mut peer, prompt).await;
    say(&mut peer, json!({"v":1,"type":"ack","id":"b0","seq":1,"payload":{}})).await;
    assert_eq!(event(&mut events).await, Inbound::Prompt { ref_id: "b1".into(), text: "write a note".into() });

    // accepted → status → entry → delta → done
    bridge.accepted("b1", InputKind::Prompt, AcceptResult::Queued, None);
    bridge.user_prompt(2, "write a note");
    bridge.turn_started();
    bridge.on_event(&AgentEvent::Delta("hello from the agent".into()), &usage(), Some(3));
    bridge.on_event(&done(TurnEnd::Complete), &usage(), None);
    let mut heard = Vec::new();
    loop {
        let frame = hear(&mut peer).await;
        let finished = frame["type"] == "status" && frame["payload"]["state"] == "idle";
        heard.push(frame);
        if finished {
            break;
        }
    }
    assert_eq!(
        types(&heard),
        [
            "accepted", "entry:complete", "status:thinking", "entry:start", "delta", "entry:complete", "done",
            "status:idle"
        ]
    );
    assert_eq!(heard[0]["payload"], json!({"ref_id":"b1","kind":"prompt","result":"queued","reason":null}));
    assert_eq!(heard[4]["payload"]["text"], "hello from the agent");
    let seqs: Vec<u64> = heard.iter().map(|frame| frame["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (2..10).collect::<Vec<_>>(), "seq increases by one per frame");
    assert!(events.try_recv().is_err(), "the duplicate prompt was dropped");

    // browser ping → pong; invalid input → accepted rejected
    say(&mut peer, json!({"v":1,"type":"ping","id":"b2","seq":2,"payload":{"ts":"p1"}})).await;
    assert_eq!(hear(&mut peer).await["payload"], json!({"ts":"p1"}));
    say(&mut peer, json!({"v":1,"type":"approve","id":"b3","seq":3,"payload":{"approval_id":"a1","decision":"maybe"}}))
        .await;
    let refused = hear(&mut peer).await;
    assert_eq!(refused["type"], "accepted");
    assert_eq!(refused["payload"]["result"], "rejected");

    // The relay drops the socket: the link reconnects with a new ticket and
    // the bridge resends a snapshot that restarts seq at 1.
    drop(peer);
    let lost = event(&mut events).await;
    assert!(matches!(lost, Inbound::Disconnected { .. }), "{lost:?}");
    bridge.observe(&lost);
    assert!(matches!(bridge.state(), LinkState::Reconnecting(_)));
    let (mut peer, uri) = accept(&listener).await;
    assert!(uri.ends_with("ticket=t2"), "a fresh ticket per connection: {uri}");
    let reconnected = event(&mut events).await;
    assert_eq!(reconnected, Inbound::Connected { reconnect: true });
    bridge.observe(&reconnected);
    say(&mut peer, hello(0)).await;
    assert_eq!(event(&mut events).await, Inbound::Peers { browsers: 0 });
    bridge.send_snapshot(view(&transcript, &usage()), SnapshotReason::Reconnect);
    let snapshot = hear(&mut peer).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["seq"], 1);
    assert_eq!(snapshot["payload"]["reason"], "reconnect");
    // The prompt id is still remembered across the reconnect.
    say(&mut peer, json!({"v":1,"type":"prompt","id":"b1","seq":1,"payload":{"text":"write a note"}})).await;
    say(&mut peer, json!({"v":1,"type":"interrupt","id":"b4","seq":2,"payload":{}})).await;
    assert_eq!(event(&mut events).await, Inbound::Interrupt { ref_id: "b4".into() });

    // A newer agent connection replaced this one: stop, do not fight it.
    peer.close(Some(CloseFrame { code: CloseCode::from(4409), reason: "replaced".into() })).await.unwrap();
    let closed = event(&mut events).await;
    assert!(matches!(&closed, Inbound::Closed { reason } if reason.contains("another terminal")), "{closed:?}");
    assert!(timeout(Duration::from_millis(300), listener.accept()).await.is_err(), "no reconnect after 4409");
    assert_eq!(relay.prepared.load(Ordering::SeqCst), 1, "prepared once, not per connection");
    assert!(!relay.disabled.load(Ordering::SeqCst), "the new owner keeps the session shared");
}

#[tokio::test]
async fn stopping_says_goodbye_closes_cleanly_and_disables_sharing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = FakeRelay::new(listener.local_addr().unwrap());
    let (mut bridge, mut events) = launch(&relay);
    let (mut peer, _) = accept(&listener).await;
    assert_eq!(event(&mut events).await, Inbound::Connected { reconnect: false });
    bridge.notice("last words", Level::Info);
    let task = bridge.stop("sharing stopped in the terminal", true).unwrap();
    assert_eq!(hear(&mut peer).await["payload"]["text"], "last words", "queued frames go first");
    let goodbye = hear(&mut peer).await;
    assert_eq!(goodbye["payload"], json!({"text": "sharing stopped in the terminal", "level": "info"}));
    let close = loop {
        match timeout(Duration::from_secs(5), peer.next()).await.unwrap() {
            Some(Ok(Message::Close(frame))) => break frame,
            Some(Ok(_)) => continue,
            other => panic!("expected a close frame, got {other:?}"),
        }
    };
    assert_eq!(close.map(|frame| u16::from(frame.code)), Some(1000));
    timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
    assert!(relay.disabled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_disabled_session_or_a_refused_ticket_stops_the_link() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = FakeRelay::new(listener.local_addr().unwrap());
    let (_bridge, mut events) = launch(&relay);
    let (mut peer, _) = accept(&listener).await;
    assert_eq!(event(&mut events).await, Inbound::Connected { reconnect: false });
    say(&mut peer, hello(0)).await;
    assert_eq!(event(&mut events).await, Inbound::Peers { browsers: 0 });
    peer.close(Some(CloseFrame { code: CloseCode::from(4403), reason: "disabled".into() })).await.unwrap();
    assert!(matches!(event(&mut events).await, Inbound::Closed { reason } if reason.contains("turned off")));
    assert!(timeout(Duration::from_millis(200), listener.accept()).await.is_err());

    // A refused ticket is retried once with a fresh one, then given up.
    let relay = FakeRelay::new(listener.local_addr().unwrap());
    let (_bridge, mut events) = launch(&relay);
    for _ in 0..2 {
        let (mut peer, _) = accept(&listener).await;
        assert_eq!(event(&mut events).await, event_connected(&relay));
        peer.close(Some(CloseFrame { code: CloseCode::from(4401), reason: "invalid ticket".into() })).await.unwrap();
        let next = event(&mut events).await;
        if relay.tickets.load(Ordering::SeqCst) == 1 {
            assert!(matches!(next, Inbound::Disconnected { .. }), "{next:?}");
        } else {
            assert!(matches!(next, Inbound::Closed { .. }), "{next:?}");
        }
    }
    assert!(timeout(Duration::from_millis(200), listener.accept()).await.is_err());
}

/// The `Connected` event the relay's next connection produces.
fn event_connected(relay: &FakeRelay) -> Inbound {
    Inbound::Connected { reconnect: relay.tickets.load(Ordering::SeqCst) > 1 }
}

/// What the sync server's refusals mean for the link: a ticket for a session
/// whose sharing was turned off elsewhere is a 409, and says exactly that.
#[test]
fn refusals_from_the_server_stop_or_retry_the_link_with_the_right_words() {
    let off = classify(SyncError::Conflict(None));
    assert_eq!(off, LinkError::Fatal("sharing was turned off for this session".into()));
    assert_eq!(classify(SyncError::Deleted(None)), off);
    assert!(matches!(classify(SyncError::Transient("timed out".into())), LinkError::Retry(_)));
    assert!(matches!(classify(SyncError::Unauthorized), LinkError::Fatal(reason) if reason.contains("signed out")));
}
