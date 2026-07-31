use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Set once the endpoint rejects `max_tokens`, so later requests in the
    /// same session go straight to `max_completion_tokens`. Shared across
    /// clones for the same reason `tokens` is.
    prefers_max_completion_tokens: Arc<AtomicBool>,
    /// Prompt tokens from the most recent reply — how full the window actually
    /// was, as measured by the provider rather than estimated from characters.
    context_tokens: Arc<AtomicU64>,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub content: String,
    /// Chain-of-thought the model exposed separately from its answer
    /// (`reasoning_content` on DeepSeek R1 and Qwen thinking builds,
    /// `reasoning` elsewhere). Not shown in the transcript, but kept so a
    /// training trace can record how the answer was reached.
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    /// True when the caller cancelled mid-stream. The content and calls
    /// gathered so far are still returned — they were generated and billed, so
    /// discarding them would lose both the text and the token count.
    pub cancelled: bool,
}

impl Completion {
    /// An empty result for a request abandoned before anything came back.
    fn cancelled() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            cancelled: true,
        }
    }
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
            prefers_max_completion_tokens: Arc::new(AtomicBool::new(false)),
            context_tokens: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Approximate tokens processed so far this session.
    pub fn tokens_used(&self) -> u64 {
        self.tokens.load(Ordering::Relaxed)
    }

    fn record_tokens(
        &self,
        reported: Option<Usage>,
        messages: &[Value],
        content: &str,
        calls: &BTreeMap<usize, PartialToolCall>,
    ) {
        let tokens = reported
            .map(|usage| usage.total)
            .unwrap_or_else(|| estimate_tokens(messages, content, calls));
        if tokens > 0 {
            self.tokens.fetch_add(tokens, Ordering::Relaxed);
        }
        // The prompt count is the exact size of the context that was just sent,
        // so the window gauge can stop guessing from character counts.
        if let Some(prompt) = reported
            .map(|usage| usage.prompt)
            .filter(|count| *count > 0)
        {
            self.context_tokens.store(prompt, Ordering::Relaxed);
        }
    }

    /// The model this provider talks to.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Tokens in the last request's prompt, or 0 before the first reply or when
    /// the provider reports no usage.
    pub fn context_tokens(&self) -> u64 {
        self.context_tokens.load(Ordering::Relaxed)
    }

    pub async fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        deltas: mpsc::UnboundedSender<String>,
        cancel: &AtomicBool,
    ) -> Result<Completion> {
        match self.protocol {
            ProviderProtocol::ChatCompletions => {
                self.complete_chat(messages, tools, deltas, cancel).await
            }
            ProviderProtocol::Responses => {
                self.complete_responses(messages, tools, deltas, cancel)
                    .await
            }
        }
    }

    async fn complete_chat(
        &self,
        messages: &[Value],
        tools: &[Value],
        deltas: mpsc::UnboundedSender<String>,
        cancel: &AtomicBool,
    ) -> Result<Completion> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            // Streamed responses omit usage unless this is asked for, which is
            // why every token figure was previously a chars/4 estimate.
            "stream_options": { "include_usage": true }
        });
        // Only advertise tools when there are some. An empty `tools` array with
        // `tool_choice` is rejected by several OpenAI-compatible servers, and
        // compaction summarisation calls pass no tools at all — the resulting
        // 400 was being swallowed as "context pressure".
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }
        if let Some(max_tokens) = self.max_output_tokens {
            body[self.output_tokens_field()] = json!(max_tokens);
        }
        // The request has to be raced as well as the stream: `post_stream` waits
        // for response headers, and a server that accepts the connection then
        // stalls would otherwise hold the turn until the client timeout with no
        // way to interrupt it.
        let sent = tokio::select! {
            biased;
            sent = self.post_stream(&body) => sent,
            () = wait_for_cancel(cancel) => return Ok(Completion::cancelled()),
        };
        let response = match sent {
            Ok(response) => response,
            // OpenAI's reasoning models reject `max_tokens` outright. Rather
            // than guess from the model name — the list keeps growing — take the
            // rejection as the signal, remember it, and retry once.
            Err(error) if is_max_tokens_rejection(&error) => {
                self.prefers_max_completion_tokens
                    .store(true, Ordering::Relaxed);
                body.as_object_mut().map(|body| body.remove("max_tokens"));
                if let Some(max_tokens) = self.max_output_tokens {
                    body["max_completion_tokens"] = json!(max_tokens);
                }
                tokio::select! {
                    biased;
                    sent = self.post_stream(&body) => sent?,
                    () = wait_for_cancel(cancel) => return Ok(Completion::cancelled()),
                }
            }
            Err(error) => return Err(error),
        };

        let mut decoder = SseDecoder::default();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
        let mut text = TextStream::default();
        let mut reported_usage: Option<Usage> = None;
        let mut stream = response.bytes_stream();

        let mut cancelled = false;
        loop {
            // Raced against the stream rather than checked per chunk: a stalled
            // or very slow response would otherwise sit in `next()` and never
            // notice the flag, so an interrupt would appear to do nothing.
            let Some(chunk) = (tokio::select! {
                biased;
                chunk = stream.next() => chunk,
                () = wait_for_cancel(cancel) => {
                    cancelled = true;
                    break;
                }
            }) else {
                break;
            };
            let chunk = chunk.context("provider stream failed")?;
            for data in decoder.push(&chunk)? {
                if data != "[DONE]" {
                    apply_chat_delta(
                        &data,
                        &mut content,
                        &mut reasoning,
                        &mut calls,
                        &deltas,
                        self.tool_format,
                        &mut text,
                    )?;
                    capture_usage(&data, &mut reported_usage, parse_chat_usage);
                }
            }
        }
        for data in decoder.finish()?.into_iter().take_while(|_| !cancelled) {
            if data != "[DONE]" {
                apply_chat_delta(
                    &data,
                    &mut content,
                    &mut reasoning,
                    &mut calls,
                    &deltas,
                    self.tool_format,
                    &mut text,
                )?;
                capture_usage(&data, &mut reported_usage, parse_chat_usage);
            }
        }
        self.record_tokens(reported_usage, messages, &content, &calls);
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
        finish_completion(content, reasoning, calls, cancelled)
    }

    async fn complete_responses(
        &self,
        messages: &[Value],
        tools: &[Value],
        deltas: mpsc::UnboundedSender<String>,
        cancel: &AtomicBool,
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
        let response = tokio::select! {
            biased;
            sent = self.post_stream(&body) => sent?,
            () = wait_for_cancel(cancel) => return Ok(Completion::cancelled()),
        };
        let mut decoder = SseDecoder::default();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls: BTreeMap<usize, PartialToolCall> = BTreeMap::new();
        let mut reported_usage: Option<Usage> = None;
        let mut stream = response.bytes_stream();

        let mut cancelled = false;
        loop {
            let Some(chunk) = (tokio::select! {
                biased;
                chunk = stream.next() => chunk,
                () = wait_for_cancel(cancel) => {
                    cancelled = true;
                    break;
                }
            }) else {
                break;
            };
            let chunk = chunk.context("provider stream failed")?;
            for data in decoder.push(&chunk)? {
                if data != "[DONE]" {
                    apply_responses_event(
                        &data,
                        &mut content,
                        &mut reasoning,
                        &mut calls,
                        &deltas,
                    )?;
                    capture_usage(&data, &mut reported_usage, parse_responses_usage);
                }
            }
        }
        for data in decoder.finish()?.into_iter().take_while(|_| !cancelled) {
            if data != "[DONE]" {
                apply_responses_event(&data, &mut content, &mut reasoning, &mut calls, &deltas)?;
                capture_usage(&data, &mut reported_usage, parse_responses_usage);
            }
        }
        self.record_tokens(reported_usage, messages, &content, &calls);
        finish_completion(content, reasoning, calls, cancelled)
    }

    /// Which output-cap parameter this endpoint accepts. Starts optimistic and
    /// is flipped by the first rejection (see `complete`).
    fn output_tokens_field(&self) -> &'static str {
        if self.prefers_max_completion_tokens.load(Ordering::Relaxed) {
            "max_completion_tokens"
        } else {
            "max_tokens"
        }
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

