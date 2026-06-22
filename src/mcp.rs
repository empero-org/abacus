use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, stream};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot},
    time::timeout,
};

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub enabled: bool,
    pub timeout_seconds: u64,
    pub auto_approve: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            transport: McpTransport::Stdio,
            command: None,
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            timeout_seconds: 60,
            auto_approve: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpTool {
    pub exposed_name: String,
    pub server: String,
    pub original_name: String,
    pub description: String,
    pub input_schema: Value,
    pub auto_approve: bool,
}

#[derive(Clone, Default)]
pub struct McpManager {
    servers: Arc<BTreeMap<String, Arc<McpClient>>>,
    tools: Arc<BTreeMap<String, McpTool>>,
    diagnostics: Arc<Vec<String>>,
}

// One client is created per configured MCP server (never in a hot path or a
// large collection), so the size gap between the stdio and http variants — which
// only crosses clippy's threshold on Windows, where process handles are larger —
// does not matter; boxing would add indirection for no real benefit.
#[allow(clippy::large_enum_variant)]
enum McpClient {
    Stdio(StdioClient),
    Http(HttpClient),
}

struct StdioClient {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: PendingRequests,
    next_id: AtomicU64,
    timeout: Duration,
}

struct HttpClient {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
    timeout: Duration,
}

impl McpManager {
    pub async fn connect(configs: &BTreeMap<String, McpServerConfig>, workspace: &Path) -> Self {
        let mut servers = BTreeMap::new();
        let mut tools = BTreeMap::new();
        let mut diagnostics = Vec::new();

        let enabled = configs
            .iter()
            .filter(|(_, config)| config.enabled)
            .map(|(name, config)| (name.clone(), config.clone()))
            .collect::<Vec<_>>();
        let workspace = workspace.to_owned();
        let mut attempts = stream::iter(enabled)
            .map(|(name, config)| {
                let workspace = workspace.clone();
                async move {
                    let result = async {
                        let client =
                            Arc::new(McpClient::connect(&name, &config, &workspace).await?);
                        let server_tools = client.list_tools().await?;
                        Ok::<_, anyhow::Error>((client, server_tools))
                    }
                    .await;
                    (name, config.auto_approve, result)
                }
            })
            .buffer_unordered(8)
            .collect::<Vec<_>>()
            .await;
        attempts.sort_by(|left, right| left.0.cmp(&right.0));

        for (name, auto_approve, result) in attempts {
            match result {
                Ok((client, server_tools)) => {
                    for tool in server_tools {
                        let exposed = namespaced(&name, &tool.name);
                        if tools.contains_key(&exposed) {
                            diagnostics.push(format!(
                                "MCP tool collision for {exposed}; later tool ignored"
                            ));
                            continue;
                        }
                        tools.insert(
                            exposed.clone(),
                            McpTool {
                                exposed_name: exposed,
                                server: name.clone(),
                                original_name: tool.name,
                                description: tool.description,
                                input_schema: tool.input_schema,
                                auto_approve,
                            },
                        );
                    }
                    servers.insert(name, client);
                }
                Err(error) => diagnostics.push(format!("MCP `{name}` failed: {error:#}")),
            }
        }

        Self {
            servers: Arc::new(servers),
            tools: Arc::new(tools),
            diagnostics: Arc::new(diagnostics),
        }
    }

    pub fn tools(&self) -> impl Iterator<Item = &McpTool> {
        self.tools.values()
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn tool_specs(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|tool| {
                json!({
                    "type":"function",
                    "function":{
                        "name":tool.exposed_name,
                        "description":format!("[MCP: {}] {}", tool.server, tool.description),
                        "parameters":tool.input_schema
                    }
                })
            })
            .collect()
    }

    pub fn needs_approval(&self, name: &str) -> Option<bool> {
        self.tools.get(name).map(|tool| !tool.auto_approve)
    }

    pub fn approval_details(&self, name: &str, arguments: &str) -> Option<String> {
        self.tools.get(name).map(|tool| {
            let pretty = serde_json::from_str::<Value>(arguments)
                .ok()
                .and_then(|value| serde_json::to_string_pretty(&value).ok())
                .unwrap_or_else(|| arguments.to_owned());
            format!(
                "MCP server: {}\nTool: {}\nArguments:\n{}",
                tool.server, tool.original_name, pretty
            )
        })
    }

