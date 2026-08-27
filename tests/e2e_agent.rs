use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use abacus_agent::{
    agent::{
        AgentEvent, AgentMode, ApprovalDecision, DoneReason, TurnOptions, initial_messages,
        run_turn,
    },
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
        mode: None,
        trace_enabled: false,
        routing: Default::default(),
        web_search: abacus_agent::web::WebConfig::default(),
        endpoint: None,
        aux_model: None,
        reasoning_effort: None,
        token_compression: false,
        one_stream: false,
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
            trace: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            token_compression: false,
            allow_subagents: true,
            papercuts: abacus_agent::papercuts::PapercutStore::default(),
            memories: abacus_agent::memories::MemoryStore::default(),
            tether: abacus_agent::tether::TetherState::default(),
            hive: abacus_agent::hive::HiveHandle::default(),
            aux_model: None,
            injections: abacus_agent::agent::InjectionQueue::default(),
            modes: abacus_agent::modes::ModeCoach::default(),
            safety: abacus_agent::safety::SafetyCache::default(),
            safety_uses_main: false,
            web_search: abacus_agent::web::WebConfig::default(),
        },
        events,
    ));

    let mut searched = false;
    let mut completed = None;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ToolStarted { name, .. } if name == "grep" => searched = true,
            AgentEvent::Done { messages, .. } => {
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
async fn a_cancelled_turn_keeps_the_work_it_already_did() {
    // The bug this pins: interrupting used to abort the agent task, and since
    // `messages` lived inside that task, every tool result from the turn was
    // discarded. The edits stayed on disk while the model lost all memory of
    // making them.
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("main.rs"), "fn main() { /* needle */ }\n").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let search = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"grep\",\"arguments\":\"{\\\"query\\\":\\\"needle\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut stream).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            search.len(),
            search
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        // A second request is accepted but deliberately never answered: this is
        // the stalled-stream case, where cancellation has to be noticed without
        // a chunk arriving to trigger the check.
        if let Ok((held, _)) = listener.accept().await {
            std::future::pending::<()>().await;
            drop(held);
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
        mode: None,
        trace_enabled: false,
        routing: Default::default(),
        web_search: abacus_agent::web::WebConfig::default(),
        endpoint: None,
        aux_model: None,
        reasoning_effort: None,
        token_compression: false,
        one_stream: false,
        paths: AbacusPaths::under(directory.path().join("home")),
    };
    let provider = Provider::new(&config).unwrap();
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"Find needle"}));
    let cancel = Arc::new(AtomicBool::new(false));
    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            trace: None,
            cancel: cancel.clone(),
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
            token_compression: false,
            allow_subagents: true,
            papercuts: abacus_agent::papercuts::PapercutStore::default(),
            memories: abacus_agent::memories::MemoryStore::default(),
            tether: abacus_agent::tether::TetherState::default(),
            hive: abacus_agent::hive::HiveHandle::default(),
            aux_model: None,
            injections: abacus_agent::agent::InjectionQueue::default(),
            modes: abacus_agent::modes::ModeCoach::default(),
            safety: abacus_agent::safety::SafetyCache::default(),
            safety_uses_main: false,
            web_search: abacus_agent::web::WebConfig::default(),
        },
        events,
    ));

    let mut completed = None;
    let mut reason = None;
    while let Some(event) = receiver.recv().await {
        match event {
            // Cancel the moment the first tool has run, mimicking a user
            // pressing esc partway through.
            AgentEvent::ToolFinished { .. } => cancel.store(true, Ordering::Relaxed),
            AgentEvent::Done {
                messages,
                reason: why,
            } => {
                completed = Some(messages);
                reason = Some(why);
                break;
            }
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.abort();

    assert_eq!(reason, Some(DoneReason::Interrupted));
    let completed = completed.expect("a cancelled turn still reports its history");
    // The assistant's tool call and the tool's result both survive, so the
    // next turn knows the search happened.
    assert!(
        completed
            .iter()
            .any(|message| message["role"] == "assistant" && message["tool_calls"].is_array()),
        "the assistant's tool call should be in history"
    );
    let tool_result = completed
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the tool result should be in history");
    assert_eq!(tool_result["name"], "grep");
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
        mode: None,
        trace_enabled: false,
        routing: Default::default(),
        web_search: abacus_agent::web::WebConfig::default(),
        endpoint: None,
        aux_model: None,
        reasoning_effort: None,
        token_compression: false,
        one_stream: false,
        paths: AbacusPaths::under(directory.path().join("home")),
    };
    let provider = Provider::new(&config).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let completion = provider
        .complete(
            &[json!({"role":"user","content":"hello"})],
            &tool_specs(),
            tx,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .await
        .unwrap();
    assert_eq!(completion.content, "ready");
    assert_eq!(
        rx.try_recv().unwrap(),
        abacus_agent::provider::Chunk::Text("ready".to_owned())
    );
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
        mode: None,
        trace_enabled: false,
        routing: Default::default(),
        web_search: abacus_agent::web::WebConfig::default(),
        endpoint: None,
        aux_model: None,
        reasoning_effort: None,
        token_compression: false,
        one_stream: false,
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
            trace: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            token_compression: false,
            allow_subagents: true,
            papercuts: abacus_agent::papercuts::PapercutStore::default(),
            memories: abacus_agent::memories::MemoryStore::default(),
            tether: abacus_agent::tether::TetherState::default(),
            hive: abacus_agent::hive::HiveHandle::default(),
            aux_model: None,
            injections: abacus_agent::agent::InjectionQueue::default(),
            modes: abacus_agent::modes::ModeCoach::default(),
            safety: abacus_agent::safety::SafetyCache::default(),
            safety_uses_main: false,
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
            trace: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            token_compression: false,
            allow_subagents: true,
            papercuts: abacus_agent::papercuts::PapercutStore::default(),
            memories: abacus_agent::memories::MemoryStore::default(),
            tether: abacus_agent::tether::TetherState::default(),
            hive: abacus_agent::hive::HiveHandle::default(),
            aux_model: None,
            injections: abacus_agent::agent::InjectionQueue::default(),
            modes: abacus_agent::modes::ModeCoach::default(),
            safety: abacus_agent::safety::SafetyCache::default(),
            safety_uses_main: false,
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
            trace: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            token_compression: false,
            allow_subagents: true,
            papercuts: abacus_agent::papercuts::PapercutStore::default(),
            memories: abacus_agent::memories::MemoryStore::default(),
            tether: abacus_agent::tether::TetherState::default(),
            hive: abacus_agent::hive::HiveHandle::default(),
            aux_model: None,
            injections: abacus_agent::agent::InjectionQueue::default(),
            modes: abacus_agent::modes::ModeCoach::default(),
            safety: abacus_agent::safety::SafetyCache::default(),
            safety_uses_main: false,
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
            trace: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            token_compression: false,
            allow_subagents: true,
            papercuts: abacus_agent::papercuts::PapercutStore::default(),
            memories: abacus_agent::memories::MemoryStore::default(),
            tether: abacus_agent::tether::TetherState::default(),
            hive: abacus_agent::hive::HiveHandle::default(),
            aux_model: None,
            injections: abacus_agent::agent::InjectionQueue::default(),
            modes: abacus_agent::modes::ModeCoach::default(),
            safety: abacus_agent::safety::SafetyCache::default(),
            safety_uses_main: false,
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
            // The reflection pass runs before summary compaction; answer it
            // with NOTHING so no side effects fire and the flow continues.
            let is_rethink = request.contains("REFLECTION PASS");
            let payload = if is_summary {
                saw_summary_server.store(true, Ordering::Relaxed);
                let summary_text = "1. Primary Request and Intent: do the thing. \
9. Required Files:\n- src/main.rs\n10. Next Step: continue.";
                let chunk =
                    serde_json::to_string(&json!({"choices":[{"delta":{"content":summary_text}}]}))
                        .unwrap();
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else if is_rethink {
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"NOTHING\"}}]}\n\n",
                    "data: [DONE]\n\n"
                )
                .to_owned()
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
            if !is_summary && !is_rethink {
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
            trace: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            token_compression: false,
            allow_subagents: true,
            papercuts: abacus_agent::papercuts::PapercutStore::default(),
            memories: abacus_agent::memories::MemoryStore::default(),
            tether: abacus_agent::tether::TetherState::default(),
            hive: abacus_agent::hive::HiveHandle::default(),
            aux_model: None,
            injections: abacus_agent::agent::InjectionQueue::default(),
            modes: abacus_agent::modes::ModeCoach::default(),
            safety: abacus_agent::safety::SafetyCache::default(),
            safety_uses_main: false,
            web_search: abacus_agent::web::WebConfig::default(),
        },
        events,
    ));

    let mut completed = None;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Done { messages, .. } => {
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

/// The tether's intent snapshot used to run *after* the answer, so every first
/// turn ended with a two-second stall. It now runs beside the turn: this proves
/// the intent request reaches the server while the main stream is still open,
/// and that the snapshot still lands on the tether by the time the turn is done.
#[tokio::test]
async fn intent_snapshot_runs_beside_the_turn_not_after_it() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Timestamps, in milliseconds since the server started, of the moment the
    // intent request arrived and the moment the main answer finished streaming.
    let intent_arrived: Arc<std::sync::Mutex<Option<u128>>> = Arc::default();
    let answer_finished: Arc<std::sync::Mutex<Option<u128>>> = Arc::default();
    let (intent_probe, answer_probe) = (intent_arrived.clone(), answer_finished.clone());

    let server = tokio::spawn(async move {
        let start = std::time::Instant::now();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (intent_probe, answer_probe) = (intent_probe.clone(), answer_probe.clone());
            tokio::spawn(async move {
                let request = String::from_utf8(read_request(&mut stream).await).unwrap();
                let is_intent = request.contains("INTENT");
                let body = if is_intent {
                    *intent_probe.lock().unwrap() = Some(start.elapsed().as_millis());
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Ship the importer fix.\"}}]}\n\n",
                        "data: [DONE]\n\n"
                    )
                } else {
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"all done\"}}]}\n\n",
                        "data: [DONE]\n\n"
                    )
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                // The main answer is served slowly, so an intent call that only
                // started afterwards could not possibly arrive before it ends.
                if !is_intent {
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                }
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
                if !is_intent {
                    *answer_probe.lock().unwrap() = Some(start.elapsed().as_millis());
                }
            });
        }
    });

    let config = test_config(&directory, &workspace, address);
    let provider = Provider::new(&config).unwrap();
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"fix the importer"}));
    let tether = abacus_agent::tether::TetherState::default();

    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            // A session id is what makes the tether run at all.
            session_id: Some("session-under-test".into()),
            tether: tether.clone(),
            ..base_options(&workspace)
        },
        events,
    ));

    let mut tethered = None;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Notice(text) if text.starts_with("tethered") => tethered = Some(text),
            AgentEvent::Done { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();

    let intent_at = intent_arrived.lock().unwrap().expect("an intent call");
    let answer_at = answer_finished.lock().unwrap().expect("an answered turn");
    assert!(
        intent_at < answer_at,
        "the intent call must overlap the answer, not follow it \
         (intent at {intent_at}ms, answer finished at {answer_at}ms)"
    );
    assert_eq!(tether.intent().as_deref(), Some("Ship the importer fix."));
    assert!(
        tethered.is_some(),
        "the user is told what the session is tethered to"
    );
}

