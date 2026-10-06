use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
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
    /// Best-effort running token counts, shared across provider clones
    /// (subagents) and rebuilds (model switches) so a session totals one set of
    /// numbers. Uses provider-reported usage when available, else a char-based
    /// estimate, so it is approximate.
    ledger: Arc<TokenLedger>,
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
    /// Set once the endpoint rejects a system message that is not first.
    ///
    /// Abacus appends its volatile context (rolling summary, goal, tasks, mode)
    /// as a trailing system block, because folding it into the leading one
    /// rewrites the request prefix and misses the prompt cache on every turn.
    /// Permissive APIs (OpenAI, Anthropic) accept that; strict chat templates —
    /// the Qwen3.5 family among them — raise "System message must be at the
    /// beginning" and 500 the request. One such rejection teaches us here, and
    /// the rest of the session pays the cache cost instead of failing.
    /// Shared across clones for the same reason `strips_reasoning` is.
    prefers_leading_system: Arc<AtomicBool>,
    /// Whether to mark prompt-cache breakpoints on Anthropic requests, cleared
    /// if the endpoint turns out not to understand `cache_control`.
    caches_prompt: Arc<AtomicBool>,
    /// Per-request cache telemetry. Shared by every clone so one process
    /// writes one file and tracks one set of conversation prefixes.
    cache_log: Option<Arc<CacheLog>>,
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
        Self::with_tokens(config, Arc::new(TokenLedger::default()))
    }

    /// Build a provider that accumulates token usage into a shared ledger.
    /// Pass the same ledger when rebuilding on a model switch so the running
    /// totals survive; subagents inherit it automatically through `clone`.
    pub fn with_tokens(config: &Config, ledger: Arc<TokenLedger>) -> Result<Self> {
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
            // Sync enables reqwest's gzip feature for the whole crate. Never
            // advertise it to a model endpoint: a compressing proxy buffers an
            // event stream until a block fills, and tokens then arrive in lumps.
            .no_gzip()
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
            ledger,
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
            prefers_leading_system: Arc::new(AtomicBool::new(false)),
            caches_prompt: Arc::new(AtomicBool::new(config.prompt_cache)),
            cache_log: Some(Arc::new(CacheLog::open(config.paths.root.join(CACHE_LOG_FILE)))),
        })
    }

    /// The message list as this endpoint accepts it: tool results lose the
    /// non-standard `name` field, and `reasoning_content` is removed once the
    /// endpoint has rejected it.
    fn sanitized_messages(&self, messages: &[Value]) -> Vec<Value> {
        let mut messages = strip_tool_names(messages);
        if self.strips_reasoning.load(Ordering::Relaxed) {
            messages = strip_reasoning_content(&messages);
        }
        if self.prefers_leading_system.load(Ordering::Relaxed) {
            messages = crate::agent::merge_system_messages(messages);
        }
        messages
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
        self.ledger.total()
    }

    /// The running counts behind that total: input, output, and how much of the
    /// input the provider served from its prompt cache.
    pub fn usage(&self) -> TokenUsage {
        self.ledger.snapshot()
    }

    /// A clone that counts into its own fresh ledger — for a subagent whose
    /// usage should be visible per worker. The caller folds the ledger back
    /// into the session total when the worker finishes.
    pub fn with_detached_counter(&self) -> (Self, Arc<TokenLedger>) {
        let ledger = Arc::new(TokenLedger::default());
        let mut detached = self.clone();
        detached.ledger = ledger.clone();
        // Explicit subagents are the sole exception to One Stream.
        detached.stream_gate = None;
        (detached, ledger)
    }

    /// Fold a finished worker's usage into this provider's running totals.
    pub fn add_tokens(&self, ledger: &TokenLedger) {
        self.ledger.absorb(ledger);
    }

    fn record_tokens(&self, reply: &Assembly, messages: &[Value], tools: usize) {
        let usage = reply.usage.unwrap_or_else(|| estimate_usage(messages, reply));
        self.ledger.record(&usage);
        if let Some(log) = &self.cache_log {
            log.request(RequestRecord {
                model: &self.model,
                upstream: reply.upstream.as_deref(),
                messages,
                tools,
                reported: reply.usage.is_some(),
                usage: &usage,
            });
        }
        // The prompt count is the exact size of the context that was just sent,
        // so the window gauge can stop guessing from character counts. Cached
        // input counts: the window was that full whether or not it was billed.
        if reply.usage.is_some() && usage.prompt > 0 {
            self.context_tokens.store(usage.prompt, Ordering::Relaxed);
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
        Provider { model: model.to_owned(), context_tokens: Arc::new(AtomicU64::new(0)), ..self.clone() }
    }

    /// This provider for a role: on the role's model when one is assigned and
    /// differs from the current one, otherwise as it is.
    pub fn for_role(&self, assigned: Option<&str>) -> Provider {
        match assigned {
            Some(model) if !model.trim().is_empty() && model != self.model => self.with_model(model),
            _ => self.clone(),
        }
    }

    /// Carry `previous`'s session id onto this freshly built provider.
    ///
    /// The id is what hosts that route by conversation are told (see
    /// `opencode_session`, and `{uuid}` in a scripted endpoint's headers), so
    /// a mid-session model or settings change — which rebuilds the provider —
    /// must not make the same conversation look like a new one.
    pub fn adopt_session(&mut self, previous: &Provider) {
        self.session_id.clone_from(&previous.session_id);
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
        match self.complete_once(messages, tools, deltas.clone(), cancel).await {
            // A strict chat template raises this from inside the template, and
            // some servers (LocalAI among them) report it as an error chunk on
            // a 200 stream rather than as a failed request — so it lands here
            // instead of on the request-level retry in `learn`. The lesson is
            // the same: merge the system blocks for the rest of the session and
            // ask once more. Nothing has been streamed to the UI, since the
            // template is rendered before the first token.
            Err(error)
                if is_leading_system_rejection(&error)
                    && !self.prefers_leading_system.swap(true, Ordering::Relaxed) =>
            {
                self.complete_once(messages, tools, deltas, cancel).await
            }
            result => result,
        }
    }

    async fn complete_once(
        &self,
        messages: &[Value],
        tools: &[Value],
        deltas: mpsc::UnboundedSender<Chunk>,
        cancel: &AtomicBool,
    ) -> Result<Completion> {
        let (mut body, event, format): (_, EventFn, _) = match self.protocol {
            ProviderProtocol::ChatCompletions => {
                (self.chat_body(messages, tools), Assembly::chat_event, self.tool_format)
            }
            ProviderProtocol::Responses => {
                (self.responses_body(messages, tools), Assembly::responses_event, ToolFormat::None)
            }
            ProviderProtocol::Anthropic => {
                (self.anthropic_body(messages, tools), Assembly::anthropic_event, self.tool_format)
            }
        };
        // Scripted overrides win over everything Abacus put in the body, and
        // its removals fire last — a required `store: false` or a rejected
        // `parallel_tool_calls` is honoured no matter what was built above.
        self.apply_scripted_body(&mut body);
        let Some(response) = self.send(&mut body, messages, cancel).await? else {
            return Ok(Completion::cancelled());
        };

        let mut reply = Assembly::default();
        let mut decoder = SseDecoder::default();
        let mut stream = response.bytes_stream();
        let mut cancelled = false;
        loop {
            // Raced against the stream rather than checked per chunk: a stalled
            // or very slow response would otherwise sit in `next()` and never
            // notice the flag, so an interrupt would appear to do nothing.
            let chunk = tokio::select! {
                biased;
                chunk = stream.next() => chunk,
                () = wait_for_cancel(cancel) => {
                    cancelled = true;
                    break;
                }
            };
            let Some(chunk) = chunk else { break };
            for data in decoder.push(&chunk.context("provider stream failed")?)? {
                event(&mut reply, &data, &deltas, format)?;
            }
        }
        if !cancelled {
            for data in decoder.finish()? {
                event(&mut reply, &data, &deltas, format)?;
            }
        }
        self.record_tokens(&reply, messages, tools.len());
        if self.protocol == ProviderProtocol::ChatCompletions {
            reply.lift_text_calls(self.tool_format);
        }
        reply.finish(cancelled)
    }

    /// Complete without an audience, for a call whose answer Abacus reads
    /// itself rather than shows as it arrives.
    pub async fn ask(&self, messages: &[Value], tools: &[Value], cancel: &AtomicBool) -> Result<Completion> {
        let (deltas, _unread) = mpsc::unbounded_channel();
        self.complete(messages, tools, deltas, cancel).await
    }

    /// What a quiet completion said, or `None` when it failed, was cancelled,
    /// or said nothing.
    pub async fn answer(&self, messages: &[Value], tools: &[Value], cancel: &AtomicBool) -> Option<String> {
        let completion = self.ask(messages, tools, cancel).await.ok()?;
        let text = completion.content.trim();
        (!completion.cancelled && !text.is_empty()).then(|| text.to_owned())
    }

    /// Send `body`, or give up with `None` when `cancel` is raised first.
    ///
    /// The request is raced as well as the stream: `post_stream` waits for
    /// response headers, and a server that accepts the connection then stalls
    /// would otherwise hold the turn until the client timeout.
    ///
    /// A rejection that names something this endpoint does not accept is a
    /// lesson rather than a failure — see [`Self::learn`] — and the request is
    /// sent again with the body corrected.
    async fn send(
        &self,
        body: &mut Value,
        messages: &[Value],
        cancel: &AtomicBool,
    ) -> Result<Option<reqwest::Response>> {
        for _ in 0..4 {
            let sent = tokio::select! {
                biased;
                sent = self.post_stream(body) => sent,
                () = wait_for_cancel(cancel) => return Ok(None),
            };
            match sent {
                Err(error) if self.learn(&error, body, messages) => continue,
                sent => return sent.map(Some),
            }
        }
        self.post_stream(body).await.map(Some)
    }

    /// Take a rejection as a signal about what this endpoint accepts, remember
    /// it for the rest of the session, and correct `body` to match. Returns
    /// whether there was anything to learn.
    ///
    /// Guessing from model names does not keep up — the lists keep growing —
    /// so every one of these starts optimistic and is settled by the first
    /// request that proves otherwise.
    fn learn(&self, error: &anyhow::Error, body: &mut Value, messages: &[Value]) -> bool {
        let first = |flag: &AtomicBool| !flag.swap(true, Ordering::Relaxed);
        let chat = self.protocol == ProviderProtocol::ChatCompletions;
        let anthropic = self.protocol == ProviderProtocol::Anthropic;
        let mut rebuild_messages = false;

        // OpenAI's reasoning models reject `max_tokens` outright.
        if chat && is_max_tokens_rejection(error) && first(&self.prefers_max_completion_tokens) {
            body.as_object_mut().map(|body| body.remove("max_tokens"));
            if let Some(max_tokens) = self.effective_output_tokens() {
                body["max_completion_tokens"] = json!(max_tokens);
            }
        // History stores `reasoning_content` because some endpoints require
        // it; the ones that reject it get it stripped from then on.
        } else if chat && format!("{error:#}").contains("reasoning_content") && first(&self.strips_reasoning) {
            rebuild_messages = true;
        // Claude 4.7+ reject manual extended thinking. The family check
        // catches the models we know; this catches the ones we do not.
        } else if anthropic && is_manual_thinking_rejection(error) && first(&self.prefers_adaptive_thinking) {
            if let Some(effort) = self.reasoning_effort {
                self.apply_anthropic_thinking(body, effort);
                self.apply_scripted_body(body);
            }
        // "max_tokens is too large: N. This model supports at most M…" —
        // detection over-reported (an upstream echoing its context window as
        // the completion cap is common on aggregators). The rejection carries
        // the real ceiling.
        } else if (chat || anthropic)
            && let Some(cap) = rejected_output_cap(error, self.effective_output_tokens())
        {
            self.learned_output_cap.store(cap as u64, Ordering::Relaxed);
            let field = if chat { self.output_tokens_field() } else { "max_tokens" };
            body[field] = json!(cap);
        // An Anthropic-compatible gateway that does not know `cache_control`.
        // Caching is an optimisation, so it is dropped rather than allowed to
        // fail the turn.
        } else if anthropic && is_cache_control_rejection(error) && self.caches_prompt.swap(false, Ordering::Relaxed) {
            rebuild_messages = true;
        // A strict chat template wants every system block at the front. This
        // session gives up the stable prefix that the cache needs.
        } else if (chat || anthropic) && is_leading_system_rejection(error) && first(&self.prefers_leading_system) {
            rebuild_messages = true;
        } else {
            return false;
        }
        if rebuild_messages && anthropic {
            let (system, converted) = self.anthropic_messages(messages);
            body["system"] = json!(system);
            body["messages"] = json!(converted);
        } else if rebuild_messages {
            body["messages"] = json!(self.sanitized_messages(messages));
        }
        true
    }

    fn chat_body(&self, messages: &[Value], tools: &[Value]) -> Value {
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
        // compaction summarisation calls pass no tools at all.
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
        // does not know the field risks a 400 from the stricter servers.
        if self.routes_upstream()
            && let Some(provider) = self.routing.body()
        {
            body["provider"] = provider;
        }
        if let Some(session) = self.openrouter_session() {
            body["session_id"] = json!(session);
        }
        body
    }

    fn responses_body(&self, messages: &[Value], tools: &[Value]) -> Value {
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
        if let Some(session) = self.openrouter_session() {
            body["session_id"] = json!(session);
        }
        body
    }

    fn anthropic_body(&self, messages: &[Value], tools: &[Value]) -> Value {
        let (system, converted) = self.anthropic_messages(messages);
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
        body
    }

    /// The history in Anthropic's shape, as this endpoint currently accepts it.
    fn anthropic_messages(&self, messages: &[Value]) -> (Vec<Value>, Vec<Value>) {
        anthropic_messages(
            &self.sanitized_messages(messages),
            self.scripted.as_ref().and_then(|scripted| scripted.system_prefix.as_deref()),
            self.caches_prompt.load(Ordering::Relaxed),
        )
    }
    /// Whether this endpoint accepts upstream provider routing.
    ///
    /// Matched on the host rather than a config flag so a profile pointed at an
    /// OpenRouter-compatible gateway behaves the same way without extra setup,
    /// and a pin silently does nothing rather than breaking a plain endpoint.
    fn routes_upstream(&self) -> bool {
        self.endpoint.contains("openrouter.ai")
    }

    /// The conversation id to send OpenRouter as its top-level `session_id`.
    ///
    /// OpenRouter serves one model from several upstream providers, and each
    /// keeps its own prompt cache — so a request routed to a different upstream
    /// than the last one misses the cache however stable its prefix is. A
    /// `session_id` pins the conversation to one upstream from its first
    /// request. Without one, OpenRouter has to infer the conversation from a
    /// hash of its opening messages, and only starts pinning once it has seen a
    /// cache hit. An explicit `provider.order` still takes priority.
    fn openrouter_session(&self) -> Option<&str> {
        self.routes_upstream().then_some(self.session_id.as_str())
    }

    /// The session id to send as `x-opencode-session`, when this endpoint is
    /// one that needs it.
    ///
    /// OpenCode's gateways (Console Go and Zen) route by conversation and
    /// reject a request that carries no session id with
    /// `400 MissingSessionID`. Matched on the host, like `routes_upstream`, so
    /// a profile simply pointed at the gateway works without extra setup.
    /// A scripted endpoint that sets the header itself stays authoritative —
    /// reqwest appends repeated headers, so sending ours too would put two
    /// values on the wire.
    fn opencode_session(&self) -> Option<&str> {
        if !self.endpoint.contains("opencode.ai") {
            return None;
        }
        if let Some(scripted) = &self.scripted
            && scripted.sets_header(OPENCODE_SESSION_HEADER)
        {
            return None;
        }
        Some(&self.session_id)
    }

    /// Which output-cap parameter this endpoint accepts. Starts optimistic and
    /// is flipped by the first rejection (see `complete`).
    fn output_tokens_field(&self) -> &'static str {
        if self.prefers_max_completion_tokens.load(Ordering::Relaxed) { "max_completion_tokens" } else { "max_tokens" }
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
        self.prefers_adaptive_thinking.load(Ordering::Relaxed) || !is_legacy_thinking_model(&self.model)
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
            let mut request = self.client.post(&self.endpoint).header(header::ACCEPT, "text/event-stream").json(&body);
            // The Messages API requires a version header on every request.
            if self.protocol == ProviderProtocol::Anthropic {
                request = request.header("anthropic-version", "2023-06-01");
            }
            // OpenCode's gateways want the conversation named on every request.
            if let Some(session) = self.opencode_session() {
                request = request.header(OPENCODE_SESSION_HEADER, session);
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
                    if attempt < 3 && (response.status().as_u16() == 429 || response.status().is_server_error()) =>
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
            bail!("provider returned {status}: {}", crate::text::clip_bytes(&detail, 2_000, "…"));
        }
        Ok(response)
    }
}

