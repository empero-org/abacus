//! Release check: notice a newer version tag and say so, once.
//!
//! Abacus is usually installed from source, so nothing tells you a release
//! happened. A background check on startup compares the running version with
//! the newest tag on GitHub and, when it is behind, leaves one line in the
//! transcript. It never downloads or installs anything — updating stays the
//! user's decision and the user's command.
//!
//! It is deliberately quiet: a cached result means at most one request a day,
//! a failure is silent (offline, rate-limited, or behind a firewall is not
//! something to nag about), and the whole thing is off when
//! `check_updates = false`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Public tag list. Tags, not releases: a repository that tags without
/// publishing a release still reports its versions here.
const TAGS_URL: &str = "https://api.github.com/repos/empero-org/abacus/tags";
/// How long a check result is trusted before asking again.
const CACHE_HOURS: i64 = 24;
/// The check must never delay startup.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Only the newest handful of tags matter, and this bounds the response.
const TAGS_PER_PAGE: usize = 20;

/// A version that is newer than the one running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub version: String,
    pub current: String,
}

impl Available {
    /// The one line the user sees.
    pub fn message(&self) -> String {
        // Kept short enough to sit on one line of the transcript.
        format!(
            "Abacus {} is available (you are on {}) — git pull && cargo build --release",
            self.version, self.current
        )
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    /// RFC 3339 timestamp of the last successful check.
    checked_at: Option<String>,
    /// Newest tag seen, whether or not it was newer than the running build.
    latest: Option<String>,
}

/// Check for a newer tag. `Ok(None)` means up to date (or checked recently and
/// found nothing); every failure is an `Err` the caller is expected to drop.
pub async fn check(cache_file: &Path, current: &str) -> Result<Option<Available>> {
    check_against(TAGS_URL, cache_file, current).await
}

/// `check` with the endpoint spelled out, so the fetch-and-pick path can be
/// driven against a local server. A parameter rather than an environment
/// variable: tests run in parallel threads of one process, and a global would
/// race between them.
pub async fn check_against(
    tags_url: &str,
    cache_file: &Path,
    current: &str,
) -> Result<Option<Available>> {
    let cache = read_cache(cache_file);
    let fresh = cache
        .checked_at
        .as_deref()
        .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
        .is_some_and(|stamp| {
            chrono::Utc::now().signed_duration_since(stamp.with_timezone(&chrono::Utc))
                < chrono::Duration::hours(CACHE_HOURS)
        });

    let latest = if fresh {
        // Answer from cache rather than asking again — but still compare, so a
        // rebuild onto an older binary is still reported within the day.
        match cache.latest.clone() {
            Some(latest) => latest,
            None => return Ok(None),
        }
    } else {
        let latest = newest_tag(tags_url).await?;
        write_cache(
            cache_file,
            &Cache {
                checked_at: Some(chrono::Utc::now().to_rfc3339()),
                latest: Some(latest.clone()),
            },
        );
        latest
    };

    Ok(is_newer(&latest, current).then(|| Available {
        version: latest,
        current: current.to_owned(),
    }))
}

async fn newest_tag(tags_url: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Tag {
        name: String,
    }
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        // GitHub rejects requests without one.
        .user_agent(concat!("abacus-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;
    let tags: Vec<Tag> = client
        .get(format!("{tags_url}?per_page={TAGS_PER_PAGE}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("fetch tags")?
        .error_for_status()
        .context("tag list")?
        .json()
        .await
        .context("parse tags")?;
    // Tag order is not guaranteed to be semantic, so pick the highest rather
    // than the first.
    tags.iter()
        .filter_map(|tag| parse_version(&tag.name).map(|parsed| (parsed, tag.name.clone())))
        .max_by_key(|(parsed, _)| *parsed)
        .map(|(_, name)| name)
        .context("no version tags")
}

/// `v1.2.3`, `1.2.3`, `abacus-1.2.3` — anything with three dotted numbers in
/// it. Trailing pre-release text is ignored, and so is a tag without numbers.
fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let digits = tag.trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = digits
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        // An unparseable version is not evidence of anything.
        _ => false,
    }
}

fn read_cache(path: &Path) -> Cache {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn write_cache(path: &Path, cache: &Cache) {
    if let Ok(content) = serde_json::to_vec_pretty(cache) {
        let _ = crate::config::atomic_write(path, &content, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_out_of_the_shapes_tags_come_in() {
        assert_eq!(parse_version("v0.6.0"), Some((0, 6, 0)));
        assert_eq!(parse_version("0.6.0"), Some((0, 6, 0)));
        assert_eq!(parse_version("abacus-1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("v2.0.0-rc.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("nightly"), None);
    }

    #[test]
    fn newer_is_compared_numerically_not_lexically() {
        assert!(is_newer("v0.10.0", "0.9.0"), "10 > 9, not \"10\" < \"9\"");
        assert!(is_newer("v1.0.0", "0.6.0"));
        assert!(!is_newer("v0.6.0", "0.6.0"), "the same version is not news");
        assert!(!is_newer("v0.5.0", "0.6.0"), "nor is an older one");
        // A tag nobody can parse never triggers a nag.
        assert!(!is_newer("nightly", "0.6.0"));
        assert!(!is_newer("v9.9.9", "not-a-version"));
    }

    #[test]
    fn a_fresh_cache_answers_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("update.json");
        write_cache(
            &file,
            &Cache {
                checked_at: Some(chrono::Utc::now().to_rfc3339()),
                latest: Some("v9.9.9".into()),
            },
        );
        // The URL is unreachable in tests; a result proves nothing was fetched.
        let found = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(check(&file, "0.6.0"))
            .unwrap();
        assert_eq!(found.map(|found| found.version).as_deref(), Some("v9.9.9"));
    }

    #[test]
    fn a_stale_cache_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("update.json");
        write_cache(
            &file,
            &Cache {
                checked_at: Some(
                    (chrono::Utc::now() - chrono::Duration::hours(CACHE_HOURS + 1)).to_rfc3339(),
                ),
                latest: Some("v9.9.9".into()),
            },
        );
        let cache = read_cache(&file);
        let fresh = cache
            .checked_at
            .as_deref()
            .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
            .is_some_and(|stamp| {
                chrono::Utc::now().signed_duration_since(stamp.with_timezone(&chrono::Utc))
                    < chrono::Duration::hours(CACHE_HOURS)
            });
        assert!(!fresh, "a day-old check is asked again");
    }

    #[test]
    fn the_message_names_both_versions() {
        let message = Available {
            version: "v0.7.0".into(),
            current: "0.6.0".into(),
        }
        .message();
        assert!(
            message.contains("v0.7.0") && message.contains("0.6.0"),
            "{message}"
        );
    }
}
