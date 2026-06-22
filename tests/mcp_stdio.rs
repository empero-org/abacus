#![cfg(unix)]

use std::{collections::BTreeMap, fs};

use abacus_agent::mcp::{McpManager, McpServerConfig, McpTransport};
use tempfile::tempdir;

#[tokio::test]
async fn negotiates_lists_and_calls_stdio_mcp() {
    if std::process::Command::new("python3")
        .arg("--version")
        .status()
        .is_err()
    {
        return;
    }
    let directory = tempdir().unwrap();
    let script = directory.path().join("server.py");
    fs::write(
        &script,
        r#"import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    method = msg.get("method")
    if method == "initialize":
        result = {"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"test","version":"1"}}
    elif method == "tools/list":
        result = {"tools":[{"name":"echo","description":"echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}
    elif method == "tools/call":
        result = {"content":[{"type":"text","text":msg["params"]["arguments"]["text"]}]}
    else:
        result = {}
    print(json.dumps({"jsonrpc":"2.0","id":msg["id"],"result":result}), flush=True)
"#,
    )
    .unwrap();
    let mut configs = BTreeMap::new();
    configs.insert(
        "local".into(),
        McpServerConfig {
            transport: McpTransport::Stdio,
            command: Some("python3".into()),
            args: vec![script.to_string_lossy().into_owned()],
            timeout_seconds: 5,
            ..McpServerConfig::default()
        },
    );
    let manager = McpManager::connect(&configs, directory.path()).await;
    assert!(
        manager.diagnostics().is_empty(),
        "{:?}",
        manager.diagnostics()
    );
    assert_eq!(
        manager.tools().next().unwrap().exposed_name,
        "mcp__local__echo"
    );
    assert_eq!(
        manager
            .execute("mcp__local__echo", r#"{"text":"hello stdio"}"#)
            .await
            .unwrap(),
        "hello stdio"
    );
}