/// How one protocol folds a stream event into the reply being assembled.
type EventFn = fn(&mut Assembly, &str, &mpsc::UnboundedSender<Chunk>, ToolFormat) -> Result<()>;

/// A reply being assembled from a stream, in whichever dialect it arrives.
#[derive(Default)]
struct Assembly {
    content: String,
    reasoning: String,
    /// Keyed by the stream's own index for the call, which is what lets
    /// fragments of several calls interleave.
    calls: BTreeMap<usize, PartialToolCall>,
    /// How much of `content` has been forwarded to the UI, and whether
    /// forwarding has stopped because tool markup began.
    emitted: usize,
    suppressed: bool,
    usage: Option<Usage>,
    /// Which upstream an aggregator routed this request to. Each keeps its own
    /// cache, so a spread here is a cache miss Abacus did not cause and cannot
    /// see any other way.
    upstream: Option<String>,
    /// The reply hit the output-token ceiling.
    truncated: bool,
}

fn parse_event(data: &str) -> Result<Option<Value>> {
    if data == "[DONE]" {
        return Ok(None);
    }
    serde_json::from_str(data).map(Some).context("invalid JSON in provider stream")
}

impl Assembly {
    /// Add to the answer, forwarding prose but stopping at the first tool-call
    /// marker: the markup is stripped only after the stream ends, so without
    /// this the user watches it scroll past and the transcript diverges from
    /// the saved history.
    fn say(&mut self, piece: &str, deltas: &mpsc::UnboundedSender<Chunk>, format: ToolFormat) {
        self.content.push_str(piece);
        if self.suppressed {
            return;
        }
        let cut = match tool_format::marker_index(format, &self.content) {
            Some(index) => {
                self.suppressed = true;
                index
            }
            None => self.content.len(),
        };
        if cut > self.emitted {
            let _ = deltas.send(Chunk::Text(self.content[self.emitted..cut].to_owned()));
            self.emitted = cut;
        }
    }

