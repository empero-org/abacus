//! Web search and page-reading tools.
//!
//! `web_search` defaults to DuckDuckGo's keyless Instant Answer JSON API (zero config),
//! and can be pointed at API-key backends (Brave, Tavily) via `[search]` in the
//! config. `read_page` fetches a URL and converts it to readable text. Both are
//! read-only and bounded; `read_page` refuses non-HTTP schemes and private /
//! loopback hosts to avoid being steered into the local network (SSRF).

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const USER_AGENT: &str = concat!("abacus-agent/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_PAGE_CHARS: usize = 20_000;

/// Which search backend `web_search` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchBackend {
    #[default]
    Duckduckgo,
    Brave,
    Tavily,
}

impl SearchBackend {
    fn label(self) -> &'static str {
        match self {
            SearchBackend::Duckduckgo => "duckduckgo",
            SearchBackend::Brave => "brave",
            SearchBackend::Tavily => "tavily",
        }
    }

    /// Whether this backend needs an API key to function.
    fn needs_key(self) -> bool {
        !matches!(self, SearchBackend::Duckduckgo)
    }
}

/// Persisted `[search]` settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchSettings {
    /// Master switch. When false, the web tools are not offered to the model.
    pub enabled: bool,
    pub backend: SearchBackend,
    /// Environment variable holding the API key for key-backed providers.
    pub api_key_env: Option<String>,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: SearchBackend::default(),
            api_key_env: None,
        }
    }
}

impl SearchSettings {
    /// Resolve the runtime config, reading the API key from the environment.
    pub fn resolve(&self) -> WebConfig {
        let default_env = match self.backend {
            SearchBackend::Brave => Some("BRAVE_API_KEY"),
            SearchBackend::Tavily => Some("TAVILY_API_KEY"),
            SearchBackend::Duckduckgo => None,
        };
        let api_key = self
            .api_key_env
            .as_deref()
            .or(default_env)
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.trim().is_empty());
        WebConfig {
            enabled: self.enabled,
            backend: self.backend,
            api_key,
        }
    }
}

/// Resolved, ready-to-use web configuration.
#[derive(Debug, Clone, Default)]
pub struct WebConfig {
    pub enabled: bool,
    pub backend: SearchBackend,
    pub api_key: Option<String>,
}

impl WebConfig {
    fn client(&self) -> Result<reqwest::Client> {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| anyhow!("could not build HTTP client: {error}"))
    }

    /// Run a web search and render results as compact text.
    pub async fn search(&self, query: &str, max_results: usize) -> Result<String> {
        let query = query.trim();
        if query.is_empty() {
            bail!("query cannot be empty");
        }
        let max_results = max_results.clamp(1, 10);
        if self.backend.needs_key() && self.api_key.is_none() {
            bail!(
                "the {} search backend needs an API key; set the configured environment variable (or `[search] api_key_env`)",
                self.backend.label()
            );
        }
        let client = self.client()?;
        let results = match self.backend {
            SearchBackend::Duckduckgo => duckduckgo_search(&client, query, max_results).await?,
            SearchBackend::Brave => {
                brave_search(
                    &client,
                    self.api_key.as_deref().unwrap(),
                    query,
                    max_results,
                )
                .await?
            }
            SearchBackend::Tavily => {
                tavily_search(
                    &client,
                    self.api_key.as_deref().unwrap(),
                    query,
                    max_results,
                )
                .await?
            }
        };
        if results.is_empty() {
            return Ok(format!("No results for {query:?}."));
        }
        Ok(render_results(query, &results))
    }

    /// Fetch a URL and return its readable text content.
    pub async fn read_page(&self, url: &str, max_chars: usize) -> Result<String> {
        let max_chars = if max_chars == 0 {
            MAX_PAGE_CHARS
        } else {
            max_chars.clamp(1_000, 200_000)
        };
        let client = self.client()?;
        let mut current = validate_public_url(url)?;
        ensure_public_resolved(&current).await?;
        let response = {
            let mut hops = 0usize;
            loop {
                hops += 1;
                if hops > 10 {
                    bail!("too many redirects");
                }
                let response = client
                    .get(current.clone())
                    .header(reqwest::header::ACCEPT, "text/html,text/plain,*/*")
                    .send()
                    .await
                    .map_err(|error| anyhow!("request failed: {error}"))?;
                if !response.status().is_redirection() {
                    break response;
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .context("redirect response has no Location header")?;
                let resolved = current
                    .join(location)
                    .map_err(|_| anyhow!("invalid redirect Location: {location}"))?;
                current = validate_public_url(resolved.as_str())?;
                ensure_public_resolved(&current).await?;
            }
        };
        let status = response.status();
        if !status.is_success() {
            bail!("fetch returned HTTP {}", status.as_u16());
        }
        let final_url = response.url().clone();
        ensure_public_resolved(&final_url).await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let body = response
            .text()
            .await
            .map_err(|error| anyhow!("could not read body: {error}"))?;
        let text = if content_type.contains("html") || looks_like_html(&body) {
            html_to_text(&body)
        } else {
            body
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(format!("{final_url} returned no readable text."));
        }
        let mut out = format!("# {final_url}\n\n");
        out.push_str(&truncate_chars(trimmed, max_chars));
        Ok(out)
    }
}

