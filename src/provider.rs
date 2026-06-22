use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::{Client, header};
use serde_json::{Value, json};
use tokio::{sync::mpsc, time::sleep};

use crate::{
    config::{Config, ProviderProtocol},
    tool_format::{self, ToolFormat},
    tools::ToolCall,
};

#[derive(Debug, Clone)]
pub struct Provider {
    client: Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    protocol: ProviderProtocol,
    max_output_tokens: Option<usize>,
    tool_format: ToolFormat,
    /// Best-effort running count of tokens processed, shared across provider
    /// clones (subagents) and rebuilds (model switches) so a session totals one
    /// number. Uses provider-reported usage when available, else a char-based
    /// estimate, so it is approximate.
    tokens: Arc<AtomicU64>,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl Provider {
    pub fn new(config: &Config) -> Result<Self> {
        Self::with_tokens(config, Arc::new(AtomicU64::new(0)))
    }

    /// Build a provider that accumulates token usage into a shared counter.
    /// Pass the same counter when rebuilding on a model switch so the running
    /// total survives; subagents inherit it automatically through `clone`.
    pub fn with_tokens(config: &Config, tokens: Arc<AtomicU64>) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("abacus-agent/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            .build()
            .context("could not create HTTP client")?;
        Ok(Self {
            client,
            endpoint: config.endpoint(),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            protocol: config.protocol,
            max_output_tokens: config.model_limits.configured_output_tokens,
            tool_format: config.tool_format,
            tokens,
        })
    }

    /// Approximate tokens processed so far this session.
    pub fn tokens_used(&self) -> u64 {
        self.tokens.load(Ordering::Relaxed)
    }

    fn record_tokens(
        &self,
        reported: Option<u64>,
        messages: &[Value],
        content: &str,
        calls: &BTreeMap<usize, PartialToolCall>,
    ) {
        let tokens = reported.unwrap_or_else(|| estimate_tokens(messages, content, calls));
        if tokens > 0 {
            self.tokens.fetch_add(tokens, Ordering::Relaxed);
        }
    }

    pub async fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        deltas: mpsc::UnboundedSender<String>,
    ) -> Result<Completion> {
        match self.protocol {
            ProviderProtocol::ChatCompletions => self.complete_chat(messages, tools, deltas).await,
            ProviderProtocol::Responses => self.complete_responses(messages, tools, deltas).await,
        }
    }

    async fn complete_chat(
        &self,
        messages: &[Value],
        tools: &[Value],
        deltas: mpsc::UnboundedSender<String>,
    ) -> Result<Completion> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
            "stream": true
        });
        if let Some(max_tokens) = self.max_output_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        let response = self.post_stream(&body).await?;

        let mut decoder = SseDecoder::default();
        let mut content = String::new();
        let mut calls: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
        let mut reported_tokens: Option<u64> = None;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("provider stream failed")?;
            for data in decoder.push(&chunk)? {
                if data != "[DONE]" {
                    apply_chat_delta(&data, &mut content, &mut calls, &deltas)?;
                    capture_usage(&data, &mut reported_tokens, parse_chat_usage);
                }
            }
        }
        for data in decoder.finish()? {
            if data != "[DONE]" {
                apply_chat_delta(&data, &mut content, &mut calls, &deltas)?;
                capture_usage(&data, &mut reported_tokens, parse_chat_usage);
            }
        }
        self.record_tokens(reported_tokens, messages, &content, &calls);
        // Fallback for models that emit tool calls as text instead of native
        // `tool_calls` (common for open-weight models via Ollama/llama.cpp/raw
        // vLLM). When no native calls arrived, parse the assistant text and lift
        // any tool calls into the same `tool_calls` the agent already dispatches.
        if calls.is_empty() && self.tool_format != ToolFormat::None {
            let (clean, parsed) = tool_format::parse(self.tool_format, &content);
            if !parsed.is_empty() {
                content = clean;
                for (index, call) in parsed.into_iter().enumerate() {
                    calls.insert(
                        index,
                        PartialToolCall {
                            id: format!("text_{index}"),
                            name: call.name,
                            arguments: call.arguments,
                        },
                    );
                }
            }
        }
        finish_completion(content, calls)
    }

    async fn complete_responses(
        &self,
        messages: &[Value],
        tools: &[Value],
        deltas: mpsc::UnboundedSender<String>,
    ) -> Result<Completion> {
        let mut body = json!({
            "model": self.model,
            "input": responses_input(messages),
            "tools": responses_tools(tools),
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "stream": true
        });
        if let Some(max_tokens) = self.max_output_tokens {
            body["max_output_tokens"] = json!(max_tokens);
        }
        let response = self.post_stream(&body).await?;
        let mut decoder = SseDecoder::default();
        let mut content = String::new();
        let mut calls: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
        let mut reported_tokens: Option<u64> = None;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("provider stream failed")?;
            for data in decoder.push(&chunk)? {
                if data != "[DONE]" {
                    apply_responses_event(&data, &mut content, &mut calls, &deltas)?;
                    capture_usage(&data, &mut reported_tokens, parse_responses_usage);
                }
            }
        }
        for data in decoder.finish()? {
            if data != "[DONE]" {
                apply_responses_event(&data, &mut content, &mut calls, &deltas)?;
                capture_usage(&data, &mut reported_tokens, parse_responses_usage);
            }
        }
        self.record_tokens(reported_tokens, messages, &content, &calls);
        finish_completion(content, calls)
    }

    async fn post_stream(&self, body: &Value) -> Result<reqwest::Response> {
        let mut attempt = 0_u32;
        let response = loop {
            attempt += 1;
            let mut request = self
                .client
                .post(&self.endpoint)
                .header(header::ACCEPT, "text/event-stream")
                .json(&body);
            if let Some(key) = &self.api_key {
                request = request.bearer_auth(key);
            }
            match request.send().await {
                Ok(response)
                    if attempt < 3
                        && (response.status().as_u16() == 429
                            || response.status().is_server_error()) =>
                {
                    let delay = response
                        .headers()
                        .get(header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap_or(u64::from(attempt));
                    sleep(Duration::from_secs(delay.min(10))).await;
                }
                Ok(response) => break response,
                Err(error) if attempt < 3 && (error.is_connect() || error.is_timeout()) => {
                    sleep(Duration::from_millis(300 * u64::from(attempt))).await;
                }
                Err(error) => return Err(error).context("provider request failed"),
            }
        };
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            bail!("provider returned {status}: {}", truncate_error(&detail));
        }
        Ok(response)
    }
}

