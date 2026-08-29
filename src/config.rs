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
fn parse_agent_mode(input: &str) -> Result<crate::agent::AgentMode, String> {
    match input.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(crate::agent::AgentMode::Auto),
        "plan" => Ok(crate::agent::AgentMode::Plan),
        "build" => Ok(crate::agent::AgentMode::Build),
        other => Err(format!(
            "unknown mode `{other}` (expected auto, plan, or build)"
        )),
    }
}

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

    /// Pin the workflow mode for a headless run: auto, plan, or build.
    /// Without this a headless run uses AUTO, letting the model choose.
    #[arg(long, value_name = "MODE", value_parser = parse_agent_mode)]
    pub mode: Option<crate::agent::AgentMode>,

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
    /// List the upstream providers that can serve the active model
    Providers,
    /// List saved sessions for this workspace
    Sessions,
    /// Sync accounts, sessions, and traces across devices
    Sync {
        #[command(subcommand)]
        action: SyncCommand,
    },
    /// Print configuration and environment diagnostics
    Doctor,
    /// Copy this machine's training traces into a directory for fine-tuning
    Pull {
        /// Where to copy them, or the literal word `all` to also rebuild traces
        /// from every saved session on this device. Defaults to the current
        /// directory. Use `--all ./all` to target a directory of that name.
        #[arg(value_name = "DEST")]
        destination: Option<PathBuf>,
        /// Also rebuild traces from saved sessions that were never captured
        /// live — everything on the device, not just what tracing recorded.
        #[arg(long)]
        all: bool,
    },
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
pub enum SyncCommand {
    /// Sign in and save a device token
    Login {
        #[arg(long, default_value = "https://abacus.empero.org")]
        server: String,
        #[arg(long)]
        email: Option<String>,
        /// Email password (password auth is the `--password-login` fallback;
        /// magic-link browser login is the default)
        #[arg(long, hide = true)]
        password: Option<String>,
        /// Use email and password instead of the browser sign-in flow
        #[arg(long)]
        password_login: bool,
    },
    /// Remove the saved sync token
    Logout,
    /// Show the current sync account
    Status,
    /// List sessions stored by the server
    Sessions,
    /// Upload local sessions and their traces
    Push {
        /// Upload one session ID or unique prefix (all local sessions by default)
        session: Option<String>,
        /// Replace a conflicting remote revision
        #[arg(long)]
        force: bool,
    },
    /// Download remote sessions and required traces into local storage
    Pull {
        /// Download one session ID (all remote sessions by default)
        session: Option<String>,
        /// Replace local sessions instead of preserving them as forks
        #[arg(long)]
        force: bool,
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
    /// Workflow mode pinned on the command line, if any.
    pub mode: Option<crate::agent::AgentMode>,
    /// Whether to append SFT training traces for this session.
    pub trace_enabled: bool,
    /// Upstream routing preferences, applied by providers that support them.
    pub routing: Routing,
    pub web_search: crate::web::WebConfig,
    /// A custom endpoint definition, when the active profile names one.
    pub endpoint: Option<crate::endpoint::ScriptedEndpoint>,
    /// The auxiliary model for secondary calls, or None to reuse `model`.
    pub aux_model: Option<String>,
    /// Reasoning effort sent with every request, when set.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Aggressively reduce upstream context and nonessential auxiliary calls.
    pub token_compression: bool,
    /// Serialize foreground and auxiliary requests on this provider session.
    pub one_stream: bool,
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

        let scripted_model = profile
            .and_then(|profile| profile.endpoint.as_deref())
            .and_then(|reference| {
                crate::endpoint::ScriptedEndpoint::resolve(reference, &paths.endpoints_dir).ok()
            })
            .and_then(|endpoint| endpoint.model.clone());
        let model = cli
            .model
            .clone()
            .or_else(|| profile.map(|value| value.model.clone()))
            .filter(|value| !value.trim().is_empty())
            .or(scripted_model)
            .context("no model configured; run `abacus setup`")?;
        // A scripted endpoint, when the profile names one, supplies the URL,
        // protocol, and (optionally) model — so a scripted profile need not
        // repeat them. It is loaded only from ~/.abacus/endpoints, never a
        // workspace, since it can run a token command and carry a bearer.
        let endpoint = profile
            .and_then(|profile| profile.endpoint.as_deref())
            .map(|reference| {
                crate::endpoint::ScriptedEndpoint::resolve(reference, &paths.endpoints_dir)
            })
            .transpose()?;

        let base_url = cli
            .base_url
            .clone()
            .or_else(|| profile.map(|value| value.base_url.clone()))
            .filter(|value| !value.trim().is_empty())
            .or_else(|| endpoint.as_ref().map(|endpoint| endpoint.url.clone()))
            .context("no provider URL configured; run `abacus setup`")?;
        // A scripted endpoint is the authority on its own wire format, so it
        // beats the profile's protocol (which is only the serde default when a
        // scripted profile omits it). A CLI flag still wins over everything.
        let protocol = cli
            .protocol
            .or_else(|| endpoint.as_ref().map(|endpoint| endpoint.protocol))
            .or_else(|| profile.map(|value| value.protocol))
            .unwrap_or_default();

        // A scripted endpoint carries its own auth, so it does not need the
        // standard API key; the ordinary sources still apply as a fallback.
        let api_key = cli.api_key.clone().or_else(|| {
            profile.and_then(|profile| {
                profile
                    .api_key_env
                    .as_deref()
                    .and_then(|name| std::env::var(name).ok())
                    .or_else(|| credentials.keys.get(&profile_name).cloned())
            })
        });

        // CLI wins for this run; then the active profile; leftover [agent]
        // fields keep older config.toml working until a profile override is set.
        let (profile_context, profile_output) = settings.profile_limits(&profile_name);
        let context_override = cli.context_window.or(profile_context);
        let output_override = cli.max_output_tokens.or(profile_output);
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
            mode: cli.mode,
            trace_enabled: settings.trace.enabled,
            routing: Routing {
                order: profile
                    .map(|profile| profile.providers.clone())
                    .unwrap_or_default(),
                allow_fallbacks: profile.is_none_or(|profile| profile.allow_fallbacks),
            },
            web_search: settings.search.resolve(),
            endpoint,
            aux_model: profile
                .and_then(|profile| profile.aux_model.clone())
                .filter(|model| !model.trim().is_empty()),
            reasoning_effort: profile.and_then(|profile| profile.reasoning_effort),
            token_compression: settings.agent.token_compression,
            one_stream: settings.agent.one_stream,
            paths,
        })
    }

    pub fn endpoint(&self) -> String {
        if let Some(scripted) = &self.endpoint {
            return scripted.request_url().to_owned();
        }
        match self.protocol {
            ProviderProtocol::ChatCompletions => format!("{}/chat/completions", self.base_url),
            ProviderProtocol::Responses => format!("{}/responses", self.base_url),
            ProviderProtocol::Anthropic => format!("{}/v1/messages", self.base_url),
        }
    }

    /// The models-list URL, or `None` when a scripted endpoint declares none —
    /// in which case limit detection is skipped rather than hitting a 404.
    pub fn models_endpoint(&self) -> Option<String> {
        if let Some(scripted) = &self.endpoint {
            return scripted.models_url.clone();
        }
        Some(format!("{}/models", self.base_url))
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
    #[serde(default)]
    pub trace: TraceSettings,
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
            trace: TraceSettings::default(),
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

    /// Effective token-limit overrides for `profile_name`: the profile's own
    /// values when set, otherwise leftover `[agent]` fields from older configs.
    pub fn profile_limits(&self, profile_name: &str) -> (Option<usize>, Option<usize>) {
        let profile = self.profiles.get(profile_name);
        (
            profile
                .and_then(|profile| profile.context_window)
                .or(self.agent.context_window),
            profile
                .and_then(|profile| profile.max_output_tokens)
                .or(self.agent.max_output_tokens),
        )
    }
}

