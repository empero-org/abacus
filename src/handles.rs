//! Context handles: large content as a variable the model programs against,
//! rather than as tokens it must swallow.
//!
//! The bet is that the root model should not see a large payload at all. It is
//! handed *metadata* — how long, how many lines, what shape — and plans a
//! strategy over that, spending model calls only on the parts that matter.
//! Abacus has no interpreter and does not want one, but it has subagents and a
//! typed fan-out, which reach the same place by another route.
//!
//! So a tool result too large to be worth reading whole is bound to a name like
//! `$h3`, and the model gets a one-line description. From there it can look at
//! the shape ([`HandleStore::execute`] → `handle_info`), pull a window
//! (`handle_slice`), search it (`handle_grep`), or map a question over the
//! whole thing concurrently (`handle_recurse`).
//!
//! ## Bounded on purpose
//!
//! Fanning model calls out over chunks goes wrong in predictable ways, and each
//! one has a countermeasure here:
//!
//! - Sequential sub-calls waste the wall-clock that fan-out was meant to save —
//!   recursion runs concurrently.
//! - Runaway fan-out turns a simple question into thousands of calls —
//!   [`MAX_CHUNKS`] caps a single recursion and [`RECURSE_BUDGET`] caps a whole
//!   session, so a loop cannot quietly spend the account.
//! - Answer-by-convention is brittle, because a model does not reliably
//!   separate its conclusion from its reasoning — an optional schema makes each
//!   sub-answer validated data instead of prose to be parsed back out.
//! - Recursion that recurses multiplies cost for little gain, so depth is fixed
//!   at one: a sub-call is a plain model call with no tools and cannot fan out
//!   again.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::provider::Provider;

/// Tool output at or above this size is bound instead of inlined.
pub const BIND_THRESHOLD_CHARS: usize = 20_000;
/// Total resident content across all handles before the oldest are evicted.
const MAX_RESIDENT_CHARS: usize = 8_000_000;
/// A single window a model may pull back into its context.
const MAX_SLICE_CHARS: usize = 20_000;
/// Matches returned by one `handle_grep`.
const MAX_GREP_MATCHES: usize = 50;
/// Sub-calls in a single `handle_recurse`.
const MAX_CHUNKS: usize = 64;
/// Default fan-out when the model does not say.
const DEFAULT_CHUNKS: usize = 8;
/// Sub-calls one session may spend across every recursion.
const RECURSE_BUDGET: usize = 512;
/// Concurrent sub-calls. Enough to beat the paper's sequential baseline
/// without opening a connection per chunk.
const DEFAULT_CONCURRENCY: usize = 6;
const MAX_CONCURRENCY: usize = 16;
/// Per-chunk ceiling, so one enormous chunk cannot overflow a sub-call.
const MAX_CHUNK_CHARS: usize = 120_000;

#[derive(Debug, Clone)]
pub struct Handle {
    pub id: String,
    /// What produced it — a tool name and its arguments, or a file path.
    pub source: String,
    pub content: Arc<String>,
}

impl Handle {
    pub fn chars(&self) -> usize {
        self.content.chars().count()
    }

    pub fn lines(&self) -> usize {
        self.content.lines().count()
    }

    /// A guess at the payload's shape, so the model can pick a strategy without
    /// materialising anything. Deliberately coarse — it is a hint for choosing
    /// between grep and recurse, not a parser.
    pub fn shape(&self) -> &'static str {
        let sample: String = self.content.chars().take(4_000).collect();
        let trimmed = sample.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if trimmed.lines().take(3).all(|line| {
                let line = line.trim();
                line.is_empty() || line.starts_with('{')
            }) && trimmed.lines().count() > 2
            {
                return "JSON lines";
            }
            return "JSON";
        }
        if trimmed.starts_with("<?xml") || trimmed.starts_with('<') {
            return "markup";
        }
        let lines: Vec<&str> = sample.lines().take(20).collect();
        if lines.len() > 2 && lines.iter().all(|line| line.contains(',')) {
            return "delimited rows";
        }
        "text"
    }

    /// The one-line stand-in the model sees in place of the payload.
    pub fn summary(&self) -> String {
        format!(
            "[bound to ${} — {} chars, {} lines, {}. Inspect with handle_info, read a window with \
             handle_slice, search with handle_grep, or map a question over all of it with \
             handle_recurse. Source: {}]",
            self.id,
            self.chars(),
            self.lines(),
            self.shape(),
            self.source
        )
    }
}

