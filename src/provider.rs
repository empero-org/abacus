use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::{Client, header};
use serde_json::{Value, json};
use tokio::{
    sync::{Semaphore, mpsc},
    time::sleep,
};

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
    /// Output-token ceiling learned from a provider rejection ("supports at
    /// most N completion tokens"), 0 when none. Detection can over-report —
    /// an upstream echoing its context window as the completion cap — and the
    /// rejection is the authoritative correction, so it is remembered for the
    /// rest of the session. Shared across clones.
    learned_output_cap: Arc<AtomicU64>,
    /// Set once the endpoint rejects `reasoning_content` in input messages.
    /// History stores it because some providers *require* prior reasoning
    /// passed back (Kimi thinking, GLM reasoning deployments) and stall
    /// without it; ones that reject it instead (DeepSeek first-party) teach
    /// us here and get it stripped from then on. Shared across clones.
    strips_reasoning: Arc<AtomicBool>,
    /// Upstream provider pins, sent only to endpoints that understand them.
    routing: crate::config::Routing,
    /// Prompt tokens from the most recent reply — how full the window actually
    /// was, as measured by the provider rather than estimated from characters.
    context_tokens: Arc<AtomicU64>,
    /// A custom endpoint definition (URL already folded into `endpoint`): its
    /// auth, extra headers, and body overrides are applied per request.
    scripted: Option<crate::endpoint::ScriptedEndpoint>,
    /// `max_tokens` for the Anthropic protocol, which requires it and has no
    /// default. The configured/detected cap when set, else a safe fallback.
    anthropic_max_tokens: usize,
    /// A per-session random id for `{uuid}` header substitution.
    session_id: String,
    /// Reasoning effort, sent in whatever shape this protocol expects.
    reasoning_effort: Option<crate::config::ReasoningEffort>,
    /// Set once the endpoint rejects manual extended thinking
    /// (`thinking.type: "enabled"` with `budget_tokens`), so the rest of the
    /// session goes straight to adaptive thinking + `output_config.effort`.
    /// Claude 4.7 and later 400 on the manual shape; the family check below
    /// catches the known ones, and this catches everything it does not.
    /// Shared across clones for the same reason `learned_output_cap` is.
    prefers_adaptive_thinking: Arc<AtomicBool>,
    /// Optional one-request gate shared by the main and auxiliary providers.
    stream_gate: Option<Arc<Semaphore>>,
}

/// A piece of streamed output. Reasoning is kept separate from the answer all
/// the way to the UI so it can be styled — or withheld — without the renderer
/// having to guess which is which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    Text(String),
    Reasoning(String),
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub content: String,
    /// Chain-of-thought the model exposed separately from its answer
    /// (`reasoning_content` on DeepSeek R1 and Qwen thinking builds,
    /// `reasoning` elsewhere). Streamed to the UI as [`Chunk::Reasoning`] so it
    /// can be shown or withheld, and kept here so a training trace can record
    /// how the answer was reached.
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    /// True when the caller cancelled mid-stream. The content and calls
    /// gathered so far are still returned — they were generated and billed, so
    /// discarding them would lose both the text and the token count.
    pub cancelled: bool,
    /// True when the stream ended with `finish_reason: "length"` — the reply
    /// hit the output-token ceiling and was cut mid-thought. Callers surface
    /// this; silently keeping half an answer reads as the model trailing off.
    pub truncated: bool,
}

