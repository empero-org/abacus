//! Scripted endpoints: completely custom HTTP targets described by a YAML file.
//!
//! Abacus talks to OpenAI-compatible providers out of the box, but some useful
//! endpoints need a shape the built-in profiles cannot express — an OAuth
//! bearer sourced from a file that refreshes, extra required headers, or body
//! fields the backend insists on (`store: false`, `reasoning: {effort: low}`)
//! and fields it rejects. A scripted endpoint captures all of that in one
//! declarative file so any such target becomes a normal profile.
//!
//! Files live in `~/.abacus/endpoints/<name>.yaml` and a profile selects one
//! by name. They are loaded ONLY from that user-owned directory (or an
//! absolute path the user typed themselves) — never auto-discovered from a
//! workspace, because a scripted endpoint can run a token command and send a
//! bearer token to an arbitrary URL, and a repo must not be able to introduce
//! one.
//!
//! Nothing here is tailored to a particular provider: `url`, `protocol`, the
//! auth source, the header name and format, the merged body, and the removed
//! keys are all free-form.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::config::ProviderProtocol;

/// A custom endpoint definition, deserialized from YAML.
#[derive(Debug, Clone, Deserialize)]
pub struct ScriptedEndpoint {
    /// Display name; falls back to the file stem.
    #[serde(default)]
    pub name: Option<String>,
    /// The full request URL, used verbatim — this replaces the base-url +
    /// protocol-path construction entirely.
    pub url: String,
    /// The wire format Abacus builds the request body in.
    #[serde(default)]
    pub protocol: ProviderProtocol,
    /// Optional model this endpoint serves, filled in when the profile leaves
    /// its model blank.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional models-list URL for `abacus models` and limit detection. When
    /// absent, detection is simply skipped for this endpoint.
    #[serde(default)]
    pub models_url: Option<String>,
    /// How to authenticate. Absent means "use the profile's ordinary API key"
    /// (or none).
    #[serde(default)]
    pub auth: Option<Auth>,
    /// Extra static headers sent on every request.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Body fields deep-merged onto every request after Abacus builds it —
    /// scripted values win, so this is how a required `store: false` or
    /// `reasoning: {effort: low}` is forced.
    #[serde(default)]
    pub body: Map<String, Value>,
    /// Body keys removed after Abacus builds the request, for backends that
    /// reject a field Abacus adds (e.g. `parallel_tool_calls`).
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Where the bearer/authorization value comes from, and how it is presented.
#[derive(Debug, Clone, Deserialize)]
pub struct Auth {
    /// Header name to set. Defaults to `Authorization`.
    #[serde(default = "default_auth_header")]
    pub header: String,
    /// How the token is wrapped; `{token}` is substituted. Defaults to
    /// `Bearer {token}`.
    #[serde(default = "default_auth_format")]
    pub format: String,
    /// A literal token. Discouraged — prefer a source that is not in the file.
    #[serde(default)]
    pub token: Option<String>,
    /// Environment variable holding the token.
    #[serde(default)]
    pub env: Option<String>,
    /// A JSON file to read the token from (e.g. an OAuth `auth.json`).
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// Dotted path (`tokens.access_token`) or JSON Pointer (`/tokens/access_token`)
    /// into `file`. When absent, the whole file is used, trimmed.
    #[serde(default)]
    pub file_field: Option<String>,
    /// A command whose stdout is the token — for a source that must refresh.
    #[serde(default)]
    pub command: Option<Vec<String>>,
}

fn default_auth_header() -> String {
    "Authorization".to_owned()
}
fn default_auth_format() -> String {
    "Bearer {token}".to_owned()
}

impl ScriptedEndpoint {
    /// Resolve a profile's `endpoint` reference to a loaded definition. A bare
    /// name maps to `<endpoints_dir>/<name>.yaml`; a value containing a path
    /// separator or ending in `.yaml`/`.yml` is treated as an explicit path.
    pub fn resolve(reference: &str, endpoints_dir: &Path) -> Result<Self> {
        let candidate = if reference.contains('/')
            || reference.ends_with(".yaml")
            || reference.ends_with(".yml")
        {
            expand_home(reference)
        } else {
            endpoints_dir.join(format!("{reference}.yaml"))
        };
        let content = std::fs::read_to_string(&candidate).with_context(|| {
            format!(
                "could not read scripted endpoint `{reference}` at {}",
                candidate.display()
            )
        })?;
        let mut endpoint: ScriptedEndpoint = serde_yaml::from_str(&content)
            .with_context(|| format!("invalid scripted endpoint {}", candidate.display()))?;
        if endpoint.url.trim().is_empty() {
            bail!("scripted endpoint {} has no `url`", candidate.display());
        }
        if endpoint.name.is_none() {
            endpoint.name = candidate
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned);
        }
        Ok(endpoint)
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("scripted endpoint")
    }

