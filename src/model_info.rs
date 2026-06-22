//! Resolve a model's context window and output cap so compaction thresholds and
//! output limits scale with the chosen model instead of being hardcoded.
//!
//! Resolution order, most authoritative first:
//! 1. Explicit override (`--context-window` / `--max-output-tokens` or settings).
//! 2. Best-effort detection from the provider's `/models` endpoint (server-
//!    authoritative when present, e.g. OpenRouter reports `context_length` and
//!    `max_completion_tokens`).
//! 3. A small heuristic table of well-known model families, matched by family
//!    (not pinned version) with current verified specs — so it does not go
//!    stale as vendors ship new minor versions.
//! 4. A conservative default (128k context, 8k output) that is safe for most
//!    modern models — it compacts a little early rather than risking overflow.
//!
//! Only an override or a successful detection is treated as "confident" enough to
//! actually send `max_tokens` to the provider. A heuristic/default estimate is
//! used solely to size the compaction budget (reserving room for output) so we
//! never artificially truncate a model whose real output ceiling we don't know.

use std::time::Duration;

use anyhow::Result;
use reqwest::{Client, header};
use serde_json::Value;

/// Conservative fallbacks when nothing else is known. Safe for most modern
/// models: compaction fires somewhat early rather than overflowing a smaller
/// window.
pub const DEFAULT_CONTEXT_WINDOW: usize = 128_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 8_192;

/// Rough chars-per-token used to convert a token budget into the char budget
/// compaction measures (`message_chars` is JSON byte length). JSON overhead
/// makes this slightly conservative, which is the safe direction (compact a
/// touch early rather than overflow).
const CHARS_PER_TOKEN: usize = 4;
/// Tokens reserved for the system prompt, injected goal/task/summary context,
/// and tool schemas before the model's input budget is computed.
const RESERVED_TOKENS: usize = 6_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    Override,
    Detected,
    Heuristic,
    Default,
}

#[derive(Debug, Clone, Copy)]
pub struct ModelLimits {
    pub context_window: usize,
    /// Estimated max output tokens — used to size the compaction input budget.
    pub max_output_tokens: usize,
    /// A confident output cap to actually send to the provider (`Some` only for
    /// an override or a successful detection; `None` means "let the server
    /// default" so we never truncate from a guess).
    pub configured_output_tokens: Option<usize>,
    pub source: LimitSource,
}

impl Default for ModelLimits {
    fn default() -> Self {
        Self {
            context_window: DEFAULT_CONTEXT_WINDOW,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            configured_output_tokens: None,
            source: LimitSource::Default,
        }
    }
}

impl ModelLimits {
    /// Synchronous resolution from the model name plus optional overrides. This
    /// covers the common case without a network call; detection can upgrade it
    /// afterward via [`Self::apply_detected`].
    pub fn resolve_from_name(
        model: &str,
        context_override: Option<usize>,
        output_override: Option<usize>,
    ) -> Self {
        let (mut context, mut output, mut source) = match known_limits(model) {
            Some((context, output)) => (context, output, LimitSource::Heuristic),
            None => (
                DEFAULT_CONTEXT_WINDOW,
                DEFAULT_MAX_OUTPUT_TOKENS,
                LimitSource::Default,
            ),
        };
        let mut configured = None;
        if let Some(value) = context_override {
            context = value;
            source = LimitSource::Override;
        }
        if let Some(value) = output_override {
            output = value;
            source = LimitSource::Override;
            configured = Some(value);
        }
        Self {
            context_window: context,
            max_output_tokens: output,
            configured_output_tokens: configured,
            source,
        }
    }

    /// Fold in server-detected limits. Never overrides an explicit user override,
    /// and never *shrinks* a recognized family's published context window: many
    /// servers (Ollama especially) self-report a default `num_ctx` like 4k that
    /// would otherwise make us compact hundreds of times too early on a model
    /// that actually handles far more. Detection still raises the window and
    /// still applies in full to unrecognized models (the conservative default).
    pub fn apply_detected(&mut self, context: usize, output: Option<usize>) {
        if self.source == LimitSource::Override {
            return;
        }
        if self.source == LimitSource::Heuristic && context < self.context_window {
            return;
        }
        self.context_window = context;
        if let Some(output) = output {
            self.max_output_tokens = output;
            self.configured_output_tokens = Some(output);
        }
        self.source = LimitSource::Detected;
    }

