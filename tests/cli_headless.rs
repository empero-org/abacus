use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
};

use tempfile::tempdir;

#[path = "support/sync_server.rs"]
mod sync_server;

#[test]
fn headless_json_runs_without_prior_setup() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        // The agent sends a best-effort GET /models probe to auto-detect the
        // model's context window before the chat call; answer it with 404 so
        // detection falls back to defaults, then serve the chat completion.
        loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 64 * 1024];
            let read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]);
            if request.starts_with("GET /v1/models") {
                let response = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                stream.write_all(response.as_bytes()).unwrap();
                continue;
            }
            assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
            let body =
                concat!("data: {\"choices\":[{\"delta\":{\"content\":\"headless works\"}}]}\n\n", "data: [DONE]\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            break;
        }
    });

    let directory = tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_abacus"))
        .current_dir(directory.path())
        .env("ABACUS_HOME", directory.path().join("home"))
        .env("ABACUS_NO_ACTIVITY", "1")
        .args([
            "--base-url",
            &format!("http://{address}/v1"),
            "--model",
            "test-model",
            "--protocol",
            "chat-completions",
            "--no-session",
            "--prompt",
            "say hello",
            "--output-format",
            "json",
        ])
        .output()
        .unwrap();
    server.join().unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["text"], "headless works");
    assert!(value["session_id"].is_null());
}

/// `--goal` keeps a headless run going, turn after turn, until the model marks
/// the goal complete.
#[test]
fn headless_goal_runs_until_the_model_completes_it() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        // The first turn only talks, so the goal carries it on; the second
        // marks the goal complete, then answers. A fourth chat request would
        // find nobody listening and fail the run.
        let complete = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"goal_update","arguments":"{\"status\":\"complete\"}"}}]}}]}"#;
        let answers = [
            "data: {\"choices\":[{\"delta\":{\"content\":\"still working \"}}]}\n\ndata: [DONE]\n\n".to_owned(),
            format!(
                "data: {complete}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
            ),
            "data: {\"choices\":[{\"delta\":{\"content\":\"all green\"}}]}\n\ndata: [DONE]\n\n".to_owned(),
        ];
        let mut served = Vec::new();
        while served.len() < answers.len() {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            if request.starts_with("GET /v1/models") {
                let response = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                stream.write_all(response.as_bytes()).unwrap();
                continue;
            }
            let body = &answers[served.len()];
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            served.push(request);
        }
        served
    });

    let directory = tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_abacus"))
        .current_dir(directory.path())
        .env("ABACUS_HOME", directory.path().join("home"))
        .env("ABACUS_NO_ACTIVITY", "1")
        .args([
            "--base-url",
            &format!("http://{address}/v1"),
            "--model",
            "test-model",
            "--protocol",
            "chat-completions",
            "--no-session",
            "--prompt",
            "finish the task",
            "--goal",
            "--output-format",
            "json",
        ])
        .output()
        .unwrap();
    let served = server.join().unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["goal"], "complete");
    assert_eq!(value["text"], "still working all green");
    assert!(served[1].contains("[goal not done]") && served[1].contains("finish the task"), "{}", served[1]);
}

/// What the scripted model reports for every request, as the provider's final
/// stream chunk: 1200 prompt tokens (1000 of them cached) and 34 generated.
const MODEL_USAGE: &str = r#"{"choices":[],"usage":{"prompt_tokens":1200,"completion_tokens":34,"total_tokens":1234,"prompt_tokens_details":{"cached_tokens":1000}}}"#;

/// A model endpoint that answers every chat request with one short reply and
/// the usage above, until the process exits.
fn model_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let request = read_request(&mut stream);
            let response = if request.starts_with("GET /v1/models") {
                "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
            } else {
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"headless works\"}}}}]}}\n\ndata: {MODEL_USAGE}\n\ndata: [DONE]\n\n"
                );
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{address}/v1")
}

/// The request head and its whole body: answering before the client has
/// finished sending would reset the connection under it.
fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut data = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..read]);
        let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") else { continue };
        let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if data.len() >= end + 4 + length {
            break;
        }
    }
    String::from_utf8_lossy(&data).into_owned()
}

/// An Abacus home signed in to `server`, beside an empty workspace.
struct SignedIn {
    directory: tempfile::TempDir,
}

