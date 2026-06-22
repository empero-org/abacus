use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use abacus_agent::{
    agent::{AgentEvent, AgentMode, ApprovalDecision, TurnOptions, initial_messages, run_turn},
    compaction::CompactionState,
    config::{AbacusPaths, Config, ProviderProtocol},
    goal::GoalState,
    model_info::{CompactionBudget, ModelLimits},
    provider::Provider,
    services::AgentServices,
    task::TaskList,
    tools::tool_specs,
};
use serde_json::json;
use tempfile::tempdir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

#[tokio::test]
async fn streamed_agent_searches_workspace_and_finishes() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("main.rs"), "fn main() { /* needle */ }\n").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let first = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"grep\",\"arguments\":\"{\\\"query\\\":\\\"needle\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let second = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Found the reference in main.rs.\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        for body in [first, second] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });

    let workspace = workspace.canonicalize().unwrap();
    let config = Config {
        workspace: workspace.clone(),
        profile: "test".into(),
        model: "test-model".into(),
        base_url: format!("http://{address}/v1"),
        protocol: ProviderProtocol::ChatCompletions,
        api_key: None,
        max_steps: 4,
        tool_output_limit: 30_000,
        yes: true,
        no_session: true,
        model_limits: ModelLimits::default(),
        tool_format: abacus_agent::tool_format::ToolFormat::default(),
        web_search: abacus_agent::web::WebConfig::default(),
        paths: AbacusPaths::under(directory.path().join("home")),
    };
    let provider = Provider::new(&config).unwrap();
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"Find needle"}));
    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            workspace: workspace.clone(),
            max_steps: 4,
            tool_output_limit: 30_000,
            mode: AgentMode::Build,
            allow_mutations: Arc::new(AtomicBool::new(true)),
            services: Arc::new(AgentServices::empty(workspace.clone())),
            session_id: None,
            goal: GoalState::default(),
            tasks: TaskList::default(),
            compaction: CompactionState::default(),
            compaction_budget: CompactionBudget::default(),
            allow_subagents: true,
            web_search: abacus_agent::web::WebConfig::default(),
        },
        events,
    ));

    let mut searched = false;
    let mut completed = None;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ToolStarted { name, .. } if name == "grep" => searched = true,
            AgentEvent::Done { messages } => {
                completed = Some(messages);
                break;
            }
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();

    let completed = completed.expect("agent should complete");
    assert!(searched);
    assert!(completed.iter().any(|message| {
        message["role"] == "tool"
            && message["content"]
                .as_str()
                .is_some_and(|content| content.contains("main.rs:1"))
    }));
    assert_eq!(
        completed.last().unwrap()["content"],
        "Found the reference in main.rs."
    );
}

#[tokio::test]
async fn responses_protocol_uses_responses_endpoint_and_stream_format() {
    let directory = tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = String::from_utf8(read_request(&mut stream).await).unwrap();
        assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
        assert!(request.contains("\"input\""));
        assert!(request.contains("\"name\":\"grep\""));
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ready\"}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let workspace = directory.path().canonicalize().unwrap();
    let config = Config {
        workspace,
        profile: "test".into(),
        model: "test-model".into(),
        base_url: format!("http://{address}/v1"),
        protocol: ProviderProtocol::Responses,
        api_key: None,
        max_steps: 2,
        tool_output_limit: 30_000,
        yes: true,
        no_session: true,
        model_limits: ModelLimits::default(),
        tool_format: abacus_agent::tool_format::ToolFormat::default(),
        web_search: abacus_agent::web::WebConfig::default(),
        paths: AbacusPaths::under(directory.path().join("home")),
    };
    let provider = Provider::new(&config).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let completion = provider
        .complete(
            &[json!({"role":"user","content":"hello"})],
            &tool_specs(),
            tx,
        )
        .await
        .unwrap();
    assert_eq!(completion.content, "ready");
    assert_eq!(rx.try_recv().unwrap(), "ready");
    server.await.unwrap();
}

