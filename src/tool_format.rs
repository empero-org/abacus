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
    if raw.contains("[TOOL_CALLS]") {
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
    if let Ok(value) = serde_json::from_str::<Value>(s.trim()) {
        if value.is_string() {
            return Value::String(s.trim().to_owned());
        }
        return value;
    }
    Value::String(s.trim().to_owned())
}

/// Validate that `arguments` is a JSON object; return its compact string form.
fn finalize_arguments(arguments: &str) -> Option<String> {
    let value: Value = serde_json::from_str(arguments.trim()).ok()?;
    if !value.is_object() {
        return None;
    }
    serde_json::to_string(&value).ok()
}

fn make_call(name: &str, arguments: &str) -> Option<ParsedToolCall> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some(ParsedToolCall {
        name: name.to_owned(),
        arguments: finalize_arguments(arguments)?,
    })
}

fn parse_json_call(body: &str, args_key: &str) -> Option<ParsedToolCall> {
    let value: Value = serde_json::from_str(body.trim()).ok()?;
    let name = value.get("name")?.as_str()?.to_owned();
    let args = value.get(args_key)?.clone();
    if !args.is_object() {
        return None;
    }
    Some(ParsedToolCall {
        name,
        arguments: serde_json::to_string(&args).ok()?,
    })
}

/// Repeatedly extract `<opener>...<closer>` blocks, apply `extract` to each
/// body, strip matched blocks from the output, return cleaned text + calls.
fn extract_tag_blocks<F>(
    raw: &str,
    opener: &str,
    closer: &str,
    extract: F,
) -> (String, Vec<ParsedToolCall>)
where
    F: Fn(&str) -> Option<ParsedToolCall>,
{
    let mut calls = Vec::new();
    let mut clean = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find(opener) {
        clean.push_str(&rest[..start]);
        let after = &rest[start + opener.len()..];
        let Some(end) = after.find(closer) else {
            clean.push_str(&rest[start..]);
            break;
        };
        let body = &after[..end];
        if let Some(call) = extract(body) {
            calls.push(call);
        }
        rest = &after[end + closer.len()..];
    }
    clean.push_str(rest);
    (clean, calls)
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

// ----- Hermes: HERMES_OPEN{json}HERMES_CLOSE -----

fn parse_hermes(raw: &str) -> (String, Vec<ParsedToolCall>) {
    extract_tag_blocks(raw, HERMES_OPEN, HERMES_CLOSE, |body| {
        let value: Value = serde_json::from_str(body.trim()).ok()?;
        let name = value.get("name")?.as_str()?.to_owned();
        let args = value
            .get("arguments")
            .or_else(|| value.get("parameters"))?
            .clone();
        if !args.is_object() {
            return None;
        }
        Some(ParsedToolCall {
            name,
            arguments: serde_json::to_string(&args).ok()?,
        })
    })
}

// ----- Llama 3: PYTHON_TAG{json} or HERMES_OPEN{json}HERMES_CLOSE -----

fn parse_llama3(raw: &str) -> (String, Vec<ParsedToolCall>) {
    let mut calls = Vec::new();
    let mut clean = String::new();
    let mut rest = raw;
    while let Some(idx) = rest.find(PYTHON_TAG) {
        clean.push_str(&rest[..idx]);
        let after = &rest[idx + PYTHON_TAG.len()..];
        let end = after.find('\n').unwrap_or(after.len());
        let body = after[..end].trim();
        if let Some(call) =
            parse_json_call(body, "parameters").or_else(|| parse_json_call(body, "arguments"))
        {
            calls.push(call);
        }
        rest = &after[end..];
    }
    let (c2, calls2) = extract_tag_blocks(rest, HERMES_OPEN, HERMES_CLOSE, |body| {
        parse_json_call(body, "parameters").or_else(|| parse_json_call(body, "arguments"))
    });
    clean.push_str(&c2);
    calls.extend(calls2);
    // Bare trailing JSON object (Llama models sometimes emit one with no tag).
    if calls.is_empty() {
        let trimmed = rest.trim();
        if trimmed.starts_with('{')
            && let Some(call) = parse_json_call(trimmed, "parameters")
                .or_else(|| parse_json_call(trimmed, "arguments"))
        {
            return (String::new(), vec![call]);
        }
        return (raw.to_owned(), Vec::new());
    }
    (clean, calls)
}

// ----- Mistral: [TOOL_CALLS][{...}, ...] -----

const MISTRAL_MARKER: &str = "[TOOL_CALLS]";

fn parse_mistral(raw: &str) -> (String, Vec<ParsedToolCall>) {
    let Some(idx) = raw.find(MISTRAL_MARKER) else {
        return (raw.to_owned(), Vec::new());
    };
    let clean = format!("{}{}", &raw[..idx], &raw[idx + MISTRAL_MARKER.len()..]);
    let payload = raw[idx + MISTRAL_MARKER.len()..].trim_start();
    (clean, parse_json_call_array(payload))
}

fn parse_json_call_array(payload: &str) -> Vec<ParsedToolCall> {
    let Some(start) = payload.find('[') else {
        return Vec::new();
    };
    let bytes = payload.as_bytes();
    let mut depth = 0i32;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        match b {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return Vec::new();
    };
    let Ok(arr) = serde_json::from_str::<Vec<Value>>(&payload[start..=end]) else {
        return Vec::new();
    };
    arr.into_iter()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?.to_owned();
            let args = item
                .get("arguments")
                .or_else(|| item.get("parameters"))?
                .clone();
            if !args.is_object() {
                return None;
            }
            Some(ParsedToolCall {
                name,
                arguments: serde_json::to_string(&args).ok()?,
            })
        })
        .collect()
}