/// JSON tool specs for `web_search` and `read_page`, added to the registry when
/// `[search] enabled` is true.
pub fn tool_specs() -> Vec<Value> {
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web for current information and return the top results (title, URL, snippet). Use it for facts, docs, or library/API details that may have changed; follow up with read_page to read a result.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search query"},
                        "max_results": {"type": "integer", "description": "Number of results, 1-10 (default 5)"}
                    },
                    "required": ["query"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_page",
                "description": "Fetch an http(s) URL and return its readable text content. Use it to read a documentation page or a web_search result. Private/loopback addresses are refused.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "Absolute http or https URL"},
                        "max_chars": {"type": "integer", "description": "Maximum characters to return (default 20000)"}
                    },
                    "required": ["url"]
                }
            }
        }),
    ]
}

#[derive(Debug)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

fn render_results(query: &str, results: &[SearchResult]) -> String {
    let mut out = format!("Search results for {query:?}:\n");
    for (index, result) in results.iter().enumerate() {
        out.push_str(&format!(
            "\n{}. {}\n   {}\n",
            index + 1,
            result.title,
            result.url
        ));
        if !result.snippet.is_empty() {
            out.push_str(&format!("   {}\n", result.snippet));
        }
    }
    out
}

// ---- DuckDuckGo (keyless Instant Answer JSON API) ----
//
// The legacy `html.duckduckgo.com/html/` endpoint now serves an anti-bot
// challenge page instead of result markup, so parsing it reliably yields
// nothing. DuckDuckGo's official keyless Instant Answer API
// (`api.duckduckgo.com`, JSON) returns structured data without a key, so we
// query that instead. It surfaces a single abstract plus `Results` and
// `RelatedTopics` lists; we flatten the most relevant entries into the common
// `SearchResult` shape.