/// The reflection pass used to spend a second full-conversation call just to
/// restate a summary it had already given. When every record in the batch is
/// accepted and the model already said what it recorded, one call is enough.
#[tokio::test]
async fn reflection_stops_after_one_call_when_every_record_lands() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let reflections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = reflections.clone();
    let server = tokio::spawn(async move {
        for _ in 0..5 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = String::from_utf8(read_request(&mut stream).await).unwrap();
            let is_reflection = request.contains("REFLECTION PASS");
            let is_summary = request.contains("context-aware state summary");
            let payload = if is_reflection {
                counter.fetch_add(1, Ordering::Relaxed);
                // A record *and* a summary in the same completion: nothing is
                // left for a second pass to discover.
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Recorded the importer quirk.\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"m1\",\"function\":{\"name\":\"memory_record\",\"arguments\":\"{\\\"title\\\":\\\"importer quirk\\\",\\\"body\\\":\\\"null keys are dropped\\\"}\"}}]}}]}\n\n",
                    "data: [DONE]\n\n"
                ).to_owned()
            } else if is_summary {
                let chunk = serde_json::to_string(
                    &json!({"choices":[{"delta":{"content":"1. Primary Request and Intent: do the thing. 10. Next Step: continue."}}]}),
                )
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
            if !is_reflection && !is_summary {
                break;
            }
        }
    });

    let config = test_config(&directory, &workspace, address);
    let provider = Provider::new(&config).unwrap();
    // Over the rolling-summary threshold, which is what runs the reflection.
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"please do the thing"}));
    messages.push(json!({"role":"assistant","content":format!("BIGBLOB{}", "x".repeat(420_000))}));
    messages.push(json!({"role":"user","content":"continue"}));

    let memories = abacus_agent::memories::MemoryStore::default();
    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            memories: memories.clone(),
            ..base_options(&workspace)
        },
        events,
    ));
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Done { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();

    assert_eq!(
        reflections.load(Ordering::Relaxed),
        1,
        "one reflection call, not a second one to repeat the summary"
    );
    // The saving must not cost the recording itself.
    let recorded = memories.snapshot();
    assert!(
        recorded
            .iter()
            .any(|memory| memory.title == "importer quirk"),
        "the memory was still recorded: {recorded:?}"
    );
}