    /// Compaction budgets (in chars) derived from these limits.
    pub fn compaction_budget(&self) -> CompactionBudget {
        CompactionBudget::from_limits(*self)
    }
}

/// Char-based thresholds consumed by the compaction pass.
#[derive(Debug, Clone, Copy)]
pub struct CompactionBudget {
    /// Compact when live context exceeds this many chars (~80% of usable input).
    pub compact_at_chars: usize,
    /// Verbatim recent window kept after a full compaction (~30% of usable input).
    pub recent_budget_chars: usize,
}

impl Default for CompactionBudget {
    fn default() -> Self {
        ModelLimits::default().compaction_budget()
    }
}

impl CompactionBudget {
    pub fn from_limits(limits: ModelLimits) -> Self {
        let usable_tokens = limits
            .context_window
            .saturating_sub(limits.max_output_tokens)
            .saturating_sub(RESERVED_TOKENS);
        let usable_chars = usable_tokens.saturating_mul(CHARS_PER_TOKEN);
        Self {
            compact_at_chars: usable_chars.saturating_mul(4) / 5,
            recent_budget_chars: usable_chars.saturating_mul(3) / 10,
        }
    }
}

/// Heuristic table for *currently shipping* frontier model families, with
/// 2026 specs verified against each vendor's primary docs. Two rules keep it
/// from going stale as vendors ship new minor versions:
///
/// 1. **Match by family, not by pinned version.** We match on the family name
///    (`gpt-5`, `gemini`, `claude`), not strings like `gpt-5.5` or
///    `gemini-3.5-flash`. A new minor still matches.
/// 2. **Detection is authoritative on top.** `/models` overrides these when the
///    server exposes `context_length` / `max_completion_tokens`; the 128k/8k
///    default is a safe floor for anything unrecognized.
///
/// Only live families are listed. Pulled/dead models (gpt-4, gpt-3.5, gpt-4o,
/// claude-2, etc.) are intentionally absent — nobody should be running them, and
/// if someone is, detection or the conservative default handles it rather than
/// a stale entry. The output estimate is the documented max output, used only to
/// size the compaction budget (reserving room for a maximal reply) — it is
/// **not** sent to the provider unless overridden or detected, so a value that
/// is slightly high only makes us compact a touch early (the safe direction),
/// never truncates the model. Specs are as of June 2026 and overridable.
fn known_limits(model: &str) -> Option<(usize, usize)> {
    let name = model.to_ascii_lowercase();
    // GPT-5.x: ~1.05M context, 128k max output (OpenAI API docs).
    if name.contains("gpt-5") {
        return Some((1_000_000, 128_000));
    }
    // Gemini ≥1.5: ~1M context (1,048,576). 3.5 Flash outputs 65k; Pro is ≥ that.
    if name.contains("gemini") {
        return Some((1_000_000, 65_536));
    }
    // Claude 3/4/5: 1M context (on by default), 128k synchronous max output.
    if name.contains("claude") {
        return Some((1_000_000, 128_000));
    }
    // DeepSeek V4 (V4-Pro / V4-Flash; legacy deepseek-chat/deepseek-reasoner
    // route here as of April 2026 and retire July 2026): 1M context, 384k max
    // output (DeepSeek V4 API docs + technical report). Pinned to the `v4`
    // string so an older V3 deployment (128k) is not over-sized — V3 falls to
    // detection or the 128k default, which is correct for it.
    if name.contains("deepseek-v4") {
        return Some((1_000_000, 384_000));
    }
    // GLM-5.2: 1M context, 131k max output (Z.ai blog, June 2026). Checked before
    // the broader GLM-5 family since `glm-5.2` contains `glm-5`.
    if name.contains("glm-5.2") {
        return Some((1_000_000, 131_072));
    }
    // GLM-5 / GLM-5.1: 200k context, 131k max output (Z.ai docs).
    if name.contains("glm-5") {
        return Some((200_000, 131_072));
    }
    // Kimi K2.x (K2.5/K2.6/thinking/turbo): 256k context, 32k default max output
    // (Moonshot Kimi API docs). Pinned to the K2 family — K1.x had a smaller
    // window and must not be over-sized.
    if name.contains("kimi-k2") {
        return Some((262_144, 32_768));
    }
    // Qwen3-Coder (all sizes): 256k native context, 65k recommended max output
    // (Alibaba Qwen3-Coder model card). Plain Qwen3 (non-coder) is ~131k and
    // is left to detection / the 128k default.
    if name.contains("qwen3-coder") {
        return Some((262_144, 65_536));
    }
    // GLM-4.6/4.7: 200k context, 128k max output (Z.ai docs). Version-pinned to
    // ≥4.6 — GLM-4.5 is 128k, so matching the bare `glm` family would over-size
    // it. GLM-4.5 instead falls to the 128k default, which is exactly right.
    if name.contains("glm-4.6") || name.contains("glm-4.7") {
        return Some((200_000, 128_000));
    }
    // DeepSeek V3 is 128k/8k — identical to the conservative default, so it is
    // intentionally not listed (detection or the default handles it; R1's 64k
    // window should be set explicitly or detected rather than guessed).
    None
}