impl Completion {
    /// An empty result for a request abandoned before anything came back.
    fn cancelled() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            cancelled: true,
            truncated: false,
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
            // Bound INACTIVITY, not total runtime. A total timeout is wrong for
            // a streaming completion: generation time scales with output length
            // and model speed, so any fixed ceiling eventually cuts off a
            // legitimate long answer mid-stream (a 13B on CPU emits ~4 tok/s, so
            // a single-file HTML app is 15+ minutes and blew the old 600s cap
            // with the plan already streamed). read_timeout fires only when no
            // byte arrives for the whole window, which still catches a genuinely
            // dead connection or a wedged server -- including on non-streaming
            // requests -- without punishing slow-but-alive ones.
            .read_timeout(Duration::from_secs(180))
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
            routing: config.routing.clone(),
            prefers_max_completion_tokens: Arc::new(AtomicBool::new(false)),
            learned_output_cap: Arc::new(AtomicU64::new(0)),
            strips_reasoning: Arc::new(AtomicBool::new(false)),
            context_tokens: Arc::new(AtomicU64::new(0)),
            scripted: config.endpoint.clone(),
            // A configured/detected cap when there is one, else a value that is
            // safe on every current Claude model without a beta output header.
            anthropic_max_tokens: config
                .model_limits
                .configured_output_tokens
                .unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS)
                .clamp(1, DEFAULT_ANTHROPIC_MAX_TOKENS),
            session_id: uuid::Uuid::new_v4().simple().to_string(),
            reasoning_effort: config.reasoning_effort,
            prefers_adaptive_thinking: Arc::new(AtomicBool::new(false)),
            stream_gate: config.one_stream.then(|| Arc::new(Semaphore::new(1))),
        })
    }

    /// The message list as this endpoint accepts it: verbatim, unless it has
    /// rejected `reasoning_content` before, in which case the field is removed.
    fn sanitized_messages(&self, messages: &[Value]) -> Vec<Value> {
        if !self.strips_reasoning.load(Ordering::Relaxed) {
            return messages.to_vec();
        }
        strip_reasoning_content(messages)
    }

    /// The output ceiling to actually send: the configured/detected value,
    /// clamped by anything a provider rejection has taught us this session.
    fn effective_output_tokens(&self) -> Option<usize> {
        let configured = self.max_output_tokens?;
        match self.learned_output_cap.load(Ordering::Relaxed) {
            0 => Some(configured),
            cap => Some(configured.min(cap as usize)),
        }
    }

    /// Approximate tokens processed so far this session.
    pub fn tokens_used(&self) -> u64 {
        self.tokens.load(Ordering::Relaxed)
    }

    /// A clone that counts into its own fresh counter — for a subagent whose
    /// usage should be visible per worker. The caller folds the counter back
    /// into the session total when the worker finishes.
    pub fn with_detached_counter(&self) -> (Self, Arc<AtomicU64>) {
        let counter = Arc::new(AtomicU64::new(0));
        let mut detached = self.clone();
        detached.tokens = counter.clone();
        // Explicit subagents are the sole exception to One Stream.
        detached.stream_gate = None;
        (detached, counter)
    }

    /// Fold a finished worker's usage into this provider's running total.
    pub fn add_tokens(&self, tokens: u64) {
        if tokens > 0 {
            self.tokens.fetch_add(tokens, Ordering::Relaxed);
        }
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

    /// A sibling provider that differs only in the model — same endpoint,
    /// auth, protocol, and scripted config. For the auxiliary model used by
    /// secondary calls (refine, drafts, tether, compaction). The billing
    /// counter is shared so aux calls count toward the session total; the
    /// context-size gauge is fresh so an aux call over the whole history does
    /// not overwrite the main conversation's "window full" figure.
    pub fn with_model(&self, model: &str) -> Provider {
        Provider {
            model: model.to_owned(),
            context_tokens: Arc::new(AtomicU64::new(0)),
            ..self.clone()
        }
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
        deltas: mpsc::UnboundedSender<Chunk>,
        cancel: &AtomicBool,
    ) -> Result<Completion> {
        let _permit = match &self.stream_gate {
            Some(gate) => Some(gate.acquire().await.context("stream gate closed")?),
            None => None,
        };
        match self.protocol {
            ProviderProtocol::ChatCompletions => {
                self.complete_chat(messages, tools, deltas, cancel).await
            }
            ProviderProtocol::Responses => {
                self.complete_responses(messages, tools, deltas, cancel)
                    .await
            }
            ProviderProtocol::Anthropic => {
                self.complete_anthropic(messages, tools, deltas, cancel)
                    .await
            }
        }
    }

    async fn complete_chat(
        &self,
        messages: &[Value],
        tools: &[Value],
        deltas: mpsc::UnboundedSender<Chunk>,
        cancel: &AtomicBool,
    ) -> Result<Completion> {
        let mut body = json!({
            "model": self.model,
            "messages": self.sanitized_messages(messages),
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
        if let Some(max_tokens) = self.effective_output_tokens() {
            body[self.output_tokens_field()] = json!(max_tokens);
        }
        if let Some(effort) = self.reasoning_effort {
            body["reasoning_effort"] = json!(effort.openai_label());
        }
        // Routing is an OpenRouter extension. Sending it to an endpoint that
        // does not know the field risks a 400 from the stricter servers, and it
        // would mean nothing to the ones that merely ignore it.
        if self.routes_upstream()
            && let Some(provider) = self.routing.body()
        {
            body["provider"] = provider;
        }
        // Scripted overrides win over everything Abacus put in the body, and
        // its removals fire last — a required `store: false` or a rejected
        // `parallel_tool_calls` is honoured no matter what was built above.
        self.apply_scripted_body(&mut body);
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
                if let Some(max_tokens) = self.effective_output_tokens() {
                    body["max_completion_tokens"] = json!(max_tokens);
                }
                tokio::select! {
                    biased;
                    sent = self.post_stream(&body) => sent?,
                    () = wait_for_cancel(cancel) => return Ok(Completion::cancelled()),
                }
            }
            // Some endpoints reject `reasoning_content` in input messages
            // (history stores it because other endpoints *require* it).
            // Strip and retry; the preference sticks for the session.
            Err(error)
                if format!("{error:#}").contains("reasoning_content")
                    && !self.strips_reasoning.load(Ordering::Relaxed) =>
            {
                self.strips_reasoning.store(true, Ordering::Relaxed);
                body["messages"] = json!(strip_reasoning_content(messages));
                tokio::select! {
                    biased;
                    sent = self.post_stream(&body) => sent?,
                    () = wait_for_cancel(cancel) => return Ok(Completion::cancelled()),
                }
            }
            // "max_tokens is too large: N. This model supports at most M…" —
            // detection over-reported (an upstream echoing its context window
            // as the completion cap is common on aggregator endpoints). The
            // rejection carries the real ceiling, so learn it, clamp, and
            // retry once; the cap sticks for the rest of the session.
            Err(error) if rejected_output_cap(&error, self.effective_output_tokens()).is_some() => {
                let cap = rejected_output_cap(&error, self.effective_output_tokens())
                    .expect("guard checked");
                self.learned_output_cap.store(cap as u64, Ordering::Relaxed);
                body[self.output_tokens_field()] = json!(cap);
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
        let mut truncated = false;
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
                    truncated |= chunk_hit_length_limit(&data);
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
                truncated |= chunk_hit_length_limit(&data);
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
        finish_completion(content, reasoning, calls, cancelled, truncated)
    }

    async fn complete_responses(
        &self,
        messages: &[Value],
        tools: &[Value],
        deltas: mpsc::UnboundedSender<Chunk>,
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
        if let Some(max_tokens) = self.effective_output_tokens() {
            body["max_output_tokens"] = json!(max_tokens);
        }
        if let Some(effort) = self.reasoning_effort {
            body["reasoning"] = json!({"effort": effort.openai_label()});
        }
        self.apply_scripted_body(&mut body);
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
        finish_completion(content, reasoning, calls, cancelled, false)
    }

    async fn complete_anthropic(
        &self,
        messages: &[Value],
        tools: &[Value],
        deltas: mpsc::UnboundedSender<Chunk>,
        cancel: &AtomicBool,
    ) -> Result<Completion> {
        let system_prefix = self
            .scripted
            .as_ref()
            .and_then(|scripted| scripted.system_prefix.as_deref());
        let (system, converted) = anthropic_messages(messages, system_prefix);
        let mut body = json!({
            "model": self.model,
            // Required, no default; scripted body may override it.
            "max_tokens": self.effective_output_tokens().unwrap_or(self.anthropic_max_tokens),
            "system": system,
            "messages": converted,
            "stream": true,
        });
        if !tools.is_empty() {
            body["tools"] = json!(anthropic_tools(tools));
            body["tool_choice"] = json!({"type": "auto"});
        }
        if let Some(effort) = self.reasoning_effort {
            self.apply_anthropic_thinking(&mut body, effort);
        }
        self.apply_scripted_body(&mut body);

        let response = tokio::select! {
            biased;
            sent = self.post_stream(&body) => sent,
            () = wait_for_cancel(cancel) => return Ok(Completion::cancelled()),
        };
        // Anthropic requires `max_tokens` and has no `max_completion_tokens`
        // variant, so a ceiling rejection is the authoritative source of the
        // model's real output cap: "max_tokens: 393216 > 128000, which is the
        // maximum allowed number of output tokens for claude-opus-4-8". A
        // detected or leftover configured cap above the ceiling must not kill
        // the session — learn the real cap, clamp, and retry once, exactly like
        // the chat path does, and remember it for the rest of the session.
        let response = match response {
            Ok(response) => response,
            // Claude 4.7+ reject manual extended thinking outright. The family
            // check above catches the models we know; this catches the ones we
            // do not — learn it, rewrite the request as adaptive, and retry
            // once, so a new model is a single wasted call rather than a dead
            // turn for the rest of the session.
            Err(error) if is_manual_thinking_rejection(&error) => {
                self.prefers_adaptive_thinking
                    .store(true, Ordering::Relaxed);
                if let Some(effort) = self.reasoning_effort {
                    self.apply_anthropic_thinking(&mut body, effort);
                    self.apply_scripted_body(&mut body);
                }
                tokio::select! {
                    biased;
                    sent = self.post_stream(&body) => sent?,
                    () = wait_for_cancel(cancel) => return Ok(Completion::cancelled()),
                }
            }
            Err(error) if rejected_output_cap(&error, self.effective_output_tokens()).is_some() => {
                let cap = rejected_output_cap(&error, self.effective_output_tokens())
                    .expect("guard checked");
                self.learned_output_cap.store(cap as u64, Ordering::Relaxed);
                body["max_tokens"] = json!(cap);
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
        let mut reported_usage: Option<Usage> = None;
        let mut truncated = false;
        let mut text = TextStream::default();
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
                apply_anthropic_event(
                    &data,
                    &mut content,
                    &mut reasoning,
                    &mut calls,
                    &deltas,
                    self.tool_format,
                    &mut text,
                    &mut reported_usage,
                    &mut truncated,
                )?;
            }
        }
        for data in decoder.finish()?.into_iter().take_while(|_| !cancelled) {
            apply_anthropic_event(
                &data,
                &mut content,
                &mut reasoning,
                &mut calls,
                &deltas,
                self.tool_format,
                &mut text,
                &mut reported_usage,
                &mut truncated,
            )?;
        }
        self.record_tokens(reported_usage, messages, &content, &calls);
        finish_completion(content, reasoning, calls, cancelled, truncated)
    }

    /// Whether this endpoint accepts upstream provider routing.
    ///
    /// Matched on the host rather than a config flag so a profile pointed at an
    /// OpenRouter-compatible gateway behaves the same way without extra setup,
    /// and a pin silently does nothing rather than breaking a plain endpoint.
    fn routes_upstream(&self) -> bool {
        self.endpoint.contains("openrouter.ai")
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

    /// Write the thinking configuration for an Anthropic request.
    ///
    /// Two incompatible shapes exist. Manual extended thinking
    /// (`{type: "enabled", budget_tokens: N}`) is the only mode on Claude 4.5
    /// and earlier; it is deprecated on 4.6 and **rejected with a 400** on 4.7
    /// and later. Adaptive thinking (`{type: "adaptive"}` plus
    /// `output_config: {effort: ...}`) is the replacement and the recommended
    /// control wherever it exists. Newer models are the ones we will keep
    /// meeting, so adaptive is the default and the budget is the fallback for
    /// families known to predate it.
    fn apply_anthropic_thinking(&self, body: &mut Value, effort: crate::config::ReasoningEffort) {
        if self.uses_adaptive_thinking() {
            // Effort governs all token spend — thinking, text, and tool calls —
            // so it applies even at minimal, where thinking itself stays off.
            body["output_config"] = json!({"effort": effort.anthropic_effort()});
            body["thinking"] = match effort {
                crate::config::ReasoningEffort::Minimal => json!({"type": "disabled"}),
                _ => json!({"type": "adaptive"}),
            };
            return;
        }
        // Manual mode takes a *budget*, and the budget has to fit under
        // max_tokens with room for an actual answer — so the cap is raised to
        // fit rather than the budget silently exceeding it.
        if let Some(budget) = effort.thinking_budget() {
            let max_tokens = body["max_tokens"].as_u64().unwrap_or(0) as usize;
            if max_tokens <= budget {
                body["max_tokens"] = json!(budget + ANTHROPIC_ANSWER_HEADROOM);
            }
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        }
    }

    /// Whether this model takes adaptive thinking + `output_config.effort`
    /// rather than a manual token budget. True once a rejection has taught us
    /// so, and by default for every model outside the known legacy families —
    /// guessing "new" is the safe direction, since the manual shape is a hard
    /// 400 on current models while adaptive is what the rest accept.
    fn uses_adaptive_thinking(&self) -> bool {
        self.prefers_adaptive_thinking.load(Ordering::Relaxed)
            || !is_legacy_thinking_model(&self.model)
    }

    /// Fold in a scripted endpoint's body overrides and removals, if any.
    fn apply_scripted_body(&self, body: &mut Value) {
        if let Some(scripted) = &self.scripted {
            scripted.apply_to_body(body);
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
            // The Messages API requires a version header on every request.
            if self.protocol == ProviderProtocol::Anthropic {
                request = request.header("anthropic-version", "2023-06-01");
            }
            let mut scripted_auth = false;
            if let Some(scripted) = &self.scripted {
                for (name, value) in scripted.resolved_headers(&self.session_id) {
                    request = request.header(name.as_str(), value);
                }
                match scripted.auth_header() {
                    Ok(Some((name, value))) => {
                        request = request.header(name.as_str(), value);
                        scripted_auth = true;
                    }
                    Ok(None) => {}
                    Err(error) => return Err(error).context("scripted endpoint auth"),
                }
            }
            if !scripted_auth && let Some(key) = &self.api_key {
                // Anthropic authenticates an API key with `x-api-key`, not a
                // bearer; a scripted OAuth endpoint provides its own header.
                if self.protocol == ProviderProtocol::Anthropic {
                    request = request.header("x-api-key", key.as_str());
                } else {
                    request = request.bearer_auth(key);
                }
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
    deltas: &mpsc::UnboundedSender<Chunk>,
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
        let _ = deltas.send(Chunk::Reasoning(piece.to_owned()));
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
                let _ = deltas.send(Chunk::Text(content[stream.emitted..cut].to_owned()));
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

/// Whether a streamed chunk reports the reply was cut by the output-token
/// ceiling (`finish_reason: "length"`).
fn chunk_hit_length_limit(data: &str) -> bool {
    serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|value| {
            value.get("choices")?.as_array()?.iter().find_map(|choice| {
                (choice.get("finish_reason")?.as_str()? == "length").then_some(true)
            })
        })
        .unwrap_or(false)
}

/// Remove `reasoning_content` from every message, for endpoints that reject
/// it in the input.
fn strip_reasoning_content(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let mut message = message.clone();
            if let Some(object) = message.as_object_mut() {
                object.remove("reasoning_content");
            }
            message
        })
        .collect()
}

/// Whether a provider error is Anthropic's rejection of manual extended
/// thinking. Claude 4.7 and later return a 400 whose message starts with
/// `"thinking.type.enabled" is not supported`; the model then wants adaptive
/// thinking instead. Matched loosely so a reworded message still lands.
fn is_manual_thinking_rejection(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("thinking.type.enabled") && text.contains("not supported")
}

/// Whether a Claude model predates adaptive thinking and so takes a manual
/// `budget_tokens` instead. Adaptive arrived with the 4.6 generation, so the
/// legacy set is closed and will not grow: Claude 4.5 and earlier, plus the
/// Claude 3 family. Matching the closed old set rather than the open new one
/// is what keeps this from going stale as models ship — the same reasoning as
/// the family table in `model_info`.
fn is_legacy_thinking_model(model: &str) -> bool {
    let name = model.to_ascii_lowercase();
    if !name.contains("claude") {
        return false;
    }
    // Claude 3.x, and the 4.0–4.5 generation in either naming style
    // (`claude-opus-4-5`, `claude-3-7-sonnet`, `claude-sonnet-4`).
    const LEGACY: [&str; 12] = [
        "claude-3", "claude-4", "-3-5-", "-3-7-", "-4-0", "-4-1", "-4-5", "opus-4", "sonnet-4",
        "haiku-4", "-4-20", "-4.5",
    ];
    // A 4.6-or-later marker wins outright: `claude-opus-4-6` contains
    // `opus-4`, but it is emphatically not legacy.
    const MODERN: [&str; 8] = [
        "-4-6", "-4-7", "-4-8", "-4-9", "-4.6", "-4.7", "-4.8", "-4.9",
    ];
    if MODERN.iter().any(|marker| name.contains(marker)) {
        return false;
    }
    LEGACY.iter().any(|marker| name.contains(marker))
}

/// Whether a provider error is the "use max_completion_tokens instead" 400 that
/// OpenAI's reasoning models return.
fn is_max_tokens_rejection(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("max_completion_tokens") && text.contains("max_tokens")
}

/// Extract the real output ceiling from a "max_tokens is too large" rejection.
///
/// Message phrasing varies by server — "supports at most 131072 completion
/// tokens", "must be less than or equal to 65536", "maximum value is 32768" —
/// so rather than pattern-match each one, the error must (a) be about the
/// output-token parameter and a limit, and (b) contain a plausible cap:
/// the largest number that is at least 1024 and strictly below what was sent.
/// Numbers under 1024 (status codes, model-name fragments) never qualify.
fn rejected_output_cap(error: &anyhow::Error, sent: Option<usize>) -> Option<usize> {
    let sent = sent?;
    let text = format!("{error:#}").to_ascii_lowercase();
    let about_output = text.contains("max_tokens")
        || text.contains("max_output_tokens")
        || text.contains("completion tokens")
        || text.contains("output tokens");
    let about_limit = text.contains("too large")
        || text.contains("at most")
        || text.contains("less than or equal")
        || text.contains("maximum")
        || text.contains("exceed");
    if !about_output || !about_limit {
        return None;
    }
    let mut best: Option<usize> = None;
    let mut current = 0_usize;
    let mut in_number = false;
    for ch in text.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() {
            current = current.saturating_mul(10) + (ch as usize - '0' as usize);
            in_number = true;
        } else if in_number {
            if (1_024..sent).contains(&current) && best.is_none_or(|value| current > value) {
                best = Some(current);
            }
            current = 0;
            in_number = false;
        }
    }
    best
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
    deltas: &mpsc::UnboundedSender<Chunk>,
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
                let _ = deltas.send(Chunk::Reasoning(piece.to_owned()));
            }
        }
        "response.output_text.delta" => {
            if let Some(piece) = value["delta"].as_str() {
                content.push_str(piece);
                let _ = deltas.send(Chunk::Text(piece.to_owned()));
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

/// Tokens left for the answer itself above an Anthropic thinking budget.
const ANTHROPIC_ANSWER_HEADROOM: usize = 8_192;

/// `max_tokens` sent to the Anthropic Messages API when none is configured —
/// the field is required and safe on every current Claude model without a beta
/// output-length header.
const DEFAULT_ANTHROPIC_MAX_TOKENS: usize = 32_000;

/// Translate Abacus's OpenAI-shaped history into the Anthropic Messages
/// request: system text blocks (billing/prefix first) and a `messages` array
/// of content blocks. Tool calls become `tool_use` blocks and tool results
/// become `tool_result` blocks in a following user turn; consecutive same-role
/// turns are coalesced so the result alternates as the API requires.
fn anthropic_messages(messages: &[Value], system_prefix: Option<&str>) -> (Vec<Value>, Vec<Value>) {
    let mut system: Vec<Value> = Vec::new();
    if let Some(prefix) = system_prefix.filter(|prefix| !prefix.trim().is_empty()) {
        system.push(json!({"type": "text", "text": prefix}));
    }
    // (role, content-blocks) pairs, coalescing consecutive same-role turns.
    let mut turns: Vec<(String, Vec<Value>)> = Vec::new();
    let mut push = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        match turns.last_mut() {
            Some((last, existing)) if last == role => existing.extend(blocks),
            _ => turns.push((role.to_owned(), blocks)),
        }
    };

    for message in messages {
        match message["role"].as_str().unwrap_or_default() {
            "system" => {
                if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
                    system.push(json!({"type": "text", "text": text}));
                }
            }
            "user" => push("user", anthropic_content_blocks(&message["content"])),
            "assistant" => {
                let mut blocks = anthropic_content_blocks(&message["content"]);
                if let Some(tool_calls) = message["tool_calls"].as_array() {
                    for call in tool_calls {
                        let arguments = call
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call["id"],
                            "name": call.pointer("/function/name"),
                            "input": serde_json::from_str::<Value>(arguments)
                                .unwrap_or_else(|_| json!({})),
                        }));
                    }
                }
                push("assistant", blocks);
            }
            // A tool result belongs in a user turn as a `tool_result` block.
            "tool" => push(
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": message["tool_call_id"],
                    "content": message["content"].as_str().unwrap_or_default(),
                })],
            ),
            _ => {}
        }
    }
    let converted = turns
        .into_iter()
        .map(|(role, content)| json!({"role": role, "content": content}))
        .collect();
    (system, converted)
}