impl SignedIn {
    fn new(server: &sync_server::SyncServer, token: &str) -> Self {
        let directory = tempdir().unwrap();
        let paths = abacus_agent::config::AbacusPaths::under(directory.path().join("home"));
        abacus_agent::config::Credentials {
            sync: Some(abacus_agent::config::SyncCredentials {
                server: server.url(),
                token: token.into(),
                email: "me@example.com".into(),
            }),
            ..Default::default()
        }
        .save(&paths)
        .unwrap();
        std::fs::create_dir(directory.path().join("work")).unwrap();
        Self { directory }
    }

    fn command(&self, model: &str, arguments: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_abacus"));
        command
            .current_dir(self.directory.path().join("work"))
            .env("ABACUS_HOME", self.directory.path().join("home"))
            .env("ABACUS_NO_ACTIVITY", "1")
            .env_remove("ABACUS_NO_USAGE")
            .args(["--base-url", model, "--model", "test-model", "--protocol", "chat-completions"])
            .args(arguments)
            .kill_on_drop(true);
        command
    }

    fn install_id(&self) -> String {
        std::fs::read_to_string(self.directory.path().join("home/install_id")).unwrap().trim().to_owned()
    }
}

#[tokio::test]
async fn headless_reports_its_usage_to_the_signed_in_account() {
    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    let output = device
        .command(&model_endpoint(), &["--prompt", "say hello", "--output-format", "json"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    // One closing report, with the model's own figures and the account's token.
    let reports = sync.seen("POST", "/v1/usage/report");
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].header("authorization"), Some("Bearer secret-token"));
    let body = reports[0].json();
    assert_eq!(body["install_id"], device.install_id());
    assert_eq!(body["client"]["kind"], "headless");
    assert_eq!(body["client"]["app_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["client"]["os"], std::env::consts::OS);
    assert_eq!(body["reports"].as_array().unwrap().len(), 1);
    let report = &body["reports"][0];
    assert_eq!(report["model"], "test-model");
    assert_eq!(report["final"], true);
    assert_eq!(report["seq"], 1);
    let requests = report["input_tokens"].as_u64().unwrap() / 1200;
    assert!(requests >= 1, "{report}");
    assert_eq!(report["input_tokens"], 1200 * requests, "the figures are the model's, once per request");
    assert_eq!(report["output_tokens"], 34 * requests);
    assert_eq!(report["cache_read_tokens"], 1000 * requests);
    assert_eq!(report["cache_write_tokens"], 0);
    assert_eq!(report["total_tokens"], 1234 * requests);
    assert_eq!(report["run_id"].as_str().unwrap().len(), 36);
    assert_eq!(report["session_id"].as_str().unwrap().len(), 36);

    // Counters and identifiers only: nothing from the conversation or the machine's files.
    let text = reports[0].body.iter().map(|byte| *byte as char).collect::<String>();
    for private in ["say hello", "headless works", "work", "ABACUS"] {
        assert!(!text.contains(private), "{private} leaked into {text}");
    }

    // Reporting did not displace the sync it runs beside.
    assert!(!sync.seen("GET", "/v1/sync/changes").is_empty(), "pulled on open");
    assert_eq!(sync.seen("PUT", "/v1/sync/sessions/").len(), 1, "pushed at the end");
}

#[tokio::test]
async fn headless_stays_quiet_when_usage_reporting_is_opted_out() {
    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    let output = device
        .command(&model_endpoint(), &["--prompt", "say hello", "--output-format", "json"])
        .env("ABACUS_NO_USAGE", "1")
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(sync.seen("POST", "/v1/usage/report").is_empty());
    assert_eq!(sync.seen("PUT", "/v1/sync/sessions/").len(), 1, "sync is its own switch");
}

#[tokio::test]
async fn headless_is_unaffected_by_a_server_that_refuses_usage() {
    // A token the server does not know: the run still succeeds and prints its answer.
    let sync = sync_server::SyncServer::start("another-token").await;
    let device = SignedIn::new(&sync, "stale-token");
    let output = device
        .command(&model_endpoint(), &["--prompt", "say hello", "--output-format", "json"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["text"], "headless works");
    assert!(String::from_utf8_lossy(&output.stderr).to_lowercase().find("usage").is_none());
}

#[tokio::test]
async fn app_server_reports_each_finished_turn_and_closes_the_run_on_shutdown() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    let mut child = device
        .command(&model_endpoint(), &["app-server"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();

    let request = |id: u32, method: &str, params: serde_json::Value| {
        format!("{}\n", serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
    };
    input.write_all(request(1, "initialize", serde_json::json!({})).as_bytes()).await.unwrap();
    input.write_all(request(2, "turn/start", serde_json::json!({"text": "say hello"})).as_bytes()).await.unwrap();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains("\"turn/completed\"") {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(completed, Ok(true), "the turn should complete");

    // The turn's report goes out in the background right after it completes.
    for _ in 0..200 {
        if !sync.seen("POST", "/v1/usage/report").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let reports = sync.seen("POST", "/v1/usage/report");
    assert_eq!(reports.len(), 1, "{reports:?}");
    let body = reports[0].json();
    assert_eq!(body["client"]["kind"], "app-server");
    let turn = &body["reports"][0];
    assert_eq!((turn["final"].clone(), turn["seq"].clone()), (serde_json::json!(false), serde_json::json!(1)));
    let requests = turn["input_tokens"].as_u64().unwrap() / 1200;
    assert!(requests >= 1, "{turn}");
    assert_eq!(turn["input_tokens"], 1200 * requests);
    assert_eq!(turn["output_tokens"], 34 * requests);

    input.write_all(request(3, "shutdown", serde_json::json!({})).as_bytes()).await.unwrap();
    // A front end that is going away closes the pipe; the process waits for it.
    drop(input);
    let status = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await.unwrap().unwrap();
    assert!(status.success());

    let reports = sync.seen("POST", "/v1/usage/report");
    assert_eq!(reports.len(), 2, "{reports:?}");
    let closing = &reports[1].json()["reports"][0];
    assert_eq!(closing["final"], true);
    assert_eq!(closing["run_id"], turn["run_id"], "one thread, one run");
    assert!(closing["seq"].as_u64() > turn["seq"].as_u64());
    assert_eq!(closing["session_id"], turn["session_id"]);
    assert!(
        !sync.seen("PUT", &format!("/v1/sync/sessions/{}", turn["session_id"].as_str().unwrap())).is_empty(),
        "the thread is uploaded after its turn"
    );
}

/// `--resume` after another device took the session further: the run must
/// build on the newer copy. Pulling only after the session was read had the
/// run continue the older copy and its upload replace the other device's turn
/// everywhere.
#[tokio::test]
async fn headless_resume_continues_the_newest_copy_and_keeps_another_devices_turn() {
    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    let model = model_endpoint();
    let output = device.command(&model, &["--prompt", "first", "--output-format", "json"]).output().await.unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let first: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = first["session_id"].as_str().unwrap().to_owned();

    // Another device answers one more prompt in the same session.
    let mut elsewhere = sync.stored(&id);
    let messages = elsewhere["messages"].as_array_mut().unwrap();
    messages.push(serde_json::json!({"role": "user", "content": "asked on the laptop"}));
    messages.push(serde_json::json!({"role": "assistant", "content": "answered on the laptop"}));
    sync.write(elsewhere, &sync.trace(&id));

    let output = device
        .command(&model, &["--resume", &id, "--prompt", "second", "--output-format", "json"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let stored = sync.stored(&id);
    let said: Vec<&str> = stored["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] != "system")
        .filter_map(|message| message["content"].as_str())
        .collect();
    let laptop = said.iter().position(|text| *text == "asked on the laptop");
    let here = said.iter().position(|text| *text == "second");
    assert!(laptop.is_some(), "the other device's turn was overwritten: {said:?}");
    assert!(here > laptop, "this run continues after it: {said:?}");
    // Nothing diverged, so nothing was forked.
    let sessions = std::fs::read_dir(device.directory.path().join("home/sessions"))
        .unwrap()
        .flatten()
        .flat_map(|shard| std::fs::read_dir(shard.path()).unwrap().flatten())
        .filter(|file| file.path().extension().is_some_and(|extension| extension == "json"))
        .count();
    assert_eq!(sessions, 1);
}

/// A model endpoint like [`model_endpoint`] that also keeps every request it
/// was sent, so a test can see what the model was given.
fn recording_model_endpoint() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let request = read_request(&mut stream);
            let response = if request.starts_with("GET /v1/models") {
                "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
            } else {
                record.lock().unwrap().push(request);
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"done\"}}}}]}}\n\ndata: {MODEL_USAGE}\n\ndata: [DONE]\n\n"
                );
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{address}/v1"), seen)
}

/// What the desktop composer leans on: commands to complete after `/`, files
/// to complete after `@`, a skill invoked by name, and a file attached as text.
#[tokio::test]
async fn app_server_completes_commands_and_files_and_expands_what_the_person_typed() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    let home = device.directory.path().join("home");
    std::fs::create_dir_all(home.join("skills/review")).unwrap();
    std::fs::write(
        home.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review code carefully\n---\nCheck every edge case.\n",
    )
    .unwrap();
    let work = device.directory.path().join("work");
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::write(work.join("src/parser.rs"), "fn parse() {}").unwrap();

    let (model, requests) = recording_model_endpoint();
    let mut child = device
        .command(&model, &["app-server"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let request = |id: u32, method: &str, params: serde_json::Value| {
        format!("{}\n", serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
    };
    for frame in [
        request(1, "initialize", serde_json::json!({})),
        request(2, "commands/list", serde_json::json!({})),
        request(3, "files/search", serde_json::json!({"query": "pars"})),
        request(4, "turn/start", serde_json::json!({"text": "/btw while idle"})),
        request(
            5,
            "turn/start",
            serde_json::json!({"input": [
                {"type": "text", "text": "/review the parser"},
                {"type": "file", "name": "notes.txt", "text": "remember the edge"},
            ]}),
        ),
    ] {
        input.write_all(frame.as_bytes()).await.unwrap();
    }

    let mut replies = std::collections::HashMap::new();
    let mut user_item = serde_json::Value::Null;
    let finished = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if let Some(id) = frame["id"].as_u64() {
                replies.insert(id, frame.clone());
            }
            if frame["method"] == "item/completed" && frame["params"]["item"]["type"] == "userMessage" {
                user_item = frame["params"]["item"].clone();
            }
            if frame["method"] == "turn/completed" {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(finished, Ok(true), "the turn should complete");

    let initialize = &replies[&1]["result"]["capabilities"];
    assert_eq!((initialize["commands"].clone(), initialize["fileSearch"].clone()), (true.into(), true.into()));
    assert_eq!(initialize["tasks"], true);
    let commands = replies[&2]["result"]["commands"].as_array().unwrap();
    let named = |name: &str| commands.iter().find(|command| command["name"] == name).cloned();
    assert_eq!(named("review").unwrap()["kind"], "skill");
    assert_eq!(named("btw").unwrap()["kind"], "builtin");
    assert_eq!(replies[&3]["result"]["files"], serde_json::json!(["src/parser.rs"]));
    assert!(replies[&4]["error"]["message"].as_str().unwrap().contains("while a turn is running"));

    // The transcript shows what was typed; the model gets the skill and the file.
    assert_eq!(user_item["text"], "/review the parser");
    assert_eq!(user_item["files"], serde_json::json!(["notes.txt"]));
    let sent = requests.lock().unwrap().join("\n");
    assert!(sent.contains("Check every edge case."), "the skill's instructions reach the model");
    assert!(sent.contains("remember the edge"), "the attached file reaches the model");

    input.write_all(request(6, "shutdown", serde_json::json!({})).as_bytes()).await.unwrap();
    drop(input);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await;
}

/// The terminal's text commands, as the desktop runs them: each answers with
/// what to show, `/goal` starts a turn on its objective, and the agent's limits
/// are settings the window can change.
#[tokio::test]
async fn app_server_runs_the_terminal_commands_and_writes_the_agent_limits() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    let (model, requests) = recording_model_endpoint();
    let mut child = device
        .command(&model, &["app-server"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let request = |id: u32, method: &str, params: serde_json::Value| {
        format!("{}\n", serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
    };
    let run = |name: &str, argument: &str| serde_json::json!({"name": name, "argument": argument});
    for frame in [
        request(1, "initialize", serde_json::json!({})),
        request(2, "config/value/write", serde_json::json!({"key": "max_steps", "value": 300})),
        request(3, "config/value/write", serde_json::json!({"key": "max_steps", "value": 5000})),
        request(4, "config/value/write", serde_json::json!({"key": "tool_output_limit", "value": 50_000})),
        request(5, "command/run", run("memory", "")),
        request(6, "command/run", run("tools", "")),
        request(7, "command/run", run("repair", "")),
        request(8, "command/run", run("memory", "nonsense")),
        request(9, "command/run", run("goal", "tidy the parser")),
    ] {
        input.write_all(frame.as_bytes()).await.unwrap();
    }

    let mut replies = std::collections::HashMap::new();
    let mut user_item = serde_json::Value::Null;
    let finished = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if let Some(id) = frame["id"].as_u64() {
                replies.insert(id, frame.clone());
            }
            if frame["method"] == "item/completed" && frame["params"]["item"]["type"] == "userMessage" {
                user_item = frame["params"]["item"].clone();
            }
            if frame["method"] == "turn/completed" {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(finished, Ok(true), "the goal's turn should complete");

    let initialize = &replies[&1]["result"];
    assert_eq!(initialize["capabilities"]["commandRun"], true);
    assert_eq!(initialize["maxSteps"], 128, "the default is what runs");
    assert_eq!(replies[&2]["result"]["maxSteps"], 300, "above the old ceiling of 128");
    assert!(replies[&3]["error"]["message"].as_str().unwrap().contains("between 1 and 1024"));
    assert_eq!(replies[&4]["result"]["toolOutputLimit"], 50_000);
    let text = |id: u64| replies[&id]["result"]["text"].as_str().unwrap_or_default().to_owned();
    assert!(text(5).starts_with("No memories here yet"), "{}", text(5));
    assert!(text(6).contains("Tools: ") && text(6).contains("read_file"), "{}", text(6));
    assert!(text(7).starts_with("No corruption found"), "{}", text(7));
    assert!(replies[&8]["error"]["message"].as_str().unwrap().contains("Usage: /memory"));
    assert_eq!(text(9), "Goal set: tidy the parser");
    assert_eq!(user_item["text"], "tidy the parser");
    assert!(requests.lock().unwrap().join("\n").contains("tidy the parser"), "the goal reaches the model");

    let settings = std::fs::read_to_string(device.directory.path().join("home/config.toml")).unwrap_or_default();
    assert!(settings.contains("max_steps = 300"), "saved for the next session too:\n{settings}");

    input.write_all(request(10, "shutdown", serde_json::json!({})).as_bytes()).await.unwrap();
    drop(input);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await;
}

/// The window's "Report a bug" goes where the terminal's /feedback goes, and
/// says which window sent it.
#[tokio::test]
async fn app_server_sends_a_report_where_feedback_goes() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let reports = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        let body = r#"{"id":"fb-1","message":"Thanks"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        request
    });
    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    std::fs::write(
        device.directory.path().join("home/config.toml"),
        format!("[feedback]\nendpoint = \"http://{address}/v1/feedback\"\n"),
    )
    .unwrap();
    let (model, _) = recording_model_endpoint();
    let mut child = device
        .command(&model, &["app-server"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let report = serde_json::json!({
        "category": "Bug",
        "message": "The plan bar overlaps the composer",
        "client": "abacus-desktop 0.1.6",
    });
    for frame in [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "feedback/submit", "params": {"message": "  "}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "feedback/submit", "params": report}),
    ] {
        input.write_all(format!("{frame}\n").as_bytes()).await.unwrap();
    }
    let mut replies = std::collections::HashMap::new();
    let answered = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if let Some(id) = frame["id"].as_u64() {
                replies.insert(id, frame);
                if replies.len() == 3 {
                    return true;
                }
            }
        }
        false
    })
    .await;
    assert_eq!(answered, Ok(true));
    assert_eq!(replies[&1]["result"]["capabilities"]["feedback"], true);
    assert!(replies[&2]["error"]["message"].as_str().unwrap().contains("describe what happened"));
    assert_eq!(replies[&3]["result"]["id"], "fb-1");

    let sent = reports.join().unwrap();
    let body: serde_json::Value = serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["category"], "bug");
    assert_eq!(body["message"], "The plan bar overlaps the composer");
    assert!(body["app_version"].as_str().unwrap().ends_with("(abacus-desktop 0.1.6)"), "{body}");

    input.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"shutdown\"}\n").await.unwrap();
    drop(input);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await;
}

/// Told to, an app server with nothing to do stops by itself, saying so first,
/// so a desktop that keeps one per thread does not keep them all running.
#[tokio::test]
async fn app_server_stops_by_itself_once_nothing_happens() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    let (model, _) = recording_model_endpoint();
    let mut child = device
        .command(&model, &["app-server"])
        .env("ABACUS_IDLE_EXIT_SECS", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let initialize = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
    input.write_all(format!("{initialize}\n").as_bytes()).await.unwrap();

    let started = std::time::Instant::now();
    let said_idle = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            if frame["method"] == "server/idle" {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(said_idle, Ok(true), "it says why it is going");
    assert!(started.elapsed() >= std::time::Duration::from_millis(900), "not before the quiet spell is up");
    let status = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await;
    assert!(status.is_ok_and(|status| status.unwrap().success()), "and exits cleanly, with stdin still open");
    drop(input);
}

/// A model endpoint whose reply to each chat request `answer` picks from the
/// request itself, and that keeps every request.
fn scripted_model_endpoint(answer: fn(&str) -> String) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let request = read_request(&mut stream);
            let response = if request.starts_with("GET /v1/models") {
                "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
            } else {
                let body = answer(&request);
                record.lock().unwrap().push(request);
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{address}/v1"), seen)
}

/// The desktop lends the agent its browser: the tool it registers reaches the
/// model, a call comes back to it as `tool/call`, and its answer (a screenshot
/// included) goes on to the model.
#[tokio::test]
async fn app_server_lets_the_front_end_provide_tools() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // The turn's own request is the one offered the browser tool; until the
    // tool has answered, the model calls it. Anything else (the app server's
    // side requests, the turn after the tool) gets a plain answer.
    fn answer(request: &str) -> String {
        if request.contains("browser_navigate") && !request.contains("Loaded Example Domain") {
            let call = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"browser_navigate","arguments":"{\"url\":\"https://example.com\"}"}}]}}]}"#;
            let finish = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
            return format!("data: {call}\n\ndata: {finish}\n\ndata: {MODEL_USAGE}\n\ndata: [DONE]\n\n");
        }
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"It is Example Domain.\"}}}}]}}\n\ndata: {MODEL_USAGE}\n\ndata: [DONE]\n\n"
        )
    }
    let (model, requests) = scripted_model_endpoint(answer);
    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    let mut child = device
        .command(&model, &["app-server"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let frame = |value: serde_json::Value| format!("{value}\n");
    let tools = serde_json::json!({"tools": [{
        "name": "browser_navigate",
        "description": "Open a URL in the desktop's browser",
        "needsApproval": false,
        "parameters": {"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"]},
    }]});
    for request in [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/register", "params": tools}),
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "turn/start", "params": {"text": "what is example.com?"}}),
    ] {
        input.write_all(frame(request).as_bytes()).await.unwrap();
    }

    let mut registered = serde_json::Value::Null;
    let mut asked = serde_json::Value::Null;
    let finished = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let message: serde_json::Value = serde_json::from_str(&line).unwrap();
            if message["id"] == 2 {
                registered = message["result"].clone();
            }
            if message["method"] == "tool/call" {
                asked = message["params"].clone();
                let answer = serde_json::json!({"jsonrpc": "2.0", "id": message["id"], "result": {
                    "content": "Loaded Example Domain (https://example.com/)",
                    "images": ["data:image/png;base64,QUJD"],
                }});
                input.write_all(frame(answer).as_bytes()).await.unwrap();
            }
            if message["method"] == "turn/completed" {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(finished, Ok(true), "the turn should complete");
    assert_eq!(registered["registered"], serde_json::json!(["browser_navigate"]));
    assert_eq!(
        (asked["name"].clone(), asked["arguments"]["url"].clone()),
        ("browser_navigate".into(), "https://example.com".into())
    );

    let sent = requests.lock().unwrap().clone();
    let after = sent.iter().find(|request| request.contains("Loaded Example Domain"));
    let after = after.expect("the tool's answer reaches the model");
    assert!(after.contains("data:image/png;base64,QUJD"), "and so does its screenshot");

    input
        .write_all(frame(serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "shutdown"})).as_bytes())
        .await
        .unwrap();
    drop(input);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await;
}