// ----- Qwen / Qwen3-coder: QWEN_OPEN FUNC_PREFIXname> PARAM_PREFIX_EQk>v PARAM_CLOSE FUNC_CLOSE QWEN_CLOSE -----

fn parse_qwen(raw: &str) -> (String, Vec<ParsedToolCall>) {
    if raw.contains(QWEN_OPEN) {
        let (clean, _) = extract_tag_blocks(raw, QWEN_OPEN, QWEN_CLOSE, |_| None);
        // Collect every function call across the whole text (a single QWEN_OPEN
        // block may hold several), then keep the cleaned prose.
        let calls = parse_all_function_blocks(raw);
        return (clean, calls);
    }
    let calls = parse_all_function_blocks(raw);
    if calls.is_empty() {
        (raw.to_owned(), Vec::new())
    } else {
        (String::new(), calls)
    }
}

fn parse_all_function_blocks(body: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find(FUNC_PREFIX) {
        let name_start = start + FUNC_PREFIX.len();
        let Some(name_end) = rest[name_start..].find('>') else {
            break;
        };
        let name = &rest[name_start..name_start + name_end];
        let after_name = &rest[name_start + name_end + 1..];
        let Some(func_close) = after_name.find(FUNC_CLOSE) else {
            break;
        };
        let inner = &after_name[..func_close];
        let mut args = serde_json::Map::new();
        let mut p = inner;
        while let Some(pstart) = p.find(PARAM_PREFIX_EQ) {
            let kstart = pstart + PARAM_PREFIX_EQ.len();
            let Some(kend) = p[kstart..].find('>') else {
                break;
            };
            let key = &p[kstart..kstart + kend];
            let after_key = &p[kstart + kend + 1..];
            let Some(pend) = after_key.find(PARAM_CLOSE) else {
                break;
            };
            let value = &after_key[..pend];
            args.insert(key.to_owned(), coerce_value(value));
            p = &after_key[pend + PARAM_CLOSE.len()..];
        }
        if let Ok(arguments) = serde_json::to_string(&Value::Object(args)) {
            calls.push(ParsedToolCall {
                name: name.to_owned(),
                arguments,
            });
        }
        rest = &after_name[func_close + FUNC_CLOSE.len()..];
    }
    calls
}

// ----- GLM: QWEN_OPEN INVOKE_PREFIX name="x"> PARAM_PREFIX name="k">v PARAM_CLOSE INVOKE_CLOSE QWEN_CLOSE -----