/// Convert one message's `content` (a string or Abacus's vision-part array)
/// into Anthropic content blocks.
fn anthropic_content_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) if !text.is_empty() => vec![json!({"type": "text", "text": text})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part["type"].as_str() {
                Some("text") => Some(json!({"type": "text", "text": part["text"]})),
                Some("image_url") => {
                    // `data:<media_type>;base64,<data>` → an Anthropic image block.
                    let url = part.pointer("/image_url/url").and_then(Value::as_str)?;
                    let rest = url.strip_prefix("data:")?;
                    let (media_type, data) = rest.split_once(";base64,")?;
                    Some(json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": media_type, "data": data},
                    }))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn anthropic_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(json!({
                "name": function["name"],
                "description": function["description"],
                "input_schema": function["parameters"],
            }))
        })
        .collect()
}

/// Parse one Anthropic SSE event, accumulating text, reasoning, tool calls,
/// usage, and the truncation flag. The content-block `index` keys the tool-call
/// map, exactly as the OpenAI paths key on their delta index.
#[allow(clippy::too_many_arguments)]
fn apply_anthropic_event(
    data: &str,
    content: &mut String,
    reasoning: &mut String,
    calls: &mut BTreeMap<usize, PartialToolCall>,
    deltas: &mpsc::UnboundedSender<Chunk>,
    format: ToolFormat,
    stream: &mut TextStream,
    usage: &mut Option<Usage>,
    truncated: &mut bool,
) -> Result<()> {
    let value: Value = serde_json::from_str(data).context("invalid JSON in provider stream")?;
    match value["type"].as_str().unwrap_or_default() {
        "message_start" => {
            if let Some(found) = usage_total(&value["message"]["usage"]) {
                *usage = Some(found);
            }
        }
        "content_block_start" => {
            let index = value["index"].as_u64().unwrap_or(0) as usize;
            let block = &value["content_block"];
            if block["type"] == "tool_use" {
                let call = calls.entry(index).or_default();
                if let Some(id) = block["id"].as_str() {
                    call.id = id.to_owned();
                }
                if let Some(name) = block["name"].as_str() {
                    call.name = name.to_owned();
                }
            }
        }
        "content_block_delta" => {
            let index = value["index"].as_u64().unwrap_or(0) as usize;
            let delta = &value["delta"];
            match delta["type"].as_str().unwrap_or_default() {
                "text_delta" => {
                    if let Some(piece) = delta["text"].as_str() {
                        content.push_str(piece);
                        if !stream.suppressed {
                            let cut = match tool_format::marker_index(format, content) {
                                Some(marker) => {
                                    stream.suppressed = true;
                                    marker
                                }
                                None => content.len(),
                            };
                            if cut > stream.emitted {
                                let _ = deltas
                                    .send(Chunk::Text(content[stream.emitted..cut].to_owned()));
                                stream.emitted = cut;
                            }
                        }
                    }
                }
                "thinking_delta" => {
                    // The readable field of an extended-thinking block; often
                    // empty (the real CoT is in the `signature` ciphertext).
                    if let Some(piece) =
                        delta["thinking"].as_str().filter(|piece| !piece.is_empty())
                    {
                        reasoning.push_str(piece);
                        let _ = deltas.send(Chunk::Reasoning(piece.to_owned()));
                    }
                }
                "input_json_delta" => {
                    if let Some(piece) = delta["partial_json"].as_str() {
                        calls.entry(index).or_default().arguments.push_str(piece);
                    }
                }
                // `signature_delta` (thinking ciphertext) is intentionally ignored.
                _ => {}
            }
        }
        "message_delta" => {
            if value.pointer("/delta/stop_reason").and_then(Value::as_str) == Some("max_tokens") {
                *truncated = true;
            }
            // The final usage carries output_tokens; fold it onto the input
            // count captured at message_start.
            if let Some(output) = value
                .pointer("/usage/output_tokens")
                .and_then(Value::as_u64)
            {
                let prompt = usage.map(|usage| usage.prompt).unwrap_or(0);
                *usage = Some(Usage {
                    prompt,
                    total: prompt + output,
                });
            }
        }
        "error" => bail!("provider stream error: {}", value["error"]),
        _ => {}
    }
    Ok(())
}