/// Best-effort detection of a model's context window and output cap from the
/// provider's `/models` endpoint. Returns `None` on any failure — callers fall
/// back to the heuristic/default. Supports common response shapes: OpenAI /
/// OpenRouter (`data`), servers that key the list under `models`, a bare root
/// array, and per-model `context_length` as either a number or a `k`/`m`-
/// suffixed string.
pub async fn detect_limits(
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
) -> Option<(usize, Option<usize>)> {
    // Short timeout: this is a non-blocking best-effort pre-flight, and a server
    // that doesn't implement /models would otherwise stall every launch.
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .user_agent(concat!("abacus-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let mut request = client
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .header(header::ACCEPT, "application/json");
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let value: Value = response.json().await.ok()?;
    extract_limits_from_models(&value, model)
}

/// Pure, network-free extraction of a model's limits from a parsed `/models`
/// body. Split out from [`detect_limits`] so the parsing logic is unit-testable
/// without standing up a mock server.
fn extract_limits_from_models(value: &Value, model: &str) -> Option<(usize, Option<usize>)> {
    let data = value["data"]
        .as_array()
        .or_else(|| value["models"].as_array())
        .or_else(|| value.as_array())?;
    let target = model.to_ascii_lowercase();
    let entry = data
        .iter()
        .find(|item| {
            item["id"]
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case(model))
        })
        .or_else(|| {
            data.iter().find(|item| {
                item["id"]
                    .as_str()
                    .is_some_and(|id| id.to_ascii_lowercase().ends_with(&target))
            })
        })?;
    let context = read_usize(
        entry,
        &["context_length", "max_context_length", "context_window"],
    )?;
    let output = read_usize(entry, &["max_completion_tokens", "max_output_tokens"]).or_else(|| {
        entry
            .get("top_provider")
            .and_then(|tp| read_usize(tp, &["max_completion_tokens", "max_output_tokens"]))
    });
    Some((context, output))
}

fn read_usize(value: &Value, keys: &[&str]) -> Option<usize> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(usize_from_value))
}

/// Read a usize from a JSON value that is either a number or a `k`/`m`-suffixed
/// numeric string (some servers expose `context_length` as `"128000"`).
fn usize_from_value(v: &Value) -> Option<usize> {
    v.as_u64()
        .map(|n| n as usize)
        .or_else(|| v.as_str().and_then(|s| parse_tokens(s).ok()))
}