#[tokio::test]
async fn edit_requires_reviewable_approval_before_atomic_write() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("value.txt"), "old\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let edit = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"edit_1\",\"function\":{\"name\":\"edit_file\",\"arguments\":\"{\\\"path\\\":\\\"value.txt\\\",\\\"old_text\\\":\\\"old\\\\n\\\",\\\"new_text\\\":\\\"new\\\\n\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let done = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Updated value.txt.\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        for body in [edit, done] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let workspace = workspace.canonicalize().unwrap();
    let config = Config {
        workspace: workspace.clone(),
        profile: "test".into(),
        model: "test-model".into(),
        base_url: format!("http://{address}/v1"),
        protocol: ProviderProtocol::ChatCompletions,
        api_key: None,
        max_steps: 4,
        tool_output_limit: 30_000,
        yes: false,
        no_session: true,
        model_limits: ModelLimits::default(),
        tool_format: abacus_agent::tool_format::ToolFormat::default(),
        web_search: abacus_agent::web::WebConfig::default(),
        paths: AbacusPaths::under(directory.path().join("home")),
    };
    let provider = Provider::new(&config).unwrap();
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"update the value"}));
    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            workspace: workspace.clone(),
            max_steps: 4,
            tool_output_limit: 30_000,
            mode: AgentMode::Build,
            allow_mutations: Arc::new(AtomicBool::new(false)),
            services: Arc::new(AgentServices::empty(workspace.clone())),
            session_id: None,
            goal: GoalState::default(),
            tasks: TaskList::default(),
            compaction: CompactionState::default(),
            compaction_budget: CompactionBudget::default(),
            allow_subagents: true,
            web_search: abacus_agent::web::WebConfig::default(),
        },
        events,
    ));
    let mut approved = false;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Approval(request) => {
                assert_eq!(request.tool, "edit_file");
                assert!(request.details.contains("-old"));
                assert!(request.details.contains("+new"));
                request.respond.send(ApprovalDecision::Once).unwrap();
                approved = true;
            }
            AgentEvent::Done { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();
    assert!(approved);
    assert_eq!(
        std::fs::read_to_string(workspace.join("value.txt")).unwrap(),
        "new\n"
    );
}

#[tokio::test]
async fn text_emitted_tool_calls_are_parsed_when_native_calls_absent() {
    // A model served without native function-calling emits a Hermes-format
    // tool call as assistant TEXT (no `tool_calls` field). With `tool_format`
    // set, the provider must parse it and the agent must dispatch the tool.
    use abacus_agent::tool_format::{ToolFormat, render_hermes_call};
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("target.txt"), "hello\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        // Turn 1: assistant text carries a Hermes read_file call, no tool_calls.
        let call = render_hermes_call("read_file", r#"{"path":"target.txt"}"#);
        let turn1 = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content": format!("Reading.\n{}", call)}}]})
        );
        // Turn 2: the model gets the tool result and finishes.
        let turn2 = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":"Done, target.txt contains hello."}}]})
        );
        for body in [turn1, turn2] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let workspace = workspace.canonicalize().unwrap();
    let mut config = test_config(&directory, &workspace, address);
    config.tool_format = ToolFormat::Hermes;
    let provider = Provider::new(&config).unwrap();
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"read target.txt"}));
    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            workspace: workspace.clone(),
            max_steps: 4,
            tool_output_limit: 30_000,
            mode: AgentMode::Auto,
            allow_mutations: Arc::new(AtomicBool::new(false)),
            services: Arc::new(AgentServices::empty(workspace.clone())),
            session_id: None,
            goal: GoalState::default(),
            tasks: TaskList::default(),
            compaction: CompactionState::default(),
            compaction_budget: CompactionBudget::default(),
            allow_subagents: true,
            web_search: abacus_agent::web::WebConfig::default(),
        },
        events,
    ));
    let mut saw_read = false;
    let mut saw_result = false;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ToolStarted { name, summary } => {
                assert_eq!(name, "read_file");
                assert!(summary.contains("target.txt"));
                saw_read = true;
            }
            AgentEvent::ToolFinished { name, output } => {
                assert_eq!(name, "read_file");
                assert!(
                    output.contains("hello"),
                    "tool output should contain file content"
                );
                saw_result = true;
            }
            AgentEvent::Done { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();
    assert!(
        saw_read,
        "text-emitted read_file call must be parsed and dispatched"
    );
    assert!(saw_result, "read_file must return the file contents");
}

