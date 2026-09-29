//! Client-side tool-call text parsing for models that emit tool calls as text
//! instead of native `tool_calls` (common for open-weight models served via
//! Ollama, llama.cpp, raw vLLM `/generate`, or providers that ignore the
//! `tools` parameter).
//!
//! Mirrors NousResearch's `hermes-agent` (`environments/tool_call_parsers/`)
//! and vLLM's `vllm/tool_parsers/`, each of which reimplements a model family's
//! `extract_tool_calls()` client-side. We do the same so abacus works with
//! Hermes, Qwen/Qwen3, Llama 3, Mistral, GLM, Kimi K2 and DeepSeek text formats
//! without relying on the server to parse.
//!
//! Integration: the provider tries native `tool_calls` first. Only when a
//! completion returns *no* native tool calls do we run the selected parser over
//! the assistant text and lift any tool calls into the same `tool_calls` the
//! agent already dispatches — so the agent loop is untouched. Parsed tool-call
//! text is stripped from `content` (prose reasoning is kept).

use serde_json::Value;

/// A single tool call parsed from model text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedToolCall {
    pub name: String,
    /// JSON object string, e.g. `{"path":"src/main.rs"}`.
    pub arguments: String,
}

/// Render a single Hermes-format tool call as model text, the inverse of
/// `parse(ToolFormat::Hermes, ..)`. Useful for building few-shot examples or
/// fixtures for Hermes-trained open-weight models. `arguments_json` is
/// embedded verbatim and must be a valid JSON object string.
pub fn render_hermes_call(name: &str, arguments_json: &str) -> String {
    format!("{HERMES_OPEN}{{\"name\":\"{name}\",\"arguments\":{arguments_json}}}{HERMES_CLOSE}")
}

/// Which text format to parse, or `Auto` to detect from the content, or `None`
/// to disable the text fallback (native `tool_calls` only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolFormat {
    /// Native `tool_calls` only — never parse text.
    None,
    /// Try native first; if absent, detect the family from delimiters. Does not
    /// run the generic JSON heuristic (avoids false positives on prose).
    #[default]
    Auto,
    Hermes,
    Qwen,
    Llama3Json,
    Mistral,
    Glm,
    Kimi,
    DeepSeek,
    /// Explicit generic JSON tool-call object/array (whole content or fenced
    /// block). Only used when explicitly selected — `Auto` will not pick it.
    Json,
}

impl ToolFormat {
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "native" => Some(Self::None),
            "auto" | "automatic" => Some(Self::Auto),
            "hermes" => Some(Self::Hermes),
            "qwen" | "qwen3" | "qwen3-coder" => Some(Self::Qwen),
            "llama3" | "llama3_json" | "llama3-json" | "llama" => Some(Self::Llama3Json),
            "mistral" => Some(Self::Mistral),
            "glm" | "glm45" | "glm47" => Some(Self::Glm),
            "kimi" | "kimi_k2" | "kimi-k2" => Some(Self::Kimi),
            "deepseek" | "deepseek_v3" | "deepseek-v3" => Some(Self::DeepSeek),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    pub fn as_arg(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Auto => "auto",
            Self::Hermes => "hermes",
            Self::Qwen => "qwen",
            Self::Llama3Json => "llama3_json",
            Self::Mistral => "mistral",
            Self::Glm => "glm",
            Self::Kimi => "kimi",
            Self::DeepSeek => "deepseek",
            Self::Json => "json",
        }
    }
}

/// Byte offset of the first tool-call marker in `text` for `format`, if any.
///
/// Used to stop streaming text to the transcript at the point the model starts
/// emitting tool markup. Parsing only runs once the stream finishes, so without
/// this the user watches raw `<tool_call>{…}` scroll past and the transcript
/// ends up permanently different from the history that was saved.
pub fn marker_index(format: ToolFormat, text: &str) -> Option<usize> {
    const ALL: &[&str] = &[
        HERMES_OPEN,
        QWEN_OPEN,
        FUNC_PREFIX,
        PYTHON_TAG,
        KIMI_SECTION_BEGIN,
        DEEPSEEK_CALLS_BEGIN,
        MISTRAL_MARKER,
    ];
    let markers: &[&str] = match format {
        ToolFormat::None => return None,
        ToolFormat::Auto => ALL,
        ToolFormat::Hermes => &[HERMES_OPEN],
        ToolFormat::Qwen | ToolFormat::Glm => &[QWEN_OPEN, FUNC_PREFIX],
        ToolFormat::Llama3Json => &[PYTHON_TAG, HERMES_OPEN],
        ToolFormat::Mistral => &[MISTRAL_MARKER],
        ToolFormat::Kimi => &[KIMI_SECTION_BEGIN],
        ToolFormat::DeepSeek => &[DEEPSEEK_CALLS_BEGIN],
        ToolFormat::Json => &[HERMES_OPEN],
    };
    markers.iter().filter_map(|marker| text.find(marker)).min()
}