/// The Grok example is a copy-paste starting point, so it is worth proving it
/// end to end rather than just parsing it: load the shipped file, point it at
/// a mock, and check what actually goes out on the wire.
#[tokio::test]
async fn shipped_grok_example_sends_a_bearer_key_to_an_openai_shaped_endpoint() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured: Arc<std::sync::Mutex<String>> = Arc::default();
    let probe = captured.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        *probe.lock().unwrap() = String::from_utf8(read_request(&mut stream).await).unwrap();
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello from grok\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    });

    // The shipped example, verbatim except for the host and a key source the
    // test can control — everything else (protocol, model, auth shape) is the
    // file as users copy it.
    let shipped = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/endpoints/grok.example.yaml"),
    )
    .unwrap();
    let key_file = directory.path().join("xai-key");
    std::fs::write(&key_file, "xai-test-key-123\n").unwrap();
    let adapted = shipped
        .replace(
            "https://api.x.ai/v1/chat/completions",
            &format!("http://{address}/v1/chat/completions"),
        )
        .replace(
            "  env: XAI_API_KEY",
            &format!("  file: {}", key_file.display()),
        );
    let endpoints = directory.path().join("endpoints");
    std::fs::create_dir(&endpoints).unwrap();
    std::fs::write(endpoints.join("grok.yaml"), adapted).unwrap();

    let mut config = test_config(&directory, &workspace, address);
    let endpoint = abacus_agent::endpoint::ScriptedEndpoint::resolve("grok", &endpoints).unwrap();
    // What `Config::resolve` does when the profile leaves its model blank: the
    // endpoint supplies it. Taken from the file so a bad slug fails here.
    config.model = endpoint
        .model
        .clone()
        .expect("the example declares a model");
    config.endpoint = Some(endpoint);
    let provider = Provider::new(&config).unwrap();

    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        vec![json!({"role":"user","content":"hi"})],
        base_options(&workspace),
        events,
    ));
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Done { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();

    let request = captured.lock().unwrap().clone();
    let (headers, body) = request.split_once("\r\n\r\n").expect("a request body");
    assert!(
        headers.contains("POST /v1/chat/completions"),
        "the url is used verbatim: {headers}"
    );
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer xai-test-key-123"),
        "the key goes out as a bearer: {headers}"
    );
    // OpenAI-shaped, not Anthropic: messages at the top level, no anthropic-version.
    let body: serde_json::Value = serde_json::from_str(body).expect("json body");
    assert_eq!(
        body["model"], "grok-4.5",
        "the model comes from the endpoint"
    );
    assert!(
        body["messages"].is_array(),
        "chat-completions shape: {body}"
    );
    assert!(
        !headers.to_ascii_lowercase().contains("anthropic-version"),
        "no anthropic headers on an OpenAI endpoint: {headers}"
    );
}