#[derive(Default)]
struct Inner {
    handles: BTreeMap<String, Handle>,
    /// Insertion order, for evicting the oldest first.
    order: Vec<String>,
    next: usize,
    resident: usize,
}

/// Session-scoped handle store. `Default` is a working in-memory store; there
/// is nothing to persist, since a handle only means anything to the session
/// that bound it.
#[derive(Clone, Default)]
pub struct HandleStore {
    inner: Arc<RwLock<Inner>>,
    /// Sub-calls spent this session, against [`RECURSE_BUDGET`].
    spent: Arc<AtomicUsize>,
}

impl HandleStore {
    /// Bind content and return the handle. Oldest handles are evicted once the
    /// store exceeds its resident ceiling.
    pub fn bind(&self, source: &str, content: String) -> Handle {
        let mut inner = self.inner.write().expect("handle lock");
        inner.next += 1;
        let id = format!("h{}", inner.next);
        let handle = Handle {
            id: id.clone(),
            source: source.chars().take(200).collect(),
            content: Arc::new(content),
        };
        inner.resident += handle.content.len();
        inner.handles.insert(id.clone(), handle.clone());
        inner.order.push(id);

        while inner.resident > MAX_RESIDENT_CHARS && !inner.order.is_empty() {
            let oldest = inner.order.remove(0);
            if let Some(evicted) = inner.handles.remove(&oldest) {
                inner.resident = inner.resident.saturating_sub(evicted.content.len());
            }
        }
        handle
    }

    pub fn get(&self, id: &str) -> Option<Handle> {
        let id = id.trim().trim_start_matches('$');
        self.inner
            .read()
            .expect("handle lock")
            .handles
            .get(id)
            .cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().expect("handle lock").handles.is_empty()
    }

    pub fn tool_specs() -> Vec<Value> {
        vec![
            spec(
                "handle_list",
                "List the large payloads bound in this session, with their sizes and shapes.",
                json!({"type":"object","properties":{}}),
            ),
            spec(
                "handle_info",
                "Describe one bound payload — size, line count, detected shape, and a short head and tail sample — without pulling the whole thing into context.",
                json!({
                    "type":"object",
                    "properties":{"id":{"type":"string","description":"Handle id, e.g. h3 or $h3"}},
                    "required":["id"]
                }),
            ),
            spec(
                "handle_slice",
                "Read a line range from a bound payload. Use after handle_info or handle_grep has told you where to look.",
                json!({
                    "type":"object",
                    "properties":{
                        "id":{"type":"string"},
                        "start_line":{"type":"integer","minimum":1,"description":"1-based, inclusive (default 1)"},
                        "end_line":{"type":"integer","minimum":1,"description":"1-based, inclusive"}
                    },
                    "required":["id"]
                }),
            ),
            spec(
                "handle_grep",
                "Search a bound payload with a regular expression and get matching lines with their line numbers.",
                json!({
                    "type":"object",
                    "properties":{
                        "id":{"type":"string"},
                        "pattern":{"type":"string","description":"Rust regex"},
                        "context":{"type":"integer","minimum":0,"maximum":10,"description":"Lines of context around each match (default 0)"}
                    },
                    "required":["id","pattern"]
                }),
            ),
            spec(
                "handle_recurse",
                "Ask one question of every part of a bound payload at once. The payload is split into chunks and a separate model call answers your question against each, concurrently; you get the answers back together. Use this when the answer could be anywhere in something too large to read — counting, extracting, or summarising across the whole thing. Prefer handle_grep when you know the string you are looking for.",
                json!({
                    "type":"object",
                    "properties":{
                        "id":{"type":"string"},
                        "prompt":{"type":"string","description":"The question to ask of each chunk. Write it so a chunk that contains nothing relevant can say so."},
                        "chunks":{"type":"integer","minimum":1,"maximum":MAX_CHUNKS,"description":"How many pieces to split into (default 8)"},
                        "schema":{"type":"object","description":"Optional JSON Schema each chunk's answer must satisfy, so you get data rather than prose"},
                        "concurrency":{"type":"integer","minimum":1,"maximum":MAX_CONCURRENCY}
                    },
                    "required":["id","prompt"]
                }),
            ),
        ]
    }

    /// The synchronous handle tools. `handle_recurse` is not here — it makes
    /// model calls, so it lives in [`recurse`].
    pub fn execute(&self, tool: &str, arguments: &str) -> Option<String> {
        let result = match tool {
            "handle_list" => Ok(self.list()),
            "handle_info" => self.info(arguments),
            "handle_slice" => self.slice(arguments),
            "handle_grep" => self.grep(arguments),
            _ => return None,
        };
        Some(result.unwrap_or_else(|error| format!("Error: {error:#}")))
    }