fn apply_chat_delta(
    data: &str,
    content: &mut String,
    calls: &mut BTreeMap<usize, PartialToolCall>,
    deltas: &mpsc::UnboundedSender<String>,
) -> Result<()> {
    let value: Value = serde_json::from_str(data).context("invalid JSON in provider stream")?;
    if let Some(error) = value.get("error") {
        bail!("provider stream error: {error}");
    }
    let Some(delta) = value.pointer("/choices/0/delta") else {
        return Ok(());
    };

    if let Some(piece) = delta.get("content").and_then(Value::as_str) {
        content.push_str(piece);
        let _ = deltas.send(piece.to_owned());
    }

    if let Some(tool_deltas) = delta.get("tool_calls").and_then(Value::as_array) {
        for tool_delta in tool_deltas {
            let index = tool_delta.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let call = calls.entry(index).or_default();
            if let Some(id) = tool_delta.get("id").and_then(Value::as_str) {
                call.id.push_str(id);
            }
            if let Some(name) = tool_delta.pointer("/function/name").and_then(Value::as_str) {
                call.name.push_str(name);
            }
            if let Some(arguments) = tool_delta
                .pointer("/function/arguments")
                .and_then(Value::as_str)
            {
                call.arguments.push_str(arguments);
            }
        }
    }
    Ok(())
}