    /// Add to the reasoning, which travels apart from the answer.
    fn think(&mut self, piece: &str, deltas: &mpsc::UnboundedSender<Chunk>) {
        self.reasoning.push_str(piece);
        let _ = deltas.send(Chunk::Reasoning(piece.to_owned()));
    }

    fn chat_event(&mut self, data: &str, deltas: &mpsc::UnboundedSender<Chunk>, format: ToolFormat) -> Result<()> {
        let Some(value) = parse_event(data)? else {
            return Ok(());
        };
        if let Some(error) = value.get("error") {
            bail!("provider stream error: {error}");
        }
        // Only the final chunk carries `usage`; an aggregator names its
        // upstream on every chunk, so the first is enough.
        if let Some(usage) = value.get("usage").and_then(usage_total) {
            self.usage = Some(usage);
        }
        if self.upstream.is_none() {
            self.upstream = value.get("provider").and_then(Value::as_str).map(str::to_owned);
        }
        let choices = value.get("choices").and_then(Value::as_array);
        self.truncated |= choices.into_iter().flatten().any(|choice| choice["finish_reason"] == "length");
        let Some(delta) = value.pointer("/choices/0/delta") else {
            return Ok(());
        };

        if let Some(piece) = delta.get("reasoning_content").or_else(|| delta.get("reasoning")).and_then(Value::as_str) {
            self.think(piece, deltas);
        }
        if let Some(piece) = delta.get("content").and_then(Value::as_str) {
            self.say(piece, deltas, format);
        }
        for tool_delta in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let id = tool_delta.get("id").and_then(Value::as_str);
            // `index` is optional outside OpenAI itself. Defaulting a missing
            // one to 0 merged unrelated calls into a single slot, concatenating
            // their names and producing `{"a":1}{"b":2}` as arguments. Fall back
            // to "the call we are already building, unless this delta announces
            // a new id", which is how servers without `index` signal a new call.
            let index = match tool_delta.get("index").and_then(Value::as_u64) {
                Some(index) => index as usize,
                None => match self.calls.iter().next_back() {
                    Some((last, call)) if id.is_none_or(|id| id == call.id) => *last,
                    Some((last, _)) => last + 1,
                    None => 0,
                },
            };
            let call = self.calls.entry(index).or_default();
            if let Some(id) = id {
                absorb(&mut call.id, id);
            }
            if let Some(name) = tool_delta.pointer("/function/name").and_then(Value::as_str) {
                absorb(&mut call.name, name);
            }
            // Arguments are the one field genuinely streamed in fragments, so
            // they always accumulate.
            if let Some(arguments) = tool_delta.pointer("/function/arguments").and_then(Value::as_str) {
                call.arguments.push_str(arguments);
            }
        }
        Ok(())
    }

    fn responses_event(&mut self, data: &str, deltas: &mpsc::UnboundedSender<Chunk>, format: ToolFormat) -> Result<()> {
        let Some(value) = parse_event(data)? else {
            return Ok(());
        };
        let event_type = value["type"].as_str().unwrap_or_default();
        if event_type == "error" || event_type == "response.failed" {
            let error = value
                .pointer("/error/message")
                .or_else(|| value.pointer("/response/error/message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown Responses API error");
            bail!("provider stream error: {error}");
        }
        if let Some(usage) = parse_responses_usage(&value) {
            self.usage = Some(usage);
        }
        let index = value["output_index"].as_u64().unwrap_or(0) as usize;
        let delta = value["delta"].as_str();
        let item = |field: &str| value["item"][field].as_str().map(str::to_owned);
        match event_type {
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                delta.into_iter().for_each(|piece| self.think(piece, deltas));
            }
            "response.output_text.delta" => {
                delta.into_iter().for_each(|piece| self.say(piece, deltas, format));
            }
            "response.output_item.added" | "response.output_item.done" if value["item"]["type"] == "function_call" => {
                let call = self.calls.entry(index).or_default();
                if let Some(id) = item("call_id").or_else(|| item("id")) {
                    call.id = id;
                }
                if let Some(name) = item("name") {
                    call.name = name;
                }
                if event_type == "response.output_item.done"
                    && let Some(arguments) = item("arguments")
                {
                    call.arguments = arguments;
                }
            }
            "response.function_call_arguments.delta" => {
                let call = self.calls.entry(index).or_default();
                call.arguments.push_str(delta.unwrap_or_default());
            }
            _ => {}
        }
        Ok(())
    }

    /// The content-block `index` keys the tool-call map, exactly as the OpenAI
    /// paths key on their delta index.
    fn anthropic_event(&mut self, data: &str, deltas: &mpsc::UnboundedSender<Chunk>, format: ToolFormat) -> Result<()> {
        let Some(value) = parse_event(data)? else {
            return Ok(());
        };
        let index = value["index"].as_u64().unwrap_or(0) as usize;
        match value["type"].as_str().unwrap_or_default() {
            "message_start" => {
                if let Some(usage) = usage_total(&value["message"]["usage"]) {
                    self.usage = Some(usage);
                }
            }
            "content_block_start" if value["content_block"]["type"] == "tool_use" => {
                let block = &value["content_block"];
                let call = self.calls.entry(index).or_default();
                if let Some(id) = block["id"].as_str() {
                    call.id = id.to_owned();
                }
                if let Some(name) = block["name"].as_str() {
                    call.name = name.to_owned();
                }
            }
            "content_block_delta" => {
                let delta = &value["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        if let Some(piece) = delta["text"].as_str() {
                            self.say(piece, deltas, format);
                        }
                    }
                    // The readable field of an extended-thinking block; often
                    // empty (the real CoT is in the `signature` ciphertext,
                    // whose own delta is ignored).
                    "thinking_delta" => {
                        if let Some(piece) = delta["thinking"].as_str().filter(|piece| !piece.is_empty()) {
                            self.think(piece, deltas);
                        }
                    }
                    "input_json_delta" => {
                        if let Some(piece) = delta["partial_json"].as_str() {
                            self.calls.entry(index).or_default().arguments.push_str(piece);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                self.truncated |= value["delta"]["stop_reason"] == "max_tokens";
                // The final usage carries output_tokens; fold it onto the input
                // and cache counts captured at message_start.
                if let Some(output) = value["usage"]["output_tokens"].as_u64() {
                    let start = self.usage.unwrap_or_default();
                    self.usage = Some(Usage { total: start.prompt + output, completion: output, ..start });
                }
            }
            "error" => bail!("provider stream error: {}", value["error"]),
            _ => {}
        }
        Ok(())
    }

    /// Fallback for models that emit tool calls as text instead of native
    /// `tool_calls` (common for open-weight models via Ollama/llama.cpp/raw
    /// vLLM). When no native calls arrived, parse the assistant text and lift
    /// any tool calls into the same shape the agent already dispatches.
    fn lift_text_calls(&mut self, format: ToolFormat) {
        if !self.calls.is_empty() || format == ToolFormat::None {
            return;
        }
        let (clean, parsed) = tool_format::parse(format, &self.content);
        if parsed.is_empty() {
            return;
        }
        self.content = clean;
        self.calls = parsed
            .into_iter()
            .enumerate()
            .map(|(index, call)| {
                let id = format!("text_{index}");
                (index, PartialToolCall { id, name: call.name, arguments: call.arguments })
            })
            .collect();
    }

    fn finish(self, cancelled: bool) -> Result<Completion> {
        // An incomplete entry is dropped rather than failing the turn. Two things
        // produce them routinely: a bare `{"index":1}` sentinel some servers emit,
        // and cancelling mid-stream. Erroring threw away the streamed content and
        // every well-formed call alongside the bad one.
        //
        // Arguments must parse as JSON too: an interrupt mid-`write_file` leaves
        // half an argument object, and once that reaches saved history strict
        // providers reject every subsequent request in the session.
        let seen = self.calls.len();
        let tool_calls = self
            .calls
            .into_values()
            .filter(|call| {
                !call.id.is_empty()
                    && !call.name.is_empty()
                    && (call.arguments.is_empty()
                        || serde_json::from_str::<serde::de::IgnoredAny>(&call.arguments).is_ok())
            })
            .map(|call| ToolCall { id: call.id, name: call.name, arguments: call.arguments })
            .collect::<Vec<_>>();
        // Only a completion that is *entirely* rubble is worth failing on. An
        // empty one is not: an empty stream happens for benign reasons, and
        // the agent's loop ends the turn cleanly on it.
        if !cancelled && tool_calls.is_empty() && seen > 0 && self.content.is_empty() {
            bail!("provider returned an incomplete tool call");
        }
        Ok(Completion {
            content: self.content,
            reasoning: self.reasoning,
            tool_calls,
            cancelled,
            truncated: self.truncated,
        })
    }
}
/// Resolves once `cancel` is raised. Polled rather than notified because the
/// flag is a plain `AtomicBool` shared with synchronous UI code; 50ms is well
/// under the threshold where a stop feels unresponsive.
async fn wait_for_cancel(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        sleep(Duration::from_millis(50)).await;
    }
}