    fn list(&self) -> String {
        let inner = self.inner.read().expect("handle lock");
        if inner.handles.is_empty() {
            return "No payloads are bound in this session.".to_owned();
        }
        let mut lines = vec!["Bound payloads:".to_owned()];
        for id in &inner.order {
            if let Some(handle) = inner.handles.get(id) {
                lines.push(format!(
                    "- ${}: {} chars, {} lines, {} — {}",
                    handle.id,
                    handle.chars(),
                    handle.lines(),
                    handle.shape(),
                    handle.source
                ));
            }
        }
        lines.join("\n")
    }

    fn handle_from(&self, id: &str) -> Result<Handle> {
        self.get(id).with_context(|| {
            format!("no bound payload `{id}` — use handle_list to see what exists")
        })
    }

    fn info(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
        }
        let args: Args =
            serde_json::from_str(arguments).context("invalid handle_info arguments")?;
        let handle = self.handle_from(&args.id)?;
        let lines: Vec<&str> = handle.content.lines().collect();
        let head: Vec<&str> = lines.iter().take(10).copied().collect();
        let tail: Vec<&str> = lines.iter().rev().take(5).rev().copied().collect();
        Ok(format!(
            "${} — {} chars, {} lines, shape: {}\nsource: {}\n\nfirst lines:\n{}\n\nlast lines:\n{}",
            handle.id,
            handle.chars(),
            handle.lines(),
            handle.shape(),
            handle.source,
            clip(&head.join("\n"), 1_500),
            clip(&tail.join("\n"), 800),
        ))
    }

    fn slice(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
            #[serde(default)]
            start_line: Option<usize>,
            #[serde(default)]
            end_line: Option<usize>,
        }
        let args: Args =
            serde_json::from_str(arguments).context("invalid handle_slice arguments")?;
        let handle = self.handle_from(&args.id)?;
        let lines: Vec<&str> = handle.content.lines().collect();
        let start = args.start_line.unwrap_or(1).max(1);
        if start > lines.len() {
            bail!(
                "start_line {start} is past the end of ${} ({} lines)",
                handle.id,
                lines.len()
            );
        }
        let end = args.end_line.unwrap_or(lines.len()).min(lines.len());
        if end < start {
            bail!("end_line {end} is before start_line {start}");
        }
        let body: String = lines[start - 1..end]
            .iter()
            .enumerate()
            .map(|(offset, line)| format!("{:>6}\t{line}", start + offset))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(format!(
            "${} lines {start}-{end} of {}:\n{}",
            handle.id,
            lines.len(),
            clip(&body, MAX_SLICE_CHARS)
        ))
    }

    fn grep(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
            pattern: String,
            #[serde(default)]
            context: Option<usize>,
        }
        let args: Args =
            serde_json::from_str(arguments).context("invalid handle_grep arguments")?;
        let handle = self.handle_from(&args.id)?;
        let regex = regex::Regex::new(&args.pattern)
            .with_context(|| format!("invalid regex `{}`", args.pattern))?;
        let lines: Vec<&str> = handle.content.lines().collect();
        let context = args.context.unwrap_or(0).min(10);

        let mut out = Vec::new();
        let mut matches = 0_usize;
        for (index, line) in lines.iter().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            matches += 1;
            if matches > MAX_GREP_MATCHES {
                break;
            }
            let from = index.saturating_sub(context);
            let to = (index + context + 1).min(lines.len());
            for (offset, body) in lines[from..to].iter().enumerate() {
                out.push(format!("{:>6}\t{body}", from + offset + 1));
            }
            if context > 0 {
                out.push("--".to_owned());
            }
        }
        if out.is_empty() {
            return Ok(format!(
                "No matches for `{}` in ${}.",
                args.pattern, handle.id
            ));
        }
        let total = if matches > MAX_GREP_MATCHES {
            format!("{MAX_GREP_MATCHES}+ matches (truncated)")
        } else {
            format!("{matches} match(es)")
        };
        Ok(format!(
            "{total} for `{}` in ${}:\n{}",
            args.pattern,
            handle.id,
            clip(&out.join("\n"), MAX_SLICE_CHARS)
        ))
    }

    /// Split into at most `chunks` pieces on line boundaries, so a chunk is
    /// never a half-line a sub-call has to guess the rest of.
    fn split(&self, handle: &Handle, chunks: usize) -> Vec<String> {
        let lines: Vec<&str> = handle.content.lines().collect();
        if lines.is_empty() {
            return Vec::new();
        }
        let chunks = chunks.clamp(1, MAX_CHUNKS).min(lines.len());
        let per = lines.len().div_ceil(chunks);
        lines
            .chunks(per)
            .map(|group| clip(&group.join("\n"), MAX_CHUNK_CHARS))
            .collect()
    }

    fn spend(&self, calls: usize) -> Result<()> {
        let spent = self.spent.fetch_add(calls, Ordering::Relaxed) + calls;
        if spent > RECURSE_BUDGET {
            self.spent.fetch_sub(calls, Ordering::Relaxed);
            bail!(
                "this session's recursion budget of {RECURSE_BUDGET} sub-calls is spent; \
                 narrow the search with handle_grep instead"
            );
        }
        Ok(())
    }
}

