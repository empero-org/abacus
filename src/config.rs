use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::mcp::McpServerConfig;
use crate::model_info::ModelLimits;
use crate::tool_format::ToolFormat;

pub const SETTINGS_VERSION: u32 = 2;

/// clap value parser that tolerates `k`/`m` suffixes (e.g. `128k`, `1m`).
fn parse_token_arg(input: &str) -> Result<usize, String> {
    crate::model_info::parse_tokens(input).map_err(|error| error.to_string())
}

/// clap value parser for the tool-call text format.
fn parse_tool_format(input: &str) -> Result<ToolFormat, String> {
    ToolFormat::parse(input).ok_or_else(|| {
        format!(
            "invalid tool format `{input}` (try auto, none, hermes, qwen, llama3_json, mistral, glm, kimi, deepseek, json)"
        )
    })
}

#[derive(Debug, Clone, Parser)]
#[command(
    name = "abacus",
    version,
    about = "A fast, focused terminal coding agent"
)]
pub struct Cli {
    /// Project directory (defaults to the current directory)
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Named provider profile from ~/.abacus/config.toml
    #[arg(long)]
    pub profile: Option<String>,

    /// Override the configured model
    #[arg(short = 'm', long, env = "ABACUS_MODEL")]
    pub model: Option<String>,

    /// Override the OpenAI-compatible API base URL
    #[arg(long, env = "ABACUS_BASE_URL")]
    pub base_url: Option<String>,

    /// Override the provider wire protocol
    #[arg(long, value_enum)]
    pub protocol: Option<ProviderProtocol>,

    /// Override the provider API key
    #[arg(long, env = "ABACUS_API_KEY", hide_env_values = true)]
    pub api_key: Option<String>,

    /// Run one prompt without opening the TUI
    #[arg(short = 'p', long)]
    pub prompt: Option<String>,

    /// Output format for headless mode
    #[arg(long, value_enum, default_value_t = OutputFormat::Plain)]
    pub output_format: OutputFormat,

    /// Continue the most recent session for this workspace
    #[arg(short = 'c', long = "continue")]
    pub continue_last: bool,

    /// Resume a session by ID (a unique prefix is accepted)
    #[arg(short = 'r', long)]
    pub resume: Option<String>,

    /// Allow edits and shell commands without asking for this run
    #[arg(short = 'y', long = "always-approve")]
    pub yes: bool,

    /// Do not create or update a persistent session
    #[arg(long)]
    pub no_session: bool,

    /// Override the maximum model/tool round trips per prompt
    #[arg(long)]
    pub max_steps: Option<usize>,

    /// Override the model context window in tokens (e.g. `128k`, `1m`); auto-detected from the provider when possible
    #[arg(long, value_name = "TOKENS", value_parser = parse_token_arg)]
    pub context_window: Option<usize>,

    /// Override the max output tokens sent to the model (e.g. `8k`, `16k`); otherwise auto-detected or left to the server default
    #[arg(long, value_name = "TOKENS", value_parser = parse_token_arg)]
    pub max_output_tokens: Option<usize>,

    /// How to parse tool calls the model emits as text (for models without native
    /// function-calling): auto, none, hermes, qwen, llama3_json, mistral, glm,
    /// kimi, deepseek, json. `auto` detects from the text; `none` uses native
    /// `tool_calls` only.
    #[arg(long, value_name = "FORMAT", value_parser = parse_tool_format)]
    pub tool_format: Option<ToolFormat>,

    /// Drive a headless Ralph loop that replays the prompt until the completion promise appears or the iteration limit is reached
    #[arg(long = "loop")]
    pub loop_run: bool,

    /// Maximum iterations for --loop (defaults to unlimited; setting a limit is strongly recommended)
    #[arg(long)]
    pub max_iterations: Option<u32>,