/// Remove `name` from tool-result messages. History keeps it (compaction reads
/// it), but Chat Completions doesn't define it and strict upstreams — e.g.
/// Anthropic behind an OpenAI-compatible gateway — reject the request with
/// `messages[N]: "name" is not supported`.
fn strip_tool_names(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let mut message = message.clone();
            if message["role"] == "tool"
                && let Some(object) = message.as_object_mut()
            {
                object.remove("name");
            }
            message
        })
        .collect()
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
        "claude-3", "claude-4", "-3-5-", "-3-7-", "-4-0", "-4-1", "-4-5", "opus-4", "sonnet-4", "haiku-4", "-4-20",
        "-4.5",
    ];
    // A 4.6-or-later marker wins outright: `claude-opus-4-6` contains
    // `opus-4`, but it is emphatically not legacy.
    const MODERN: [&str; 8] = ["-4-6", "-4-7", "-4-8", "-4-9", "-4.6", "-4.7", "-4.8", "-4.9"];
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

/// Whether the endpoint refused a system message that was not first.
///
/// Strict chat templates raise it from inside the template rather than
/// validating the request, so it arrives as a 500 with the template's own words
/// ("System message must be at the beginning", Qwen3.5 and relatives). Matched
/// on the two parts that phrasing keeps: the role, and the position.
fn is_leading_system_rejection(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("system message") && (text.contains("beginning") || text.contains("first") || text.contains("start"))
}

/// Whether the endpoint refused the prompt-cache markers. Anthropic-compatible
/// gateways mostly ignore fields they do not know, but a strict one 400s on
/// `cache_control`; caching is an optimisation, so it is dropped rather than
/// allowed to fail the turn.
fn is_cache_control_rejection(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("cache_control")
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

/// What a provider reported for one request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Prompt plus completion, added to the running session total.
    pub total: u64,
    /// Prompt alone — the exact size of the context that was sent, which is a
    /// far better "how full is the window" figure than counting characters.
    /// Cached input is included: a cache hit is still context the model read.
    pub prompt: u64,
    /// Generated tokens.
    pub completion: u64,
    /// Prompt tokens served from a prompt cache, billed at a fraction of the
    /// uncached rate. Anthropic reports it directly; OpenAI-compatible hosts
    /// report it under `prompt_tokens_details`; DeepSeek names it
    /// `prompt_cache_hit_tokens`.
    pub cache_read: u64,
    /// Prompt tokens written *into* the cache by this request (Anthropic's
    /// `cache_creation_input_tokens`). Billed above the uncached rate, so it
    /// is tracked separately rather than folded into the hit rate.
    pub cache_write: u64,
}

impl Usage {
    /// Prompt tokens the model had to read in full — everything not served
    /// from the cache.
    pub fn uncached(&self) -> u64 {
        self.prompt.saturating_sub(self.cache_read)
    }
}

/// Running token counters for one session, shared by every provider clone
/// (subagents, aux/compaction siblings) and across provider rebuilds on a model
/// switch, so a session totals one set of numbers.
///
/// The split matters for cost: input and output are priced differently, and
/// cached input an order of magnitude below uncached. One "tokens used" figure
/// cannot show whether a long session is re-reading a cached prefix cheaply or
/// paying full price for it every turn.
#[derive(Debug, Default)]
pub struct TokenLedger {
    total: AtomicU64,
    input: AtomicU64,
    output: AtomicU64,
    cache_read: AtomicU64,
    cache_write: AtomicU64,
}

