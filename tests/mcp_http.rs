use std::collections::BTreeMap;

use abacus_agent::mcp::{McpManager, McpServerConfig, McpTransport};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[tokio::test]
async fn negotiates_lists_and_calls_streamable_http_mcp() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for index in 0..4 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let header_end = request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap();
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let body: Value = serde_json::from_slice(&request[header_end + 4..]).unwrap();
            if index > 0 {
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("mcp-session-id: test-session")
                );
            }
            let (status, response, session) = match body["method"].as_str().unwrap() {
                "initialize" => (
                    "200 OK",
                    json!({
                        "jsonrpc":"2.0","id":body["id"],
                        "result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"1"}}
                    })
                    .to_string(),
                    true,
                ),
                "notifications/initialized" => ("202 Accepted", String::new(), false),
                "tools/list" => (
                    "200 OK",
                    json!({
                        "jsonrpc":"2.0","id":body["id"],
                        "result":{"tools":[{"name":"echo","description":"Echo input","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}
                    })
                    .to_string(),
                    false,
                ),
                "tools/call" => (
                    "200 OK",
                    json!({
                        "jsonrpc":"2.0","id":body["id"],
                        "result":{"content":[{"type":"text","text":body["params"]["arguments"]["text"]}]}
                    })
                    .to_string(),
                    false,
                ),
                other => panic!("unexpected method {other}"),
            };
            let session_header = if session {
                "mcp-session-id: test-session\r\n"
            } else {
                ""
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{session_header}content-length: {}\r\nconnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let mut configs = BTreeMap::new();
    configs.insert(
        "docs".to_owned(),
        McpServerConfig {
            transport: McpTransport::Http,
            url: Some(format!("http://{address}/mcp")),
            ..McpServerConfig::default()
        },
    );
    let workspace = tempdir().unwrap();
    let manager = McpManager::connect(&configs, workspace.path()).await;
    assert!(
        manager.diagnostics().is_empty(),
        "{:?}",
        manager.diagnostics()
    );
    let tool = manager.tools().next().unwrap();
    assert_eq!(tool.exposed_name, "mcp__docs__echo");
    assert_eq!(manager.needs_approval(&tool.exposed_name), Some(true));
    let result = manager
        .execute(&tool.exposed_name, r#"{"text":"hello mcp"}"#)
        .await
        .unwrap();
    assert_eq!(result, "hello mcp");
    server.await.unwrap();
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