fn apply_responses_event(
    data: &str,
    content: &mut String,
    calls: &mut BTreeMap<usize, PartialToolCall>,
    deltas: &mpsc::UnboundedSender<String>,
) -> Result<()> {
    let value: Value = serde_json::from_str(data).context("invalid JSON in provider stream")?;
    let event_type = value["type"].as_str().unwrap_or_default();
    if event_type == "error" || event_type == "response.failed" {
        let error = value
            .pointer("/error/message")
            .or_else(|| value.pointer("/response/error/message"))
            .and_then(Value::as_str)
            .unwrap_or("unknown Responses API error");
        bail!("provider stream error: {error}");
    }

    match event_type {
        "response.output_text.delta" => {
            if let Some(piece) = value["delta"].as_str() {
                content.push_str(piece);
                let _ = deltas.send(piece.to_owned());
            }
        }
        "response.output_item.added" | "response.output_item.done"
            if value.pointer("/item/type").and_then(Value::as_str) == Some("function_call") =>
        {
            let index = value["output_index"].as_u64().unwrap_or(0) as usize;
            let call = calls.entry(index).or_default();
            if let Some(id) = value
                .pointer("/item/call_id")
                .or_else(|| value.pointer("/item/id"))
                .and_then(Value::as_str)
            {
                call.id = id.to_owned();
            }
            if let Some(name) = value.pointer("/item/name").and_then(Value::as_str) {
                call.name = name.to_owned();
            }
            if event_type == "response.output_item.done"
                && let Some(arguments) = value.pointer("/item/arguments").and_then(Value::as_str)
            {
                call.arguments = arguments.to_owned();
            }
        }
        "response.function_call_arguments.delta" => {
            let index = value["output_index"].as_u64().unwrap_or(0) as usize;
            if let Some(arguments) = value["delta"].as_str() {
                calls
                    .entry(index)
                    .or_default()
                    .arguments
                    .push_str(arguments);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Only the final usage chunk carries `usage`; the `contains` guard keeps the
/// common delta path from re-parsing JSON it has already consumed.
fn capture_usage(data: &str, total: &mut Option<u64>, parse: fn(&Value) -> Option<u64>) {
    if !data.contains("usage") {
        return;
    }
    if let Ok(value) = serde_json::from_str::<Value>(data)
        && let Some(found) = parse(&value)
    {
        *total = Some(found);
    }
}

fn parse_chat_usage(value: &Value) -> Option<u64> {
    usage_total(value.get("usage")?)
}

fn parse_responses_usage(value: &Value) -> Option<u64> {
    let usage = value
        .pointer("/response/usage")
        .or_else(|| value.get("usage"))?;
    usage_total(usage)
}

fn usage_total(usage: &Value) -> Option<u64> {
    if let Some(total) = usage.get("total_tokens").and_then(Value::as_u64) {
        return Some(total);
    }
    let input = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let sum = input + output;
    (sum > 0).then_some(sum)
}

/// Rough fallback (~4 chars/token over prompt + completion) for providers that
/// never report usage, so the running total is never stuck at zero.
fn estimate_tokens(
    messages: &[Value],
    content: &str,
    calls: &BTreeMap<usize, PartialToolCall>,
) -> u64 {
    let mut chars = content.chars().count();
    for call in calls.values() {
        chars += call.name.chars().count() + call.arguments.chars().count();
    }
    for message in messages {
        if let Some(text) = message["content"].as_str() {
            chars += text.chars().count();
        }
        if let Some(tool_calls) = message["tool_calls"].as_array() {
            for call in tool_calls {
                if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    chars += args.chars().count();
                }
            }
        }
    }
    (chars / 4) as u64
}

fn responses_input(messages: &[Value]) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        match message["role"].as_str().unwrap_or_default() {
            role @ ("system" | "user" | "assistant") => {
                if let Some(content) = message["content"].as_str()
                    && !content.is_empty()
                {
                    input.push(json!({"role": role, "content": content}));
                }
                if role == "assistant"
                    && let Some(tool_calls) = message["tool_calls"].as_array()
                {
                    for call in tool_calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": call["id"],
                            "name": call.pointer("/function/name"),
                            "arguments": call.pointer("/function/arguments")
                        }));
                    }
                }
            }
            "tool" => input.push(json!({
                "type": "function_call_output",
                "call_id": message["tool_call_id"],
                "output": message["content"]
            })),
            _ => {}
        }
    }
    input
}

fn responses_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(json!({
                "type": "function",
                "name": function["name"],
                "description": function["description"],
                "parameters": function["parameters"]
            }))
        })
        .collect()
}

fn finish_completion(
    content: String,
    calls: BTreeMap<usize, PartialToolCall>,
) -> Result<Completion> {
    let tool_calls = calls
        .into_values()
        .map(|call| {
            if call.id.is_empty() || call.name.is_empty() {
                Err(anyhow!("provider returned an incomplete tool call"))
            } else {
                Ok(ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                })
            }
        })
        .collect::<Result<Vec<_>>>()?;
    if content.is_empty() && tool_calls.is_empty() {
        bail!("provider returned an empty completion; verify model tool-calling compatibility");
    }
    Ok(Completion {
        content,
        tool_calls,
    })
}