/// A consistent-enough read of a [`TokenLedger`] for display.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub total: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenUsage {
    /// Input tokens the provider billed at the full uncached rate.
    pub fn uncached_input(&self) -> u64 {
        self.input.saturating_sub(self.cache_read)
    }

    /// Share of input tokens served from the cache, 0-100. `None` when no
    /// input has been counted yet, or when the endpoint reports no cache
    /// figures at all — an honest blank beats a confident 0%.
    pub fn cache_rate(&self) -> Option<u8> {
        if self.input == 0 || (self.cache_read == 0 && self.cache_write == 0) {
            return None;
        }
        Some(((self.cache_read * 100) / self.input).min(100) as u8)
    }
}

impl TokenLedger {
    /// A ledger resuming a session that already spent `total` tokens. The
    /// breakdown starts empty: older sessions recorded only the total, and
    /// inventing a split for it would be worse than showing this run's.
    pub fn new(total: u64) -> Self {
        Self { total: AtomicU64::new(total), ..Self::default() }
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    pub fn store_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn add_total(&self, tokens: u64) {
        if tokens > 0 {
            self.total.fetch_add(tokens, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> TokenUsage {
        TokenUsage {
            total: self.total(),
            input: self.input.load(Ordering::Relaxed),
            output: self.output.load(Ordering::Relaxed),
            cache_read: self.cache_read.load(Ordering::Relaxed),
            cache_write: self.cache_write.load(Ordering::Relaxed),
        }
    }

    /// Fold a finished worker's ledger into this one.
    pub fn absorb(&self, other: &TokenLedger) {
        let usage = other.snapshot();
        self.add_total(usage.total);
        self.input.fetch_add(usage.input, Ordering::Relaxed);
        self.output.fetch_add(usage.output, Ordering::Relaxed);
        self.cache_read.fetch_add(usage.cache_read, Ordering::Relaxed);
        self.cache_write.fetch_add(usage.cache_write, Ordering::Relaxed);
    }

    pub(crate) fn record(&self, usage: &Usage) {
        self.add_total(usage.total);
        self.input.fetch_add(usage.prompt, Ordering::Relaxed);
        self.output.fetch_add(usage.completion, Ordering::Relaxed);
        self.cache_read.fetch_add(usage.cache_read, Ordering::Relaxed);
        self.cache_write.fetch_add(usage.cache_write, Ordering::Relaxed);
    }
}

/// Where per-request cache telemetry is written, under the Abacus home.
const CACHE_LOG_FILE: &str = "cache.jsonl";
/// Past this size the log is rotated once, at startup, to `cache.jsonl.1`.
const CACHE_LOG_ROTATE_BYTES: u64 = 16 * 1024 * 1024;
/// Conversations whose last prefix is remembered. Aux and detached calls each
/// open their own, so the map is cleared rather than grown without bound.
const CACHE_LOG_CONVERSATIONS: usize = 64;

/// Per-request cache telemetry, one JSON line per request.
///
/// A session cache rate says *that* requests are missing the cache, never
/// *why*. Each line here says why: what the provider reported, which upstream
/// served it (an aggregator routes between providers, and each keeps its own
/// cache), and how much of the prompt was byte-identical to the previous request
/// in the same conversation. A request whose prefix was stable but came back
/// uncached is a routing or provider miss; one whose prefix changed is
/// Abacus's own doing. Without the second figure the two are indistinguishable.
#[derive(Debug)]
pub struct CacheLog {
    path: PathBuf,
    /// Hash and serialised length of each message of the last request, per
    /// conversation.
    prefixes: std::sync::Mutex<HashMap<u64, Vec<(u64, usize)>>>,
}

/// One request, as `CacheLog::request` receives it.
struct RequestRecord<'a> {
    model: &'a str,
    upstream: Option<&'a str>,
    messages: &'a [Value],
    tools: usize,
    reported: bool,
    usage: &'a Usage,
}

impl CacheLog {
    fn open(path: PathBuf) -> Self {
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > CACHE_LOG_ROTATE_BYTES) {
            let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
        }
        Self { path, prefixes: std::sync::Mutex::default() }
    }

    /// Identify a conversation the way OpenRouter's sticky routing does: by the
    /// first system message and the first message that is not one. The main
    /// loop, and the compaction and refine calls made inside it, share a key;
    /// a detached side call gets its own.
    fn conversation(messages: &[Value]) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        messages.iter().find(|message| message["role"] == "system").map(Value::to_string).hash(&mut hasher);
        messages.iter().find(|message| message["role"] != "system").map(Value::to_string).hash(&mut hasher);
        hasher.finish()
    }

    /// Characters of `messages` identical to the previous request in the same
    /// conversation, and the total — then remember this request for the next.
    fn stable_prefix(&self, conversation: u64, messages: &[Value]) -> (usize, usize) {
        let current: Vec<(u64, usize)> = messages
            .iter()
            .map(|message| {
                let text = message.to_string();
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                text.hash(&mut hasher);
                (hasher.finish(), text.len())
            })
            .collect();
        let total = current.iter().map(|(_, len)| len).sum();
        let Ok(mut prefixes) = self.prefixes.lock() else {
            return (0, total);
        };
        let stable = prefixes.get(&conversation).map_or(0, |previous| {
            previous.iter().zip(&current).take_while(|(before, now)| before.0 == now.0).map(|(_, now)| now.1).sum()
        });
        if prefixes.len() >= CACHE_LOG_CONVERSATIONS && !prefixes.contains_key(&conversation) {
            prefixes.clear();
        }
        prefixes.insert(conversation, current);
        (stable, total)
    }

    fn request(&self, record: RequestRecord<'_>) {
        let conversation = Self::conversation(record.messages);
        let (stable_chars, prompt_chars) = self.stable_prefix(conversation, record.messages);
        let usage = record.usage;
        let line = json!({
            "ts": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs_f64())
                .unwrap_or_default(),
            "model": record.model,
            "upstream": record.upstream,
            "conversation": format!("{conversation:016x}"),
            "messages": record.messages.len(),
            "tools": record.tools,
            "reported": record.reported,
            "prompt": usage.prompt,
            "cached": usage.cache_read,
            "cache_write": usage.cache_write,
            "completion": usage.completion,
            "cached_pct": (usage.prompt > 0).then(|| usage.cache_read * 100 / usage.prompt),
            "prompt_chars": prompt_chars,
            "stable_chars": stable_chars,
            "stable_pct": (prompt_chars > 0).then(|| stable_chars * 100 / prompt_chars),
        });
        // Telemetry must never fail a request, so write errors are dropped.
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(file, "{line}");
        }
    }
}

fn parse_responses_usage(value: &Value) -> Option<Usage> {
    let usage = value.pointer("/response/usage").or_else(|| value.get("usage"))?;
    usage_total(usage)
}

fn usage_total(usage: &Value) -> Option<Usage> {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    let reported_prompt =
        usage.get("prompt_tokens").or_else(|| usage.get("input_tokens")).and_then(Value::as_u64).unwrap_or(0);
    let completion =
        usage.get("completion_tokens").or_else(|| usage.get("output_tokens")).and_then(Value::as_u64).unwrap_or(0);
    // Three spellings of "served from cache", in decreasing specificity:
    // Anthropic's own field, OpenAI's nested detail, DeepSeek's flat pair.
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64))
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_write = field("cache_creation_input_tokens");
    // Anthropic's `input_tokens` counts only what it read uncached — cached and
    // cache-written input are reported beside it. Everywhere else the prompt
    // figure is already the whole prompt and the cache count is a subset of it.
    let prompt = if usage.get("cache_read_input_tokens").is_some() || usage.get("cache_creation_input_tokens").is_some()
    {
        reported_prompt + cache_read + cache_write
    } else {
        reported_prompt.max(cache_read)
    };
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .filter(|reported| *reported >= prompt)
        .unwrap_or(prompt + completion);
    if total == 0 && prompt == 0 {
        return None;
    }
    Some(Usage { total, prompt, completion, cache_read, cache_write })
}

