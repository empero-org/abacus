use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
};

use tempfile::tempdir;

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
                let response =
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                stream.write_all(response.as_bytes()).unwrap();
                continue;
            }
            assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"headless works\"}}]}\n\n",
                "data: [DONE]\n\n"
            );
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

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
                let response =
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                stream.write_all(response.as_bytes()).unwrap();
                continue;
            }
            // First chat turn: no promise yet. Second: emits DONE and ends the loop.
            let content = if served_chat == 0 {
                "still working"
            } else {
                "all green DONE"
            };
            let body = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\ndata: [DONE]\n\n"
            );
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

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert!(value["text"].as_str().unwrap().contains("DONE"));
    // The server served exactly two chat turns (plus the /models probe); a third
    // chat turn would have hung the test.
}
