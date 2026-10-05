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
    bridge.on_event(&AgentEvent::Delta("Hel".into()), &usage());
    bridge.on_event(&AgentEvent::Delta("lo".into()), &usage());
    bridge.on_event(&AgentEvent::ToolStarted { name: "run_command".into(), summary: "cargo test".into() }, &usage());
    bridge.on_event(&AgentEvent::ToolFinished { name: "run_command".into(), output: "ok: 3 passed".into() }, &usage());
    bridge.on_event(&done(TurnEnd::Complete), &usage());
    let frames = drain(&mut frames);
    assert_eq!(
        types(&frames),
        [
            "status:thinking", "entry:start", "delta", "entry:complete", "tool:start", "status:tool", "tool:finish",
            "status:thinking", "done", "status:idle",
        ]
    );
    // The two deltas were coalesced into one frame, on the block's id.
    let entry_id = frames[1]["payload"]["entry"]["id"].clone();
    assert_eq!(frames[1]["payload"]["entry"]["kind"], "assistant");
    assert_eq!(frames[2]["payload"], json!({"entry_id": entry_id, "kind": "text", "text": "Hello"}));
    assert_eq!(frames[3]["payload"]["entry"]["text"], "Hello");
    // Tool start and finish share an entry and a call id.
    assert_eq!(frames[4]["payload"]["entry_id"], frames[6]["payload"]["entry_id"]);
    assert_ne!(frames[4]["payload"]["entry_id"], entry_id);
    assert_eq!(frames[4]["payload"]["call_id"], "t1");
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
    bridge.on_event(&AgentEvent::Reasoning("**Checking** things".into()), &usage());
    bridge.on_event(&AgentEvent::Delta("Answer".into()), &usage());
    bridge.on_event(&done(TurnEnd::Complete), &usage());
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
    bridge.on_event(&AgentEvent::Delta(big.clone()), &usage());
    bridge.on_event(&done(TurnEnd::Complete), &usage());
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
    bridge.on_event(&AgentEvent::Delta("a".into()), &usage());
    bridge.on_event(&AgentEvent::Delta("b".into()), &usage());
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
    bridge.on_event(&AgentEvent::Delta("Let me edit".into()), &usage());
    let approval = bridge.approval_opened("write_file", "notes.txt", "Write notes.txt\n+hello");
    assert_eq!(approval, "a1");
    bridge.approval_resolved(&approval, ApprovalDecision::Always, By::Browser);
    let question = bridge.question_opened("Pick", "Which one?", &["1 — Rewrite".into(), "2".into()], false);
    assert_eq!(question, "q1");
    bridge.question_resolved(&question, &["1".into()], None, By::Terminal);
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
    bridge.on_event(&AgentEvent::ToolStarted { name: "write_file".into(), summary: "x".into() }, &usage());
    let approval = bridge.approval_opened("write_file", "x", "details");
    drain(&mut frames);
    bridge.turn_ended(DoneReason::Interrupted, &usage());
    let frames = drain(&mut frames);
    assert_eq!(types(&frames), ["tool:finish", "approval_resolved", "done", "status:idle"]);
    assert_eq!(frames[0]["payload"]["status"], "failed");
    assert_eq!(frames[0]["payload"]["output"], "interrupted");
    assert_eq!(frames[1]["payload"]["approval_id"], approval);
    assert_eq!(frames[1]["payload"]["decision"], "reject");
    assert_eq!(frames[2]["payload"]["reason"], "interrupted");
    assert_eq!(frames[3]["payload"]["label"], "interrupted");
}

#[test]
fn a_failed_turn_reports_the_error_then_done() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.turn_started();
    bridge.on_event(&AgentEvent::Delta("partial".into()), &usage());
    bridge.on_event(&AgentEvent::Failed { error: "provider returned 500".into(), messages: Vec::new() }, &usage());
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
    bridge.user_prompt("Fix the parser");
    bridge.on_event(&AgentEvent::ModeChanged { mode: AgentMode::Plan, reason: "read first".into() }, &usage());
    bridge.on_event(&AgentEvent::Notice("cut short".into()), &usage());
    bridge.on_event(&AgentEvent::TraceFailed { error: "disk full".into() }, &usage());
    let frames = drain(&mut frames);
    assert_eq!(types(&frames), ["entry:complete", "mode", "notice", "notice"]);
    assert_eq!(frames[0]["payload"]["entry"]["kind"], "user");
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