    pub async fn execute(&self, name: &str, arguments: &str) -> Option<String> {
        let tool = self.tools.get(name)?;
        let result = async {
            let arguments: Value =
                serde_json::from_str(arguments).context("invalid MCP arguments")?;
            let client = self
                .servers
                .get(&tool.server)
                .context("MCP server is not connected")?;
            let result = client
                .request(
                    "tools/call",
                    json!({"name":tool.original_name,"arguments":arguments}),
                )
                .await?;
            format_tool_result(&result)
        }
        .await;
        Some(result.unwrap_or_else(|error| format!("Error: {error:#}")))
    }
}

#[derive(Debug)]
struct ListedTool {
    name: String,
    description: String,
    input_schema: Value,
}

impl McpClient {
    async fn connect(name: &str, config: &McpServerConfig, workspace: &Path) -> Result<Self> {
        let client = match config.transport {
            McpTransport::Stdio => Self::Stdio(StdioClient::spawn(name, config, workspace).await?),
            McpTransport::Http => Self::Http(HttpClient::new(config)?),
        };
        let initialized = client
            .request(
                "initialize",
                json!({
                    "protocolVersion":MCP_PROTOCOL_VERSION,
                    "capabilities":{},
                    "clientInfo":{"name":"abacus-agent","version":env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        let negotiated = initialized["protocolVersion"]
            .as_str()
            .context("MCP initialize response omitted protocolVersion")?;
        if negotiated != MCP_PROTOCOL_VERSION {
            bail!(
                "MCP server negotiated unsupported protocol {negotiated}; Abacus requires {MCP_PROTOCOL_VERSION}"
            );
        }
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match self {
            Self::Stdio(client) => client.request(method, params).await,
            Self::Http(client) => client.request(method, params).await,
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        match self {
            Self::Stdio(client) => client.notify(method, params).await,
            Self::Http(client) => client.notify(method, params).await,
        }
    }

    async fn list_tools(&self) -> Result<Vec<ListedTool>> {
        let mut cursor = None;
        let mut output = Vec::new();
        loop {
            let params = cursor
                .as_ref()
                .map(|cursor| json!({"cursor":cursor}))
                .unwrap_or_else(|| json!({}));
            let result = self.request("tools/list", params).await?;
            let listed = result["tools"]
                .as_array()
                .context("MCP tools/list response omitted tools")?;
            for tool in listed {
                let name = tool["name"]
                    .as_str()
                    .context("MCP tool omitted name")?
                    .to_owned();
                let description = tool["description"]
                    .as_str()
                    .unwrap_or("MCP tool")
                    .to_owned();
                let input_schema = tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object","properties":{}}));
                output.push(ListedTool {
                    name,
                    description,
                    input_schema,
                });
            }
            cursor = result["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(output)
    }
}

impl StdioClient {
    async fn spawn(name: &str, config: &McpServerConfig, workspace: &Path) -> Result<Self> {
        let command = config
            .command
            .as_deref()
            .with_context(|| format!("stdio MCP `{name}` is missing command"))?;
        let mut process = Command::new(command);
        process
            .args(&config.args)
            .current_dir(resolve_cwd(config.cwd.as_deref(), workspace))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (key, value) in &config.env {
            process.env(key, expand_env(value)?);
        }
        let mut child = process
            .spawn()
            .with_context(|| format!("could not start MCP `{name}` command `{command}`"))?;
        let stdin = child.stdin.take().context("MCP stdin was not captured")?;
        let stdout = child.stdout.take().context("MCP stdout was not captured")?;
        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(id) = message["id"].as_u64() else {
                    continue;
                };
                if let Some(sender) = reader_pending.lock().await.remove(&id) {
                    let response = if let Some(error) = message.get("error") {
                        Err(error.to_string())
                    } else {
                        Ok(message.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = sender.send(response);
                }
            }
            let mut pending = reader_pending.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err("MCP process closed stdout".to_owned()));
            }
        });
        Ok(Self {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            pending,
            next_id: AtomicU64::new(1),
            timeout: Duration::from_secs(config.timeout_seconds.clamp(1, 600)),
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        if let Err(error) = self.write(&message).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        let result = timeout(self.timeout, receiver)
            .await
            .map_err(|_| anyhow!("MCP request `{method}` timed out"))?
            .map_err(|_| anyhow!("MCP response channel closed"))?
            .map_err(|error| anyhow!("MCP error: {error}"))?;
        Ok(result)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    async fn write(&self, message: &Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        let mut encoded = serde_json::to_vec(message)?;
        encoded.push(b'\n');
        stdin.write_all(&encoded).await?;
        stdin.flush().await?;
        Ok(())
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
    }
}

impl HttpClient {
    fn new(config: &McpServerConfig) -> Result<Self> {
        let url = config.url.clone().context("HTTP MCP is missing url")?;
        let mut headers = HeaderMap::new();
        for (name, value) in &config.headers {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes())?,
                HeaderValue::from_str(&expand_env(value)?)?,
            );
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(config.timeout_seconds.clamp(1, 600)))
                .build()?,
            url,
            headers,
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
            timeout: Duration::from_secs(config.timeout_seconds.clamp(1, 600)),
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let response = self.send(message).await?;
        if let Some(error) = response.get("error") {
            bail!("MCP error: {error}");
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({"jsonrpc":"2.0","method":method,"params":params});
        let _ = self.send(message).await?;
        Ok(())
    }

    async fn send(&self, message: Value) -> Result<Value> {
        let mut request = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
            .json(&message);
        if let Some(session) = self.session_id.lock().await.clone() {
            request = request.header("mcp-session-id", session);
        }
        let response = timeout(self.timeout, request.send())
            .await
            .map_err(|_| anyhow!("HTTP MCP request timed out"))??;
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *self.session_id.lock().await = Some(session.to_owned());
        }
        let status = response.status();
        if !status.is_success() {
            bail!(
                "HTTP MCP returned {status}: {}",
                response.text().await.unwrap_or_default()
            );
        }
        if status.as_u16() == 202 {
            return Ok(Value::Null);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = response.text().await?;
        if content_type.contains("text/event-stream") {
            for line in body.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    let value: Value = serde_json::from_str(data.trim())?;
                    if value.get("id") == message.get("id") {
                        return Ok(value);
                    }
                }
            }
            bail!("HTTP MCP event stream contained no matching response")
        } else if body.trim().is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_str(&body).context("HTTP MCP returned invalid JSON")
        }
    }
}

fn resolve_cwd(configured: Option<&Path>, workspace: &Path) -> PathBuf {
    match configured {
        Some(path) if path.is_absolute() => path.to_owned(),
        Some(path) => workspace.join(path),
        None => workspace.to_owned(),
    }
}

fn expand_env(value: &str) -> Result<String> {
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').context("unterminated ${ENV} reference")?;
        let name = &after[..end];
        let resolved = std::env::var(name)
            .with_context(|| format!("environment variable `{name}` is not set"))?;
        output.push_str(&resolved);
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn namespaced(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitize(server), sanitize(tool))
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn format_tool_result(result: &Value) -> Result<String> {
    let mut parts = Vec::new();
    if let Some(content) = result["content"].as_array() {
        for block in content {
            match block["type"].as_str().unwrap_or_default() {
                "text" => parts.push(block["text"].as_str().unwrap_or_default().to_owned()),
                "image" => parts.push(format!(
                    "[image: {}]",
                    block["mimeType"].as_str().unwrap_or("unknown")
                )),
                "audio" => parts.push(format!(
                    "[audio: {}]",
                    block["mimeType"].as_str().unwrap_or("unknown")
                )),
                _ => parts.push(serde_json::to_string_pretty(block)?),
            }
        }
    }
    if let Some(structured) = result.get("structuredContent") {
        parts.push(serde_json::to_string_pretty(structured)?);
    }
    if parts.is_empty() {
        parts.push(serde_json::to_string_pretty(result)?);
    }
    let output = parts.join("\n");
    if result["isError"].as_bool().unwrap_or(false) {
        Ok(format!("Error: {output}"))
    } else {
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_tool_names() {
        assert_eq!(
            namespaced("git hub", "search/code"),
            "mcp__git_hub__search_code"
        );
    }

    #[test]
    fn formats_text_and_structured_results() {
        let output = format_tool_result(&json!({
            "content":[{"type":"text","text":"hello"}],
            "structuredContent":{"count":1}
        }))
        .unwrap();
        assert!(output.contains("hello"));
        assert!(output.contains("\"count\": 1"));
    }
}