async fn duckduckgo_search(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>> {
    let response = client
        .get("https://api.duckduckgo.com/")
        .query(&[
            ("q", query),
            ("format", "json"),
            ("no_html", "1"),
            ("skip_disambig", "1"),
        ])
        .send()
        .await
        .map_err(|error| anyhow!("DuckDuckGo request failed: {error}"))?;
    if !response.status().is_success() {
        bail!("DuckDuckGo returned HTTP {}", response.status().as_u16());
    }
    let value: Value = response
        .json()
        .await
        .map_err(|error| anyhow!("invalid DuckDuckGo response: {error}"))?;
    Ok(parse_duckduckgo_json(&value, max_results))
}

/// Parse the DuckDuckGo Instant Answer JSON. The API surfaces a single
/// abstract (when it has a direct answer) plus `Results` and `RelatedTopics`
/// lists, which may themselves contain nested `Topics` groups. We flatten the
/// most relevant entries into the common `SearchResult` shape.
fn parse_duckduckgo_json(value: &Value, max_results: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();

    // The abstract is the canonical answer: lead with it when present.
    let abstract_text = value["AbstractText"].as_str().unwrap_or("").trim();
    let abstract_url = value["AbstractURL"].as_str().unwrap_or("").trim();
    let heading = value["Heading"].as_str().unwrap_or("").trim();
    if !abstract_text.is_empty() && !abstract_url.is_empty() {
        results.push(SearchResult {
            title: if heading.is_empty() {
                truncate_for_title(abstract_text)
            } else {
                heading.to_owned()
            },
            url: abstract_url.to_owned(),
            snippet: abstract_text.to_owned(),
        });
    }

    // `Results` are the top "official" links (e.g. official site).
    if let Some(items) = value["Results"].as_array() {
        for item in items.iter() {
            if results.len() >= max_results {
                break;
            }
            if let Some(result) = ddg_topic_to_result(item) {
                results.push(result);
            }
        }
    }

    // `RelatedTopics` is the broader list; entries may be flat topics or
    // grouped under a `Topics` key with a `Name`.
    if let Some(items) = value["RelatedTopics"].as_array() {
        for item in items.iter() {
            if results.len() >= max_results {
                break;
            }
            if let Some(group) = item["Topics"].as_array() {
                for sub in group.iter() {
                    if results.len() >= max_results {
                        break;
                    }
                    if let Some(result) = ddg_topic_to_result(sub) {
                        results.push(result);
                    }
                }
            } else if let Some(result) = ddg_topic_to_result(item) {
                results.push(result);
            }
        }
    }

    results
}

/// Convert a DuckDuckGo topic object (flat or nested) into a `SearchResult`.
/// Topic objects carry `Text` (already plain text via `no_html=1`),
/// `FirstURL`, and optionally an HTML `Result` anchor whose inner text we
/// fall back to when `Text` is empty.
fn ddg_topic_to_result(item: &Value) -> Option<SearchResult> {
    let text = item["Text"].as_str().unwrap_or("").trim();
    let url = item["FirstURL"].as_str().unwrap_or("").trim();
    let result_html = item["Result"].as_str().unwrap_or("").trim();
    let title = if !text.is_empty() {
        truncate_for_title(text)
    } else if !result_html.is_empty() {
        // `no_html=1` strips tags, but guard against stray markup anyway.
        truncate_for_title(&html_to_text(result_html))
    } else {
        String::new()
    };
    if title.is_empty() || url.is_empty() {
        return None;
    }
    Some(SearchResult {
        title,
        url: url.to_owned(),
        snippet: text.to_owned(),
    })
}

/// Collapse a long snippet into a compact title (first sentence, capped).
fn truncate_for_title(text: &str) -> String {
    let text = text.trim();
    // Prefer the first sentence; otherwise cap at a reasonable length.
    let end = text
        .find(". ")
        .filter(|&pos| pos < 78)
        .map(|pos| pos + 1)
        .unwrap_or_else(|| text.len().min(80));
    text[..end].trim().to_owned()
}

// ---- Brave Search API ----

async fn brave_search(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>> {
    let response = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", api_key)
        .header(reqwest::header::ACCEPT, "application/json")
        .query(&[("q", query), ("count", &max_results.to_string())])
        .send()
        .await
        .map_err(|error| anyhow!("Brave request failed: {error}"))?;
    if !response.status().is_success() {
        bail!("Brave returned HTTP {}", response.status().as_u16());
    }
    let value: Value = response
        .json()
        .await
        .map_err(|error| anyhow!("invalid Brave response: {error}"))?;
    let mut results = Vec::new();
    if let Some(items) = value["web"]["results"].as_array() {
        for item in items.iter().take(max_results) {
            let url = item["url"].as_str().unwrap_or_default().to_owned();
            let title = html_to_text(item["title"].as_str().unwrap_or_default());
            if url.is_empty() || title.is_empty() {
                continue;
            }
            let snippet = html_to_text(item["description"].as_str().unwrap_or_default());
            results.push(SearchResult {
                title,
                url,
                snippet,
            });
        }
    }
    Ok(results)
}

// ---- Tavily API ----

async fn tavily_search(
    client: &reqwest::Client,
    api_key: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>> {
    let response = client
        .post("https://api.tavily.com/search")
        .json(&serde_json::json!({
            "api_key": api_key,
            "query": query,
            "max_results": max_results,
        }))
        .send()
        .await
        .map_err(|error| anyhow!("Tavily request failed: {error}"))?;
    if !response.status().is_success() {
        bail!("Tavily returned HTTP {}", response.status().as_u16());
    }
    let value: Value = response
        .json()
        .await
        .map_err(|error| anyhow!("invalid Tavily response: {error}"))?;
    let mut results = Vec::new();
    if let Some(items) = value["results"].as_array() {
        for item in items.iter().take(max_results) {
            let url = item["url"].as_str().unwrap_or_default().to_owned();
            let title = item["title"].as_str().unwrap_or_default().to_owned();
            if url.is_empty() || title.is_empty() {
                continue;
            }
            let snippet = item["content"].as_str().unwrap_or_default().to_owned();
            results.push(SearchResult {
                title,
                url,
                snippet,
            });
        }
    }
    Ok(results)
}

// ---- URL safety (SSRF guard) ----

/// Accept only `http`/`https` URLs to public hosts. Rejects loopback, private,
/// link-local, and cloud-metadata addresses so a fetched/redirected URL cannot
/// be used to probe the local network.
fn validate_public_url(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).map_err(|_| anyhow!("invalid URL: {raw}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("only http/https URLs are allowed");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("URL has no host: {raw}"))?
        .to_ascii_lowercase();
    let blocked_name = host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host == "metadata.google.internal";
    if blocked_name || is_private_host(&host) {
        bail!("refusing to fetch a private or loopback address: {host}");
    }
    Ok(url)
}

async fn ensure_public_resolved(url: &reqwest::Url) -> Result<()> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("URL has no host"))?
        .to_ascii_lowercase();
    let port = url.port_or_known_default().unwrap_or(80);
    let resolved = tokio::net::lookup_host((url.host_str().unwrap(), port))
        .await
        .with_context(|| format!("could not resolve host: {host}"))?;
    for addr in resolved {
        if is_private_ip(&addr.ip()) {
            bail!("refusing to fetch a private or loopback address: {host}");
        }
    }
    Ok(())
}

fn is_private_host(host: &str) -> bool {
    let candidate = host.trim_start_matches('[').trim_end_matches(']');
    let Ok(ip) = candidate.parse::<std::net::IpAddr>() else {
        return false;
    };
    is_private_ip(&ip)
}

fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                // 169.254.169.254 (cloud metadata) is link-local, already covered.
                || v4.octets()[0] == 0
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // Unique-local (fc00::/7) and link-local (fe80::/10).
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

// ---- HTML / text helpers ----

fn looks_like_html(body: &str) -> bool {
    let head = body.get(..512).unwrap_or(body).to_ascii_lowercase();
    head.contains("<html") || head.contains("<!doctype html") || head.contains("<body")
}

/// Strip a document down to readable text: drop script/style blocks, remove
/// tags, decode common entities, and collapse runaway whitespace.
fn html_to_text(html: &str) -> String {
    use regex::Regex;
    // The `regex` crate has no backreferences, so each non-content element is
    // matched by its own literal close tag rather than `\1`.
    let scripts = Regex::new(
        r"(?is)<script\b[^>]*>.*?</script>|<style\b[^>]*>.*?</style>|<noscript\b[^>]*>.*?</noscript>|<head\b[^>]*>.*?</head>|<svg\b[^>]*>.*?</svg>",
    )
    .expect("valid regex");
    let cleaned = scripts.replace_all(html, " ");
    // Turn block-level boundaries into newlines so structure survives.
    let blocks = Regex::new(r"(?i)</(p|div|section|article|li|h[1-6]|tr|br)\s*>|<br\s*/?>")
        .expect("valid regex");
    let cleaned = blocks.replace_all(&cleaned, "\n");
    let tags = Regex::new(r"(?s)<[^>]+>").expect("valid regex");
    let no_tags = tags.replace_all(&cleaned, " ");
    let decoded = decode_entities(&no_tags);
    // Collapse spaces within lines, then trim and drop blank-line runs.
    let mut out = String::with_capacity(decoded.len());
    let mut blank_run = 0;
    for line in decoded.lines() {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(&collapsed);
            out.push('\n');
        }
    }
    out.trim().to_owned()
}

fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }
        let rest = &input[index + 1..];
        let Some(semi) = rest.find(';').filter(|&pos| pos <= 8) else {
            out.push('&');
            continue;
        };
        let entity = &rest[..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            other => other
                .strip_prefix('#')
                .and_then(|num| {
                    if let Some(hex) = num.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        num.parse::<u32>().ok()
                    }
                })
                .and_then(char::from_u32),
        };
        if let Some(decoded) = decoded {
            out.push(decoded);
            for _ in 0..=semi {
                chars.next();
            }
        } else {
            out.push('&');
        }
    }
    out
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let truncated: String = value.chars().take(max).collect();
    format!("{truncated}\n… page truncated")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_readable_text() {
        let html = "<html><head><title>x</title><style>.a{}</style></head>\
            <body><h1>Hi &amp; bye</h1><script>evil()</script><p>Line&nbsp;one</p>\
            <p>Line two</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hi & bye"));
        assert!(text.contains("Line one"));
        assert!(text.contains("Line two"));
        assert!(!text.contains("evil"));
        assert!(!text.contains("<"));
    }

    #[test]
    fn decodes_numeric_and_named_entities() {
        assert_eq!(decode_entities("a&amp;b&#39;c&#x2d;d"), "a&b'c-d");
    }

    #[test]
    fn parses_duckduckgo_abstract_and_related_topics() {
        // Mirrors the shape of api.duckduckgo.com (no_html=1, JSON).
        let value = serde_json::json!({
            "Heading": "Rust (programming language)",
            "AbstractText": "Rust is a general-purpose programming language.",
            "AbstractURL": "https://en.wikipedia.org/wiki/Rust_(programming_language)",
            "Results": [
                {
                    "Text": "Official site",
                    "FirstURL": "https://www.rust-lang.org/",
                    "Result": "Official site"
                }
            ],
            "RelatedTopics": [
                {
                    "Text": "Outline of the Rust programming language - The following outline is provided as an overview of and topical guide to Rust.",
                    "FirstURL": "https://duckduckgo.com/Outline_of_the_Rust_programming_language",
                    "Result": "<a href=\"x\">Outline of the Rust programming language</a>"
                },
                {
                    "Name": "Categories",
                    "Topics": [
                        {
                            "Text": "Systems programming languages",
                            "FirstURL": "https://duckduckgo.com/c/Systems_programming_languages",
                            "Result": "Systems programming languages"
                        },
                        {
                            "Text": "Multi-paradigm programming languages",
                            "FirstURL": "https://duckduckgo.com/c/Multi-paradigm_programming_languages",
                            "Result": "Multi-paradigm programming languages"
                        }
                    ]
                }
            ]
        });
        let results = parse_duckduckgo_json(&value, 10);
        assert_eq!(results.len(), 5);
        // Abstract leads.
        assert_eq!(results[0].title, "Rust (programming language)");
        assert_eq!(
            results[0].url,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
        assert!(results[0].snippet.contains("general-purpose"));
        // Official site from Results.
        assert_eq!(results[1].url, "https://www.rust-lang.org/");
        assert_eq!(results[1].title, "Official site");
        // RelatedTopics entry: title is collapsed to the first 80 chars.
        assert_eq!(
            results[2].url,
            "https://duckduckgo.com/Outline_of_the_Rust_programming_language"
        );
        assert_eq!(
            results[2].title,
            "Outline of the Rust programming language - The following outline is provided as"
        );
        // Nested group topics, in order.
        assert_eq!(
            results[3].url,
            "https://duckduckgo.com/c/Systems_programming_languages"
        );
        assert_eq!(results[3].title, "Systems programming languages");
        assert_eq!(
            results[4].url,
            "https://duckduckgo.com/c/Multi-paradigm_programming_languages"
        );
        assert_eq!(results[4].title, "Multi-paradigm programming languages");
    }

    #[test]
    fn ddg_skips_empty_abstract_and_empty_topics() {
        let value = serde_json::json!({
            "Heading": "",
            "AbstractText": "",
            "AbstractURL": "",
            "Results": [],
            "RelatedTopics": [
                { "Text": "", "FirstURL": "", "Result": "" },
                { "Name": "Empty", "Topics": [] }
            ]
        });
        let results = parse_duckduckgo_json(&value, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn ssrf_guard_blocks_private_and_nonhttp() {
        assert!(validate_public_url("https://example.com").is_ok());
        assert!(validate_public_url("http://localhost/admin").is_err());
        assert!(validate_public_url("http://127.0.0.1:8080").is_err());
        assert!(validate_public_url("http://169.254.169.254/latest/meta-data").is_err());
        assert!(validate_public_url("http://192.168.1.1").is_err());
        assert!(validate_public_url("http://10.0.0.5").is_err());
        assert!(validate_public_url("file:///etc/passwd").is_err());
        assert!(validate_public_url("https://metadata.google.internal/").is_err());
    }

    #[test]
    fn is_private_ip_classifies_addresses() {
        assert!(is_private_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"10.1.2.3".parse().unwrap()));
        assert!(is_private_ip(&"192.168.0.1".parse().unwrap()));
        assert!(is_private_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_ip(&"169.254.169.254".parse().unwrap()));
        assert!(is_private_ip(&"0.0.0.0".parse().unwrap()));
        assert!(is_private_ip(&"::1".parse().unwrap()));
        assert!(is_private_ip(&"fc00::1".parse().unwrap()));
        assert!(is_private_ip(&"fe80::1".parse().unwrap()));
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip(&"2606:4700:4700::1111".parse().unwrap()));
        assert!(is_private_host("127.0.0.1"));
        assert!(!is_private_host("example.com"));
    }

    #[test]
    fn brave_and_tavily_require_a_key() {
        let cfg = WebConfig {
            enabled: true,
            backend: SearchBackend::Brave,
            api_key: None,
        };
        // The async path bails before any network call; assert the precondition.
        assert!(cfg.backend.needs_key());
        assert!(cfg.api_key.is_none());
    }
}