/// Parse `raw` assistant text under `format`, returning the cleaned prose
/// (tool-call blocks removed) and any tool calls found.
pub fn parse(format: ToolFormat, raw: &str) -> (String, Vec<ParsedToolCall>) {
    match format {
        ToolFormat::None => (raw.to_owned(), Vec::new()),
        ToolFormat::Auto => parse_auto(raw),
        ToolFormat::Hermes => parse_hermes(raw),
        ToolFormat::Qwen => parse_qwen(raw),
        ToolFormat::Llama3Json => parse_llama3(raw),
        ToolFormat::Mistral => parse_mistral(raw),
        ToolFormat::Glm => parse_glm(raw),
        ToolFormat::Kimi => parse_kimi(raw),
        ToolFormat::DeepSeek => parse_deepseek(raw),
        ToolFormat::Json => parse_json_explicit(raw),
    }
}

fn parse_auto(raw: &str) -> (String, Vec<ParsedToolCall>) {
    // Order by delimiter specificity. No generic-JSON fallback — that is opt-in
    // via `Json` to avoid mistaking prose for a tool call.
    if raw.contains(KIMI_SECTION_BEGIN) {
        return parse_kimi(raw);
    }
    if raw.contains(DEEPSEEK_CALLS_BEGIN) {
        return parse_deepseek(raw);
    }
    if raw.contains(MISTRAL_MARKER) {
        return parse_mistral(raw);
    }
    if raw.contains(HERMES_OPEN) {
        return parse_hermes(raw);
    }
    // GLM and Qwen3-coder both wrap calls in a `<tool_calls>` block; GLM uses
    // an `<invoke>` tag inside, Qwen uses `<function=...>`. Check the wrapper
    // before the bare `<function=` Llama check below, since Qwen also emits
    // `<function=...>` (but inside the wrapper).
    if raw.contains(QWEN_OPEN) {
        if raw.contains(INVOKE_PREFIX) {
            return parse_glm(raw);
        }
        return parse_qwen(raw);
    }
    if raw.contains(PYTHON_TAG) || raw.contains(FUNC_PREFIX) {
        return parse_llama3(raw);
    }
    (raw.to_owned(), Vec::new())
}

// ----- shared helpers -----

/// Coerce a Qwen/GLM parameter value: `10` → int, `true` → bool, `[1,2]` →
/// array, a bare word → string. A bare word is not valid JSON, so it naturally
/// falls through to the string branch.
fn coerce_value(s: &str) -> Value {
    match serde_json::from_str::<Value>(s.trim()) {
        Ok(value) if !value.is_string() => value,
        _ => Value::String(s.trim().to_owned()),
    }
}

/// A call to `name`, provided `arguments` is the text of a JSON object.
fn make_call(name: &str, arguments: &str) -> Option<ParsedToolCall> {
    let name = name.trim();
    let value: Value = serde_json::from_str(arguments.trim()).ok()?;
    (!name.is_empty() && value.is_object())
        .then(|| ParsedToolCall { name: name.to_owned(), arguments: value.to_string() })
}

/// A `{"name": …, "arguments": {…}}` value as a call. Some models say
/// `parameters` instead; whichever holds an object is taken.
fn json_call(value: &Value) -> Option<ParsedToolCall> {
    let name = value.get("name")?.as_str()?.to_owned();
    let arguments = ["arguments", "parameters"]
        .iter()
        .filter_map(|key| value.get(key))
        .find(|arguments| arguments.is_object())?;
    Some(ParsedToolCall { name, arguments: arguments.to_string() })
}

/// The call in a text that is one JSON call object.
fn json_text_call(text: &str) -> Option<ParsedToolCall> {
    json_call(&serde_json::from_str(text.trim()).ok()?)
}

/// The calls in a text that is one JSON call object or an array of them.
fn json_calls(text: &str) -> Vec<ParsedToolCall> {
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(Value::Array(items)) => items.iter().filter_map(json_call).collect(),
        Ok(value) => json_call(&value).into_iter().collect(),
        Err(_) => Vec::new(),
    }
}