/// Parse a token count from a CLI/settings string, tolerating `k`/`m` suffixes
/// (e.g. `128k`, `1m`, `200000`).
pub fn parse_tokens(input: &str) -> Result<usize> {
    let trimmed = input.trim().to_ascii_lowercase();
    let (digits, multiplier) = match trimmed.chars().last() {
        Some('k') => (&trimmed[..trimmed.len() - 1], 1_000),
        Some('m') => (&trimmed[..trimmed.len() - 1], 1_000_000),
        _ => (trimmed.as_str(), 1),
    };
    let value: usize = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid token count `{input}` (try `128k` or `200000`)"))?;
    Ok(value.saturating_mul(multiplier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_beats_table_and_default() {
        // claude is in the heuristic table (1M); an explicit override must win.
        let limits = ModelLimits::resolve_from_name("claude-opus-4-8", Some(200_000), Some(16_000));
        assert_eq!(limits.context_window, 200_000);
        assert_eq!(limits.max_output_tokens, 16_000);
        assert_eq!(limits.configured_output_tokens, Some(16_000));
        assert_eq!(limits.source, LimitSource::Override);
    }

    #[test]
    fn family_match_is_version_agnostic() {
        // Future minor versions must still match the family heuristic — the
        // table pins families, not version strings, so it does not go stale.
        let claude = ModelLimits::resolve_from_name("claude-opus-4-8-20260101", None, None);
        assert_eq!(claude.context_window, 1_000_000);
        assert_eq!(claude.max_output_tokens, 128_000);
        assert_eq!(claude.source, LimitSource::Heuristic);
        let gemini = ModelLimits::resolve_from_name("gemini-3.5-pro-preview", None, None);
        assert_eq!(gemini.context_window, 1_000_000);
        assert_eq!(gemini.max_output_tokens, 65_536);
        assert_eq!(gemini.source, LimitSource::Heuristic);
        let gpt5 = ModelLimits::resolve_from_name("gpt-5.5", None, None);
        assert_eq!(gpt5.context_window, 1_000_000);
        assert_eq!(gpt5.max_output_tokens, 128_000);
        assert_eq!(gpt5.source, LimitSource::Heuristic);
    }

    #[test]
    fn unknown_frontier_falls_to_default_not_a_guess() {
        // A genuinely unrecognized model must fall to the conservative default
        // (safe: compacts early) and rely on detection, rather than a stale
        // hardcoded number for a model we haven't verified.
        let limits = ModelLimits::resolve_from_name("acme-frontier-internal", None, None);
        assert_eq!(limits.context_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(limits.source, LimitSource::Default);
    }

    #[test]
    fn unknown_model_uses_conservative_default() {
        let limits = ModelLimits::resolve_from_name("some-unknown-model-7b", None, None);
        assert_eq!(limits.context_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(limits.source, LimitSource::Default);
    }

    #[test]
    fn detected_limits_do_not_override_explicit_override() {
        let mut limits = ModelLimits::resolve_from_name("claude-opus-4-8", Some(150_000), None);
        limits.apply_detected(200_000, Some(8_192));
        assert_eq!(limits.context_window, 150_000, "override must win");
        assert_eq!(limits.source, LimitSource::Override);
    }

    #[test]
    fn detected_limits_upgrade_heuristic() {
        let mut limits = ModelLimits::resolve_from_name("unknown-via-proxy", None, None);
        limits.apply_detected(256_000, Some(12_000));
        assert_eq!(limits.context_window, 256_000);
        assert_eq!(limits.configured_output_tokens, Some(12_000));
        assert_eq!(limits.source, LimitSource::Detected);
    }

    #[test]
    fn budgets_scale_with_context_window() {
        // An unrecognized model falls to the conservative 128k default (small);
        // a 1M-context model compacts far later (large).
        let small = ModelLimits::resolve_from_name("deepseek-v3", None, None).compaction_budget();
        let large = ModelLimits {
            context_window: 1_000_000,
            max_output_tokens: 8_192,
            configured_output_tokens: None,
            source: LimitSource::Heuristic,
        }
        .compaction_budget();
        assert!(small.compact_at_chars < large.compact_at_chars);
        assert!(
            large.compact_at_chars > 3_000_000,
            "1M window should compact late"
        );
        // recent window must be smaller than the trigger.
        assert!(small.recent_budget_chars < small.compact_at_chars);
    }

    #[test]
    fn default_budget_is_safe_for_128k() {
        let budget = CompactionBudget::default();
        // Roughly the old hardcoded values (~400k trigger, ~140k recent).
        assert!(budget.compact_at_chars > 300_000 && budget.compact_at_chars < 450_000);
        assert!(budget.recent_budget_chars > 100_000 && budget.recent_budget_chars < 160_000);
    }

    #[test]
    fn parse_tokens_accepts_suffixes() {
        assert_eq!(parse_tokens("128k").unwrap(), 128_000);
        assert_eq!(parse_tokens("1m").unwrap(), 1_000_000);
        assert_eq!(parse_tokens("200000").unwrap(), 200_000);
        assert!(parse_tokens("abc").is_err());
    }

    #[test]
    fn open_frontier_families_have_verified_limits() {
        // Specs verified June 2026 against each vendor's docs. These exceed the
        // 128k default, so the heuristic genuinely defers compaction for them.
        let qwen = ModelLimits::resolve_from_name("Qwen3-Coder-480B-A35B-Instruct", None, None);
        assert_eq!(qwen.context_window, 262_144);
        assert_eq!(qwen.max_output_tokens, 65_536);
        assert_eq!(qwen.source, LimitSource::Heuristic);

        let kimi = ModelLimits::resolve_from_name("kimi-k2.6", None, None);
        assert_eq!(kimi.context_window, 262_144);
        assert_eq!(kimi.max_output_tokens, 32_768);
        assert_eq!(kimi.source, LimitSource::Heuristic);

        let glm = ModelLimits::resolve_from_name("glm-4.6", None, None);
        assert_eq!(glm.context_window, 200_000);
        assert_eq!(glm.max_output_tokens, 128_000);
        assert_eq!(glm.source, LimitSource::Heuristic);

        // DeepSeek V4 (April 2026) and GLM-5.2 (June 2026): both 1M context.
        let ds = ModelLimits::resolve_from_name("deepseek-v4-flash", None, None);
        assert_eq!(ds.context_window, 1_000_000);
        assert_eq!(ds.max_output_tokens, 384_000);
        assert_eq!(ds.source, LimitSource::Heuristic);

        let glm52 = ModelLimits::resolve_from_name("glm-5.2", None, None);
        assert_eq!(glm52.context_window, 1_000_000);
        assert_eq!(glm52.max_output_tokens, 131_072);
        assert_eq!(glm52.source, LimitSource::Heuristic);

        // GLM-5 / 5.1 stayed at 200k before 5.2's jump to 1M.
        let glm5 = ModelLimits::resolve_from_name("glm-5.1", None, None);
        assert_eq!(glm5.context_window, 200_000);
        assert_eq!(glm5.max_output_tokens, 131_072);

        // Heuristic limits are sizing-only: nothing is sent to the provider.
        assert_eq!(qwen.configured_output_tokens, None);
        assert_eq!(kimi.configured_output_tokens, None);
        assert_eq!(glm.configured_output_tokens, None);
        assert_eq!(ds.configured_output_tokens, None);
        assert_eq!(glm52.configured_output_tokens, None);
    }

    #[test]
    fn glm_4_5_is_not_over_sized_by_the_4_6_entry() {
        // GLM-4.5 is 128k; the 200k entry is pinned to >=4.6 so 4.5 falls to the
        // conservative default rather than being over-sized (which would risk
        // overflow by deferring compaction past its real window).
        let glm45 = ModelLimits::resolve_from_name("glm-4.5", None, None);
        assert_eq!(glm45.context_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(glm45.source, LimitSource::Default);
    }

    #[test]
    fn deepseek_v3_is_not_over_sized_by_the_v4_entry() {
        // DeepSeek V3 is 128k; the 1M V4 entry is pinned to `deepseek-v4` so V3
        // is not over-sized — it falls to detection / the 128k default.
        let v3 = ModelLimits::resolve_from_name("deepseek-v3", None, None);
        assert_eq!(v3.context_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(v3.source, LimitSource::Default);
    }

    #[test]
    fn extract_limits_reads_openrouter_shape_and_string_context() {
        // OpenRouter-style: `data` array, output nested under `top_provider`.
        let body = serde_json::json!({
            "data": [{
                "id": "deepseek/deepseek-v3",
                "context_length": 128_000,
                "top_provider": { "max_completion_tokens": 8_192 }
            }]
        });
        let (ctx, out) = extract_limits_from_models(&body, "deepseek/deepseek-v3").unwrap();
        assert_eq!(ctx, 128_000);
        assert_eq!(out, Some(8_192));
    }

    #[test]
    fn extract_limits_handles_models_key_and_string_context() {
        // A server that keys the list under `models` and exposes context as a
        // `k`-suffixed string.
        let body = serde_json::json!({
            "models": [{
                "id": "qwen3-coder",
                "context_length": "256k",
                "max_output_tokens": "65536"
            }]
        });
        let (ctx, out) = extract_limits_from_models(&body, "qwen3-coder").unwrap();
        assert_eq!(ctx, 256_000);
        assert_eq!(out, Some(65_536));
    }

    #[test]
    fn extract_limits_returns_none_without_context_length() {
        // DashScope-style: a models list with no context field. We refuse to
        // guess and let the caller fall back to the heuristic/default.
        let body = serde_json::json!({ "data": [{ "id": "qwen-max" }] });
        assert!(extract_limits_from_models(&body, "qwen-max").is_none());
    }
}