    /// The request URL — always the verbatim `url`, the `protocol` argument is
    /// only used to decide which body Abacus builds.
    pub fn request_url(&self) -> &str {
        &self.url
    }

    /// Resolve the current auth token, fresh — OAuth tokens refresh on disk, so
    /// this is called per request rather than cached.
    pub fn resolve_token(&self) -> Result<Option<String>> {
        let Some(auth) = &self.auth else {
            return Ok(None);
        };
        let token = if let Some(literal) = &auth.token {
            literal.clone()
        } else if let Some(name) = &auth.env {
            std::env::var(name).with_context(|| {
                format!("auth env `{name}` is not set for {}", self.display_name())
            })?
        } else if let Some(file) = &auth.file {
            token_from_file(
                &expand_home(&file.to_string_lossy()),
                auth.file_field.as_deref(),
            )?
        } else if let Some(command) = &auth.command {
            token_from_command(command)?
        } else {
            bail!(
                "scripted endpoint {} has `auth` but no token source (token/env/file/command)",
                self.display_name()
            )
        };
        let token = token.trim();
        if token.is_empty() {
            bail!("resolved an empty auth token for {}", self.display_name());
        }
        Ok(Some(auth.format.replace("{token}", token)))
    }

    /// The header name and resolved value for the auth header, if any.
    pub fn auth_header(&self) -> Result<Option<(String, String)>> {
        match self.resolve_token()? {
            Some(value) => Ok(Some((
                self.auth
                    .as_ref()
                    .map(|auth| auth.header.clone())
                    .unwrap_or_else(default_auth_header),
                value,
            ))),
            None => Ok(None),
        }
    }

    /// Deep-merge the scripted body overrides onto `body`, then drop removed
    /// keys. Scripted values win; nested objects merge rather than replace.
    pub fn apply_to_body(&self, body: &mut Value) {
        let Some(object) = body.as_object_mut() else {
            return;
        };
        for (key, value) in &self.body {
            merge_value(object.entry(key.clone()).or_insert(Value::Null), value);
        }
        for key in &self.remove {
            object.remove(key);
        }
    }
}

/// Deep-merge `overlay` into `base`, overlay winning at the leaves.
fn merge_value(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                merge_value(base.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

fn token_from_file(path: &Path, field: Option<&str>) -> Result<String> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("could not read auth file {}", path.display()))?;
    let Some(field) = field else {
        return Ok(content.trim().to_owned());
    };
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("auth file {} is not JSON", path.display()))?;
    let found = if let Some(pointer) = field.strip_prefix('/') {
        value.pointer(&format!("/{pointer}"))
    } else {
        // Dotted path: walk object keys.
        let mut current = &value;
        for key in field.split('.') {
            current = current.get(key).unwrap_or(&Value::Null);
        }
        Some(current)
    };
    found
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("auth file {} has no string at `{field}`", path.display()))
}