/// Strip every `opener`…`closer` block out of `raw` and hand each block body to
/// `extract`. An opener that is never closed stays in the text as written.
///
/// `extract` returns a *list*: one block can legitimately hold several calls —
/// GLM and Qwen emit parallel calls as sibling `<invoke>` elements inside a
/// single wrapper — and the block has already been removed from `clean` by the
/// time it is parsed, so anything the extractor drops is lost silently.
fn extract_tag_blocks(
    raw: &str,
    opener: &str,
    closer: &str,
    extract: impl Fn(&str) -> Vec<ParsedToolCall>,
) -> (String, Vec<ParsedToolCall>) {
    let mut calls = Vec::new();
    let mut clean = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find(opener) {
        let after = &rest[start + opener.len()..];
        let Some(end) = after.find(closer) else { break };
        clean.push_str(&rest[..start]);
        calls.extend(extract(&after[..end]));
        rest = &after[end + closer.len()..];
    }
    clean.push_str(rest);
    (clean, calls)
}

/// Every `<prefix…>body<close>` element of `text`, as its opening tag and body.
/// Stops at the first element that is cut short.
fn elements<'a>(text: &'a str, prefix: &str, close: &str) -> Vec<(&'a str, &'a str)> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(prefix) {
        let Some((tag, inner)) = rest[start..].split_once('>') else { break };
        let Some(end) = inner.find(close) else { break };
        found.push((tag, &inner[..end]));
        rest = &inner[end + close.len()..];
    }
    found
}

/// The calls written as elements: `open`…`close` is one call, each `param`
/// element inside it one argument, and `name_of` reads a name out of an opening
/// tag. An element without a name is skipped; its siblings are kept.
fn tagged_calls(
    text: &str,
    (open, close, param): (&str, &str, &str),
    name_of: fn(&str) -> Option<String>,
) -> Vec<ParsedToolCall> {
    elements(text, open, close)
        .into_iter()
        .filter_map(|(tag, body)| {
            let arguments: serde_json::Map<String, Value> = elements(body, param, PARAM_CLOSE)
                .into_iter()
                .filter_map(|(tag, value)| Some((name_of(tag)?, coerce_value(value))))
                .collect();
            Some(ParsedToolCall {
                name: name_of(tag)?,
                arguments: Value::Object(arguments).to_string(),
            })
        })
        .collect()
}

/// The name in a `<tag=name` opening tag.
fn assigned_name(tag: &str) -> Option<String> {
    Some(tag.split_once('=')?.1.to_owned())
}

/// The name in a `<tag name="…"` opening tag.
fn named_attribute(tag: &str) -> Option<String> {
    Some(tag.split_once("name=\"")?.1.split_once('"')?.0.to_owned())
}

/// The special tokens of a format that fences all its calls into one section.
struct Fence {
    begin: &'static str,
    end: &'static str,
    call: &'static str,
    arguments: &'static str,
    call_end: &'static str,
}

/// Cut the fenced section out of `raw` and read the calls inside it, passing
/// each name through `name_of`.
fn parse_fenced(
    raw: &str,
    fence: &Fence,
    name_of: fn(&str) -> &str,
) -> (String, Vec<ParsedToolCall>) {
    let Some(start) = raw.find(fence.begin) else {
        return (raw.to_owned(), Vec::new());
    };
    // Two different offsets: where the section's *content* stops, and where the
    // surrounding prose resumes. Using one for both left the literal end marker
    // at the head of the visible content.
    let (stop, resume) = match raw[start..].find(fence.end) {
        Some(offset) => (start + offset, start + offset + fence.end.len()),
        None => (raw.len(), raw.len()),
    };
    let mut calls = Vec::new();
    let mut rest = &raw[start..stop];
    while let Some((_, call)) = rest.split_once(fence.call) {
        let Some((name, after)) = call.split_once(fence.arguments) else { break };
        let Some((arguments, tail)) = after.split_once(fence.call_end) else { break };
        calls.extend(make_call(name_of(name.trim()), arguments));
        rest = tail;
    }
    (format!("{}{}", &raw[..start], &raw[resume..]), calls)
}

// ----- Tag literals. The first character after `<` is hex-escaped so the
// source file never contains a literal tool-call delimiter (which would
// collide with transport framing); Rust still compiles these to the real tag.