/// Rough fallback (~4 chars/token) for providers that never report usage, so
/// the running totals are never stuck at zero. Prompt and completion are
/// counted separately — one lumped figure cannot say which direction the
/// tokens went, which is the first thing the status row shows.
fn estimate_usage(messages: &[Value], reply: &Assembly) -> Usage {
    let mut chars = reply.content.chars().count();
    for call in reply.calls.values() {
        chars += call.name.chars().count() + call.arguments.chars().count();
    }
    let completion = (chars / 4) as u64;
    let mut chars = 0usize;
    for message in messages {
        chars += crate::text::content_text(&message["content"]).chars().count();
        if let Some(tool_calls) = message["tool_calls"].as_array() {
            for call in tool_calls {
                if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    chars += args.chars().count();
                }
            }
        }
    }
    let prompt = (chars / 4) as u64;
    Usage { total: prompt + completion, prompt, completion, ..Usage::default() }
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
                } else if let Some(parts) = message["content"].as_array() {
                    // A prompt with images: the Responses API spells the parts
                    // `input_text` and `input_image`, not Chat Completions'
                    // `text` and `image_url`. Skipping the array instead used
                    // to drop the whole prompt, text and all.
                    let parts: Vec<Value> = parts
                        .iter()
                        .filter_map(|part| match part["type"].as_str()? {
                            "text" => Some(json!({"type": "input_text", "text": part["text"]})),
                            "image_url" => Some(json!({
                                "type": "input_image",
                                "image_url": part.pointer("/image_url/url")?,
                            })),
                            _ => None,
                        })
                        .collect();
                    if !parts.is_empty() {
                        input.push(json!({"role": role, "content": parts}));
                    }
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

/// The header naming the conversation to OpenCode's gateways. They route by
/// it and answer `400 MissingSessionID` without it.
const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

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
/// Split Abacus's message list into the Anthropic `system` blocks and `messages`
/// turns.
///
/// Only *leading* system messages become `system` blocks. A system message that
/// appears after the conversation has started is a session context block Abacus
/// appended to the history (see `sync_session_context` in the agent), and it is
/// rendered where it stands, as a text block in a user turn: hoisting it would
/// move it ahead of every turn that preceded it, rewriting the prefix a cache
/// needs to hold still. One that is not already tagged — a compaction note — is
/// tagged here so the model does not read it as the user speaking.
///
/// With `cache` set, two `cache_control` breakpoints are marked: the end of the
/// system blocks, and the end of the request. The history is append-only, so
/// the whole request is the prefix of the next one: each turn reads everything
/// the last one wrote and writes only its own delta.
fn anthropic_messages(messages: &[Value], system_prefix: Option<&str>, cache: bool) -> (Vec<Value>, Vec<Value>) {
    let mut system: Vec<Value> = Vec::new();
    if let Some(prefix) = system_prefix.filter(|prefix| !prefix.trim().is_empty()) {
        system.push(json!({"type": "text", "text": prefix}));
    }
    // (role, content-blocks) pairs, coalescing consecutive same-role turns.
    let mut turns: Vec<(String, Vec<Value>)> = Vec::new();
    // Whether any conversation turn has been emitted yet, which is what makes a
    // system message leading (standing instructions) or trailing (volatile
    // context). Tracked separately because `push` holds the borrow on `turns`.
    let mut started = false;
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
                let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) else {
                    continue;
                };
                if !started {
                    system.push(json!({"type": "text", "text": text}));
                } else {
                    let text = if text.starts_with("<session_context") {
                        text.to_owned()
                    } else {
                        format!("<session_context>\n{text}\n</session_context>")
                    };
                    push("user", vec![json!({"type": "text", "text": text})]);
                }
            }
            "user" => {
                started = true;
                push("user", anthropic_content_blocks(&message["content"]));
            }
            "assistant" => {
                started = true;
                let mut blocks = anthropic_content_blocks(&message["content"]);
                if let Some(tool_calls) = message["tool_calls"].as_array() {
                    for call in tool_calls {
                        let arguments = call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}");
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
            "tool" => {
                started = true;
                push(
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": message["tool_call_id"],
                        "content": message["content"].as_str().unwrap_or_default(),
                    })],
                );
            }
            _ => {}
        }
    }
    if cache {
        if let Some(last) = system.last_mut() {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
        mark_history_breakpoint(&mut turns);
    }
    let converted = turns.into_iter().map(|(role, content)| json!({"role": role, "content": content})).collect();
    (system, converted)
}