/// The meter a front end draws is how full the context is, which stays put
/// while the session's spending grows with every turn. Reporting the running
/// total as the context put every long session at 100%.
#[tokio::test]
async fn app_server_reports_how_full_the_context_is() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // A prompt far larger than the estimate from the messages, so the
    // provider's own count is what the meter shows.
    const PROMPT: u64 = 50_000;
    fn answer(_request: &str) -> String {
        let usage = r#"{"choices":[],"usage":{"prompt_tokens":50000,"completion_tokens":34,"total_tokens":50034}}"#;
        format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"Done.\"}}}}]}}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
    }
    let (model, _requests) = scripted_model_endpoint(answer);
    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    let mut child = device
        .command(&model, &["app-server"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let frame = |value: serde_json::Value| format!("{value}\n");
    input
        .write_all(
            frame(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})).as_bytes(),
        )
        .await
        .unwrap();

    let mut reports = Vec::new();
    for (id, text) in [(2, "first"), (3, "second")] {
        let start = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "turn/start", "params": {"text": text}});
        input.write_all(frame(start).as_bytes()).await.unwrap();
        let report = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let mut last = serde_json::Value::Null;
            while let Some(line) = lines.next_line().await.unwrap() {
                let message: serde_json::Value = serde_json::from_str(&line).unwrap();
                if message["method"] == "thread/tokenUsage/updated" {
                    last = message["params"].clone();
                }
                if message["method"] == "turn/completed" {
                    return last;
                }
            }
            serde_json::Value::Null
        })
        .await
        .expect("the turn should complete");
        reports.push(report);
    }

    for report in &reports {
        assert_eq!(report["contextTokens"], PROMPT, "the last prompt's size: {report}");
    }
    let spent = |report: &serde_json::Value| report["tokensUsed"].as_u64().unwrap();
    assert!(spent(&reports[0]) > PROMPT, "spending counts the reply as well");
    assert!(spent(&reports[1]) >= spent(&reports[0]) + PROMPT, "and grows with every turn, unlike the context");

    input
        .write_all(frame(serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "shutdown"})).as_bytes())
        .await
        .unwrap();
    drop(input);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await;
}