const HERMES_OPEN: &str = "<\x74ool_call>";
const HERMES_CLOSE: &str = "</\x74ool_call>";
const QWEN_OPEN: &str = "<\x74ool_calls>";
const QWEN_CLOSE: &str = "</\x74ool_calls>";
const FUNC_PREFIX: &str = "<\x66unction=";
const FUNC_CLOSE: &str = "</\x66unction>";
const PARAM_PREFIX: &str = "<\x70arameter";
const PARAM_PREFIX_EQ: &str = "<\x70arameter=";
const PARAM_CLOSE: &str = "</\x70arameter>";
const INVOKE_PREFIX: &str = "<\x69nvoke";
const INVOKE_CLOSE: &str = "</\x69nvoke>";
const PYTHON_TAG: &str = "<\x7cpython_tag\x7c>";
const MISTRAL_MARKER: &str = "[TOOL_CALLS]";
const KIMI_SECTION_BEGIN: &str = "<\x7ctool_calls_section_begin\x7c>";
const KIMI_SECTION_END: &str = "<\x7ctool_calls_section_end\x7c>";
const KIMI_CALL_BEGIN: &str = "<\x7ctool_call_begin\x7c>";
const KIMI_CALL_ARG_BEGIN: &str = "<\x7ctool_call_argument_begin\x7c>";
const KIMI_CALL_END: &str = "<\x7ctool_call_end\x7c>";
const DEEPSEEK_CALLS_BEGIN: &str = "<\u{ff5c}tool\u{2581}calls\u{2581}begin\u{ff5c}>";
const DEEPSEEK_CALLS_END: &str = "<\u{ff5c}tool\u{2581}calls\u{2581}end\u{ff5c}>";
const DEEPSEEK_CALL_BEGIN: &str = "<\u{ff5c}tool\u{2581}call\u{2581}begin\u{ff5c}>";
const DEEPSEEK_CALL_ARG_BEGIN: &str =
    "<\u{ff5c}tool\u{2581}call\u{2581}argument\u{2581}begin\u{ff5c}>";
const DEEPSEEK_CALL_END: &str = "<\u{ff5c}tool\u{2581}call\u{2581}end\u{ff5c}>";

const KIMI: Fence = Fence {
    begin: KIMI_SECTION_BEGIN,
    end: KIMI_SECTION_END,
    call: KIMI_CALL_BEGIN,
    arguments: KIMI_CALL_ARG_BEGIN,
    call_end: KIMI_CALL_END,
};
const DEEPSEEK: Fence = Fence {
    begin: DEEPSEEK_CALLS_BEGIN,
    end: DEEPSEEK_CALLS_END,
    call: DEEPSEEK_CALL_BEGIN,
    arguments: DEEPSEEK_CALL_ARG_BEGIN,
    call_end: DEEPSEEK_CALL_END,
};

// ----- Hermes: HERMES_OPEN{json}HERMES_CLOSE -----

fn parse_hermes(raw: &str) -> (String, Vec<ParsedToolCall>) {
    extract_tag_blocks(raw, HERMES_OPEN, HERMES_CLOSE, |body| {
        json_text_call(body).into_iter().collect()
    })
}

// ----- Llama 3: PYTHON_TAG{json} or HERMES_OPEN{json}HERMES_CLOSE -----

fn parse_llama3(raw: &str) -> (String, Vec<ParsedToolCall>) {
    let mut calls = Vec::new();
    let mut clean = String::new();
    let mut rest = raw;
    while let Some((before, after)) = rest.split_once(PYTHON_TAG) {
        clean.push_str(before);
        let end = after.find('\n').unwrap_or(after.len());
        calls.extend(json_text_call(&after[..end]));
        rest = &after[end..];
    }
    let (tail, tagged) = parse_hermes(rest);
    clean.push_str(&tail);
    calls.extend(tagged);
    if !calls.is_empty() {
        return (clean, calls);
    }
    // Bare trailing JSON object (Llama models sometimes emit one with no tag).
    match json_text_call(rest) {
        Some(call) => (String::new(), vec![call]),
        None => (raw.to_owned(), Vec::new()),
    }
}

// ----- Mistral: [TOOL_CALLS][{...}, ...] -----

fn parse_mistral(raw: &str) -> (String, Vec<ParsedToolCall>) {
    let Some((before, payload)) = raw.split_once(MISTRAL_MARKER) else {
        return (raw.to_owned(), Vec::new());
    };
    let (calls, consumed) = parse_json_call_array(payload);
    // Remove the marker *and* the array it introduces. Stripping only the
    // marker left the whole JSON payload in the assistant's prose, where it was
    // rendered to the user and written to history.
    (format!("{before}{}", &payload[consumed..]), calls)
}