/// How much of the assistant's text has been forwarded to the UI, and whether
/// forwarding has stopped because tool markup began.
#[derive(Default)]
struct TextStream {
    emitted: usize,
    suppressed: bool,
}

fn apply_chat_delta(
    data: &str,
    content: &mut String,
    reasoning: &mut String,
    calls: &mut BTreeMap<usize, PartialToolCall>,
    deltas: &mpsc::UnboundedSender<String>,
    format: ToolFormat,
    stream: &mut TextStream,
) -> Result<()> {
    let value: Value = serde_json::from_str(data).context("invalid JSON in provider stream")?;
    if let Some(error) = value.get("error") {
        bail!("provider stream error: {error}");
    }
    let Some(delta) = value.pointer("/choices/0/delta") else {
        return Ok(());
    };

    // Providers put the model's private reasoning in a sibling field. It is
    // deliberately not forwarded to the transcript — only recorded.
    if let Some(piece) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .and_then(Value::as_str)
    {
        reasoning.push_str(piece);
    }
    if let Some(piece) = delta.get("content").and_then(Value::as_str) {
        content.push_str(piece);
        // Forward prose, but stop at the first tool-call marker: the markup is
        // stripped only after the stream ends, so without this the user watches
        // it scroll past and the transcript diverges from the saved history.
        if !stream.suppressed {
            let cut = match tool_format::marker_index(format, content) {
                Some(index) => {
                    stream.suppressed = true;
                    index
                }
                None => content.len(),
            };
            if cut > stream.emitted {
                let _ = deltas.send(content[stream.emitted..cut].to_owned());
                stream.emitted = cut;
            }
        }
    }

    if let Some(tool_deltas) = delta.get("tool_calls").and_then(Value::as_array) {
        for tool_delta in tool_deltas {
            let id = tool_delta.get("id").and_then(Value::as_str);
            // `index` is optional outside OpenAI itself. Defaulting a missing
            // one to 0 merged unrelated calls into a single slot, concatenating
            // their names and producing `{"a":1}{"b":2}` as arguments. Fall back
            // to "the call we are already building, unless this delta announces
            // a new id", which is how servers without `index` signal a new call.
            let index = match tool_delta.get("index").and_then(Value::as_u64) {
                Some(index) => index as usize,
                None => match calls.iter().next_back() {
                    Some((last, call)) if id.is_none_or(|id| id == call.id) => *last,
                    Some((last, _)) => last + 1,
                    None => 0,
                },
            };
            let call = calls.entry(index).or_default();
            if let Some(id) = id {
                absorb(&mut call.id, id);
            }
            if let Some(name) = tool_delta.pointer("/function/name").and_then(Value::as_str) {
                absorb(&mut call.name, name);
            }
            // Arguments are the one field genuinely streamed in fragments, so
            // they always accumulate.
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

/// Resolves once `cancel` is raised. Polled rather than notified because the
/// flag is a plain `AtomicBool` shared with synchronous UI code; 50ms is well
/// under the threshold where a stop feels unresponsive.
async fn wait_for_cancel(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        sleep(Duration::from_millis(50)).await;
    }
}

/// Whether a provider error is the "use max_completion_tokens instead" 400 that
/// OpenAI's reasoning models return.
fn is_max_tokens_rejection(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("max_completion_tokens") && text.contains("max_tokens")
}

/// Merge a tool call's `id` or `name` fragment into what has been seen so far.
///
/// The spec streams these once, but plenty of OpenAI-compatible servers repeat
/// the full value on every delta for the same call. Blindly appending turned
/// `read_file` into `read_fileread_fileread_file`, which then dispatched as an
/// unknown tool. Repeats are therefore idempotent while genuine fragments still
/// accumulate — the only case this gets wrong is a fragment identical to what
/// precedes it, which no real tool name produces.
fn absorb(field: &mut String, piece: &str) {
    if field != piece {
        field.push_str(piece);
    }
}

fn apply_responses_event(
    data: &str,
    content: &mut String,
    reasoning: &mut String,
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
        // Reasoning summaries arrive on their own event, never mixed into the
        // answer text, so recording them costs nothing at the transcript's end.
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(piece) = value["delta"].as_str() {
                reasoning.push_str(piece);
            }
        }
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
/// What a provider reported for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Prompt plus completion, added to the running session total.
    pub total: u64,
    /// Prompt alone — the exact size of the context that was sent, which is a
    /// far better "how full is the window" figure than counting characters.
    pub prompt: u64,
}

fn capture_usage(data: &str, usage: &mut Option<Usage>, parse: fn(&Value) -> Option<Usage>) {
    if !data.contains("usage") {
        return;
    }
    if let Ok(value) = serde_json::from_str::<Value>(data)
        && let Some(found) = parse(&value)
    {
        *usage = Some(found);
    }
}

fn parse_chat_usage(value: &Value) -> Option<Usage> {
    usage_total(value.get("usage")?)
}

fn parse_responses_usage(value: &Value) -> Option<Usage> {
    let usage = value
        .pointer("/response/usage")
        .or_else(|| value.get("usage"))?;
    usage_total(usage)
}

fn usage_total(usage: &Value) -> Option<Usage> {
    let prompt = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(prompt + completion);
    if total == 0 && prompt == 0 {
        return None;
    }
    Some(Usage { total, prompt })
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
    reasoning: String,
    calls: BTreeMap<usize, PartialToolCall>,
    cancelled: bool,
) -> Result<Completion> {
    // An incomplete entry is dropped rather than failing the turn. Two things
    // produce them routinely: a bare `{"index":1}` sentinel some servers emit,
    // and cancelling mid-stream. Erroring threw away the streamed content and
    // every well-formed call alongside the bad one.
    let seen = calls.len();
    let tool_calls = calls
        .into_values()
        .filter(|call| !call.id.is_empty() && !call.name.is_empty())
        .map(|call| ToolCall {
            id: call.id,
            name: call.name,
            arguments: call.arguments,
        })
        .collect::<Vec<_>>();
    // Only a completion that is *entirely* rubble is worth failing on: nothing
    // usable came back and something was clearly malformed.
    if !cancelled && tool_calls.is_empty() && seen > 0 && content.is_empty() {
        return Err(anyhow!("provider returned an incomplete tool call"));
    }
    // Don't bail on an empty completion. An empty stream happens for benign
    // reasons (post-compaction empty request, transient stream hiccup, model
    // that emits a final empty chunk) and a tool-compatible model is not at
    // fault. Return an empty Completion so the agent's loop can end the turn
    // cleanly instead of firing a misleading "verify model tool-calling
    // compatibility" error that kills the session.
    Ok(Completion {
        content,
        reasoning,
        tool_calls,
        cancelled,
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

    /// Drive a sequence of Chat-Completions delta payloads through the parser.
    fn chat_deltas(payloads: &[&str]) -> (String, BTreeMap<usize, PartialToolCall>, Vec<String>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls = BTreeMap::new();
        let mut text = TextStream::default();
        for payload in payloads {
            apply_chat_delta(
                payload,
                &mut content,
                &mut reasoning,
                &mut calls,
                &tx,
                ToolFormat::None,
                &mut text,
            )
            .unwrap();
        }
        drop(tx);
        let mut seen = Vec::new();
        while let Ok(piece) = rx.try_recv() {
            seen.push(piece);
        }
        (content, calls, seen)
    }

    #[test]
    fn assembles_streamed_tool_arguments() {
        let (_, calls, _) = chat_deltas(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"src/main.rs\"}"}}]}}]}"#,
        ]);
        assert_eq!(calls[&0].arguments, r#"{"path":"src/main.rs"}"#);
    }

    /// Several OpenAI-compatible servers repeat the full `id` and
    /// `function.name` on every delta for the same call. Appending them turned
    /// `read_file` into `read_fileread_file`, which dispatched as an unknown
    /// tool.
    #[test]
    fn a_repeated_tool_name_is_not_concatenated() {
        let (_, calls, _) = chat_deltas(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"\"path\":\"a\"}"}}]}}]}"#,
        ]);
        assert_eq!(calls[&0].name, "read_file");
        assert_eq!(calls[&0].id, "call_1");
        assert_eq!(calls[&0].arguments, r#"{"path":"a"}"#);
    }

    /// A name genuinely split across deltas must still accumulate.
    #[test]
    fn a_fragmented_tool_name_still_accumulates() {
        let (_, calls, _) = chat_deltas(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"file"}}]}}]}"#,
        ]);
        assert_eq!(calls[&0].name, "read_file");
    }

    /// Without `index`, a new `id` starts a new call. Defaulting to slot 0
    /// merged unrelated calls and concatenated their argument JSON.
    #[test]
    fn calls_without_an_index_are_kept_apart() {
        let (_, calls, _) = chat_deltas(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"a","function":{"name":"read_file","arguments":"{\"path\":\"x\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"b","function":{"name":"grep","arguments":"{\"query\":\"y\"}"}}]}}]}"#,
        ]);
        assert_eq!(calls.len(), 2, "two ids must not share a slot");
        assert_eq!(calls[&0].name, "read_file");
        assert_eq!(calls[&1].name, "grep");
        assert_eq!(calls[&1].arguments, r#"{"query":"y"}"#);
    }

    /// Tool markup must not reach the transcript: it is stripped only after the
    /// stream ends, so anything forwarded before then is never taken back.
    #[test]
    fn tool_markup_is_not_streamed_to_the_transcript() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls = BTreeMap::new();
        let mut text = TextStream::default();
        for piece in [
            "Let me read it.",
            "\n<\x74ool_call>",
            r#"{"name":"read_file"}"#,
        ] {
            let payload = json!({"choices":[{"delta":{"content": piece}}]}).to_string();
            apply_chat_delta(
                &payload,
                &mut content,
                &mut reasoning,
                &mut calls,
                &tx,
                ToolFormat::Hermes,
                &mut text,
            )
            .unwrap();
        }
        drop(tx);
        let mut seen = String::new();
        while let Ok(piece) = rx.try_recv() {
            seen.push_str(&piece);
        }
        assert_eq!(seen, "Let me read it.\n");
        assert!(content.contains("read_file"), "history keeps the raw text");
    }

    #[test]
    fn assembles_responses_api_text_and_tool_call() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls = BTreeMap::new();
        apply_responses_event(
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
            &mut content,
            &mut reasoning,
            &mut calls,
            &tx,
        )
        .unwrap();
        apply_responses_event(
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_7","name":"grep","arguments":""}}"#,
            &mut content,
            &mut reasoning,
            &mut calls,
            &tx,
        )
        .unwrap();
        apply_responses_event(
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"query\":\"todo\"}"}"#,
            &mut content,
            &mut reasoning,
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
        assert_eq!(
            parse_chat_usage(&chat),
            Some(Usage {
                total: 15,
                prompt: 10
            })
        );
        // A total with no breakdown still gives a usable session count, and the
        // context gauge simply keeps its previous value.
        let chat_total = json!({"usage": {"total_tokens": 99}});
        assert_eq!(
            parse_chat_usage(&chat_total),
            Some(Usage {
                total: 99,
                prompt: 0
            })
        );
        let responses = json!({
            "type": "response.completed",
            "response": {"usage": {"input_tokens": 20, "output_tokens": 7, "total_tokens": 27}}
        });
        assert_eq!(
            parse_responses_usage(&responses),
            Some(Usage {
                total: 27,
                prompt: 20
            })
        );
    }

    #[test]
    fn capture_usage_ignores_deltas_then_captures_final_chunk() {
        let mut usage = None;
        capture_usage(
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
            &mut usage,
            parse_chat_usage,
        );
        assert_eq!(usage, None);
        capture_usage(
            r#"{"choices":[],"usage":{"total_tokens":42,"prompt_tokens":30}}"#,
            &mut usage,
            parse_chat_usage,
        );
        assert_eq!(
            usage,
            Some(Usage {
                total: 42,
                prompt: 30
            })
        );
    }

    #[test]
    fn estimates_tokens_when_usage_is_absent() {
        // 8 prompt chars + 4 completion chars = 12 chars, ~4 chars/token => 3.
        let messages = vec![json!({"role": "user", "content": "12345678"})];
        assert_eq!(estimate_tokens(&messages, "abcd", &BTreeMap::new()), 3);
    }
}