/// A snapshot taken from an opening "hi" used to be the yardstick forever: the
/// only refresh ran before rolling-summary compaction, which a big-context
/// model never reaches. Every turn now refreshes it, carrying the previous
/// snapshot so the call updates rather than starts over.
#[tokio::test]
async fn a_later_turn_refreshes_a_stale_intent_snapshot() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let intent_request: Arc<std::sync::Mutex<String>> = Arc::default();
    let probe = intent_request.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let probe = probe.clone();
            tokio::spawn(async move {
                let request = String::from_utf8(read_request(&mut stream).await).unwrap();
                let is_intent = request.contains("You summarise coding-agent sessions");
                let body = if is_intent {
                    *probe.lock().unwrap() = request;
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"Build the SaaS backend: auth, plans, billing.\"}}]}\n\n",
                        "data: [DONE]\n\n"
                    )
                } else {
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"on it\"}}]}\n\n",
                        "data: [DONE]\n\n"
                    )
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            });
        }
    });

    let config = test_config(&directory, &workspace, address);
    let provider = Provider::new(&config).unwrap();
    // The session already carries the thin snapshot from its opening turn.
    let stale = "The user greeted the agent and pointed it at empero.org as a theme guide.";
    let tether = abacus_agent::tether::TetherState::new(Some(stale.to_owned()));

    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"hi! use empero.org as the theme guide"}));
    messages.push(json!({"role":"assistant","content":"Sure — what shall we build?"}));
    messages
        .push(json!({"role":"user","content":"build the full SaaS backend: auth, plans, billing"}));

    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            session_id: Some("session-under-test".into()),
            tether: tether.clone(),
            ..base_options(&workspace)
        },
        events,
    ));
    let mut notices = Vec::new();
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Notice(text) => notices.push(text),
            AgentEvent::Done { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();

    let request = intent_request.lock().unwrap().clone();
    assert!(
        !request.is_empty(),
        "a later turn still snapshots the intent"
    );
    assert!(
        request.contains("Previous intent snapshot"),
        "the refresh updates rather than starting over: {request}"
    );
    assert!(
        request.contains("build the full SaaS backend"),
        "and it sees what the user has since asked for"
    );
    assert_eq!(
        tether.intent().as_deref(),
        Some("Build the SaaS backend: auth, plans, billing."),
        "the stale snapshot is replaced"
    );
    assert!(
        !notices.iter().any(|notice| notice.starts_with("tethered")),
        "a refresh is silent — only the first snapshot is announced: {notices:?}"
    );
}