fn parse_glm(raw: &str) -> (String, Vec<ParsedToolCall>) {
    extract_tag_blocks(raw, QWEN_OPEN, QWEN_CLOSE, |body| {
        let mut rest = body;
        while let Some(start) = rest.find(INVOKE_PREFIX) {
            let after = &rest[start..];
            let Some(name_open_end) = after.find('>') else {
                break;
            };
            let tag = &after[..name_open_end + 1];
            let name = extract_attr(tag, "name")?;
            let inner = &after[name_open_end + 1..];
            let Some(inv_close) = inner.find(INVOKE_CLOSE) else {
                break;
            };
            let invoke_body = &inner[..inv_close];
            let mut args = serde_json::Map::new();
            let mut p = invoke_body;
            while let Some(pstart) = p.find(PARAM_PREFIX) {
                let after_p = &p[pstart..];
                let Some(ptag_end) = after_p.find('>') else {
                    break;
                };
                let ptag = &after_p[..ptag_end + 1];
                let key = extract_attr(ptag, "name")?;
                let after_ptag = &after_p[ptag_end + 1..];
                let Some(pend) = after_ptag.find(PARAM_CLOSE) else {
                    break;
                };
                let value = &after_ptag[..pend];
                args.insert(key, coerce_value(value));
                p = &after_ptag[pend + PARAM_CLOSE.len()..];
            }
            if let Ok(arguments) = serde_json::to_string(&Value::Object(args)) {
                return Some(ParsedToolCall { name, arguments });
            }
            rest = &inner[inv_close + INVOKE_CLOSE.len()..];
        }
        None
    })
}

fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

// ----- Kimi K2: special-token-delimited sections -----

fn parse_kimi(raw: &str) -> (String, Vec<ParsedToolCall>) {
    let Some(sec_start) = raw.find(KIMI_SECTION_BEGIN) else {
        return (raw.to_owned(), Vec::new());
    };
    let after_sec = &raw[sec_start + KIMI_SECTION_BEGIN.len()..];
    let section_end = after_sec
        .find(KIMI_SECTION_END)
        .map(|e| e + sec_start + KIMI_SECTION_BEGIN.len())
        .unwrap_or(raw.len());
    let section = &raw[sec_start..section_end];
    let clean = format!(
        "{}{}",
        &raw[..sec_start],
        &raw[section_end.min(raw.len())..]
    );
    (clean, parse_kimi_section(section))
}

fn parse_kimi_section(section: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();
    let mut rest = section;
    while let Some(start) = rest.find(KIMI_CALL_BEGIN) {
        let after = &rest[start + KIMI_CALL_BEGIN.len()..];
        let Some(arg_idx) = after.find(KIMI_CALL_ARG_BEGIN) else {
            break;
        };
        let name = after[..arg_idx].trim();
        // Kimi emits `functions.get_weather:0`; strip the `:N` id suffix and any
        // `functions.` prefix.
        let name = name.rsplit_once(':').map(|(n, _)| n).unwrap_or(name);
        let name = name.strip_prefix("functions.").unwrap_or(name);
        let after_arg = &after[arg_idx + KIMI_CALL_ARG_BEGIN.len()..];
        let Some(end) = after_arg.find(KIMI_CALL_END) else {
            break;
        };
        if let Some(call) = make_call(name, &after_arg[..end]) {
            calls.push(call);
        }
        rest = &after_arg[end + KIMI_CALL_END.len()..];
    }
    calls
}

// ----- DeepSeek: special-token-delimited sections -----