fn spec(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type":"function",
        "function":{"name":name,"description":description,"parameters":parameters}
    })
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n… truncated")
}

/// Map a question over every chunk of a bound payload, concurrently.
///
/// Depth is one by construction: a sub-call is a bare model call with no tools,
/// so it cannot recurse. That is the paper's own choice, and it is what keeps a
/// fan-out from becoming a fan-out of fan-outs.
pub async fn recurse(
    provider: &Provider,
    store: &HandleStore,
    arguments: &str,
    cancel: &AtomicBool,
) -> String {
    match recurse_inner(provider, store, arguments, cancel).await {
        Ok(output) => output,
        Err(error) => format!("Error: {error:#}"),
    }
}

async fn recurse_inner(
    provider: &Provider,
    store: &HandleStore,
    arguments: &str,
    cancel: &AtomicBool,
) -> Result<String> {
    #[derive(Deserialize)]
    struct Args {
        id: String,
        prompt: String,
        #[serde(default)]
        chunks: Option<usize>,
        #[serde(default)]
        schema: Option<Value>,
        #[serde(default)]
        concurrency: Option<usize>,
    }
    let args: Args = serde_json::from_str(arguments).context("invalid handle_recurse arguments")?;
    let handle = store
        .get(&args.id)
        .with_context(|| format!("no bound payload `{}`", args.id))?;

    let pieces = store.split(&handle, args.chunks.unwrap_or(DEFAULT_CHUNKS));
    if pieces.is_empty() {
        bail!("${} is empty", handle.id);
    }
    store.spend(pieces.len())?;
    let concurrency = args
        .concurrency
        .unwrap_or(DEFAULT_CONCURRENCY)
        .clamp(1, MAX_CONCURRENCY);

    let system = match &args.schema {
        Some(schema) => format!(
            "You are answering a question about one chunk of a larger document. Answer only from \
             the chunk you are given. If it contains nothing relevant, say so in the shape below \
             rather than guessing. Return only JSON matching this schema:\n{}",
            serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
        ),
        None => "You are answering a question about one chunk of a larger document. Answer only \
                 from the chunk you are given, concisely. If it contains nothing relevant, reply \
                 exactly NOTHING RELEVANT."
            .to_owned(),
    };

    let total = pieces.len();
    let answers = stream::iter(pieces.into_iter().enumerate().map(|(index, chunk)| {
        let system = system.clone();
        let question = args.prompt.clone();
        let schema = args.schema.clone();
        async move {
            let conversation = vec![
                json!({"role":"system","content":system}),
                json!({
                    "role":"user",
                    "content": format!(
                        "<chunk index=\"{}\" of=\"{total}\">\n{chunk}\n</chunk>\n\n{question}",
                        index + 1
                    )
                }),
            ];
            let (deltas, _sink) = mpsc::unbounded_channel();
            let completion = provider.complete(&conversation, &[], deltas, cancel).await;
            (index, completion, schema)
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;

    let mut rows: Vec<(usize, String)> = Vec::new();
    let mut structured: Vec<Value> = Vec::new();
    let mut empty = 0_usize;
    let mut failed = 0_usize;
    for (index, completion, schema) in answers {
        let text = match completion {
            Err(error) => {
                failed += 1;
                rows.push((index, format!("chunk {}: failed — {error:#}", index + 1)));
                continue;
            }
            Ok(completion) if completion.cancelled => bail!("cancelled during recursion"),
            Ok(completion) => completion.content.trim().to_owned(),
        };
        if let Some(schema) = schema {
            match crate::refine::extract_json(&text)
                .and_then(|value| crate::schema::validate(&value, &schema).map(|()| value))
            {
                Ok(value) => {
                    structured.push(json!({"chunk": index + 1, "result": value}));
                    rows.push((index, String::new()));
                }
                Err(error) => {
                    failed += 1;
                    // Not retried: a whole extra pass over every bad chunk is
                    // the proliferation this design is built to avoid. The
                    // model can re-ask a narrower question instead.
                    rows.push((
                        index,
                        format!("chunk {}: unusable answer — {error:#}", index + 1),
                    ));
                }
            }
            continue;
        }
        if text.is_empty() || text.eq_ignore_ascii_case("NOTHING RELEVANT") {
            empty += 1;
            rows.push((index, String::new()));
            continue;
        }
        rows.push((index, format!("chunk {}: {text}", index + 1)));
    }
    rows.sort_by_key(|(index, _)| *index);

    if args.schema.is_some() {
        let mut report =
            serde_json::to_string_pretty(&structured).unwrap_or_else(|_| "[]".to_owned());
        let problems: Vec<String> = rows
            .into_iter()
            .map(|(_, row)| row)
            .filter(|row| !row.is_empty())
            .collect();
        if !problems.is_empty() {
            report.push_str(&format!(
                "\n\n{failed} chunk(s) unusable:\n{}",
                problems.join("\n")
            ));
        }
        return Ok(report);
    }

    let body: Vec<String> = rows
        .into_iter()
        .map(|(_, row)| row)
        .filter(|row| !row.is_empty())
        .collect();
    // A silent nothing is a real answer and must be distinguishable from a
    // fan-out that failed.
    let header = format!(
        "Asked {total} chunk(s) of ${}: {} answered, {empty} had nothing relevant, {failed} failed.",
        handle.id,
        total - empty - failed
    );
    if body.is_empty() {
        return Ok(format!("{header}\nNo chunk contained anything relevant."));
    }
    Ok(format!("{header}\n\n{}", body.join("\n\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(content: &str) -> (HandleStore, Handle) {
        let store = HandleStore::default();
        let handle = store.bind("run_command: cat big.log", content.to_owned());
        (store, handle)
    }

    fn log(lines: usize) -> String {
        (1..=lines)
            .map(|index| format!("line {index}: something happened"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn binding_returns_metadata_not_content() {
        let (_, handle) = store_with(&log(500));
        let summary = handle.summary();
        // The whole point: the model sees a description, never the payload.
        assert!(summary.contains("$h1"), "{summary}");
        assert!(summary.contains("500 lines"), "{summary}");
        assert!(!summary.contains("something happened"), "{summary}");
        assert!(summary.contains("cat big.log"), "the source stays visible");
    }

    #[test]
    fn handles_resolve_with_or_without_the_dollar_prefix() {
        let (store, _) = store_with("x");
        assert!(store.get("h1").is_some());
        assert!(store.get("$h1").is_some());
        assert!(store.get(" $h1 ").is_some());
        assert!(store.get("h99").is_none());
    }

    #[test]
    fn slice_returns_a_numbered_window_and_rejects_a_bad_range() {
        let (store, _) = store_with(&log(100));
        let output = store
            .execute(
                "handle_slice",
                &json!({"id":"h1","start_line":3,"end_line":5}).to_string(),
            )
            .unwrap();
        assert!(output.contains("line 3:"), "{output}");
        assert!(
            output.contains("     5\t"),
            "line numbers are shown: {output}"
        );
        assert!(!output.contains("line 6:"), "{output}");

        let past_end = store
            .execute(
                "handle_slice",
                &json!({"id":"h1","start_line":500}).to_string(),
            )
            .unwrap();
        assert!(past_end.starts_with("Error:"), "{past_end}");
        let backwards = store
            .execute(
                "handle_slice",
                &json!({"id":"h1","start_line":9,"end_line":2}).to_string(),
            )
            .unwrap();
        assert!(backwards.starts_with("Error:"), "{backwards}");
    }

    #[test]
    fn grep_finds_lines_and_reports_a_clean_miss() {
        let mut content = log(50);
        content.push_str("\nERROR: DATABASE_URL must be set");
        let (store, _) = store_with(&content);

        let hit = store
            .execute(
                "handle_grep",
                &json!({"id":"h1","pattern":"DATABASE_URL"}).to_string(),
            )
            .unwrap();
        assert!(hit.contains("1 match(es)"), "{hit}");
        assert!(hit.contains("DATABASE_URL must be set"), "{hit}");

        let miss = store
            .execute(
                "handle_grep",
                &json!({"id":"h1","pattern":"zzz"}).to_string(),
            )
            .unwrap();
        assert!(miss.starts_with("No matches"), "{miss}");

        let bad = store
            .execute("handle_grep", &json!({"id":"h1","pattern":"["}).to_string())
            .unwrap();
        assert!(
            bad.starts_with("Error:"),
            "an invalid regex reports itself: {bad}"
        );
    }

    #[test]
    fn a_missing_handle_says_how_to_find_the_right_one() {
        let (store, _) = store_with("x");
        let output = store
            .execute("handle_info", &json!({"id":"nope"}).to_string())
            .unwrap();
        assert!(output.contains("handle_list"), "{output}");
    }

    #[test]
    fn splitting_lands_on_line_boundaries_and_covers_everything() {
        let (store, handle) = store_with(&log(100));
        let pieces = store.split(&handle, 7);
        assert!(pieces.len() <= 7);
        for piece in &pieces {
            assert!(
                piece.starts_with("line "),
                "chunks start mid-line: {piece:?}"
            );
        }
        // Nothing is dropped between chunks.
        let rejoined = pieces.join("\n");
        assert!(rejoined.contains("line 1:"));
        assert!(rejoined.contains("line 100:"));
        assert_eq!(rejoined.lines().count(), 100);
    }

    #[test]
    fn splitting_never_exceeds_the_chunk_cap() {
        let (store, handle) = store_with(&log(10_000));
        // The paper's sub-call proliferation failure: an unbounded fan-out is
        // how a simple task turns into thousands of calls.
        assert!(store.split(&handle, 10_000).len() <= MAX_CHUNKS);
        // Fewer lines than chunks asked for yields one chunk per line, not empties.
        let (small, handle) = store_with(&log(3));
        assert_eq!(small.split(&handle, 20).len(), 3);
    }

    #[test]
    fn the_session_recursion_budget_is_enforced() {
        let (store, _) = store_with("x");
        assert!(store.spend(RECURSE_BUDGET).is_ok());
        let over = store.spend(1).unwrap_err().to_string();
        assert!(over.contains("budget"), "{over}");
        // A refused spend is not charged, so the store does not drift.
        assert!(store.spend(0).is_ok());
    }

    #[test]
    fn eviction_bounds_resident_memory_oldest_first() {
        let store = HandleStore::default();
        let big = "x".repeat(MAX_RESIDENT_CHARS / 2 + 1);
        store.bind("first", big.clone());
        store.bind("second", big.clone());
        assert!(store.get("h1").is_none(), "the oldest is evicted");
        assert!(store.get("h2").is_some());
        store.bind("third", big);
        assert!(store.get("h2").is_none());
        assert!(store.get("h3").is_some());
    }

    #[test]
    fn shape_detection_distinguishes_the_common_payloads() {
        let (_, handle) = store_with("{\"a\":1}\n{\"a\":2}\n{\"a\":3}");
        assert_eq!(handle.shape(), "JSON lines");
        let (_, handle) = store_with("{\n  \"a\": 1\n}");
        assert_eq!(handle.shape(), "JSON");
        let (_, handle) = store_with("<html><body>hi</body></html>");
        assert_eq!(handle.shape(), "markup");
        let (_, handle) = store_with("a,b,c\n1,2,3\n4,5,6\n7,8,9");
        assert_eq!(handle.shape(), "delimited rows");
        let (_, handle) = store_with(&log(10));
        assert_eq!(handle.shape(), "text");
    }

    #[test]
    fn list_reports_every_handle_with_its_source() {
        let store = HandleStore::default();
        store.bind("grep: TODO", log(10));
        store.bind("read_file: notes.md", log(20));
        let listing = store.execute("handle_list", "{}").unwrap();
        assert!(listing.contains("$h1"), "{listing}");
        assert!(listing.contains("$h2"), "{listing}");
        assert!(listing.contains("notes.md"), "{listing}");

        assert!(
            HandleStore::default()
                .execute("handle_list", "{}")
                .unwrap()
                .contains("No payloads")
        );
    }

    #[test]
    fn unknown_tools_are_not_claimed() {
        let (store, _) = store_with("x");
        assert!(store.execute("read_file", "{}").is_none());
        // handle_recurse makes model calls and is dispatched separately.
        assert!(store.execute("handle_recurse", "{}").is_none());
    }
}