/// Profile map keys must stay TOML-table-safe. Display names can be anything;
/// this only gates rename / create of the id itself.
pub fn validate_profile_id(id: &str) -> Result<&str> {
    let id = id.trim();
    if id.is_empty() || id.len() > 64 || !id.chars().all(is_profile_id_char) {
        bail!("profile id must be 1–64 characters: letters, digits, `.`, `_`, or `-`");
    }
    Ok(id)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub protocol: ProviderProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// A cheaper/secondary model used for background calls (rethink, draft
    /// recommendations, tether, compaction summary, command classification)
    /// on this same endpoint. None means "use the main model".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aux_model: Option<String>,
    /// How hard this profile's model should think. Unset leaves it to the
    /// provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Name (or path) of a scripted endpoint YAML under ~/.abacus/endpoints.
    /// When set, its URL, auth, headers, and body overrides drive the request
    /// and `base_url`/`protocol` here are only fallbacks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Upstream providers to route to, most preferred first.
    ///
    /// OpenRouter fronts many suppliers for the same model and they differ in
    /// context length, quantization, and speed — one `glm-5.2` endpoint offers
    /// 1M context at fp8, another 96k at fp4. Pinning is how you stop landing
    /// on whichever happens to be cheapest today.
    ///
    /// Written through verbatim as `provider.order`, so use whatever spelling
    /// OpenRouter reports (`Together`, `Z.AI`, `deepinfra/fp8`) — `abacus
    /// providers` lists them for the active model. Empty means "let OpenRouter
    /// choose". Ignored by providers that do not support routing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
    /// Whether OpenRouter may fall back to a provider outside `providers` when
    /// none of them can serve the request. False makes the pin strict: the
    /// request fails rather than quietly landing somewhere else.
    #[serde(default = "default_true")]
    pub allow_fallbacks: bool,
    /// Context window override in tokens for this profile. Unset means detect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// Max output tokens sent as `max_tokens` for this profile. Unset means
    /// detect, or leave it to the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
}