fn parse_deepseek(raw: &str) -> (String, Vec<ParsedToolCall>) {
    let Some(sec_start) = raw.find(DEEPSEEK_CALLS_BEGIN) else {
        return (raw.to_owned(), Vec::new());
    };
    let after_sec = &raw[sec_start + DEEPSEEK_CALLS_BEGIN.len()..];
    let section_end = after_sec
        .find(DEEPSEEK_CALLS_END)
        .map(|e| e + sec_start + DEEPSEEK_CALLS_BEGIN.len())
        .unwrap_or(raw.len());
    let section = &raw[sec_start..section_end];
    let clean = format!(
        "{}{}",
        &raw[..sec_start],
        &raw[section_end.min(raw.len())..]
    );
    let mut calls = Vec::new();
    let mut rest = section;
    while let Some(start) = rest.find(DEEPSEEK_CALL_BEGIN) {
        let after = &rest[start + DEEPSEEK_CALL_BEGIN.len()..];
        let Some(arg_idx) = after.find(DEEPSEEK_CALL_ARG_BEGIN) else {
            break;
        };
        let name = after[..arg_idx].trim();
        let after_arg = &after[arg_idx + DEEPSEEK_CALL_ARG_BEGIN.len()..];
        let Some(end) = after_arg.find(DEEPSEEK_CALL_END) else {
            break;
        };
        if let Some(call) = make_call(name, &after_arg[..end]) {
            calls.push(call);
        }
        rest = &after_arg[end + DEEPSEEK_CALL_END.len()..];
    }
    (clean, calls)
}

// ----- Explicit generic JSON (whole content or fenced block) -----

fn parse_json_explicit(raw: &str) -> (String, Vec<ParsedToolCall>) {
    let mut calls = Vec::new();
    let mut clean = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find("```json") {
        clean.push_str(&rest[..start]);
        let after = &rest[start + "```json".len()..];
        let Some(end) = after.find("```") else {
            clean.push_str(&rest[start..]);
            return (clean, calls);
        };
        calls.extend(json_calls_from_text(after[..end].trim()));
        rest = &after[end + 3..];
    }
    clean.push_str(rest);
    if calls.is_empty() {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let whole = json_calls_from_text(trimmed);
            if !whole.is_empty() {
                return (String::new(), whole);
            }
        }
        return (raw.to_owned(), Vec::new());
    }
    (clean, calls)
}

fn json_calls_from_text(text: &str) -> Vec<ParsedToolCall> {
    let Ok(obj) = serde_json::from_str::<Value>(text.trim()) else {
        return Vec::new();
    };
    if let Some(arr) = obj.as_array() {
        return arr
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?.to_owned();
                let args = item
                    .get("arguments")
                    .or_else(|| item.get("parameters"))?
                    .clone();
                if !args.is_object() {
                    return None;
                }
                Some(ParsedToolCall {
                    name,
                    arguments: serde_json::to_string(&args).ok()?,
                })
            })
            .collect();
    }
    if let Some(name) = obj.get("name").and_then(Value::as_str)
        && let Some(args) = obj.get("arguments").or_else(|| obj.get("parameters"))
        && args.is_object()
        && let Ok(arguments) = serde_json::to_string(args)
    {
        return vec![ParsedToolCall {
            name: name.to_owned(),
            arguments,
        }];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &str) -> ParsedToolCall {
        ParsedToolCall {
            name: name.to_owned(),
            arguments: args.to_owned(),
        }
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
        assert!(clean.contains("Sure."));
        assert!(!clean.contains("[TOOL_CALLS]"));
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

    #[test]
    fn kimi_k2_section() {
        let raw = format!(
            "I'll read it.\n{KIMI_SECTION_BEGIN}{KIMI_CALL_BEGIN}functions.read_file:0{KIMI_CALL_ARG_BEGIN}{{\"path\":\"a.rs\"}}{KIMI_CALL_END}{KIMI_SECTION_END}"
        );
        let (clean, calls) = parse(ToolFormat::Kimi, &raw);
        assert_eq!(calls, vec![call("read_file", r#"{"path":"a.rs"}"#)]);
        assert!(clean.contains("I'll read it."));
        assert!(!clean.contains(KIMI_SECTION_BEGIN));
    }

    #[test]
    fn deepseek_format() {
        let raw = format!(
            "Reading.\n{DEEPSEEK_CALLS_BEGIN}{DEEPSEEK_CALL_BEGIN}read_file{DEEPSEEK_CALL_ARG_BEGIN}{{\"path\":\"a.rs\"}}{DEEPSEEK_CALL_END}{DEEPSEEK_CALLS_END}"
        );
        let (_, calls) = parse(ToolFormat::DeepSeek, &raw);
        assert_eq!(calls, vec![call("read_file", r#"{"path":"a.rs"}"#)]);
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