    /// Completion promise that ends a --loop (defaults to COMPLETE)
    #[arg(long)]
    pub completion_promise: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    pub fn has_inline_provider(&self) -> bool {
        self.model.is_some() && self.base_url.is_some()
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Configure a provider and model
    Setup {
        /// Replace the current default profile without confirmation
        #[arg(long)]
        force: bool,
    },
    /// List models reported by the active provider
    Models,
    /// List saved sessions for this workspace
    Sessions,
    /// Print configuration and environment diagnostics
    Doctor,
    /// Generate shell completion definitions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Discover and inspect Agent Skills
    Skills {
        #[command(subcommand)]
        action: Option<SkillsCommand>,
    },
    /// Manage local plugins
    Plugins {
        #[command(subcommand)]
        action: Option<PluginsCommand>,
    },
    /// Inspect configured MCP servers and tools
    Mcp,
    /// Trust project-local plugins, hooks, and MCP configuration
    Trust,
    /// Revoke trust for project-local executable extensions
    Untrust,
    /// Manage persistent scheduled agent jobs
    Cron {
        #[command(subcommand)]
        action: CronCommand,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum CronCommand {
    /// List scheduled jobs
    List,
    /// Add a scheduled headless agent job
    Add {
        #[arg(long)]
        name: String,
        /// Five-field Unix cron or six/seven-field cron expression
        #[arg(long)]
        schedule: String,
        #[arg(long)]
        prompt: String,
        /// Workspace for the job (defaults to the current workspace)
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long)]
        profile: Option<String>,
        /// Permit edits and commands during unattended execution
        #[arg(long)]
        always_approve: bool,
        /// Stop a run after this many minutes
        #[arg(long, default_value_t = 120)]
        timeout_minutes: u64,
    },
    /// Remove a job by ID or unique ID prefix
    Remove { id: String },
    /// Enable a job
    Enable { id: String },
    /// Disable a job
    Disable { id: String },
    /// Run a job immediately
    Run { id: String },
    /// Print recent output for a job
    Logs {
        id: String,
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Run the scheduler in the foreground
    Daemon {
        /// Process due jobs once and exit
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 30, hide = true)]
        poll_seconds: u64,
    },
    /// Install and start the per-user background scheduler service
    Install,
    /// Stop and remove the per-user background scheduler service
    Uninstall,
}

#[derive(Debug, Clone, Subcommand)]
pub enum SkillsCommand {
    /// List discovered skills
    List,
    /// Print one skill's metadata and instructions
    Inspect { name: String },
}

#[derive(Debug, Clone, Subcommand)]
pub enum PluginsCommand {
    /// List enabled plugins
    List,
    /// Install a plugin directory into ~/.abacus/plugins
    Install {
        path: PathBuf,
        /// Replace an installed plugin with the same name
        #[arg(long)]
        force: bool,
    },
    /// Remove an installed plugin
    Remove { name: String },
    /// Inspect one enabled plugin
    Inspect { name: String },
    /// Enable a plugin disabled in user configuration
    Enable { name: String },
    /// Disable a plugin without removing it
    Disable { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Plain,
    Json,
    StreamingJson,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub workspace: PathBuf,
    pub profile: String,
    pub model: String,
    pub base_url: String,
    pub protocol: ProviderProtocol,
    pub api_key: Option<String>,
    pub max_steps: usize,
    pub tool_output_limit: usize,
    pub yes: bool,
    pub no_session: bool,
    pub model_limits: ModelLimits,
    pub tool_format: ToolFormat,
    pub web_search: crate::web::WebConfig,
    pub paths: AbacusPaths,
}

impl Config {
    pub fn resolve(
        cli: &Cli,
        settings: &Settings,
        credentials: &Credentials,
        paths: AbacusPaths,
    ) -> Result<Self> {
        let workspace = resolve_workspace(cli.path.as_deref())?;
        let profile_name = cli
            .profile
            .clone()
            .unwrap_or_else(|| settings.default_profile.clone());
        let profile = settings.profiles.get(&profile_name);

        let model = cli
            .model
            .clone()
            .or_else(|| profile.map(|value| value.model.clone()))
            .filter(|value| !value.trim().is_empty())
            .context("no model configured; run `abacus setup`")?;
        let base_url = cli
            .base_url
            .clone()
            .or_else(|| profile.map(|value| value.base_url.clone()))
            .filter(|value| !value.trim().is_empty())
            .context("no provider URL configured; run `abacus setup`")?;
        let protocol = cli
            .protocol
            .or_else(|| profile.map(|value| value.protocol))
            .unwrap_or_default();

        let api_key = cli.api_key.clone().or_else(|| {
            profile.and_then(|profile| {
                profile
                    .api_key_env
                    .as_deref()
                    .and_then(|name| std::env::var(name).ok())
                    .or_else(|| credentials.keys.get(&profile_name).cloned())
            })
        });

        let context_override = cli.context_window.or(settings.agent.context_window);
        let output_override = cli.max_output_tokens.or(settings.agent.max_output_tokens);
        let model_limits =
            ModelLimits::resolve_from_name(&model, context_override, output_override);
        let tool_format = cli
            .tool_format
            .or_else(|| {
                settings
                    .agent
                    .tool_format
                    .as_deref()
                    .and_then(ToolFormat::parse)
            })
            .unwrap_or_default();

        Ok(Self {
            workspace,
            profile: if profile.is_some() {
                profile_name
            } else {
                "cli".to_owned()
            },
            model,
            base_url: base_url.trim_end_matches('/').to_owned(),
            protocol,
            api_key,
            max_steps: cli
                .max_steps
                .unwrap_or(settings.agent.max_steps)
                .clamp(1, 128),
            tool_output_limit: settings.agent.tool_output_limit.clamp(2_000, 200_000),
            yes: cli.yes || settings.ui.permission_mode == PermissionMode::AlwaysApprove,
            no_session: cli.no_session,
            model_limits,
            tool_format,
            web_search: settings.search.resolve(),
            paths,
        })
    }