/// Parse a leading `[{...}, ...]` tool-call array, returning the calls and how
/// many bytes of `payload` the array occupied.
///
/// Uses a real JSON reader rather than counting brackets: a byte-level scan
/// closes the array at the first `]`, including one inside a string value. For
/// a coding agent that fires constantly — `{"pattern":"[a-z]+"}` was enough to
/// drop every call in the array.
fn parse_json_call_array(payload: &str) -> (Vec<ParsedToolCall>, usize) {
    let Some(start) = payload.find('[') else {
        return (Vec::new(), 0);
    };
    let mut stream = serde_json::Deserializer::from_str(&payload[start..]).into_iter::<Value>();
    let Some(Ok(value)) = stream.next() else {
        return (Vec::new(), 0);
    };
    let calls = value.as_array().into_iter().flatten().filter_map(json_call).collect();
    (calls, start + stream.byte_offset())
}

// ----- Qwen / Qwen3-coder: QWEN_OPEN FUNC_PREFIXname> PARAM_PREFIX_EQk>v PARAM_CLOSE FUNC_CLOSE QWEN_CLOSE -----

fn parse_qwen(raw: &str) -> (String, Vec<ParsedToolCall>) {
    // Collect every function call across the whole text (a single QWEN_OPEN
    // block may hold several), then keep the prose around the blocks.
    let calls = tagged_calls(raw, (FUNC_PREFIX, FUNC_CLOSE, PARAM_PREFIX_EQ), assigned_name);
    if raw.contains(QWEN_OPEN) {
        (extract_tag_blocks(raw, QWEN_OPEN, QWEN_CLOSE, |_| Vec::new()).0, calls)
    } else if calls.is_empty() {
        (raw.to_owned(), calls)
    } else {
        (String::new(), calls)
    }
}

// ----- GLM: QWEN_OPEN INVOKE_PREFIX name="x"> PARAM_PREFIX name="k">v PARAM_CLOSE INVOKE_CLOSE QWEN_CLOSE -----

fn parse_glm(raw: &str) -> (String, Vec<ParsedToolCall>) {
    extract_tag_blocks(raw, QWEN_OPEN, QWEN_CLOSE, |body| {
        tagged_calls(body, (INVOKE_PREFIX, INVOKE_CLOSE, PARAM_PREFIX), named_attribute)
    })
}

// ----- Kimi K2 and DeepSeek: special-token-delimited sections -----

fn parse_kimi(raw: &str) -> (String, Vec<ParsedToolCall>) {
    // Kimi emits `functions.get_weather:0`; strip the `:N` id suffix and any
    // `functions.` prefix.
    parse_fenced(raw, &KIMI, |name| {
        let name = name.rsplit_once(':').map_or(name, |(name, _)| name);
        name.strip_prefix("functions.").unwrap_or(name)
    })
}

fn parse_deepseek(raw: &str) -> (String, Vec<ParsedToolCall>) {
    parse_fenced(raw, &DEEPSEEK, |name| name)
}

// ----- Explicit generic JSON (whole content or fenced block) -----