fn finish_completion(
    content: String,
    reasoning: String,
    calls: BTreeMap<usize, PartialToolCall>,
    cancelled: bool,
    truncated: bool,
) -> Result<Completion> {
    // An incomplete entry is dropped rather than failing the turn. Two things
    // produce them routinely: a bare `{"index":1}` sentinel some servers emit,
    // and cancelling mid-stream. Erroring threw away the streamed content and
    // every well-formed call alongside the bad one.
    //
    // Arguments must parse as JSON too: an interrupt mid-`write_file` leaves
    // half an argument object, and once that reaches saved history strict
    // providers reject every subsequent request in the session.
    let seen = calls.len();
    let tool_calls = calls
        .into_values()
        .filter(|call| {
            !call.id.is_empty()
                && !call.name.is_empty()
                && (call.arguments.is_empty()
                    || serde_json::from_str::<serde::de::IgnoredAny>(&call.arguments).is_ok())
        })
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
        truncated,
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
    fn anthropic_messages_extracts_system_and_shapes_the_tool_round_trip() {
        let history = vec![
            json!({"role": "system", "content": "You are Abacus."}),
            json!({"role": "user", "content": "read the file"}),
            json!({"role": "assistant", "content": "On it.", "reasoning_content": "opaque",
                   "tool_calls": [{"id": "call_1", "type": "function",
                       "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "fn a() {}"}),
        ];
        let (system, messages) =
            anthropic_messages(&history, Some("x-anthropic-billing-header: cc"));

        // Billing prefix is the first system block, then the system prompt.
        assert_eq!(system[0]["text"], "x-anthropic-billing-header: cc");
        assert_eq!(system[1]["text"], "You are Abacus.");

        // user, then assistant (text + tool_use), then a user turn holding the
        // tool_result — the alternation the Messages API requires.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "read the file");

        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["text"], "On it.");
        let tool_use = &messages[1]["content"][1];
        assert_eq!(tool_use["type"], "tool_use");
        assert_eq!(tool_use["id"], "call_1");
        assert_eq!(tool_use["name"], "read_file");
        assert_eq!(
            tool_use["input"]["path"], "a.rs",
            "arguments parsed to an object"
        );

        assert_eq!(messages[2]["role"], "user");
        let result = &messages[2]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "call_1");
        assert_eq!(result["content"], "fn a() {}");
    }

    #[test]
    fn anthropic_tools_use_input_schema_and_images_convert() {
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "grep", "description": "search",
                "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}
        })];
        let converted = anthropic_tools(&tools);
        assert_eq!(converted[0]["name"], "grep");
        assert!(converted[0].get("input_schema").is_some());
        assert!(
            converted[0].get("parameters").is_none(),
            "renamed, not duplicated"
        );

        // A vision part becomes an Anthropic base64 image block.
        let content = json!([
            {"type": "text", "text": "look"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}
        ]);
        let blocks = anthropic_content_blocks(&content);
        assert_eq!(blocks[0]["text"], "look");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "QUJD");
    }

    #[test]
    fn anthropic_stream_parses_text_tools_usage_and_truncation() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls = BTreeMap::new();
        let mut text = TextStream::default();
        let mut usage = None;
        let mut truncated = false;
        let events = [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":40}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"there"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_9","name":"grep"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"x\"}"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":12}}"#,
        ];
        for event in events {
            apply_anthropic_event(
                event,
                &mut content,
                &mut reasoning,
                &mut calls,
                &tx,
                ToolFormat::None,
                &mut text,
                &mut usage,
                &mut truncated,
            )
            .unwrap();
        }
        assert_eq!(content, "Hi there");
        assert_eq!(calls[&1].id, "call_9");
        assert_eq!(calls[&1].name, "grep");
        assert_eq!(calls[&1].arguments, r#"{"q":"x"}"#);
        assert!(truncated, "max_tokens stop reason marks truncation");
        let usage = usage.unwrap();
        assert_eq!(usage.prompt, 40);
        assert_eq!(usage.total, 52, "input + output");
        // Text was streamed to the UI as it arrived.
        let mut streamed = String::new();
        while let Ok(chunk) = rx.try_recv() {
            if let Chunk::Text(piece) = chunk {
                streamed.push_str(&piece);
            }
        }
        assert_eq!(streamed, "Hi there");
    }

    #[test]
    fn detached_subagents_bypass_one_stream_gate() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config {
            workspace: directory.path().to_path_buf(),
            profile: "test".into(),
            model: "test".into(),
            base_url: "http://127.0.0.1:9".into(),
            protocol: ProviderProtocol::ChatCompletions,
            api_key: None,
            max_steps: 8,
            tool_output_limit: 30_000,
            yes: false,
            no_session: true,
            model_limits: crate::model_info::ModelLimits::default(),
            tool_format: ToolFormat::default(),
            mode: None,
            trace_enabled: false,
            routing: Default::default(),
            web_search: crate::web::WebConfig::default(),
            endpoint: None,
            aux_model: None,
            reasoning_effort: None,
            token_compression: false,
            one_stream: true,
            paths: crate::config::AbacusPaths::under(directory.path().join("home")),
        };
        let provider = Provider::new(&config).unwrap();
        assert!(provider.stream_gate.is_some());
        assert!(provider.with_model("aux").stream_gate.is_some());
        assert!(provider.with_detached_counter().0.stream_gate.is_none());

        config.one_stream = false;
        assert!(Provider::new(&config).unwrap().stream_gate.is_none());
    }

    /// The body is the whole contract: an adaptive model must get
    /// `output_config.effort`, and a legacy one must get `budget_tokens`.
    #[test]
    fn anthropic_thinking_body_matches_the_model_generation() {
        fn provider_for(model: &str) -> Provider {
            let directory = tempfile::tempdir().unwrap();
            let config = Config {
                workspace: directory.path().to_path_buf(),
                profile: "test".into(),
                model: model.into(),
                base_url: "https://api.anthropic.com".into(),
                protocol: ProviderProtocol::Anthropic,
                api_key: None,
                max_steps: 8,
                tool_output_limit: 30_000,
                yes: false,
                no_session: true,
                model_limits: crate::model_info::ModelLimits::default(),
                tool_format: ToolFormat::default(),
                mode: None,
                trace_enabled: false,
                routing: Default::default(),
                web_search: crate::web::WebConfig::default(),
                endpoint: None,
                aux_model: None,
                reasoning_effort: None,
                token_compression: false,
                one_stream: false,
                paths: crate::config::AbacusPaths::under(directory.path().join("home")),
            };
            Provider::new(&config).unwrap()
        }

        // Claude 4.7+: adaptive thinking plus an effort level. Sending the
        // manual shape here is a 400.
        let modern = provider_for("claude-opus-4-8");
        let mut body = json!({"max_tokens": 32_000});
        modern.apply_anthropic_thinking(&mut body, crate::config::ReasoningEffort::XHigh);
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(body["output_config"], json!({"effort": "xhigh"}));
        assert!(
            body["thinking"].get("budget_tokens").is_none(),
            "the deprecated budget must not be sent to an adaptive model"
        );
        assert_eq!(body["max_tokens"], json!(32_000), "no budget, no juggling");

        // Minimal turns thinking off, but effort still shapes total spend.
        let mut body = json!({"max_tokens": 32_000});
        modern.apply_anthropic_thinking(&mut body, crate::config::ReasoningEffort::Minimal);
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert_eq!(body["output_config"], json!({"effort": "low"}));

        // Claude 4.5 and earlier: the manual budget is the only mode.
        let legacy = provider_for("claude-opus-4-5");
        let mut body = json!({"max_tokens": 32_000});
        legacy.apply_anthropic_thinking(&mut body, crate::config::ReasoningEffort::Medium);
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 16_384})
        );
        assert!(
            body.get("output_config").is_none(),
            "a legacy model does not know output_config"
        );

        // A budget at or above max_tokens raises the cap so the answer has room.
        let mut body = json!({"max_tokens": 4_096});
        legacy.apply_anthropic_thinking(&mut body, crate::config::ReasoningEffort::Low);
        assert_eq!(body["max_tokens"], json!(4_096 + ANTHROPIC_ANSWER_HEADROOM));

        // Once a rejection teaches us, even a legacy-looking model goes adaptive.
        legacy
            .prefers_adaptive_thinking
            .store(true, Ordering::Relaxed);
        let mut body = json!({"max_tokens": 32_000});
        legacy.apply_anthropic_thinking(&mut body, crate::config::ReasoningEffort::High);
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(body["output_config"], json!({"effort": "high"}));
    }

    /// The manual-thinking shape is a hard 400 on Claude 4.7+, so which shape
    /// a model gets is the difference between a working turn and a dead one.
    #[test]
    fn thinking_shape_follows_the_model_family() {
        // Legacy: manual extended thinking is the only mode there.
        assert!(is_legacy_thinking_model("claude-opus-4-5"));
        assert!(is_legacy_thinking_model("claude-sonnet-4-5-20250929"));
        assert!(is_legacy_thinking_model("claude-3-7-sonnet-latest"));
        assert!(is_legacy_thinking_model("claude-opus-4-20250514"));

        // 4.6 and later take adaptive thinking. `claude-opus-4-6` contains
        // "opus-4", so the modern marker has to win over the legacy one.
        assert!(!is_legacy_thinking_model("claude-opus-4-6"));
        assert!(!is_legacy_thinking_model("claude-opus-4-7"));
        assert!(!is_legacy_thinking_model("claude-opus-4-8"));
        assert!(!is_legacy_thinking_model("claude-sonnet-4-6-20260101"));

        // Unknown and future names default to adaptive: guessing "new" is the
        // safe direction, since manual is a 400 on everything current.
        assert!(!is_legacy_thinking_model("claude-opus-5"));
        assert!(!is_legacy_thinking_model("claude-sonnet-5"));
        assert!(!is_legacy_thinking_model("claude-fable-5"));
        assert!(!is_legacy_thinking_model("gpt-5"));
    }

    /// A model we guessed wrong about must teach us, not kill the session.
    #[test]
    fn manual_thinking_rejection_is_recognized() {
        let error = anyhow!(
            "provider returned 400 Bad Request: {{\"error\":{{\"type\":\"invalid_request_error\",\
             \"message\":\"`thinking.type.enabled` is not supported on this model. Use \
             `thinking.type.adaptive` instead.\"}}}}"
        );
        assert!(is_manual_thinking_rejection(&error));

        // Unrelated 400s must not trigger the adaptive fallback.
        let error = anyhow!("provider returned 400: model not found: claude-opus-9");
        assert!(!is_manual_thinking_rejection(&error));
        let error = anyhow!(
            "provider returned 400: max_tokens: 393216 > 128000, which is the maximum allowed"
        );
        assert!(!is_manual_thinking_rejection(&error));
    }

    #[test]
    fn rejected_output_cap_reads_the_real_ceiling_from_the_message() {
        let error = anyhow!(
            "provider returned 400 Bad Request: {{\"error\":{{\"type\":\"invalid_request_error\",\
             \"message\":\"Error from provider (Console): Upstream request failed: [bad_request] \
             bad request: max_tokens is too large: 262144. This model supports at most 131072 \
             completion tokens.\"}}}}"
        );
        assert_eq!(rejected_output_cap(&error, Some(262_144)), Some(131_072));

        // OpenAI-style phrasing.
        let error = anyhow!(
            "provider returned 400: max_tokens must be less than or equal to 65536, got 262144"
        );
        assert_eq!(rejected_output_cap(&error, Some(262_144)), Some(65_536));

        // Anthropic Messages API phrasing (claude-opus-4-8, 128k ceiling).
        let error = anyhow!(
            "provider returned 400 Bad Request: {{\"type\":\"error\",\"error\":{{\"type\":\
             \"invalid_request_error\",\"message\":\"max_tokens: 393216 > 128000, which is the \
             maximum allowed number of output tokens for claude-opus-4-8\"}}}}"
        );
        assert_eq!(rejected_output_cap(&error, Some(393_216)), Some(128_000));

        // Not an output-limit error: no cap, no retry.
        let error = anyhow!("provider returned 400: model not found: deepseek-v99");
        assert_eq!(rejected_output_cap(&error, Some(262_144)), None);
        // A limit error whose numbers are all >= what we sent teaches nothing.
        let error = anyhow!("max_tokens is too large: 262144");
        assert_eq!(rejected_output_cap(&error, Some(262_144)), None);
        // Nothing was sent, so nothing can be clamped.
        let error = anyhow!("max_tokens is too large: at most 131072 completion tokens");
        assert_eq!(rejected_output_cap(&error, None), None);
    }

    #[test]
    fn cancelled_stream_drops_truncated_tool_call_but_keeps_valid_ones() {
        // Interrupting mid-`write_file` cuts the arguments mid-string. The
        // truncated call must not survive into history (strict providers
        // reject the whole session over it), while a call that finished
        // streaming before the interrupt is kept.
        let mut calls = BTreeMap::new();
        calls.insert(
            0,
            PartialToolCall {
                id: "done".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"a\"}".into(),
            },
        );
        calls.insert(
            1,
            PartialToolCall {
                id: "cut".into(),
                name: "write_file".into(),
                arguments: "{\"content\": \"unterminat".into(),
            },
        );
        let completion =
            finish_completion("partial prose".into(), String::new(), calls, true, false).unwrap();
        assert!(completion.cancelled);
        assert_eq!(completion.content, "partial prose");
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].id, "done");
    }

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
        while let Ok(chunk) = rx.try_recv() {
            if let Chunk::Text(piece) = chunk {
                seen.push(piece);
            }
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
        while let Ok(chunk) = rx.try_recv() {
            if let Chunk::Text(piece) = chunk {
                seen.push_str(&piece);
            }
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
        assert_eq!(rx.try_recv().unwrap(), Chunk::Text("hello".to_owned()));
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