fn token_from_command(command: &[String]) -> Result<String> {
    let (program, args) = command.split_first().context("auth command is empty")?;
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("could not run auth command `{program}`"))?;
    if !output.status.success() {
        bail!(
            "auth command `{program}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Expand a leading `~` to the home directory; other paths pass through.
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &Path, name: &str, yaml: &str) {
        std::fs::write(dir.join(format!("{name}.yaml")), yaml).unwrap();
    }

    #[test]
    fn resolves_by_name_and_applies_body_and_auth() {
        let dir = tempfile::tempdir().unwrap();
        // A JSON token file in the OAuth shape from the reference doc.
        let auth_file = dir.path().join("auth.json");
        std::fs::write(
            &auth_file,
            json!({"tokens": {"access_token": "sk-oauth-123"}}).to_string(),
        )
        .unwrap();
        write(
            dir.path(),
            "codex",
            &format!(
                "url: https://chatgpt.com/backend-api/codex/responses\n\
                 protocol: responses\n\
                 auth:\n  file: {}\n  file_field: tokens.access_token\n\
                 body:\n  store: false\n  reasoning:\n    effort: low\n\
                 remove:\n  - parallel_tool_calls\n",
                auth_file.display()
            ),
        );

        let endpoint = ScriptedEndpoint::resolve("codex", dir.path()).unwrap();
        assert_eq!(endpoint.protocol, ProviderProtocol::Responses);
        assert_eq!(
            endpoint.request_url(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        let (header, value) = endpoint.auth_header().unwrap().unwrap();
        assert_eq!(header, "Authorization");
        assert_eq!(value, "Bearer sk-oauth-123");

        // Abacus builds a Responses body; the endpoint forces its fields.
        let mut body = json!({
            "model": "gpt-5.6-luna",
            "stream": true,
            "store": true,
            "parallel_tool_calls": true,
            "reasoning": {"effort": "high", "summary": "auto"}
        });
        endpoint.apply_to_body(&mut body);
        assert_eq!(body["store"], json!(false), "override wins");
        assert_eq!(body["reasoning"]["effort"], json!("low"), "deep merge");
        assert_eq!(
            body["reasoning"]["summary"],
            json!("auto"),
            "unmentioned nested keys survive"
        );
        assert!(
            body.get("parallel_tool_calls").is_none(),
            "removed key gone"
        );
        assert_eq!(body["stream"], json!(true), "untouched field stays");
    }

    #[test]
    fn token_sources_env_and_custom_header_format() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test.
        unsafe { std::env::set_var("SCRIPTED_TEST_TOKEN", "abc") };
        write(
            dir.path(),
            "custom",
            "url: https://example.test/v1/chat/completions\n\
             auth:\n  env: SCRIPTED_TEST_TOKEN\n  header: X-Api-Key\n  format: \"{token}\"\n",
        );
        let endpoint = ScriptedEndpoint::resolve("custom", dir.path()).unwrap();
        let (header, value) = endpoint.auth_header().unwrap().unwrap();
        assert_eq!(header, "X-Api-Key");
        assert_eq!(value, "abc", "no Bearer prefix with a custom format");
        unsafe { std::env::remove_var("SCRIPTED_TEST_TOKEN") };
    }

    #[test]
    fn no_auth_block_means_no_auth_header() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "open",
            "url: https://example.test/v1/chat/completions\n",
        );
        let endpoint = ScriptedEndpoint::resolve("open", dir.path()).unwrap();
        assert!(endpoint.auth_header().unwrap().is_none());
    }

    #[test]
    fn missing_file_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let error = ScriptedEndpoint::resolve("absent", dir.path()).unwrap_err();
        assert!(error.to_string().contains("absent"), "{error}");
    }

    #[test]
    fn json_pointer_field_also_works() {
        let dir = tempfile::tempdir().unwrap();
        let auth_file = dir.path().join("a.json");
        std::fs::write(&auth_file, json!({"a": {"b": "tok"}}).to_string()).unwrap();
        let token = token_from_file(&auth_file, Some("/a/b")).unwrap();
        assert_eq!(token, "tok");
        let dotted = token_from_file(&auth_file, Some("a.b")).unwrap();
        assert_eq!(dotted, "tok");
    }
}