#[derive(Debug, Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.buffer.extend_from_slice(bytes);
        self.lines(false)
    }

    fn finish(&mut self) -> Result<Vec<String>> {
        self.lines(true)
    }

    fn lines(&mut self, flush: bool) -> Result<Vec<String>> {
        let mut output = Vec::new();
        let mut consumed = 0;
        while let Some(relative) = self.buffer[consumed..]
            .iter()
            .position(|&byte| byte == b'\n')
        {
            let end = consumed + relative;
            decode_sse_line(&self.buffer[consumed..end], &mut output)?;
            consumed = end + 1;
        }
        if flush && consumed < self.buffer.len() {
            decode_sse_line(&self.buffer[consumed..], &mut output)?;
            consumed = self.buffer.len();
        }
        self.buffer.drain(..consumed);
        Ok(output)
    }
}

fn decode_sse_line(line: &[u8], output: &mut Vec<String>) -> Result<()> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let line = std::str::from_utf8(line).context("provider stream was not UTF-8")?;
    if let Some(data) = line.strip_prefix("data:") {
        output.push(data.trim_start().to_owned());
    }
    Ok(())
}

fn truncate_error(value: &str) -> String {
    const LIMIT: usize = 2_000;
    if value.len() <= LIMIT {
        return value.to_owned();
    }
    let mut boundary = LIMIT;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &value[..boundary])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_decoder_handles_split_utf8_and_lines() {
        let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"🧮\"}}]}\n\ndata: [DONE]\n";
        let bytes = raw.as_bytes();
        let split = raw.find('🧮').unwrap() + 2;
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(&bytes[..split]).unwrap().is_empty());
        let events = decoder.push(&bytes[split..]).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1], "[DONE]");
    }

    #[test]
    fn assembles_streamed_tool_arguments() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut content = String::new();
        let mut calls = BTreeMap::new();
        apply_chat_delta(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
            &mut content,
            &mut calls,
            &tx,
        ).unwrap();
        apply_chat_delta(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"src/main.rs\"}"}}]}}]}"#,
            &mut content,
            &mut calls,
            &tx,
        ).unwrap();
        assert_eq!(calls[&0].arguments, r#"{"path":"src/main.rs"}"#);
    }

    #[test]
    fn assembles_responses_api_text_and_tool_call() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut content = String::new();
        let mut calls = BTreeMap::new();
        apply_responses_event(
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
            &mut content,
            &mut calls,
            &tx,
        )
        .unwrap();
        apply_responses_event(
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_7","name":"grep","arguments":""}}"#,
            &mut content,
            &mut calls,
            &tx,
        )
        .unwrap();
        apply_responses_event(
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"query\":\"todo\"}"}"#,
            &mut content,
            &mut calls,
            &tx,
        )
        .unwrap();
        assert_eq!(rx.try_recv().unwrap(), "hello");
        assert_eq!(content, "hello");
        assert_eq!(calls[&1].id, "call_7");
        assert_eq!(calls[&1].arguments, r#"{"query":"todo"}"#);
    }

    #[test]
    fn translates_chat_history_for_responses_api() {
        let input = responses_input(&[
            json!({"role":"user","content":"find it"}),
            json!({"role":"assistant","content":null,"tool_calls":[{
                "id":"call_1","type":"function","function":{"name":"grep","arguments":"{}"}
            }]}),
            json!({"role":"tool","tool_call_id":"call_1","content":"result"}),
        ]);
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["name"], "grep");
        assert_eq!(input[1]["arguments"], "{}");
        assert_eq!(input[2]["type"], "function_call_output");
    }

    #[test]
    fn parses_reported_usage_for_both_protocols() {
        let chat = json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}});
        assert_eq!(parse_chat_usage(&chat), Some(15));
        let chat_total = json!({"usage": {"total_tokens": 99}});
        assert_eq!(parse_chat_usage(&chat_total), Some(99));
        let responses = json!({
            "type": "response.completed",
            "response": {"usage": {"input_tokens": 20, "output_tokens": 7, "total_tokens": 27}}
        });
        assert_eq!(parse_responses_usage(&responses), Some(27));
    }

    #[test]
    fn capture_usage_ignores_deltas_then_captures_final_chunk() {
        let mut total = None;
        capture_usage(
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
            &mut total,
            parse_chat_usage,
        );
        assert_eq!(total, None);
        capture_usage(
            r#"{"choices":[],"usage":{"total_tokens":42}}"#,
            &mut total,
            parse_chat_usage,
        );
        assert_eq!(total, Some(42));
    }

    #[test]
    fn estimates_tokens_when_usage_is_absent() {
        // 8 prompt chars + 4 completion chars = 12 chars, ~4 chars/token => 3.
        let messages = vec![json!({"role": "user", "content": "12345678"})];
        assert_eq!(estimate_tokens(&messages, "abcd", &BTreeMap::new()), 3);
    }
}
