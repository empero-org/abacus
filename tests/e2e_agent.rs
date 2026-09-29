mod support;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use abacus_agent::{
    agent::{AgentEvent, AgentMode, ApprovalDecision, DoneReason, TurnOptions},
    config::ProviderProtocol,
    provider::Provider,
    tools::tool_specs,
};
use serde_json::json;
use support::{Mock, Reply, calls, contents, project, says, turn};

const EDIT: &str = r#"{"path":"value.txt","old_text":"old\n","new_text":"new\n"}"#;

fn edit_call() -> String {
    calls(&[("edit_1", "edit_file", serde_json::from_str(EDIT).unwrap())])
}

fn locked() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[tokio::test]
async fn streamed_agent_searches_workspace_and_finishes() {
    let project = project(&[("main.rs", "fn main() { /* needle */ }\n")]);
    let mock = Mock::script([
        calls(&[("call_1", "grep", json!({"query": "needle"}))]),
        says("Found the reference in main.rs."),
    ])
    .await;

    let mut searched = false;
    let (completed, _) = turn(
        project.provider(&mock),
        project.asks("Find needle"),
        TurnOptions { allow_subagents: true, ..project.options() },
        |event| searched |= matches!(event, AgentEvent::ToolStarted { name, .. } if name == "grep"),
    )
    .await;
    mock.finish().await;

    assert!(searched);
    assert!(completed.iter().any(|message| {
        message["role"] == "tool" && message["content"].as_str().is_some_and(|content| content.contains("main.rs:1"))
    }));
    assert_eq!(completed.last().unwrap()["content"], "Found the reference in main.rs.");
}

/// The bug this pins: interrupting used to abort the agent task, and since
/// `messages` lived inside that task, every tool result from the turn was
/// discarded. The edits stayed on disk while the model lost all memory of
/// making them.
#[tokio::test]
async fn a_cancelled_turn_keeps_the_work_it_already_did() {
    let project = project(&[("main.rs", "fn main() { /* needle */ }\n")]);
    // The second request is accepted but never answered: the stalled-stream
    // case, where cancellation has to be noticed without a chunk arriving.
    let mut search = Some(calls(&[("call_1", "grep", json!({"query": "needle"}))]));
    let mock = Mock::answering(move |_| search.take().map_or(Reply::Hang, Reply::Send)).await;

    let cancel = locked();
    let raised = cancel.clone();
    let (completed, reason) = turn(
        project.provider(&mock),
        project.asks("Find needle"),
        TurnOptions { cancel, allow_subagents: true, ..project.options() },
        // Cancel the moment the first tool has run, mimicking a user pressing
        // esc partway through.
        |event| {
            if matches!(event, AgentEvent::ToolFinished { .. }) {
                raised.store(true, Ordering::Relaxed);
            }
        },
    )
    .await;
    mock.abort();

    assert_eq!(reason, DoneReason::Interrupted);
    // The assistant's tool call and the tool's result both survive, so the
    // next turn knows the search happened.
    assert!(
        completed.iter().any(|message| message["role"] == "assistant" && message["tool_calls"].is_array()),
        "the assistant's tool call should be in history"
    );
    let tool_result =
        completed.iter().find(|message| message["role"] == "tool").expect("the tool result should be in history");
    assert_eq!(tool_result["name"], "grep");
}

#[tokio::test]
async fn responses_protocol_uses_responses_endpoint_and_stream_format() {
    let project = project(&[]);
    let mock = Mock::script([concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ready\"}\n\n",
        "data: {\"type\":\"response.completed\"}\n\n"
    )
    .to_owned()])
    .await;
    let config = abacus_agent::config::Config { protocol: ProviderProtocol::Responses, ..project.config(mock.address) };
    let (deltas, mut streamed) = tokio::sync::mpsc::unbounded_channel();
    let completion = Provider::new(&config)
        .unwrap()
        .complete(&[json!({"role":"user","content":"hello"})], &tool_specs(), deltas, &AtomicBool::new(false))
        .await
        .unwrap();

    assert_eq!(completion.content, "ready");
    assert_eq!(streamed.try_recv().unwrap(), abacus_agent::provider::Chunk::Text("ready".to_owned()));
    let request = &mock.finish().await[0];
    assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(request.contains("\"input\""));
    assert!(request.contains("\"name\":\"grep\""));
}