/// PLAN used to send every shell command to a classifier that was told to
/// refuse when unsure, so `grep` cost a round trip and `python -c` was refused
/// for what python can do rather than what the command does. Models papercut
/// the mode as unusable. Inspection now runs directly: the mock serves the
/// turn only, and a second connection would mean a classifier call happened.
#[tokio::test]
async fn plan_mode_runs_inspection_without_a_classifier_call() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("notes.txt"), "the needle is here\n").unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = connections.clone();
    let server = tokio::spawn(async move {
        let first = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"run_command\",\"arguments\":\"{\\\"command\\\":\\\"grep -rn needle .\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let second = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"found it\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        for body in [first, second] {
            let (mut stream, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, Ordering::Relaxed);
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

    let config = test_config(&directory, &workspace, address);
    let provider = Provider::new(&config).unwrap();
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"where is the needle?"}));

    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            mode: AgentMode::Plan,
            // Approval is deliberately NOT pre-granted: a command judged to
            // change nothing should run in PLAN without a prompt, and there is
            // nothing here that could answer one.
            allow_mutations: Arc::new(AtomicBool::new(false)),
            ..base_options(&workspace)
        },
        events,
    ));

    let mut blocked = false;
    let mut output = String::new();
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ToolFinished { output: text, .. } => {
                if text.contains("Blocked by PLAN MODE") {
                    blocked = true;
                }
                output.push_str(&text);
            }
            AgentEvent::Done { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();

    assert!(!blocked, "grep only inspects: {output}");
    assert!(
        !output.contains("User rejected"),
        "inspection must not wait on an approval nobody can give: {output}"
    );
    assert!(
        output.contains("needle"),
        "the command actually ran: {output}"
    );
    assert_eq!(
        connections.load(Ordering::Relaxed),
        2,
        "two turn requests and no classifier call in between"
    );
}

