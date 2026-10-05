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

#[test]
fn headless_loop_stops_when_promise_appears() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        // The /models probe lands first; answer 404, then serve two chat turns.
        let mut served_chat = 0;
        while served_chat < 2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 64 * 1024];
            let read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]);
            if request.starts_with("GET /v1/models") {
                let response = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                stream.write_all(response.as_bytes()).unwrap();
                continue;
            }
            // First chat turn: no promise yet. Second: emits DONE and ends the loop.
            let content = if served_chat == 0 { "still working" } else { "all green DONE" };
            let body =
                format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\ndata: [DONE]\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            served_chat += 1;
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
            "finish the task",
            "--loop",
            "--completion-promise",
            "DONE",
            "--max-iterations",
            "5",
            "--output-format",
            "json",
        ])
        .output()
        .unwrap();
    server.join().unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert!(value["text"].as_str().unwrap().contains("DONE"));
    // The server served exactly two chat turns (plus the /models probe); a third
    // chat turn would have hung the test.
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