/// Put the rolling cache breakpoint on the last content block of the request.
fn mark_history_breakpoint(turns: &mut [(String, Vec<Value>)]) {
    if let Some(block) = turns.last_mut().and_then(|(_, blocks)| blocks.last_mut()) {
        block["cache_control"] = json!({"type": "ephemeral"});
    }
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
        while let Some(relative) = self.buffer[consumed..].iter().position(|&byte| byte == b'\n') {
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    /// Drive `events` through one dialect's parser, returning what was
    /// assembled and the answer text that was streamed to the UI on the way.
    fn assemble(event: EventFn, format: ToolFormat, events: &[&str]) -> (Assembly, String) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut reply = Assembly::default();
        for data in events {
            event(&mut reply, data, &tx, format).unwrap();
        }
        drop(tx);
        let mut streamed = String::new();
        while let Ok(chunk) = rx.try_recv() {
            if let Chunk::Text(piece) = chunk {
                streamed.push_str(&piece);
            }
        }
        (reply, streamed)
    }

    fn chat_deltas(payloads: &[&str]) -> BTreeMap<usize, PartialToolCall> {
        assemble(Assembly::chat_event, ToolFormat::None, payloads).0.calls
    }

    fn parse_chat_usage(value: &Value) -> Option<Usage> {
        value.get("usage").and_then(usage_total)
    }

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
        let (system, messages) = anthropic_messages(&history, Some("x-anthropic-billing-header: cc"), false);

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
        assert_eq!(tool_use["input"]["path"], "a.rs", "arguments parsed to an object");

        assert_eq!(messages[2]["role"], "user");
        let result = &messages[2]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "call_1");
        assert_eq!(result["content"], "fn a() {}");
    }

    #[test]
    fn tool_results_are_sent_without_a_name() {
        let messages = vec![
            json!({"role": "user", "content": "hi", "name": "op"}),
            json!({"role": "tool", "tool_call_id": "c1", "name": "read_file", "content": "x"}),
        ];
        let sent = strip_tool_names(&messages);
        assert_eq!(sent[0]["name"], "op");
        assert!(sent[1].get("name").is_none());
        assert_eq!(sent[1]["tool_call_id"], "c1");
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
        assert!(converted[0].get("parameters").is_none(), "renamed, not duplicated");

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
        let (reply, streamed) = assemble(
            Assembly::anthropic_event,
            ToolFormat::None,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":40}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi "}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"there"}}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_9","name":"grep"}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"x\"}"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":12}}"#,
            ],
        );
        assert_eq!(reply.content, "Hi there");
        assert_eq!(reply.calls[&1].id, "call_9");
        assert_eq!(reply.calls[&1].name, "grep");
        assert_eq!(reply.calls[&1].arguments, r#"{"q":"x"}"#);
        assert!(reply.truncated, "max_tokens stop reason marks truncation");
        let usage = reply.usage.unwrap();
        assert_eq!(usage.prompt, 40);
        assert_eq!(usage.total, 52, "input + output");
        assert_eq!(streamed, "Hi there", "text was streamed to the UI as it arrived");
    }

    #[test]
    fn detached_subagents_bypass_one_stream_gate() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config {
            profile: "test".into(),
            max_steps: 8,
            no_session: true,
            one_stream: true,
            ..Config::for_endpoint(
                directory.path().to_path_buf(),
                "http://127.0.0.1:9".into(),
                "test".into(),
                crate::config::AbacusPaths::under(directory.path().join("home")),
            )
        };
        let provider = Provider::new(&config).unwrap();
        assert!(provider.stream_gate.is_some());
        assert!(provider.with_model("aux").stream_gate.is_some());
        assert!(provider.with_detached_counter().0.stream_gate.is_none());

        config.one_stream = false;
        assert!(Provider::new(&config).unwrap().stream_gate.is_none());
    }

    /// OpenCode's gateways answer `400 MissingSessionID` without the header,
    /// and want one id for the whole conversation — so the aux model's calls
    /// have to carry the same one.
    #[test]
    fn opencode_endpoints_get_a_stable_session_header() {
        fn provider_for(base_url: &str) -> Provider {
            let directory = tempfile::tempdir().unwrap();
            let config = Config {
                profile: "test".into(),
                max_steps: 8,
                no_session: true,
                ..Config::for_endpoint(
                    directory.path().to_path_buf(),
                    base_url.into(),
                    "test".into(),
                    crate::config::AbacusPaths::under(directory.path().join("home")),
                )
            };
            Provider::new(&config).unwrap()
        }

        // OpenRouter is sent the same id as its `session_id`, and nobody else is.
        assert!(provider_for("https://openrouter.ai/api/v1").openrouter_session().is_some());
        assert_eq!(provider_for("https://api.openai.com/v1").openrouter_session(), None);

        let mut provider = provider_for("https://opencode.ai/zen/go/v1");
        let session = provider.opencode_session().expect("gateway needs a session");
        assert!(!session.is_empty());
        assert_eq!(
            provider.with_model("aux").opencode_session(),
            Some(session),
            "the aux model is the same conversation"
        );

        // A model switch or settings save rebuilds the provider mid-run; the
        // gateway must still see one conversation.
        let mut rebuilt = provider_for("https://opencode.ai/zen/go/v1");
        assert_ne!(rebuilt.opencode_session(), Some(session));
        rebuilt.adopt_session(&provider);
        assert_eq!(rebuilt.opencode_session(), Some(session));

        // Every other endpoint is left alone.
        assert_eq!(provider_for("https://api.openai.com/v1").opencode_session(), None);

        // A scripted endpoint that sets the header itself stays authoritative,
        // so the request carries one value rather than two.
        provider.scripted = Some(
            serde_yaml::from_str(
                "url: https://opencode.ai/zen/go/v1/chat/completions\nheaders:\n  X-OpenCode-Session: mine\n",
            )
            .unwrap(),
        );
        assert_eq!(provider.opencode_session(), None);
    }

    /// The body is the whole contract: an adaptive model must get
    /// `output_config.effort`, and a legacy one must get `budget_tokens`.
    #[test]
    fn anthropic_thinking_body_matches_the_model_generation() {
        fn provider_for(model: &str) -> Provider {
            let directory = tempfile::tempdir().unwrap();
            let config = Config {
                profile: "test".into(),
                protocol: ProviderProtocol::Anthropic,
                max_steps: 8,
                no_session: true,
                ..Config::for_endpoint(
                    directory.path().to_path_buf(),
                    "https://api.anthropic.com".into(),
                    model.into(),
                    crate::config::AbacusPaths::under(directory.path().join("home")),
                )
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
        assert_eq!(body["thinking"], json!({"type": "enabled", "budget_tokens": 16_384}));
        assert!(body.get("output_config").is_none(), "a legacy model does not know output_config");

        // A budget at or above max_tokens raises the cap so the answer has room.
        let mut body = json!({"max_tokens": 4_096});
        legacy.apply_anthropic_thinking(&mut body, crate::config::ReasoningEffort::Low);
        assert_eq!(body["max_tokens"], json!(4_096 + ANTHROPIC_ANSWER_HEADROOM));

        // Once a rejection teaches us, even a legacy-looking model goes adaptive.
        legacy.prefers_adaptive_thinking.store(true, Ordering::Relaxed);
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
        let error = anyhow!("provider returned 400: max_tokens: 393216 > 128000, which is the maximum allowed");
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
        let error = anyhow!("provider returned 400: max_tokens must be less than or equal to 65536, got 262144");
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
            PartialToolCall { id: "done".into(), name: "read_file".into(), arguments: "{\"path\":\"a\"}".into() },
        );
        calls.insert(
            1,
            PartialToolCall {
                id: "cut".into(),
                name: "write_file".into(),
                arguments: "{\"content\": \"unterminat".into(),
            },
        );
        let reply = Assembly { content: "partial prose".into(), calls, ..Assembly::default() };
        let completion = reply.finish(true).unwrap();
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

    #[test]
    fn assembles_streamed_tool_arguments() {
        let calls = chat_deltas(&[
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
        let calls = chat_deltas(&[
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
        let calls = chat_deltas(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"file"}}]}}]}"#,
        ]);
        assert_eq!(calls[&0].name, "read_file");
    }

    /// Without `index`, a new `id` starts a new call. Defaulting to slot 0
    /// merged unrelated calls and concatenated their argument JSON.
    #[test]
    fn calls_without_an_index_are_kept_apart() {
        let calls = chat_deltas(&[
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
        let payloads = ["Let me read it.", "\n<\x74ool_call>", r#"{"name":"read_file"}"#]
            .map(|piece| json!({"choices":[{"delta":{"content": piece}}]}).to_string());
        let payloads: Vec<&str> = payloads.iter().map(String::as_str).collect();
        let (reply, seen) = assemble(Assembly::chat_event, ToolFormat::Hermes, &payloads);
        assert_eq!(seen, "Let me read it.\n");
        assert!(reply.content.contains("read_file"), "history keeps the raw text");
    }

    #[test]
    fn assembles_responses_api_text_and_tool_call() {
        let (reply, streamed) = assemble(
            Assembly::responses_event,
            ToolFormat::None,
            &[
                r#"{"type":"response.output_text.delta","delta":"hello"}"#,
                r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_7","name":"grep","arguments":""}}"#,
                r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"query\":\"todo\"}"}"#,
            ],
        );
        assert_eq!(streamed, "hello");
        assert_eq!(reply.content, "hello");
        assert_eq!(reply.calls[&1].id, "call_7");
        assert_eq!(reply.calls[&1].arguments, r#"{"query":"todo"}"#);
    }

    #[test]
    fn responses_api_gets_image_prompts_as_input_parts() {
        let input = responses_input(&[json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
            ],
        })]);
        assert_eq!(
            input,
            [json!({"role": "user", "content": [
                {"type": "input_text", "text": "what is this?"},
                {"type": "input_image", "image_url": "data:image/png;base64,QUJD"},
            ]})]
        );
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
        assert_eq!(parse_chat_usage(&chat), Some(Usage { total: 15, prompt: 10, completion: 5, ..Usage::default() }));
        // A total with no breakdown still gives a usable session count, and the
        // context gauge simply keeps its previous value.
        let chat_total = json!({"usage": {"total_tokens": 99}});
        assert_eq!(parse_chat_usage(&chat_total), Some(Usage { total: 99, ..Usage::default() }));
        let responses = json!({
            "type": "response.completed",
            "response": {"usage": {"input_tokens": 20, "output_tokens": 7, "total_tokens": 27}}
        });
        assert_eq!(
            parse_responses_usage(&responses),
            Some(Usage { total: 27, prompt: 20, completion: 7, ..Usage::default() })
        );
    }

    #[test]
    fn cached_prompt_tokens_are_read_in_every_dialect() {
        // OpenAI: the cached count is a subset of `prompt_tokens`.
        let openai = json!({"usage": {
            "prompt_tokens": 1_000, "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 900}
        }});
        let usage = parse_chat_usage(&openai).unwrap();
        assert_eq!((usage.prompt, usage.cache_read, usage.uncached()), (1_000, 900, 100));

        // DeepSeek: a flat hit/miss pair, hits again a subset of the prompt.
        let deepseek = json!({"usage": {
            "prompt_tokens": 1_000, "completion_tokens": 20,
            "prompt_cache_hit_tokens": 768, "prompt_cache_miss_tokens": 232
        }});
        assert_eq!(parse_chat_usage(&deepseek).unwrap().cache_read, 768);

        // Anthropic: `input_tokens` counts ONLY the uncached read, so the real
        // prompt size is the three fields added together. Reporting the raw
        // field as the prompt would show a 40k context as 200 tokens.
        let anthropic = json!({"usage": {
            "input_tokens": 200, "output_tokens": 50,
            "cache_read_input_tokens": 38_000, "cache_creation_input_tokens": 1_800
        }});
        let usage = usage_total(&anthropic["usage"]).unwrap();
        assert_eq!(usage.prompt, 40_000);
        assert_eq!(usage.cache_read, 38_000);
        assert_eq!(usage.cache_write, 1_800);
        assert_eq!(usage.uncached(), 2_000, "read plus written is what was paid for");
        assert_eq!(usage.total, 40_050);
    }

    #[test]
    fn ledger_splits_directions_and_reports_a_cache_rate() {
        let ledger = TokenLedger::new(1_000);
        assert_eq!(ledger.snapshot().cache_rate(), None, "nothing counted yet");
        ledger.record(&Usage { total: 1_100, prompt: 1_000, completion: 100, cache_read: 800, cache_write: 200 });
        let usage = ledger.snapshot();
        assert_eq!(usage.total, 2_100, "resumed total is carried, not replaced");
        assert_eq!((usage.input, usage.output), (1_000, 100));
        assert_eq!(usage.uncached_input(), 200);
        assert_eq!(usage.cache_rate(), Some(80));

        // An endpoint that reports no cache figures at all says nothing rather
        // than claiming a 0% hit rate.
        let silent = TokenLedger::default();
        silent.record(&Usage { total: 300, prompt: 200, completion: 100, ..Usage::default() });
        assert_eq!(silent.snapshot().cache_rate(), None);

        // A worker's ledger folds in whole.
        ledger.absorb(&silent);
        assert_eq!(ledger.snapshot().input, 1_200);
    }

    #[test]
    fn usage_is_taken_from_the_final_chunk_not_the_deltas() {
        let delta = r#"{"choices":[{"delta":{"content":"hi"}}]}"#;
        let last = r#"{"choices":[],"usage":{"total_tokens":42,"prompt_tokens":30}}"#;
        let (reply, _) = assemble(Assembly::chat_event, ToolFormat::None, &[delta]);
        assert_eq!(reply.usage, None);
        let (reply, _) = assemble(Assembly::chat_event, ToolFormat::None, &[delta, last]);
        assert_eq!(reply.usage, Some(Usage { total: 42, prompt: 30, ..Usage::default() }));
    }

    #[test]
    fn session_context_renders_in_place_and_the_request_end_is_cached() {
        let block = "<session_context layer=\"goal\">\nship it\n</session_context>";
        let history = vec![
            json!({"role": "system", "content": "You are Abacus."}),
            json!({"role": "user", "content": "read the file"}),
            json!({"role": "assistant", "content": "On it."}),
            json!({"role": "system", "content": block}),
            json!({"role": "system", "content": "3 older messages were omitted"}),
        ];
        let (system, messages) = anthropic_messages(&history, None, true);

        // Only the leading block is a system block; hoisting a later one would
        // rewrite the prefix it follows.
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");

        let last = messages.last().unwrap();
        assert_eq!(last["role"], "user");
        let blocks = last["content"].as_array().unwrap();
        assert_eq!(blocks[0]["text"], block, "already tagged, so not wrapped twice");
        let note = blocks[1]["text"].as_str().unwrap();
        assert!(note.starts_with("<session_context>"), "an untagged note is tagged: {note}");

        // The history is append-only, so the whole request is the next one's
        // prefix and the breakpoint goes at its very end.
        assert_eq!(blocks[1]["cache_control"]["type"], "ephemeral");
        assert!(blocks[0].get("cache_control").is_none());
        assert!(messages[1]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn cache_breakpoints_are_absent_when_caching_is_off() {
        let history =
            vec![json!({"role": "system", "content": "You are Abacus."}), json!({"role": "user", "content": "hi"})];
        let (system, messages) = anthropic_messages(&history, None, false);
        assert!(system[0].get("cache_control").is_none());
        assert!(messages[0]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn a_system_message_before_any_turn_is_still_an_instruction() {
        // Compaction inserts its drop-notice mid-array, before the kept tail.
        let history = vec![
            json!({"role": "system", "content": "You are Abacus."}),
            json!({"role": "system", "content": "3 older messages were omitted"}),
            json!({"role": "user", "content": "carry on"}),
        ];
        let (system, messages) = anthropic_messages(&history, None, false);
        assert_eq!(system.len(), 2, "both precede the conversation");
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn the_upstream_an_aggregator_names_is_captured_once() {
        let plain = r#"{"choices":[{"delta":{"content":"hi"}}]}"#;
        let (reply, _) = assemble(Assembly::chat_event, ToolFormat::None, &[plain]);
        assert_eq!(reply.upstream, None);
        let (reply, _) = assemble(
            Assembly::chat_event,
            ToolFormat::None,
            &[
                r#"{"id":"gen-1","provider":"DeepInfra","choices":[{"delta":{"content":"a"}}]}"#,
                r#"{"id":"gen-1","provider":"Novita","choices":[{"delta":{"content":"b"}}]}"#,
            ],
        );
        assert_eq!(reply.upstream.as_deref(), Some("DeepInfra"), "one request, one upstream");
    }

    #[test]
    fn the_cache_log_tells_a_stable_prefix_from_a_rewritten_one() {
        let directory = tempfile::tempdir().unwrap();
        let log = CacheLog::open(directory.path().join(CACHE_LOG_FILE));
        let usage = Usage { total: 110, prompt: 100, completion: 10, cache_read: 60, cache_write: 0 };
        let request = |messages: &[Value]| {
            log.request(RequestRecord {
                model: "m",
                upstream: Some("DeepInfra"),
                messages,
                tools: 3,
                reported: true,
                usage: &usage,
            })
        };
        let system = json!({"role": "system", "content": "You are Abacus."});
        let user = json!({"role": "user", "content": "fix it"});
        let assistant = json!({"role": "assistant", "content": "on it"});
        let tail = |text: &str| json!({"role": "system", "content": text});

        request(&[system.clone(), user.clone(), tail("mode: plan")]);
        // The history grew and the volatile tail changed: everything before
        // the tail is a reusable prefix.
        request(&[system.clone(), user.clone(), assistant.clone(), tail("mode: build")]);
        // A rewritten system prompt is a different conversation altogether.
        request(&[tail("You are Abacus. goal: x"), user.clone(), tail("mode: build")]);

        let lines: Vec<Value> = std::fs::read_to_string(directory.path().join(CACHE_LOG_FILE))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["stable_chars"], 0, "nothing to compare the first with");
        let shared = (system.to_string().len() + user.to_string().len()) as u64;
        assert_eq!(lines[1]["stable_chars"], shared);
        assert_eq!(lines[1]["conversation"], lines[0]["conversation"]);
        assert_ne!(lines[2]["conversation"], lines[1]["conversation"]);
        assert_eq!(lines[2]["stable_chars"], 0);
        assert_eq!(lines[1]["upstream"], "DeepInfra");
        assert_eq!(lines[1]["cached_pct"], 60);
    }

    #[test]
    fn estimates_tokens_when_usage_is_absent() {
        // 8 prompt chars => 2 tokens, 4 completion chars => 1, at ~4 chars each.
        let messages = vec![json!({"role": "user", "content": "12345678"})];
        let reply = Assembly { content: "abcd".into(), ..Assembly::default() };
        let usage = estimate_usage(&messages, &reply);
        assert_eq!((usage.prompt, usage.completion, usage.total), (2, 1, 3));
    }
}