fn is_profile_id_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')
}

/// How hard the model should think before answering. Left unset, nothing is
/// sent and the provider's own default applies — the levels are only useful on
/// models that expose reasoning, and sending one to a model that does not is
/// how you collect 400s.
///
/// The ladder spans both worlds: OpenAI-shaped protocols know
/// `minimal|low|medium|high`, while the Anthropic Messages API knows
/// `low|medium|high|xhigh|max`. Each end is mapped to what it accepts rather
/// than sending a level the endpoint would reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// As little as the model allows — fastest and cheapest.
    Minimal,
    Low,
    Medium,
    High,
    /// Extended capability for long-horizon agentic and coding work.
    /// Anthropic's recommended starting point for coding on current Opus.
    XHigh,
    /// Absolute maximum capability, no constraint on token spend.
    Max,
}

impl ReasoningEffort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "minimal" | "min" | "none" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x-high" | "extra-high" => Some(Self::XHigh),
            "max" | "maximum" => Some(Self::Max),
            _ => None,
        }
    }

    /// The level as OpenAI-shaped protocols (`reasoning_effort`,
    /// `reasoning.effort`) accept it. They top out at `high`, so the two
    /// Anthropic-only rungs above it clamp down rather than 400.
    pub fn openai_label(self) -> &'static str {
        match self {
            Self::XHigh | Self::Max => "high",
            other => other.label(),
        }
    }

    /// The level as the Anthropic Messages API accepts it in
    /// `output_config.effort`. Anthropic has no `minimal`, so the floor is
    /// `low`; thinking itself is left off for minimal by the caller.
    pub fn anthropic_effort(self) -> &'static str {
        match self {
            Self::Minimal => "low",
            other => other.label(),
        }
    }

    /// Thinking budget for the Anthropic protocol, which takes a token budget
    /// rather than a level. `None` for minimal — thinking is simply not
    /// enabled. Only used on Claude 4.5 and earlier, where manual extended
    /// thinking is the sole available mode; newer models take an effort level.
    /// The ceiling stays at 32k: past that Anthropic recommends batch
    /// processing, since long-running requests hit connection limits.
    pub fn thinking_budget(self) -> Option<usize> {
        match self {
            Self::Minimal => None,
            Self::Low => Some(4_096),
            Self::Medium => Some(16_384),
            Self::High | Self::XHigh | Self::Max => Some(32_768),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderProtocol {
    #[default]
    ChatCompletions,
    Responses,
    /// The Anthropic Messages API — a different wire shape from the two
    /// OpenAI formats: a top-level `system` array, `max_tokens` required,
    /// tools with `input_schema`, and a `content_block_*` SSE stream.
    Anthropic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub permission_mode: PermissionMode,
    pub vim_mode: bool,
    pub animations: bool,
    pub show_tooltips: bool,
    pub theme: crate::theme::ThemeChoice,
    /// Show the model's reasoning in the transcript, where the provider streams
    /// it apart from the answer.
    #[serde(default = "default_true")]
    pub show_thinking: bool,
    /// Check GitHub for a newer version tag on startup, at most once a day,
    /// and say so in the transcript. Never downloads or installs anything.
    #[serde(default = "default_true")]
    pub check_updates: bool,
    /// Judge borderline commands and paths with the main model rather than the
    /// auxiliary one. The decision gates what the agent may do, so a stronger
    /// model is sometimes worth the cost.
    #[serde(default)]
    pub safety_uses_main: bool,
    /// Show a live generation rate while a turn runs. Off by default — it is a
    /// diagnostic, and a number that moves every frame is a distraction when
    /// you are reading.
    #[serde(default)]
    pub show_token_rate: bool,
    /// Draft a likely next message in the empty composer once a turn finishes.
    /// Costs one short model call per turn, so it is a setting rather than
    /// always-on behaviour.
    #[serde(default = "default_true")]
    pub draft_replies: bool,
}

fn default_true() -> bool {
    true
}

/// Upstream provider routing for a request.
#[derive(Debug, Clone, Default)]
pub struct Routing {
    /// Preferred providers, most preferred first.
    pub order: Vec<String>,
    /// Whether anything outside `order` may serve the request.
    pub allow_fallbacks: bool,
}

impl Routing {
    pub fn is_pinned(&self) -> bool {
        !self.order.is_empty()
    }

    /// The `provider` object to merge into a request body, or `None` when
    /// nothing is pinned — an unpinned request carries no routing field at all
    /// rather than an empty one.
    pub fn body(&self) -> Option<serde_json::Value> {
        if !self.is_pinned() {
            return None;
        }
        Some(serde_json::json!({
            "order": self.order,
            "allow_fallbacks": self.allow_fallbacks,
        }))
    }

    /// Parse a user-typed list. Commas or whitespace separate entries, so both
    /// `Together, Anthropic` and `Together Anthropic` work.
    pub fn parse_order(input: &str) -> Vec<String> {
        input
            .split([',', ' ', '\t', '\n'])
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

/// Training-trace capture. On by default: the data is only useful if it exists
/// by the time you want it, and it never leaves the machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceSettings {
    pub enabled: bool,
}

impl Default for TraceSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            permission_mode: PermissionMode::Ask,
            vim_mode: true,
            animations: true,
            show_tooltips: true,
            theme: crate::theme::ThemeChoice::Auto,
            show_thinking: true,
            check_updates: true,
            safety_uses_main: false,
            show_token_rate: false,
            draft_replies: true,
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
    /// Balanced high-savings mode for upstream tokens.
    #[serde(default)]
    pub token_compression: bool,
    /// Allow only one non-subagent upstream request at a time.
    #[serde(default)]
    pub one_stream: bool,
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
            token_compression: false,
            one_stream: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    pub keys: BTreeMap<String, String>,
    #[serde(default)]
    pub sync: Option<SyncCredentials>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncCredentials {
    pub server: String,
    pub token: String,
    pub email: String,
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
    pub traces_dir: PathBuf,
    pub attachments_dir: PathBuf,
    pub papercuts_file: PathBuf,
    pub memories_file: PathBuf,
    pub hive_file: PathBuf,
    pub endpoints_dir: PathBuf,
    pub modes_file: PathBuf,
    /// Caches the last release check so startup asks GitHub at most daily.
    pub update_file: PathBuf,
    /// Where a turn cut short mid-stream is written, so the text survives.
    pub recovery_file: PathBuf,
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
            traces_dir: root.join("traces"),
            attachments_dir: root.join("attachments"),
            papercuts_file: root.join("papercuts.json"),
            memories_file: root.join("memories.json"),
            hive_file: root.join("hive.json"),
            endpoints_dir: root.join("endpoints"),
            modes_file: root.join("modes.json"),
            update_file: root.join("update.json"),
            recovery_file: root.join("recovered.md"),
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

    /// The pin is only meaningful if it reaches the request, and only safe if
    /// it stays off requests to endpoints that do not know the field.
    #[test]
    fn effort_parses_aliases_and_maps_to_a_thinking_budget() {
        assert_eq!(ReasoningEffort::parse("HIGH"), Some(ReasoningEffort::High));
        assert_eq!(ReasoningEffort::parse("med"), Some(ReasoningEffort::Medium));
        assert_eq!(ReasoningEffort::parse(" low "), Some(ReasoningEffort::Low));
        assert_eq!(ReasoningEffort::parse("ludicrous"), None);

        // The two Anthropic-only rungs above `high`. `max` is now its own
        // level, not an alias for `high` — it means unconstrained spend.
        assert_eq!(ReasoningEffort::parse("max"), Some(ReasoningEffort::Max));
        assert_eq!(
            ReasoningEffort::parse("xhigh"),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(
            ReasoningEffort::parse("x-high"),
            Some(ReasoningEffort::XHigh)
        );

        // Anthropic takes a budget, and minimal means "do not enable thinking".
        assert_eq!(ReasoningEffort::Minimal.thinking_budget(), None);
        assert!(
            ReasoningEffort::Low.thinking_budget() < ReasoningEffort::High.thinking_budget(),
            "budgets rise with effort"
        );
    }

    /// Each protocol only knows its own rungs: sending `xhigh` to an
    /// OpenAI-shaped endpoint, or `minimal` to Anthropic, is a 400.
    #[test]
    fn effort_levels_map_to_what_each_protocol_accepts() {
        // OpenAI-shaped tops out at high; the rungs above clamp down.
        assert_eq!(ReasoningEffort::XHigh.openai_label(), "high");
        assert_eq!(ReasoningEffort::Max.openai_label(), "high");
        assert_eq!(ReasoningEffort::Minimal.openai_label(), "minimal");
        assert_eq!(ReasoningEffort::Medium.openai_label(), "medium");

        // Anthropic has no `minimal`, so the floor is `low`, and it keeps the
        // two levels above `high` that OpenAI does not have.
        assert_eq!(ReasoningEffort::Minimal.anthropic_effort(), "low");
        assert_eq!(ReasoningEffort::XHigh.anthropic_effort(), "xhigh");
        assert_eq!(ReasoningEffort::Max.anthropic_effort(), "max");
        assert_eq!(ReasoningEffort::High.anthropic_effort(), "high");
    }

    #[test]
    fn routing_becomes_a_provider_object_only_when_pinned() {
        let none = Routing::default();
        assert!(!none.is_pinned());
        assert!(
            none.body().is_none(),
            "an unpinned request carries no field"
        );

        let pinned = Routing {
            order: vec!["Together".into(), "Anthropic".into()],
            allow_fallbacks: false,
        };
        let body = pinned.body().expect("a provider object");
        assert_eq!(body["order"][0], "Together");
        assert_eq!(body["order"][1], "Anthropic");
        assert_eq!(body["allow_fallbacks"], false);
    }

    #[test]
    fn an_order_can_be_written_with_commas_or_spaces() {
        assert_eq!(
            Routing::parse_order("Together, Anthropic"),
            vec!["Together", "Anthropic"]
        );
        assert_eq!(
            Routing::parse_order("  DeepInfra   Novita  "),
            vec!["DeepInfra", "Novita"]
        );
        assert_eq!(Routing::parse_order("Z.AI"), vec!["Z.AI"]);
        assert!(Routing::parse_order("  ,, ").is_empty());
    }
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
                aux_model: None,
                reasoning_effort: None,
                endpoint: None,
                providers: Vec::new(),
                allow_fallbacks: true,
                context_window: None,
                max_output_tokens: None,
            },
        );
        settings.profiles.get_mut("local").unwrap().context_window = Some(1_000_000);
        settings
            .profiles
            .get_mut("local")
            .unwrap()
            .max_output_tokens = Some(64_000);
        settings.default_profile = "local".into();
        settings.agent.token_compression = true;
        settings.agent.one_stream = true;
        settings.save(&paths).unwrap();
        let loaded = Settings::load(&paths).unwrap();
        assert_eq!(loaded.profiles["local"].model, "codestral");
        assert_eq!(loaded.profiles["local"].context_window, Some(1_000_000));
        assert_eq!(loaded.profiles["local"].max_output_tokens, Some(64_000));
        assert_eq!(loaded.version, SETTINGS_VERSION);
        assert!(loaded.feedback.enabled);
        assert!(loaded.ui.animations);
        assert!(loaded.agent.token_compression);
        assert!(loaded.agent.one_stream);
    }

    #[test]
    fn profile_limits_prefer_the_profile_then_leftover_agent_fields() {
        let mut settings = Settings::default();
        settings.agent.context_window = Some(128_000);
        settings.agent.max_output_tokens = Some(8_000);
        settings.profiles.insert(
            "claude".into(),
            ProviderProfile {
                name: "Claude".into(),
                base_url: "https://api.anthropic.com".into(),
                model: "claude".into(),
                protocol: ProviderProtocol::Anthropic,
                api_key_env: None,
                aux_model: None,
                reasoning_effort: None,
                endpoint: None,
                providers: Vec::new(),
                allow_fallbacks: true,
                context_window: Some(1_000_000),
                max_output_tokens: Some(64_000),
            },
        );
        settings.profiles.insert(
            "local".into(),
            ProviderProfile {
                name: "Local".into(),
                base_url: "http://localhost:11434/v1".into(),
                model: "codestral".into(),
                protocol: ProviderProtocol::ChatCompletions,
                api_key_env: None,
                aux_model: None,
                reasoning_effort: None,
                endpoint: None,
                providers: Vec::new(),
                allow_fallbacks: true,
                context_window: None,
                max_output_tokens: None,
            },
        );
        assert_eq!(
            settings.profile_limits("claude"),
            (Some(1_000_000), Some(64_000))
        );
        assert_eq!(
            settings.profile_limits("local"),
            (Some(128_000), Some(8_000))
        );
    }

    #[test]
    fn validate_profile_id_rejects_empty_and_unsafe_names() {
        assert!(validate_profile_id("claude-4").is_ok());
        assert!(validate_profile_id("local.v2").is_ok());
        assert!(validate_profile_id("").is_err());
        assert!(validate_profile_id("has space").is_err());
        assert!(validate_profile_id("slash/name").is_err());
    }

    #[test]
    fn resolve_prefers_cli_then_profile_then_leftover_agent_limits() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        let mut settings = Settings {
            default_profile: "claude".into(),
            ..Settings::default()
        };
        settings.agent.context_window = Some(128_000);
        settings.agent.max_output_tokens = Some(8_000);
        settings.profiles.insert(
            "claude".into(),
            ProviderProfile {
                name: "Claude".into(),
                base_url: "https://api.anthropic.com".into(),
                model: "claude".into(),
                protocol: ProviderProtocol::Anthropic,
                api_key_env: None,
                aux_model: None,
                reasoning_effort: None,
                endpoint: None,
                providers: Vec::new(),
                allow_fallbacks: true,
                context_window: Some(1_000_000),
                max_output_tokens: Some(64_000),
            },
        );
        settings.profiles.insert(
            "local".into(),
            ProviderProfile {
                name: "Local".into(),
                base_url: "http://localhost:11434/v1".into(),
                model: "codestral".into(),
                protocol: ProviderProtocol::ChatCompletions,
                api_key_env: None,
                aux_model: None,
                reasoning_effort: None,
                endpoint: None,
                providers: Vec::new(),
                allow_fallbacks: true,
                context_window: None,
                max_output_tokens: None,
            },
        );
        let credentials = Credentials::default();

        let from_profile = Config::resolve(
            &Cli::parse_from(["abacus", workspace.to_str().unwrap()]),
            &settings,
            &credentials,
            paths.clone(),
        )
        .unwrap();
        assert_eq!(from_profile.model_limits.context_window, 1_000_000);
        assert_eq!(
            from_profile.model_limits.configured_output_tokens,
            Some(64_000)
        );

        let from_agent = Config::resolve(
            &Cli::parse_from(["abacus", "--profile", "local", workspace.to_str().unwrap()]),
            &settings,
            &credentials,
            paths.clone(),
        )
        .unwrap();
        assert_eq!(from_agent.model_limits.context_window, 128_000);
        assert_eq!(
            from_agent.model_limits.configured_output_tokens,
            Some(8_000)
        );

        let from_cli = Config::resolve(
            &Cli::parse_from([
                "abacus",
                "--context-window",
                "2000000",
                "--max-output-tokens",
                "32000",
                workspace.to_str().unwrap(),
            ]),
            &settings,
            &credentials,
            paths,
        )
        .unwrap();
        assert_eq!(from_cli.model_limits.context_window, 2_000_000);
        assert_eq!(from_cli.model_limits.configured_output_tokens, Some(32_000));
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