#[tokio::test]
async fn auto_mode_blocks_mutation_until_model_selects_build() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("value.txt"), "old\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let edit = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"edit_1\",\"function\":{\"name\":\"edit_file\",\"arguments\":\"{\\\"path\\\":\\\"value.txt\\\",\\\"old_text\\\":\\\"old\\\\n\\\",\\\"new_text\\\":\\\"new\\\\n\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let done = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"I need to select a mode first.\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        for body in [edit, done] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let workspace = workspace.canonicalize().unwrap();
    let config = test_config(&directory, &workspace, address);
    let provider = Provider::new(&config).unwrap();
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"update the value"}));
    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            workspace: workspace.clone(),
            max_steps: 4,
            tool_output_limit: 30_000,
            mode: AgentMode::Auto,
            allow_mutations: Arc::new(AtomicBool::new(true)),
            services: Arc::new(AgentServices::empty(workspace.clone())),
            session_id: None,
            goal: GoalState::default(),
            tasks: TaskList::default(),
            compaction: CompactionState::default(),
            compaction_budget: CompactionBudget::default(),
            allow_subagents: true,
            web_search: abacus_agent::web::WebConfig::default(),
        },
        events,
    ));
    let mut blocked = false;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ToolFinished { name, output } if name == "edit_file" => {
                blocked = output.contains("Blocked by AUTO MODE")
            }
            AgentEvent::Done { .. } => break,
            AgentEvent::Approval(_) => panic!("blocked AUTO mutation requested approval"),
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();
    assert!(blocked);
    assert_eq!(
        std::fs::read_to_string(workspace.join("value.txt")).unwrap(),
        "old\n"
    );
}

#[tokio::test]
async fn auto_mode_selection_enables_later_tool_in_same_completion() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("value.txt"), "old\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let tools = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"mode_1\",\"function\":{\"name\":\"mode_set\",\"arguments\":\"{\\\"mode\\\":\\\"build\\\",\\\"reason\\\":\\\"The user requested implementation\\\"}\"}},{\"index\":1,\"id\":\"edit_1\",\"function\":{\"name\":\"edit_file\",\"arguments\":\"{\\\"path\\\":\\\"value.txt\\\",\\\"old_text\\\":\\\"old\\\\n\\\",\\\"new_text\\\":\\\"new\\\\n\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let done = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Updated value.txt.\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        for body in [tools, done] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let workspace = workspace.canonicalize().unwrap();
    let config = test_config(&directory, &workspace, address);
    let provider = Provider::new(&config).unwrap();
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"update the value"}));
    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            workspace: workspace.clone(),
            max_steps: 4,
            tool_output_limit: 30_000,
            mode: AgentMode::Auto,
            allow_mutations: Arc::new(AtomicBool::new(true)),
            services: Arc::new(AgentServices::empty(workspace.clone())),
            session_id: None,
            goal: GoalState::default(),
            tasks: TaskList::default(),
            compaction: CompactionState::default(),
            compaction_budget: CompactionBudget::default(),
            allow_subagents: true,
            web_search: abacus_agent::web::WebConfig::default(),
        },
        events,
    ));
    let mut selected_build = false;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ModeChanged { mode, .. } => selected_build = mode == AgentMode::Build,
            AgentEvent::Done { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();
    assert!(selected_build);
    assert_eq!(
        std::fs::read_to_string(workspace.join("value.txt")).unwrap(),
        "new\n"
    );
}