#[tokio::test]
async fn edit_requires_reviewable_approval_before_atomic_write() {
    let project = project(&[("value.txt", "old\n")]);
    let mock = Mock::script([edit_call(), says("Updated value.txt.")]).await;

    let mut approved = false;
    turn(
        project.provider(&mock),
        project.asks("update the value"),
        TurnOptions { allow_mutations: locked(), allow_subagents: true, ..project.options() },
        |event| {
            if let AgentEvent::Approval(request) = event {
                assert_eq!(request.tool, "edit_file");
                assert!(request.details.contains("-old"));
                assert!(request.details.contains("+new"));
                request.respond.send(ApprovalDecision::Once).unwrap();
                approved = true;
            }
        },
    )
    .await;
    mock.finish().await;

    assert!(approved);
    assert_eq!(project.read("value.txt"), "new\n");
}

/// A model served without native function-calling emits a Hermes-format tool
/// call as assistant TEXT (no `tool_calls` field). With `tool_format` set, the
/// provider must parse it and the agent must dispatch the tool.
#[tokio::test]
async fn text_emitted_tool_calls_are_parsed_when_native_calls_absent() {
    use abacus_agent::tool_format::{ToolFormat, render_hermes_call};
    let project = project(&[("target.txt", "hello\n")]);
    let call = render_hermes_call("read_file", r#"{"path":"target.txt"}"#);
    let mock = Mock::script([says(&format!("Reading.\n{call}")), says("Done, target.txt contains hello.")]).await;
    let config = abacus_agent::config::Config { tool_format: ToolFormat::Hermes, ..project.config(mock.address) };

    let (mut saw_read, mut saw_result) = (false, false);
    turn(
        Provider::new(&config).unwrap(),
        project.asks("read target.txt"),
        TurnOptions { mode: AgentMode::Auto, allow_mutations: locked(), allow_subagents: true, ..project.options() },
        |event| match event {
            AgentEvent::ToolStarted { name, summary } => {
                assert_eq!(name, "read_file");
                assert!(summary.contains("target.txt"));
                saw_read = true;
            }
            AgentEvent::ToolFinished { name, output } => {
                assert_eq!(name, "read_file");
                assert!(output.contains("hello"), "tool output should contain file content");
                saw_result = true;
            }
            _ => {}
        },
    )
    .await;
    mock.finish().await;

    assert!(saw_read, "text-emitted read_file call must be parsed and dispatched");
    assert!(saw_result, "read_file must return the file contents");
}

#[tokio::test]
async fn auto_mode_blocks_mutation_until_model_selects_build() {
    let project = project(&[("value.txt", "old\n")]);
    let mock = Mock::script([edit_call(), says("I need to select a mode first.")]).await;

    let mut blocked = false;
    turn(
        project.provider(&mock),
        project.asks("update the value"),
        TurnOptions { mode: AgentMode::Auto, allow_subagents: true, ..project.options() },
        |event| match event {
            AgentEvent::ToolFinished { name, output } if name == "edit_file" => {
                blocked = output.contains("Blocked by AUTO MODE")
            }
            AgentEvent::Approval(_) => panic!("blocked AUTO mutation requested approval"),
            _ => {}
        },
    )
    .await;
    mock.finish().await;

    assert!(blocked);
    assert_eq!(project.read("value.txt"), "old\n");
}

#[tokio::test]
async fn auto_mode_selection_enables_later_tool_in_same_completion() {
    let project = project(&[("value.txt", "old\n")]);
    let choose = json!({"mode": "build", "reason": "The user requested implementation"});
    let mock = Mock::script([
        calls(&[("mode_1", "mode_set", choose), ("edit_1", "edit_file", serde_json::from_str(EDIT).unwrap())]),
        says("Updated value.txt."),
    ])
    .await;

    let mut selected_build = false;
    turn(
        project.provider(&mock),
        project.asks("update the value"),
        TurnOptions { mode: AgentMode::Auto, allow_subagents: true, ..project.options() },
        |event| {
            if let AgentEvent::ModeChanged { mode, .. } = event {
                selected_build = mode == AgentMode::Build;
            }
        },
    )
    .await;
    mock.finish().await;

    assert!(selected_build);
    assert_eq!(project.read("value.txt"), "new\n");
}

/// What kind of background request the agent is making. A summariser or
/// reflector that is the conversation's own model asks from inside the live
/// context; one on another model builds a detached prompt. Both are recognised.
#[derive(PartialEq)]
enum Asked {
    Summary,
    ReviewGate,
    RefinePlan,
    Turn,
}

const IN_CONTEXT_SUMMARY: &str = "Your context is full";

fn asked(request: &str) -> Asked {
    let any = |phrases: &[&str]| phrases.iter().any(|phrase| request.contains(phrase));
    if any(&[IN_CONTEXT_SUMMARY, "context-aware state summary"]) {
        Asked::Summary
    } else if any(&["refinement review gate", "Looking back at the turn you have just finished"]) {
        Asked::ReviewGate
    } else if any(&["continual-harness refiner", "to your own reusable state"]) {
        Asked::RefinePlan
    } else {
        Asked::Turn
    }
}

/// A conversation over the compaction threshold (400k chars), carried by a
/// large assistant message that microcompaction cannot shrink away — which
/// forces the rolling-summary path.
fn oversized(project: &support::Project) -> Vec<serde_json::Value> {
    let mut messages = project.asks("please do the thing");
    messages.push(json!({"role":"assistant","content":format!("BIGBLOB{}", "x".repeat(420_000))}));
    messages.push(json!({"role":"user","content":"continue"}));
    messages
}

#[tokio::test]
async fn rolling_summary_compaction_fires_on_large_context() {
    let project = project(&[]);
    let mock = Mock::answering(|request| match asked(request) {
        Asked::Summary => Reply::Send(says(
            "1. Primary Request and Intent: do the thing. \
             9. Required Files:\n- src/main.rs\n10. Next Step: continue.",
        )),
        // The review gate runs before summary compaction; refuse it so no
        // planning call fires and the flow continues.
        Asked::ReviewGate => Reply::Send(says(r#"{"should_refine": false, "rationale": "nothing to keep"}"#)),
        _ => Reply::Last(says("all done")),
    })
    .await;

    let mut messages = oversized(&project);
    messages.push(json!({"role":"assistant","content":"working on it"}));
    let (completed, _) =
        turn(project.provider(&mock), messages, TurnOptions { allow_subagents: true, ..project.options() }, |_| {})
            .await;
    let requests = mock.finish().await;

    let summary = requests.iter().find(|request| asked(request) == Asked::Summary);
    let summary = summary.expect("compaction summarization call was not made");
    // With no compaction model assigned the summariser is the conversation's
    // own model, so the summary is asked for inside the live context — the
    // request whose prefix the provider already has cached.
    assert!(
        summary.contains(IN_CONTEXT_SUMMARY) && summary.contains("BIGBLOB"),
        "summary was not requested in-context"
    );
    let contents = contents(&completed);
    // The LLM path was taken, not the drop-only fallback and its system note.
    assert!(
        !contents.iter().any(|content| content.contains("were omitted")),
        "fallback drop-only path was used instead of LLM summarization"
    );
    assert!(!contents.iter().any(|content| content.contains("BIGBLOB")), "compacted middle was not dropped");
    assert!(contents.contains(&"working on it"), "recent tail was not preserved");
    assert_eq!(completed.last().unwrap()["content"], "all done");
}

/// The tether's intent snapshot used to run *after* the answer, so every first
/// turn ended with a two-second stall. It now runs beside the turn: this proves
/// the intent request reaches the server while the main stream is still open,
/// and that the snapshot still lands on the tether by the time the turn is done.
#[tokio::test]
async fn intent_snapshot_runs_beside_the_turn_not_after_it() {
    let project = project(&[]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Milliseconds since the server started at which the intent request
    // arrived, and at which the main answer finished streaming.
    let intent_arrived: Arc<Mutex<Option<u128>>> = Arc::default();
    let answer_finished: Arc<Mutex<Option<u128>>> = Arc::default();
    let (intent_probe, answer_probe) = (intent_arrived.clone(), answer_finished.clone());
    let server = tokio::spawn(async move {
        let start = std::time::Instant::now();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (intent_probe, answer_probe) = (intent_probe.clone(), answer_probe.clone());
            tokio::spawn(async move {
                if support::read_request(&mut stream).await.contains("INTENT") {
                    *intent_probe.lock().unwrap() = Some(start.elapsed().as_millis());
                    return support::respond(&mut stream, &says("Ship the importer fix.")).await;
                }
                // The main answer is served slowly, so an intent call that only
                // started afterwards could not possibly arrive before it ends.
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                support::respond(&mut stream, &says("all done")).await;
                *answer_probe.lock().unwrap() = Some(start.elapsed().as_millis());
            });
        }
    });

    let tether = abacus_agent::tether::TetherState::default();
    let mut tethered = false;
    turn(
        Provider::new(&project.config(address)).unwrap(),
        project.asks("fix the importer"),
        TurnOptions {
            // A session id is what makes the tether run at all.
            session_id: Some("session-under-test".into()),
            tether: tether.clone(),
            ..project.options()
        },
        |event| tethered |= matches!(event, AgentEvent::Notice(text) if text.starts_with("tethered")),
    )
    .await;
    server.await.unwrap();

    let intent_at = intent_arrived.lock().unwrap().expect("an intent call");
    let answer_at = answer_finished.lock().unwrap().expect("an answered turn");
    assert!(
        intent_at < answer_at,
        "the intent call must overlap the answer, not follow it \
         (intent at {intent_at}ms, answer finished at {answer_at}ms)"
    );
    assert_eq!(tether.intent().as_deref(), Some("Ship the importer fix."));
    assert!(tethered, "the user is told what the session is tethered to");
}

/// Refinement is two calls, not one: a cheap gate decides whether the turn
/// taught anything before the planning call runs. A "no" must cost exactly one
/// call — the old rethink pass planned unconditionally, and most long turns
/// have nothing worth keeping.
#[tokio::test]
async fn a_refusing_review_gate_skips_the_planning_call() {
    let project = project(&[]);
    let mock = Mock::answering(|request| match asked(request) {
        Asked::ReviewGate => Reply::Send(says(r#"{"should_refine": false, "rationale": "routine work"}"#)),
        Asked::RefinePlan => Reply::Send(says(r#"{"summary":"s","rationale":"r","expected_outcome":"e","edits":[]}"#)),
        Asked::Summary => Reply::Send(says("1. Primary Request and Intent: do the thing. 10. Next Step: continue.")),
        Asked::Turn => Reply::Last(says("all done")),
    })
    .await;

    let harness = abacus_agent::harness::HarnessStore::default();
    turn(
        project.provider(&mock),
        // Over the rolling-summary threshold, which is what runs the reflection.
        oversized(&project),
        TurnOptions { harness: harness.clone(), ..project.options() },
        |_| {},
    )
    .await;
    let requests = mock.finish().await;

    let count = |kind| requests.iter().filter(|request| asked(request) == kind).count();
    assert_eq!(count(Asked::ReviewGate), 1, "the gate is consulted once");
    assert_eq!(count(Asked::RefinePlan), 0, "a refused gate must not pay for the planning call");
    assert!(harness.snapshot().is_empty(), "nothing is written when the gate refuses");
}

/// The Grok example is a copy-paste starting point, so it is worth proving it
/// end to end rather than just parsing it: load the shipped file, point it at
/// a mock, and check what actually goes out on the wire.
#[tokio::test]
async fn shipped_grok_example_sends_a_bearer_key_to_an_openai_shaped_endpoint() {
    let project = project(&[]);
    let mock = Mock::script([says("hello from grok")]).await;

    // The shipped example, verbatim except for the host and a key source the
    // test can control — everything else (protocol, model, auth shape) is the
    // file as users copy it.
    let shipped = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/endpoints/grok.example.yaml"),
    )
    .unwrap();
    let home = project.directory.path();
    let key_file = home.join("xai-key");
    std::fs::write(&key_file, "xai-test-key-123\n").unwrap();
    let adapted = shipped
        .replace("https://api.x.ai/v1/chat/completions", &format!("http://{}/v1/chat/completions", mock.address))
        .replace("  env: XAI_API_KEY", &format!("  file: {}", key_file.display()));
    let endpoints = home.join("endpoints");
    std::fs::create_dir(&endpoints).unwrap();
    std::fs::write(endpoints.join("grok.yaml"), adapted).unwrap();

    let mut config = project.config(mock.address);
    let endpoint = abacus_agent::endpoint::ScriptedEndpoint::resolve("grok", &endpoints).unwrap();
    // What `Config::resolve` does when the profile leaves its model blank: the
    // endpoint supplies it. Taken from the file so a bad slug fails here.
    config.model = endpoint.model.clone().expect("the example declares a model");
    config.endpoint = Some(endpoint);

    let opening = vec![json!({"role":"user","content":"hi"})];
    turn(Provider::new(&config).unwrap(), opening, project.options(), |_| {}).await;
    let request = mock.finish().await.remove(0);

    let (headers, body) = request.split_once("\r\n\r\n").expect("a request body");
    let lowered = headers.to_ascii_lowercase();
    assert!(headers.contains("POST /v1/chat/completions"), "the url is used verbatim: {headers}");
    assert!(lowered.contains("authorization: bearer xai-test-key-123"), "the key goes out as a bearer: {headers}");
    // OpenAI-shaped, not Anthropic: messages at the top level, no anthropic-version.
    let body: serde_json::Value = serde_json::from_str(body).expect("json body");
    assert_eq!(body["model"], "grok-4.5", "the model comes from the endpoint");
    assert!(body["messages"].is_array(), "chat-completions shape: {body}");
    assert!(!lowered.contains("anthropic-version"), "no anthropic headers on an OpenAI endpoint: {headers}");
}

fn grep_command() -> String {
    calls(&[("c1", "run_command", json!({"command": "grep -rn needle ."}))])
}

/// PLAN used to send every shell command to a classifier that was told to
/// refuse when unsure, so `grep` cost a round trip and `python -c` was refused
/// for what python can do rather than what the command does. Inspection now
/// runs directly: the mock serves the turn only, and a third request would
/// mean a classifier call happened.
#[tokio::test]
async fn plan_mode_runs_inspection_without_a_classifier_call() {
    let project = project(&[("notes.txt", "the needle is here\n")]);
    let mock = Mock::script([grep_command(), says("found it")]).await;

    let mut output = String::new();
    turn(
        project.provider(&mock),
        project.asks("where is the needle?"),
        TurnOptions {
            mode: AgentMode::Plan,
            // Approval is deliberately NOT pre-granted: a command judged to
            // change nothing should run in PLAN without a prompt, and there is
            // nothing here that could answer one.
            allow_mutations: locked(),
            ..project.options()
        },
        |event| {
            if let AgentEvent::ToolFinished { output: text, .. } = event {
                output.push_str(&text);
            }
        },
    )
    .await;
    let requests = mock.finish().await;

    assert!(!output.contains("Blocked by PLAN MODE"), "grep only inspects: {output}");
    assert!(!output.contains("User rejected"), "inspection must not wait on an approval nobody can give: {output}");
    assert!(output.contains("needle"), "the command actually ran: {output}");
    assert_eq!(requests.len(), 2, "two turn requests and no classifier call in between");
}

/// Reading outside the workspace used to be impossible, so models detoured
/// through an interpreter to reach a sibling checkout — extra latency to end
/// up in the same place. It is now allowed under the safety layer, while
/// credentials stay refused whatever anyone thinks.
#[tokio::test]
async fn an_outside_read_is_cleared_but_a_credential_is_not() {
    let project = project(&[]);
    // A sibling checkout, and a private key, both outside the workspace.
    let outside = |directory: &str, name: &str, content: &str| {
        let directory = project.directory.path().join(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(name), content).unwrap();
        directory.join(name).canonicalize().unwrap()
    };
    let sibling = outside("other/src", "lib.rs", "pub fn shared() {}\n");
    let key = outside(".ssh", "id_ed25519", "PRIVATE KEY MATERIAL\n");
    let mock = Mock::script([
        calls(&[("c1", "read_file", json!({"path": sibling}))]),
        calls(&[("c2", "read_file", json!({"path": key}))]),
        says("done"),
    ])
    .await;

    let mut outputs = Vec::new();
    turn(
        project.provider(&mock),
        project.asks("look at the sibling project"),
        TurnOptions { mode: AgentMode::Plan, ..project.options() },
        |event| {
            if let AgentEvent::ToolFinished { output, .. } = event {
                outputs.push(output);
            }
        },
    )
    .await;
    mock.finish().await;

    assert_eq!(outputs.len(), 2, "both reads were attempted: {outputs:?}");
    assert!(outputs[0].contains("pub fn shared()"), "the sibling checkout is readable: {}", outputs[0]);
    assert!(!outputs[1].contains("PRIVATE KEY MATERIAL"), "the key must never be returned: {}", outputs[1]);
    assert!(outputs[1].contains("private data"), "and it says why: {}", outputs[1]);
}

/// The inspection skip is scoped to PLAN. BUILD can mutate, so its approval
/// prompt is the user's control over what happens to their machine — a `grep`
/// there must still ask, or the skip has quietly disarmed the whole gate.
#[tokio::test]
async fn build_mode_still_asks_before_running_a_command() {
    let project = project(&[("notes.txt", "the needle is here\n")]);
    let mock = Mock::script([grep_command(), says("done")]).await;

    let asks = AtomicUsize::new(0);
    let mut output = String::new();
    turn(
        project.provider(&mock),
        project.asks("find the needle"),
        TurnOptions { allow_mutations: locked(), ..project.options() },
        |event| match event {
            AgentEvent::Approval(_) => drop(asks.fetch_add(1, Ordering::Relaxed)),
            AgentEvent::ToolFinished { output: text, .. } => output.push_str(&text),
            _ => {}
        },
    )
    .await;
    mock.finish().await;

    assert!(asks.into_inner() > 0, "BUILD must still request approval for a shell command");
    assert!(!output.contains("the needle is here"), "and must not run it unapproved: {output}");
}

/// A tool result too large to be worth reading whole is replaced by a handle
/// the model can interrogate — checked through the real loop rather than
/// against the store in isolation.
#[tokio::test]
async fn an_oversized_tool_result_is_bound_instead_of_flooding_the_context() {
    // read_file returns at most 400 lines by default, so the lines have to be
    // long enough that a bounded read still clears the bind threshold.
    let huge: String = (0..4_000).map(|index| format!("line {index}: {}\n", "filler ".repeat(40))).collect();
    let project = project(&[("big.log", &huge)]);
    let mock = Mock::script([calls(&[("t1", "read_file", json!({"path": "big.log"}))]), says("done")]).await;

    let handles = abacus_agent::handles::HandleStore::default();
    let (completed, _) = turn(
        project.provider(&mock),
        project.asks("read big.log"),
        TurnOptions { handles: handles.clone(), ..project.options() },
        |_| {},
    )
    .await;
    mock.finish().await;

    let tool_result = completed
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("a tool result was recorded");
    assert!(tool_result.contains("[bound to $h1"), "{tool_result}");
    assert!(tool_result.contains("read_file"), "the source is named");
    // The payload itself never entered the conversation.
    assert!(!tool_result.contains("filler filler"), "{tool_result}");
    assert!(tool_result.len() < 1_000, "the stand-in is small: {}", tool_result.len());
    // And it is still reachable in full.
    let bound = handles.get("h1").expect("content is retained");
    assert!(bound.content.contains("filler filler"));
    assert!(bound.chars() > 20_000, "the full payload is kept");
}