    pub fn endpoint(&self) -> String {
        match self.protocol {
            ProviderProtocol::ChatCompletions => format!("{}/chat/completions", self.base_url),
            ProviderProtocol::Responses => format!("{}/responses", self.base_url),
        }
    }

    pub fn models_endpoint(&self) -> String {
        format!("{}/models", self.base_url)
    }

    pub fn workspace_name(&self) -> &str {
        self.workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub default_profile: String,
    pub profiles: BTreeMap<String, ProviderProfile>,
    pub ui: UiSettings,
    pub agent: AgentSettings,
    pub skills: DiscoverySettings,
    pub plugins: PluginSettings,
    pub mcp: BTreeMap<String, McpServerConfig>,
    pub trust: TrustSettings,
    pub feedback: FeedbackSettings,
    pub activity: ActivitySettings,
    pub search: crate::web::SearchSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            default_profile: "default".to_owned(),
            profiles: BTreeMap::new(),
            ui: UiSettings::default(),
            agent: AgentSettings::default(),
            skills: DiscoverySettings::default(),
            plugins: PluginSettings::default(),
            mcp: BTreeMap::new(),
            trust: TrustSettings::default(),
            feedback: FeedbackSettings::default(),
            activity: ActivitySettings::default(),
            search: crate::web::SearchSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverySettings {
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSettings {
    pub paths: Vec<PathBuf>,
    pub disabled: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustSettings {
    pub projects: BTreeSet<String>,
}

impl TrustSettings {
    pub fn contains(&self, workspace: &Path) -> bool {
        self.projects.contains(workspace.to_string_lossy().as_ref())
    }

    pub fn set(&mut self, workspace: &Path, trusted: bool) {
        let workspace = workspace.to_string_lossy().into_owned();
        if trusted {
            self.projects.insert(workspace);
        } else {
            self.projects.remove(&workspace);
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProjectExtensions {
    pub skills: DiscoverySettings,
    pub plugins: PluginSettings,
    pub mcp: BTreeMap<String, McpServerConfig>,
}

impl ProjectExtensions {
    pub fn load(workspace: &Path) -> Result<Self> {
        let path = workspace.join(".abacus/config.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("invalid project extension config: {}", path.display()))
    }
}

impl Settings {
    pub fn load(paths: &AbacusPaths) -> Result<Self> {
        if !paths.config_file.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&paths.config_file)
            .with_context(|| format!("could not read {}", paths.config_file.display()))?;
        let mut settings: Self = toml::from_str(&content)
            .with_context(|| format!("invalid config: {}", paths.config_file.display()))?;
        if settings.version > SETTINGS_VERSION {
            bail!(
                "config version {} is newer than this Abacus supports ({SETTINGS_VERSION})",
                settings.version
            );
        }
        settings.version = SETTINGS_VERSION;
        Ok(settings)
    }

    pub fn save(&self, paths: &AbacusPaths) -> Result<()> {
        paths.ensure()?;
        let content = toml::to_string_pretty(self).context("could not encode configuration")?;
        atomic_write(&paths.config_file, content.as_bytes(), false)
    }

    pub fn is_configured(&self) -> bool {
        self.profiles.contains_key(&self.default_profile)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub name: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub protocol: ProviderProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderProtocol {
    #[default]
    ChatCompletions,
    Responses,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub permission_mode: PermissionMode,
    pub vim_mode: bool,
    pub animations: bool,
    pub show_tooltips: bool,
    pub theme: crate::theme::ThemeChoice,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            permission_mode: PermissionMode::Ask,
            vim_mode: true,
            animations: true,
            show_tooltips: true,
            theme: crate::theme::ThemeChoice::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    Ask,
    AlwaysApprove,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    pub max_steps: usize,
    pub tool_output_limit: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
    /// Tool-call text format for models without native function-calling
    /// (`auto`, `none`, `hermes`, `qwen`, `llama3_json`, `mistral`, `glm`,
    /// `kimi`, `deepseek`, `json`). Unset means `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedbackSettings {
    pub enabled: bool,
    pub endpoint: String,
    pub include_diagnostics: bool,
}

impl Default for FeedbackSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: crate::feedback::DEFAULT_FEEDBACK_ENDPOINT.to_owned(),
            include_diagnostics: false,
        }
    }
}

/// Anonymous session activity reporting (see [`crate::activity`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ActivitySettings {
    pub enabled: bool,
    pub endpoint: String,
}

impl Default for ActivitySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: crate::activity::DEFAULT_ACTIVITY_ENDPOINT.to_owned(),
        }
    }
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            max_steps: 512,
            tool_output_limit: 30_000,
            context_window: None,
            max_output_tokens: None,
            tool_format: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    pub keys: BTreeMap<String, String>,
}

impl Credentials {
    pub fn load(paths: &AbacusPaths) -> Result<Self> {
        if !paths.credentials_file.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&paths.credentials_file)
            .with_context(|| format!("could not read {}", paths.credentials_file.display()))?;
        toml::from_str(&content).context("invalid credentials file")
    }

    pub fn save(&self, paths: &AbacusPaths) -> Result<()> {
        paths.ensure()?;
        let content = toml::to_string(self).context("could not encode credentials")?;
        atomic_write(&paths.credentials_file, content.as_bytes(), true)
    }
}

#[derive(Debug, Clone)]
pub struct AbacusPaths {
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub credentials_file: PathBuf,
    pub sessions_dir: PathBuf,
}

impl AbacusPaths {
    pub fn discover() -> Result<Self> {
        if let Some(root) = std::env::var_os("ABACUS_HOME") {
            return Ok(Self::under(PathBuf::from(root)));
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .context("could not find a home directory; set ABACUS_HOME")?;
        Ok(Self::under(home.join(".abacus")))
    }

    pub fn under(root: PathBuf) -> Self {
        Self {
            config_file: root.join("config.toml"),
            credentials_file: root.join("credentials.toml"),
            sessions_dir: root.join("sessions"),
            root,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.sessions_dir)
            .with_context(|| format!("could not create {}", self.root.display()))?;
        Ok(())
    }
}

fn resolve_workspace(path: Option<&Path>) -> Result<PathBuf> {
    let path = match path {
        Some(path) => path.to_owned(),
        None => std::env::current_dir().context("could not determine current directory")?,
    };
    if !path.is_dir() {
        bail!(
            "workspace does not exist or is not a directory: {}",
            path.display()
        );
    }
    path.canonicalize()
        .with_context(|| format!("could not resolve workspace: {}", path.display()))
}

pub fn workspace_from_cli(cli: &Cli) -> Result<PathBuf> {
    resolve_workspace(cli.path.as_deref())
}

pub fn atomic_write(path: &Path, content: &[u8], private: bool) -> Result<()> {
    let parent = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("abacus"),
        std::process::id()
    ));
    let mut file = File::create(&temp)
        .with_context(|| format!("could not create temporary file in {}", parent.display()))?;

    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let _ = private;

    file.write_all(content)?;
    file.sync_all()?;
    drop(file);

    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temp, path).with_context(|| format!("could not replace {}", path.display()))?;

    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn settings_round_trip() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        let mut settings = Settings::default();
        settings.profiles.insert(
            "local".into(),
            ProviderProfile {
                name: "Local".into(),
                base_url: "http://localhost:11434/v1".into(),
                model: "codestral".into(),
                protocol: ProviderProtocol::ChatCompletions,
                api_key_env: None,
            },
        );
        settings.default_profile = "local".into();
        settings.save(&paths).unwrap();
        let loaded = Settings::load(&paths).unwrap();
        assert_eq!(loaded.profiles["local"].model, "codestral");
        assert_eq!(loaded.version, SETTINGS_VERSION);
        assert!(loaded.feedback.enabled);
        assert!(loaded.ui.animations);
    }

    #[test]
    fn credentials_are_separate_from_settings() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        let mut credentials = Credentials::default();
        credentials.keys.insert("default".into(), "secret".into());
        credentials.save(&paths).unwrap();
        assert!(!paths.config_file.exists());
        assert!(
            fs::read_to_string(&paths.credentials_file)
                .unwrap()
                .contains("secret")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&paths.credentials_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