/// Reading outside the workspace used to be impossible, so models detoured
/// through an interpreter to reach a sibling checkout — extra latency to end
/// up in the same place. It is now allowed under the safety layer, while
/// credentials stay refused whatever anyone thinks.
#[tokio::test]
async fn an_outside_read_is_cleared_but_a_credential_is_not() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    // A sibling checkout, and a private key, both outside the workspace.
    let sibling = directory.path().join("other/src");
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("lib.rs"), "pub fn shared() {}\n").unwrap();
    let keys = directory.path().join(".ssh");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(keys.join("id_ed25519"), "PRIVATE KEY MATERIAL\n").unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let sibling_file = sibling.join("lib.rs").canonicalize().unwrap();
    let key_file = keys.join("id_ed25519").canonicalize().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let calls = [
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\"function\":{{\"name\":\"read_file\",\"arguments\":\"{{\\\"path\\\":\\\"{}\\\"}}\"}}}}]}}}}]}}\n\ndata: [DONE]\n\n",
                sibling_file.display()
            ),
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c2\",\"function\":{{\"name\":\"read_file\",\"arguments\":\"{{\\\"path\\\":\\\"{}\\\"}}\"}}}}]}}}}]}}\n\ndata: [DONE]\n\n",
                key_file.display()
            ),
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n"
                .to_owned(),
        ];
        for body in calls {
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

    let config = test_config(&directory, &workspace, address);
    let provider = Provider::new(&config).unwrap();
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"look at the sibling project"}));

    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            mode: AgentMode::Plan,
            ..base_options(&workspace)
        },
        events,
    ));

    let mut outputs = Vec::new();
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::ToolFinished { output, .. } => outputs.push(output),
            AgentEvent::Done { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();

    assert_eq!(outputs.len(), 2, "both reads were attempted: {outputs:?}");
    assert!(
        outputs[0].contains("pub fn shared()"),
        "the sibling checkout is readable: {}",
        outputs[0]
    );
    assert!(
        !outputs[1].contains("PRIVATE KEY MATERIAL"),
        "the key must never be returned: {}",
        outputs[1]
    );
    assert!(
        outputs[1].contains("private data"),
        "and it says why: {}",
        outputs[1]
    );
}

/// The inspection skip is scoped to PLAN. BUILD can mutate, so its approval
/// prompt is the user's control over what happens to their machine — a `grep`
/// there must still ask, or the skip has quietly disarmed the whole gate.
#[tokio::test]
async fn build_mode_still_asks_before_running_a_command() {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("notes.txt"), "the needle is here\n").unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let first = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"run_command\",\"arguments\":\"{\\\"command\\\":\\\"grep -rn needle .\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let second = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
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

    let config = test_config(&directory, &workspace, address);
    let provider = Provider::new(&config).unwrap();
    let mut messages = initial_messages(&workspace);
    messages.push(json!({"role":"user","content":"find the needle"}));

    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(
        provider,
        messages,
        TurnOptions {
            mode: AgentMode::Build,
            allow_mutations: Arc::new(AtomicBool::new(false)),
            ..base_options(&workspace)
        },
        events,
    ));

    let mut asked = false;
    let mut output = String::new();
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Approval(_) => asked = true,
            AgentEvent::ToolFinished { output: text, .. } => output.push_str(&text),
            AgentEvent::Done { .. } => break,
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    agent.await.unwrap();
    server.await.unwrap();

    assert!(
        asked,
        "BUILD must still request approval for a shell command"
    );
    assert!(
        !output.contains("the needle is here"),
        "and must not run it unapproved: {output}"
    );
}

/// Defaults for tests that only care about one or two options.
fn base_options(workspace: &std::path::Path) -> TurnOptions {
    TurnOptions {
        trace: None,
        cancel: Arc::new(AtomicBool::new(false)),
        workspace: workspace.to_owned(),
        max_steps: 4,
        tool_output_limit: 30_000,
        mode: AgentMode::Build,
        allow_mutations: Arc::new(AtomicBool::new(true)),
        services: Arc::new(AgentServices::empty(workspace.to_owned())),
        session_id: None,
        goal: GoalState::default(),
        tasks: TaskList::default(),
        compaction: CompactionState::default(),
        compaction_budget: CompactionBudget::default(),
        token_compression: false,
        allow_subagents: false,
        papercuts: abacus_agent::papercuts::PapercutStore::default(),
        memories: abacus_agent::memories::MemoryStore::default(),
        tether: abacus_agent::tether::TetherState::default(),
        hive: abacus_agent::hive::HiveHandle::default(),
        aux_model: None,
        injections: abacus_agent::agent::InjectionQueue::default(),
        modes: abacus_agent::modes::ModeCoach::default(),
        safety: abacus_agent::safety::SafetyCache::default(),
        safety_uses_main: false,
        web_search: abacus_agent::web::WebConfig::default(),
    }
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
        mode: None,
        trace_enabled: false,
        routing: Default::default(),
        web_search: abacus_agent::web::WebConfig::default(),
        endpoint: None,
        aux_model: None,
        reasoning_effort: None,
        token_compression: false,
        one_stream: false,
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