#[test]
fn a_snapshot_keeps_the_streaming_block_id_and_redacts_pairing_links() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.turn_started();
    bridge.on_event(&AgentEvent::Delta("Working on".into()), &usage());
    let approval = bridge.approval_opened("edit_file", "src/x.rs", "details");
    bridge.on_event(&AgentEvent::Reasoning("hidden thoughts".into()), &usage());
    let streamed = drain(&mut frames);
    let reasoning_id = streamed
        .iter()
        .rev()
        .find(|frame| frame["type"] == "entry" && frame["payload"]["entry"]["kind"] == "reasoning")
        .unwrap()["payload"]["entry"]["id"]
        .clone();

    let mut tool = ui::ToolCall::running("run_command", "ls");
    tool.status = ui::ToolStatus::Ok;
    tool.full = "a\nb".into();
    let entries = vec![
        ui::Entry::new(ui::EntryKind::User, "hello"),
        ui::Entry::new(ui::EntryKind::System, "Pairing link: https://x.test/pair#t=SECRET-token_1 (single use)"),
        ui::Entry::tool(tool),
        ui::Entry::new(ui::EntryKind::Assistant, "Working on"),
    ];
    let view = SnapshotView { session: session_info(), entries: &entries, live: true, usage: &usage() };
    bridge.send_snapshot(view, SnapshotReason::Requested);
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
    assert_eq!(entries[4]["id"], reasoning_id);
    assert_eq!(entries[4]["text"], "hidden thoughts");
    let system = entries[1]["text"].as_str().unwrap();
    assert!(!system.contains("SECRET") && system.contains("#t=…"), "{system}");
    assert_eq!(entries[2]["tool"]["output"], "a\nb");
    assert_eq!(entries[2]["tool"]["status"], "ok");
    let ids: std::collections::HashSet<&str> = entries.iter().map(|entry| entry["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), entries.len(), "ids are unique");

    // Deltas after the snapshot continue the same block.
    bridge.on_event(&AgentEvent::Reasoning(" more".into()), &usage());
    bridge.break_text();
    let after = drain(&mut frames);
    let delta = after.iter().find(|frame| frame["type"] == "delta").unwrap();
    assert_eq!(delta["payload"]["entry_id"], reasoning_id);
}

#[test]
fn a_snapshot_reuses_the_open_text_block_when_the_terminal_shows_it() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    bridge.on_event(&AgentEvent::Delta("Hi".into()), &usage());
    let started = drain(&mut frames);
    let id = started.iter().find(|frame| frame["type"] == "entry").unwrap()["payload"]["entry"]["id"].clone();
    let entries = vec![ui::Entry::new(ui::EntryKind::Assistant, "Hi")];
    bridge.send_snapshot(
        SnapshotView { session: session_info(), entries: &entries, live: true, usage: &usage() },
        SnapshotReason::Connect,
    );
    let pages: Vec<Value> = drain(&mut frames).into_iter().filter(|frame| frame["type"] == "snapshot").collect();
    let entries = pages[0]["payload"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], id);
}

#[test]
fn long_transcripts_snapshot_their_recent_end_in_bounded_pages() {
    let (mut bridge, mut frames) = Bridge::detached("s1");
    let entries: Vec<ui::Entry> =
        (0..3_000).map(|n| ui::Entry::new(ui::EntryKind::Assistant, format!("{n}: {}", "w".repeat(2_000)))).collect();
    bridge.send_snapshot(
        SnapshotView { session: session_info(), entries: &entries, live: false, usage: &usage() },
        SnapshotReason::Connect,
    );
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
    let entries = Vec::new();
    bridge.send_snapshot(
        SnapshotView { session: session_info(), entries: &entries, live: false, usage: &usage() },
        SnapshotReason::Requested,
    );
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
    let view = SnapshotView { session: session_info(), entries: &transcript, live: false, usage: &usage() };
    bridge.send_snapshot(view, SnapshotReason::Connect);
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
    bridge.user_prompt("write a note");
    bridge.turn_started();
    bridge.on_event(&AgentEvent::Delta("hello from the agent".into()), &usage());
    bridge.on_event(&done(TurnEnd::Complete), &usage());
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
    let view = SnapshotView { session: session_info(), entries: &transcript, live: false, usage: &usage() };
    bridge.send_snapshot(view, SnapshotReason::Reconnect);
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