/// A front end's tool acts on state that moves between calls: reading the page
/// after each click repeats `browser_read_page {}` exactly, and every one of
/// those reads has to reach the front end rather than be blocked as a loop.
#[tokio::test]
async fn front_end_tools_may_repeat_their_arguments() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // The turn reads the page until it has three snapshots, then answers. A
    // blocked call ends it early, so the test fails rather than spinning.
    fn answer(request: &str) -> String {
        let reads = request.matches("Page snapshot").count();
        if request.contains("browser_read_page") && !request.contains("Blocked: the same tool call") && reads < 3 {
            let call = format!(
                r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"call_{}","type":"function","function":{{"name":"browser_read_page","arguments":"{{}}"}}}}]}}}}]}}"#,
                reads + 1
            );
            let finish = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
            return format!("data: {call}\n\ndata: {finish}\n\ndata: {MODEL_USAGE}\n\ndata: [DONE]\n\n");
        }
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"The page settled.\"}}}}]}}\n\ndata: {MODEL_USAGE}\n\ndata: [DONE]\n\n"
        )
    }
    let (model, requests) = scripted_model_endpoint(answer);
    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    let mut child = device
        .command(&model, &["app-server"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let frame = |value: serde_json::Value| format!("{value}\n");
    let tools = serde_json::json!({"tools": [{
        "name": "browser_read_page",
        "description": "Read the page in the desktop's browser",
        "needsApproval": false,
        "parameters": {"type": "object", "properties": {}},
    }]});
    for request in [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/register", "params": tools}),
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "turn/start", "params": {"text": "watch the page"}}),
    ] {
        input.write_all(frame(request).as_bytes()).await.unwrap();
    }

    let mut asked = Vec::new();
    let finished = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let message: serde_json::Value = serde_json::from_str(&line).unwrap();
            if message["method"] == "tool/call" {
                asked.push(message["params"].clone());
                let answer = serde_json::json!({"jsonrpc": "2.0", "id": message["id"], "result": {
                    "content": format!("Page snapshot {}", asked.len()),
                }});
                input.write_all(frame(answer).as_bytes()).await.unwrap();
            }
            if message["method"] == "turn/completed" {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(finished, Ok(true), "the turn should complete");
    let calls: Vec<_> = asked.iter().map(|call| (call["name"].clone(), call["arguments"].clone())).collect();
    assert_eq!(
        calls,
        vec![(serde_json::Value::from("browser_read_page"), serde_json::json!({})); 3],
        "every read reaches the front end"
    );

    let sent = requests.lock().unwrap().clone();
    assert!(sent.iter().all(|request| !request.contains("Blocked: the same tool call")), "no read is blocked");
    assert!(sent.iter().any(|request| request.contains("Page snapshot 3")), "the third read reaches the model");

    input
        .write_all(frame(serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "shutdown"})).as_bytes())
        .await
        .unwrap();
    drop(input);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await;
}