#[tokio::test]
async fn rolling_summary_compaction_fires_on_large_context() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let saw_summary = Arc::new(AtomicBool::new(false));
    let saw_summary_server = saw_summary.clone();
    let server = tokio::spawn(async move {
        // Serve up to a few connections. The first should be the compaction
        // summarization call (no tools, contains the summary prompt); the next
        // is the normal turn. Stop once the real turn is served.
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = String::from_utf8(read_request(&mut stream).await).unwrap();
            let is_summary = request.contains("context-aware state summary");
            let payload = if is_summary {
                saw_summary_server.store(true, Ordering::Relaxed);
                let summary_text = "1. Primary Request and Intent: do the thing. \
9. Required Files:\n- src/main.rs\n10. Next Step: continue.";
                let chunk =
                    serde_json::to_string(&json!({"choices":[{"delta":{"content":summary_text}}]}))
                        .unwrap();
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else {
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"all done\"}}]}\n\n",
                    "data: [DONE]\n\n"
                )
                .to_owned()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            if !is_summary {
                break;
            }
        }
    });

    let config = test_config(&directory, &workspace, address);
    let provider = Provider::new(&config).unwrap();
    // Build a conversation over the compaction threshold (400k chars) with a
    // non-compactable large assistant message so microcompaction cannot shrink it
    // away — forcing the rolling-summary path.
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"please do the thing"}));
    let big = format!("BIGBLOB{}", "x".repeat(420_000));
    messages.push(json!({"role":"assistant","content":big}));
    messages.push(json!({"role":"user","content":"continue"}));
    messages.push(json!({"role":"assistant","content":"working on it"}));

    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            workspace: workspace.clone(),
            max_steps: 4,
            tool_output_limit: 30_000,
            mode: AgentMode::Build,
            allow_mutations: Arc::new(AtomicBool::new(true)),
            services: Arc::new(AgentServices::empty(workspace.clone())),
            session_id: None,
            goal: GoalState::default(),
            tasks: TaskList::default(),
            compaction: CompactionState::default(),
            compaction_budget: CompactionBudget::default(),
            allow_subagents: true,
            web_search: abacus_agent::web::WebConfig::default(),
        },
        events,
    ));

    let mut completed = None;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Done { messages } => {
                completed = Some(messages);
                break;
            }
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();

    let completed = completed.expect("agent should complete");
    // The rolling-summary LLM call fired.
    assert!(
        saw_summary.load(Ordering::Relaxed),
        "compaction summarization call was not made"
    );
    // The LLM path was taken (not the drop-only fallback, which would inject an
    // "older conversation messages were omitted" system note).
    assert!(
        !completed.iter().any(|m| m["content"]
            .as_str()
            .is_some_and(|c| c.contains("were omitted"))),
        "fallback drop-only path was used instead of LLM summarization"
    );
    // The compacted middle (the 420k blob) is gone from the live history.
    assert!(
        !completed
            .iter()
            .any(|m| m["content"].as_str().is_some_and(|c| c.contains("BIGBLOB"))),
        "compacted middle was not dropped"
    );
    // The verbatim recent tail survives.
    assert!(
        completed
            .iter()
            .any(|m| m["content"].as_str() == Some("working on it")),
        "recent tail was not preserved"
    );
    assert_eq!(completed.last().unwrap()["content"], "all done");
}

fn test_config(
    directory: &tempfile::TempDir,
    workspace: &std::path::Path,
    address: std::net::SocketAddr,
) -> Config {
    Config {
        workspace: workspace.to_owned(),
        profile: "test".into(),
        model: "test-model".into(),
        base_url: format!("http://{address}/v1"),
        protocol: ProviderProtocol::ChatCompletions,
        api_key: None,
        max_steps: 4,
        tool_output_limit: 30_000,
        yes: true,
        no_session: true,
        model_limits: ModelLimits::default(),
        tool_format: abacus_agent::tool_format::ToolFormat::default(),
        web_search: abacus_agent::web::WebConfig::default(),
        paths: AbacusPaths::under(directory.path().join("home")),
    }
}

async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buffer = vec![0_u8; 1_000_000];
    let mut used = 0;
    let mut expected = None;
    loop {
        let read = stream.read(&mut buffer[used..]).await.unwrap();
        used += read;
        if expected.is_none()
            && let Some(header_end) = buffer[..used]
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
        {
            let headers = String::from_utf8_lossy(&buffer[..header_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            expected = Some(header_end + 4 + length);
        }
        if read == 0 || expected.is_some_and(|expected| used >= expected) {
            break;
        }
    }
    buffer.truncate(used);
    buffer
}