fn parse_json_explicit(raw: &str) -> (String, Vec<ParsedToolCall>) {
    let (clean, calls) = extract_tag_blocks(raw, "```json", "```", json_calls);
    if !calls.is_empty() {
        return (clean, calls);
    }
    match json_calls(raw) {
        whole if whole.is_empty() => (raw.to_owned(), whole),
        whole => (String::new(), whole),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &str) -> ParsedToolCall {
        ParsedToolCall { name: name.to_owned(), arguments: args.to_owned() }
    }

    fn hermes_call(name: &str, args_key: &str, args: &str) -> String {
        format!("{HERMES_OPEN}{{\"name\":\"{name}\",\"{args_key}\":{args}}}{HERMES_CLOSE}")
    }

    #[test]
    fn hermes_single_call() {
        let raw = format!(
            "Let me look.\n\n{}\n",
            hermes_call("read_file", "arguments", r#"{"path":"src/main.rs"}"#)
        );
        let (clean, calls) = parse(ToolFormat::Hermes, &raw);
        assert_eq!(calls, vec![call("read_file", r#"{"path":"src/main.rs"}"#)]);
        assert!(clean.contains("Let me look."));
        assert!(!clean.contains(HERMES_OPEN));
    }

    #[test]
    fn an_unterminated_block_is_left_in_place_once() {
        let complete = hermes_call("grep", "arguments", r#"{"pattern":"x"}"#);
        let raw = format!("First.{complete} Then {HERMES_OPEN}{{\"name\":\"cut");
        let (clean, calls) = parse(ToolFormat::Hermes, &raw);
        assert_eq!(calls, vec![call("grep", r#"{"pattern":"x"}"#)]);
        assert_eq!(clean, format!("First. Then {HERMES_OPEN}{{\"name\":\"cut"));
    }

    #[test]
    fn hermes_uses_parameters_key() {
        let raw = hermes_call("grep", "parameters", r#"{"pattern":"todo"}"#);
        let (_, calls) = parse(ToolFormat::Hermes, &raw);
        assert_eq!(calls, vec![call("grep", r#"{"pattern":"todo"}"#)]);
    }

    #[test]
    fn hermes_multiple_calls() {
        let raw = format!(
            "{}{}",
            hermes_call("read_file", "arguments", r#"{"path":"a"}"#),
            hermes_call("grep", "arguments", r#"{"pattern":"x"}"#)
        );
        let (_, calls) = parse(ToolFormat::Hermes, &raw);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn qwen_parameter_tags() {
        let raw = format!(
            "{QWEN_OPEN}{FUNC_PREFIX}read_file>{PARAM_PREFIX_EQ}path>src/main.rs{PARAM_CLOSE}{FUNC_CLOSE}{QWEN_CLOSE}"
        );
        let (_, calls) = parse(ToolFormat::Qwen, &raw);
        assert_eq!(calls, vec![call("read_file", r#"{"path":"src/main.rs"}"#)]);
    }

    #[test]
    fn qwen_coerces_numeric_and_bool() {
        let raw = format!(
            "{QWEN_OPEN}{FUNC_PREFIX}search>{PARAM_PREFIX_EQ}query>todos{PARAM_CLOSE}{PARAM_PREFIX_EQ}limit>10{PARAM_CLOSE}{PARAM_PREFIX_EQ}regex>true{PARAM_CLOSE}{FUNC_CLOSE}{QWEN_CLOSE}"
        );
        let (_, calls) = parse(ToolFormat::Qwen, &raw);
        assert_eq!(calls.len(), 1);
        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["limit"], 10);
        assert_eq!(args["regex"], true);
        assert_eq!(args["query"], "todos");
    }

    #[test]
    fn mistral_tool_calls_array() {
        let raw = "Sure.\n[TOOL_CALLS][{\"name\":\"read_file\",\"arguments\":{\"path\":\"a\"}},{\"name\":\"grep\",\"arguments\":{\"pattern\":\"x\"}}]";
        let (clean, calls) = parse(ToolFormat::Mistral, raw);
        assert_eq!(calls.len(), 2);
        // Exact, not "does not contain the marker": asserting only the absence
        // of `[TOOL_CALLS]` is what let the whole JSON payload survive in the
        // visible content for so long.
        assert_eq!(clean, "Sure.\n");
    }

    /// A `]` inside a string value used to close the array early, so the calls
    /// were dropped and the raw JSON was shown to the user. Regex arguments hit
    /// this constantly.
    #[test]
    fn mistral_array_survives_a_bracket_inside_a_string() {
        let raw =
            "Sure.\n[TOOL_CALLS][{\"name\":\"grep\",\"arguments\":{\"pattern\":\"[a-z]+]\"}}]";
        let (clean, calls) = parse(ToolFormat::Mistral, raw);
        assert_eq!(calls, vec![call("grep", r#"{"pattern":"[a-z]+]"}"#)]);
        assert_eq!(clean, "Sure.\n");
    }

    /// Text after the array is prose again and must survive.
    #[test]
    fn mistral_keeps_text_after_the_array() {
        let raw =
            "Before.[TOOL_CALLS][{\"name\":\"grep\",\"arguments\":{\"pattern\":\"x\"}}] After.";
        let (clean, calls) = parse(ToolFormat::Mistral, raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(clean, "Before. After.");
    }

    #[test]
    fn llama3_python_tag() {
        let raw = format!(
            "Thinking.\n{PYTHON_TAG}{{\"name\":\"read_file\",\"parameters\":{{\"path\":\"a.rs\"}}}}"
        );
        let (clean, calls) = parse(ToolFormat::Llama3Json, &raw);
        assert_eq!(calls, vec![call("read_file", r#"{"path":"a.rs"}"#)]);
        assert!(clean.contains("Thinking."));
        assert!(!clean.contains(PYTHON_TAG));
    }

    #[test]
    fn glm_invoke_parameter_tags() {
        let raw = format!(
            "{QWEN_OPEN}{INVOKE_PREFIX} name=\"read_file\">{PARAM_PREFIX} name=\"path\">src/main.rs{PARAM_CLOSE}{INVOKE_CLOSE}{QWEN_CLOSE}"
        );
        let (_, calls) = parse(ToolFormat::Glm, &raw);
        assert_eq!(calls, vec![call("read_file", r#"{"path":"src/main.rs"}"#)]);
    }

    /// GLM emits parallel calls as sibling `<invoke>` elements in one wrapper.
    /// Only the first was parsed, and because the wrapper had already been
    /// stripped from the content, the rest vanished without a trace.
    #[test]
    fn glm_parses_every_invoke_in_a_block() {
        let raw = format!(
            "{QWEN_OPEN}\
             {INVOKE_PREFIX} name=\"read_file\">{PARAM_PREFIX} name=\"path\">a.rs{PARAM_CLOSE}{INVOKE_CLOSE}\
             {INVOKE_PREFIX} name=\"grep\">{PARAM_PREFIX} name=\"query\">todo{PARAM_CLOSE}{INVOKE_CLOSE}\
             {QWEN_CLOSE}"
        );
        let (clean, calls) = parse(ToolFormat::Glm, &raw);
        assert_eq!(
            calls,
            vec![call("read_file", r#"{"path":"a.rs"}"#), call("grep", r#"{"query":"todo"}"#),]
        );
        assert_eq!(clean, "");
    }

    /// One malformed sibling must not take the others down with it.
    #[test]
    fn glm_skips_a_bad_invoke_and_keeps_the_rest() {
        let raw = format!(
            "{QWEN_OPEN}\
             {INVOKE_PREFIX}>{PARAM_PREFIX} name=\"path\">a.rs{PARAM_CLOSE}{INVOKE_CLOSE}\
             {INVOKE_PREFIX} name=\"grep\">{PARAM_PREFIX} name=\"query\">todo{PARAM_CLOSE}{INVOKE_CLOSE}\
             {QWEN_CLOSE}"
        );
        let (_, calls) = parse(ToolFormat::Glm, &raw);
        assert_eq!(calls, vec![call("grep", r#"{"query":"todo"}"#)]);
    }

    #[test]
    fn kimi_k2_section() {
        let raw = format!(
            "I'll read it.\n{KIMI_SECTION_BEGIN}{KIMI_CALL_BEGIN}functions.read_file:0{KIMI_CALL_ARG_BEGIN}{{\"path\":\"a.rs\"}}{KIMI_CALL_END}{KIMI_SECTION_END}"
        );
        let (clean, calls) = parse(ToolFormat::Kimi, &raw);
        assert_eq!(calls, vec![call("read_file", r#"{"path":"a.rs"}"#)]);
        // Exact: checking only for the *begin* marker let the end marker leak
        // into the visible content.
        assert_eq!(clean, "I'll read it.\n");
    }

    #[test]
    fn deepseek_format() {
        let raw = format!(
            "Reading.\n{DEEPSEEK_CALLS_BEGIN}{DEEPSEEK_CALL_BEGIN}read_file{DEEPSEEK_CALL_ARG_BEGIN}{{\"path\":\"a.rs\"}}{DEEPSEEK_CALL_END}{DEEPSEEK_CALLS_END}"
        );
        let (clean, calls) = parse(ToolFormat::DeepSeek, &raw);
        assert_eq!(calls, vec![call("read_file", r#"{"path":"a.rs"}"#)]);
        assert_eq!(clean, "Reading.\n");
    }

    /// The invariant every one of these parsers has to hold, checked in one
    /// place: whatever is left over is prose, with no delimiter of any dialect
    /// still in it.
    #[test]
    fn no_dialect_leaves_markup_in_the_content() {
        let markers = [
            HERMES_OPEN,
            HERMES_CLOSE,
            QWEN_OPEN,
            QWEN_CLOSE,
            INVOKE_PREFIX,
            INVOKE_CLOSE,
            PARAM_PREFIX,
            PARAM_CLOSE,
            PYTHON_TAG,
            KIMI_SECTION_BEGIN,
            KIMI_SECTION_END,
            KIMI_CALL_BEGIN,
            KIMI_CALL_END,
            DEEPSEEK_CALLS_BEGIN,
            DEEPSEEK_CALLS_END,
            DEEPSEEK_CALL_BEGIN,
            DEEPSEEK_CALL_END,
            MISTRAL_MARKER,
        ];
        let cases: Vec<(ToolFormat, String)> = vec![
            (
                ToolFormat::Hermes,
                format!("Hi.{HERMES_OPEN}{{\"name\":\"grep\",\"arguments\":{{}}}}{HERMES_CLOSE}"),
            ),
            (
                ToolFormat::Glm,
                format!(
                    "Hi.{QWEN_OPEN}{INVOKE_PREFIX} name=\"grep\">{PARAM_PREFIX} name=\"q\">x{PARAM_CLOSE}{INVOKE_CLOSE}{QWEN_CLOSE}"
                ),
            ),
            (
                ToolFormat::Kimi,
                format!(
                    "Hi.{KIMI_SECTION_BEGIN}{KIMI_CALL_BEGIN}functions.grep:0{KIMI_CALL_ARG_BEGIN}{{}}{KIMI_CALL_END}{KIMI_SECTION_END}"
                ),
            ),
            (
                ToolFormat::DeepSeek,
                format!(
                    "Hi.{DEEPSEEK_CALLS_BEGIN}{DEEPSEEK_CALL_BEGIN}grep{DEEPSEEK_CALL_ARG_BEGIN}{{}}{DEEPSEEK_CALL_END}{DEEPSEEK_CALLS_END}"
                ),
            ),
            (
                ToolFormat::Mistral,
                "Hi.[TOOL_CALLS][{\"name\":\"grep\",\"arguments\":{}}]".to_owned(),
            ),
            (
                ToolFormat::Llama3Json,
                format!("Hi.{PYTHON_TAG}{{\"name\":\"grep\",\"parameters\":{{}}}}"),
            ),
        ];
        for (format, raw) in cases {
            let (clean, calls) = parse(format, &raw);
            assert!(!calls.is_empty(), "{format:?} parsed no calls");
            for marker in markers {
                assert!(
                    !clean.contains(marker),
                    "{format:?} left {marker:?} in the content: {clean:?}"
                );
            }
            assert_eq!(clean.trim(), "Hi.", "{format:?} lost or kept prose");
        }
    }

    #[test]
    fn json_fenced_block() {
        let raw = "Here:\n```json\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.rs\"}}\n```\nDone.";
        let (_, calls) = parse(ToolFormat::Json, raw);
        assert_eq!(calls, vec![call("read_file", r#"{"path":"a.rs"}"#)]);
    }

    #[test]
    fn json_whole_object() {
        let raw = "{\"name\":\"grep\",\"arguments\":{\"pattern\":\"todo\"}}";
        let (_, calls) = parse(ToolFormat::Json, raw);
        assert_eq!(calls, vec![call("grep", r#"{"pattern":"todo"}"#)]);
    }

    #[test]
    fn auto_detects_each_family() {
        let hermes = hermes_call("read_file", "arguments", r#"{"path":"a"}"#);
        let qwen = format!(
            "{QWEN_OPEN}{FUNC_PREFIX}read_file>{PARAM_PREFIX_EQ}path>a{PARAM_CLOSE}{FUNC_CLOSE}{QWEN_CLOSE}"
        );
        let glm = format!(
            "{QWEN_OPEN}{INVOKE_PREFIX} name=\"read_file\">{PARAM_PREFIX} name=\"path\">a{PARAM_CLOSE}{INVOKE_CLOSE}{QWEN_CLOSE}"
        );
        let mistral = "[TOOL_CALLS][{\"name\":\"read_file\",\"arguments\":{\"path\":\"a\"}}]";
        assert_eq!(parse(ToolFormat::Auto, &hermes).1.len(), 1);
        assert_eq!(parse(ToolFormat::Auto, &qwen).1.len(), 1);
        assert_eq!(parse(ToolFormat::Auto, &glm).1.len(), 1);
        assert_eq!(parse(ToolFormat::Auto, mistral).1.len(), 1);
    }

    #[test]
    fn none_never_parses() {
        let raw = hermes_call("read_file", "arguments", r#"{"path":"a"}"#);
        let (clean, calls) = parse(ToolFormat::None, &raw);
        assert!(calls.is_empty());
        assert_eq!(clean, raw);
    }

    #[test]
    fn invalid_arguments_dropped() {
        let raw = hermes_call("read_file", "arguments", "\"not an object\"");
        let (_, calls) = parse(ToolFormat::Hermes, &raw);
        assert!(calls.is_empty(), "non-object arguments must be rejected");
    }

    #[test]
    fn format_parse_round_trip() {
        for (input, expected) in [
            ("none", ToolFormat::None),
            ("auto", ToolFormat::Auto),
            ("hermes", ToolFormat::Hermes),
            ("qwen3-coder", ToolFormat::Qwen),
            ("llama3_json", ToolFormat::Llama3Json),
            ("mistral", ToolFormat::Mistral),
            ("glm47", ToolFormat::Glm),
            ("kimi-k2", ToolFormat::Kimi),
            ("deepseek-v3", ToolFormat::DeepSeek),
            ("json", ToolFormat::Json),
        ] {
            assert_eq!(ToolFormat::parse(input), Some(expected));
        }
        assert!(ToolFormat::parse("nonsense").is_none());
    }
}