/// What a turn's file tools changed is shown under it, with its own diff, and
/// can be taken back; the model hears of it, and a reopened thread shows it.
#[tokio::test]
async fn app_server_shows_a_turns_edits_and_undoes_them() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // The turn writes a file once; after that, and for everything else, the
    // model just answers.
    fn answer(request: &str) -> String {
        if request.contains("write the notes") && !request.contains("Wrote notes.txt") {
            let call = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"notes.txt\",\"content\":\"one\\ntwo\\n\"}"}}]}}]}"#;
            let finish = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
            return format!("data: {call}\n\ndata: {finish}\n\ndata: {MODEL_USAGE}\n\ndata: [DONE]\n\n");
        }
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"Done.\"}}}}]}}\n\ndata: {MODEL_USAGE}\n\ndata: [DONE]\n\n"
        )
    }
    let (model, requests) = scripted_model_endpoint(answer);
    let sync = sync_server::SyncServer::start("secret-token").await;
    let device = SignedIn::new(&sync, "secret-token");
    let notes = device.directory.path().join("work/notes.txt");
    let mut child = device
        .command(&model, &["app-server"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let frame = |value: serde_json::Value| format!("{value}\n");
    for request in [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "config/value/write", "params": {"key": "mode", "value": "yolo"}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "turn/start", "params": {"text": "write the notes"}}),
    ] {
        input.write_all(frame(request).as_bytes()).await.unwrap();
    }

    let mut edits = serde_json::Value::Null;
    let mut answers = std::collections::HashMap::new();
    let mut undone = serde_json::Value::Null;
    let mut asked_diff = false;
    let finished = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let message: serde_json::Value = serde_json::from_str(&line).unwrap();
            if message["method"] == "item/completed" && message["params"]["item"]["type"] == "fileEdits" {
                edits = message["params"]["item"].clone();
            }
            if message["method"] == "turn/undone" {
                undone = message["params"].clone();
            }
            if let Some(id) = message["id"].as_i64().filter(|_| message.get("method").is_none()) {
                answers.insert(id, message.clone());
                if id == 6 {
                    return true;
                }
            }
            if message["method"] == "turn/completed" && !asked_diff {
                asked_diff = true;
                let checkpoint = edits["checkpoint"].clone();
                for request in [
                    serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "thread/diff/read", "params": {"checkpoint": checkpoint}}),
                    serde_json::json!({"jsonrpc": "2.0", "id": 5, "method": "turn/undo", "params": {"checkpoint": checkpoint}}),
                    // Reopened from what was saved.
                    serde_json::json!({"jsonrpc": "2.0", "id": 6, "method": "thread/read", "params": {"threadId": message["params"]["threadId"]}}),
                ] {
                    input.write_all(frame(request).as_bytes()).await.unwrap();
                }
            }
        }
        false
    })
    .await;
    assert_eq!(finished, Ok(true), "the turn should complete and the requests be answered");
    assert_eq!(
        edits["files"],
        serde_json::json!([{"path": "notes.txt", "status": "added", "added": 2, "removed": 0}]),
        "{edits}"
    );
    let diff = answers[&4]["result"]["diff"].as_str().unwrap_or_default().to_owned();
    assert!(diff.contains("+++ b/notes.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n"), "{diff}");
    assert_eq!(answers[&5]["result"]["files"], serde_json::json!(["notes.txt"]), "{:?}", answers[&5]);
    assert_eq!(undone["files"], serde_json::json!(["notes.txt"]));
    assert!(!notes.exists(), "undo takes the new file away");
    let items = answers[&6]["result"]["items"].as_array().cloned().unwrap_or_default();
    let shown: Vec<_> = items.iter().filter(|item| item["type"] == "fileEdits").collect();
    assert_eq!(shown.len(), 1, "{items:?}");
    assert_eq!(shown[0]["undone"], true);
    assert!(
        !items.iter().any(|item| item["text"].as_str().is_some_and(|text| text.contains("undid"))),
        "what the model is told of the undo is not shown as said"
    );

    // The next turn tells the model, and never sends it the marker.
    input
        .write_all(
            frame(
                serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "turn/start", "params": {"text": "and now?"}}),
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains("\"turn/completed\"") {
                return;
            }
        }
    })
    .await;
    let sent = requests.lock().unwrap().clone();
    let last = sent.iter().rev().find(|request| request.contains("and now?")).expect("the next turn reached the model");
    assert!(last.contains("undid the edits you made"), "the model hears of the undo");
    let checkpoint = edits["checkpoint"].as_str().unwrap();
    assert!(!last.contains(checkpoint), "the marker stays out of requests");

    input
        .write_all(frame(serde_json::json!({"jsonrpc": "2.0", "id": 8, "method": "shutdown"})).as_bytes())
        .await
        .unwrap();
    drop(input);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await;
}
