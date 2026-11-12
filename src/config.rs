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
use crate::providers::{Connection, ProviderConfig, join_ref, split_ref};
use crate::tool_format::ToolFormat;

/// Version 3 replaced per-endpoint profiles with providers (connections) and
/// profiles (a `provider/model` and how to run it). [`Settings::load`]
/// migrates older files in place, keeping a copy of the original.
pub const SETTINGS_VERSION: u32 = 3;

/// clap value parser that tolerates `k`/`m` suffixes (e.g. `128k`, `1m`).
fn parse_token_arg(input: &str) -> Result<usize, String> {
    crate::model_info::parse_tokens(input).map_err(|error| error.to_string())
}

fn parse_permission(input: &str) -> Result<Permission, String> {
    Permission::parse(input).ok_or_else(|| format!("unknown mode `{}` (expected auto or yolo)", input.trim()))
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
#[command(name = "abacus", version, about = "A fast, focused terminal coding agent")]
pub struct Cli {
    /// Project directory (defaults to the current directory)
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Profile to run, by name (see `abacus profiles`)
    #[arg(long)]
    pub profile: Option<String>,

    /// Model for this run: `provider/model`, or a model id on the profile's own provider
    #[arg(short = 'm', long, env = "ABACUS_MODEL")]
    pub model: Option<String>,

    /// Talk to this endpoint instead of a configured provider (with --model)
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

    /// Attach an image to the headless prompt (PNG, JPEG, GIF or WebP; repeatable)
    #[arg(short = 'i', long = "image", value_name = "PATH", requires = "prompt")]
    pub images: Vec<PathBuf>,

    /// Output format for headless mode
    #[arg(long, value_enum, default_value_t = OutputFormat::Plain)]
    pub output_format: OutputFormat,

    /// Continue the most recent session for this workspace
    #[arg(short = 'c', long = "continue")]
    pub continue_last: bool,

    /// Resume a session by ID (a unique prefix is accepted)
    #[arg(short = 'r', long)]
    pub resume: Option<String>,

    /// YOLO mode for this run: every action runs without review
    #[arg(short = 'y', long = "yolo", alias = "always-approve")]
    pub yes: bool,

    /// Permission mode for this run: auto (actions are reviewed) or yolo
    #[arg(long, value_name = "MODE", value_parser = parse_permission)]
    pub mode: Option<Permission>,

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

    /// Make the prompt the goal and keep working, turn after turn, until the model marks it complete or pauses it
    #[arg(long)]
    pub goal: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// Whether the command line names an endpoint of its own, so no stored
    /// profile is needed.
    pub fn has_inline_provider(&self) -> bool {
        self.model.is_some() && self.base_url.is_some()
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Connect a provider and choose a model
    Setup,
    /// List the models every provider serves, as `provider/model`
    Models {
        /// Only this provider's models
        #[arg(long)]
        provider: Option<String>,
        /// Ask the providers again instead of using the cached lists
        #[arg(long)]
        refresh: bool,
        /// Print JSON, for a front end
        #[arg(long)]
        json: bool,
    },
    /// List the configured providers
    Providers {
        /// Print JSON, for a front end
        #[arg(long)]
        json: bool,
    },
    /// List the profiles, the active one marked
    Profiles {
        /// Print JSON, for a front end
        #[arg(long)]
        json: bool,
    },
    /// List the OpenRouter upstreams that can serve the active model
    Upstreams,
    /// List saved sessions for this workspace
    Sessions {
        /// List every workspace that has saved sessions, not just this one
        #[arg(long)]
        all: bool,
        /// Print JSON, for a front end
        #[arg(long)]
        json: bool,
    },
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
    /// Serve a session over newline-delimited JSON on stdin/stdout, for a
    /// desktop front end. One process, one workspace, one active session.
    AppServer {
        /// Hold conversations rather than work on a project: the model gets
        /// the web, Python and questions to the person, not the files,
        /// shell or git of the folder
        #[arg(long)]
        chat: bool,
    },
    /// Install the newest release, and update the desktop app if it is installed
    Update {
        /// Also install the desktop app if it is not installed yet
        #[arg(long)]
        desktop: bool,
        /// Only say whether a newer release exists
        #[arg(long, conflicts_with = "desktop")]
        check: bool,
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
    /// Show the account, this device, and what is waiting to sync
    Status,
    /// List sessions stored by the server
    Sessions,
    /// Upload local sessions that changed since they last synced
    Push {
        /// Upload one session ID or unique prefix (every changed session by default)
        session: Option<String>,
        /// Replace a conflicting remote revision
        #[arg(long)]
        force: bool,
    },
    /// Download sessions that changed on the server
    Pull {
        /// Download one session ID or prefix (everything that changed by default)
        session: Option<String>,
        /// Replace local sessions instead of preserving them as forks
        #[arg(long)]
        force: bool,
    },
    /// Print a single-use link that signs a phone into this account
    Pair {
        /// Open this session on the phone (ID or unique prefix)
        #[arg(long)]
        session: Option<String>,
        /// Print only the URL
        #[arg(long)]
        url_only: bool,
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
        /// Run in YOLO mode: actions run unreviewed while nobody is watching
        #[arg(long = "yolo", alias = "always-approve")]
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

/// A model on a provider, resolved for use.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub connection: Connection,
    /// The id the provider knows the model by.
    pub model: String,
}

impl Target {
    /// `provider/model`.
    pub fn reference(&self) -> String {
        join_ref(&self.connection.provider, &self.model)
    }
}

/// The models a profile assigned to background jobs (see [`crate::roles`]).
/// Only explicit assignments are here: an unassigned role stays `None` and
/// follows the main model wherever `/profile` moves it.
#[derive(Debug, Clone, Default)]
pub struct RoleTargets {
    pub aux: Option<Target>,
    pub subagent: Option<Target>,
    pub compaction: Option<Target>,
    /// The auto-mode reviewer. Unassigned it uses the `aux` model.
    pub review: Option<Target>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub workspace: PathBuf,
    /// The active profile's name, or `cli` for an endpoint given on the
    /// command line.
    pub profile: String,
    /// The model id the provider knows, `openai/gpt-6` on OpenRouter.
    pub model: String,
    /// The provider the model is on, resolved.
    pub connection: Connection,
    pub max_steps: usize,
    pub tool_output_limit: usize,
    /// How many timers the model may have waiting at once; 0 offers none.
    pub max_timers: usize,
    /// Whether actions are reviewed (AUTO) or simply run (YOLO).
    pub permission: Permission,
    pub no_session: bool,
    pub model_limits: ModelLimits,
    pub tool_format: ToolFormat,
    /// Whether to append SFT training traces for this session.
    pub trace_enabled: bool,
    /// Upstream routing preferences, applied by providers that support them.
    pub routing: Routing,
    pub web_search: crate::web::WebConfig,
    pub roles: RoleTargets,
    /// Reasoning effort sent with every request, when set.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Aggressively reduce upstream context and nonessential auxiliary calls.
    pub token_compression: bool,
    /// Serialize foreground and auxiliary requests on this provider session.
    pub one_stream: bool,
    /// Mark prompt-cache breakpoints on requests that support them.
    pub prompt_cache: bool,
    /// Summarise a long session that sits idle while its prompt cache is
    /// still warm (see [`Config::compacts_when_idle`]).
    pub idle_compaction: bool,
    /// A conversation rather than work on the workspace: a chat's own system
    /// prompt and tools (see `abacus app-server --chat`).
    pub chat: bool,
    pub paths: AbacusPaths,
}

impl Config {
    /// A configuration pointed at one endpoint, with every option at its
    /// default. What an embedder or a test starts from.
    pub fn for_endpoint(workspace: PathBuf, base_url: String, model: String, paths: AbacusPaths) -> Self {
        Self {
            workspace,
            profile: "default".to_owned(),
            model,
            connection: Connection::inline(&base_url, ProviderProtocol::default(), None),
            max_steps: 32,
            tool_output_limit: 30_000,
            max_timers: crate::timers::DEFAULT_MAX_TIMERS,
            permission: Permission::default(),
            no_session: false,
            model_limits: ModelLimits::default(),
            tool_format: ToolFormat::default(),
            trace_enabled: false,
            routing: Routing::default(),
            web_search: Default::default(),
            roles: RoleTargets::default(),
            reasoning_effort: None,
            token_compression: false,
            one_stream: false,
            prompt_cache: true,
            idle_compaction: true,
            chat: false,
            paths,
        }
    }

    /// A configuration with no provider yet, for the app server, which helps
    /// a front end set one up rather than refusing to start.
    pub fn unconfigured(cli: &Cli, settings: &Settings, paths: AbacusPaths) -> Result<Self> {
        let workspace = resolve_workspace(cli.path.as_deref())?;
        let mut config = Self::for_endpoint(workspace, String::new(), String::new(), paths);
        config.profile = String::new();
        config.connection = Connection::default();
        config.apply_agent_settings(cli, settings);
        Ok(config)
    }

    pub fn resolve(cli: &Cli, settings: &Settings, credentials: &Credentials, paths: AbacusPaths) -> Result<Self> {
        let workspace = resolve_workspace(cli.path.as_deref())?;
        let mut config = Self::for_endpoint(workspace, String::new(), String::new(), paths);
        config.apply_agent_settings(cli, settings);

        // An endpoint named on the command line needs no stored profile: the
        // way a CI job or a one-off run points Abacus somewhere.
        if let Some(base_url) = cli.base_url.as_deref().map(str::trim).filter(|url| !url.is_empty()) {
            let model = cli
                .model
                .clone()
                .filter(|model| !model.trim().is_empty())
                .context("--base-url needs --model to say which model to run")?;
            config.profile = "cli".to_owned();
            config.connection = Connection::inline(base_url, cli.protocol.unwrap_or_default(), cli.api_key.clone());
            config.model_limits = ModelLimits::resolve_from_name(&model, cli.context_window, cli.max_output_tokens);
            config.tool_format = cli.tool_format.unwrap_or_default();
            config.model = model;
            return Ok(config);
        }

        let name = cli.profile.clone().unwrap_or_else(|| settings.default_profile.clone());
        let profile = settings.profiles.get(&name).with_context(|| {
            if settings.profiles.is_empty() {
                "no profile configured yet; run `abacus setup`".to_owned()
            } else {
                format!(
                    "no profile named `{name}` (there is {}); `abacus profiles` lists them",
                    settings.profiles.keys().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
                )
            }
        })?;
        let reference = match cli.model.as_deref().map(str::trim).filter(|model| !model.is_empty()) {
            Some(model) => settings.qualify(model, profile),
            None => profile.model.clone(),
        };
        let target = settings.target(&reference, credentials)?;
        config.profile = name;
        config.connection = target.connection;
        if let Some(key) = cli.api_key.clone() {
            config.connection.api_key = Some(key);
        }
        if let Some(protocol) = cli.protocol {
            config.connection.protocol = protocol;
        }
        config.model_limits = ModelLimits::resolve_from_name(
            &target.model,
            cli.context_window.or(profile.context_window),
            cli.max_output_tokens.or(profile.max_output_tokens),
        );
        config.tool_format =
            cli.tool_format.or_else(|| profile.tool_format.as_deref().and_then(ToolFormat::parse)).unwrap_or_default();
        config.model = target.model;
        config.apply_profile(settings, profile, credentials);
        Ok(config)
    }

    /// The settings that are the agent's rather than the model's.
    fn apply_agent_settings(&mut self, cli: &Cli, settings: &Settings) {
        self.max_steps = cli.max_steps.unwrap_or(settings.agent.max_steps).clamp(*MAX_STEPS.start(), *MAX_STEPS.end());
        self.tool_output_limit =
            settings.agent.tool_output_limit.clamp(*TOOL_OUTPUT_LIMIT.start(), *TOOL_OUTPUT_LIMIT.end());
        self.max_timers = settings.agent.max_timers.min(*crate::timers::MAX_TIMERS.end());
        self.permission = if cli.yes { Permission::Yolo } else { cli.mode.unwrap_or(settings.agent.mode) };
        self.no_session = cli.no_session;
        self.chat = matches!(cli.command, Some(Command::AppServer { chat: true }));
        self.trace_enabled = settings.trace.enabled;
        self.web_search = settings.search.resolve();
        self.token_compression = settings.agent.token_compression;
        self.one_stream = settings.agent.one_stream;
        self.prompt_cache = settings.agent.prompt_cache;
        self.idle_compaction = settings.agent.idle_compaction;
    }

    /// Whether a long session that goes quiet is compacted before the person
    /// comes back. Not on a server on this machine or its local network: it
    /// bills nothing for resending the history and keeps its own cache, so a
    /// summary would only lose detail.
    pub fn compacts_when_idle(&self) -> bool {
        self.idle_compaction && !crate::providers::is_local_network_url(&self.connection.base_url)
    }

    /// Where compaction steps in on this model, for a context meter to mark.
    pub fn compaction_marks(&self) -> crate::compaction::Marks {
        let budget = crate::agent::compression_budget(self.model_limits.compaction_budget(), self.token_compression);
        crate::compaction::marks(&budget, self.compacts_when_idle())
    }

    /// Point this configuration at another profile, the way `/profile` and
    /// the desktop's profile picker switch mid-session. Limits come from the
    /// profile, then from the provider's cached model list.
    pub fn use_profile(&mut self, settings: &Settings, credentials: &Credentials, name: &str) -> Result<()> {
        let profile = settings.profiles.get(name).with_context(|| format!("no profile named `{name}`"))?;
        let target = settings.target(&profile.model, credentials)?;
        self.profile = name.to_owned();
        self.connection = target.connection;
        self.model = target.model;
        self.model_limits =
            ModelLimits::resolve_from_name(&self.model, profile.context_window, profile.max_output_tokens);
        if self.model_limits.source != crate::model_info::LimitSource::Override
            && let Some(card) = crate::providers::cached_catalog(&self.paths, &self.connection.provider)
                .and_then(|catalog| catalog.card(&self.model).cloned())
            && let Some(context) = card.context_length
        {
            self.model_limits.apply_detected(context, card.max_output_tokens);
        }
        self.tool_format = profile.tool_format.as_deref().and_then(ToolFormat::parse).unwrap_or_default();
        self.apply_profile(settings, profile, credentials);
        Ok(())
    }

    /// What a profile sets beyond its model: effort, routing, and the models
    /// its background jobs run on.
    pub fn apply_profile(&mut self, settings: &Settings, profile: &Profile, credentials: &Credentials) {
        self.reasoning_effort = profile.reasoning_effort;
        self.routing = Routing { order: profile.upstreams.clone(), allow_fallbacks: profile.allow_fallbacks };
        // A role naming a provider that is gone falls back to the main model
        // rather than stopping the session from opening at all.
        let role = |id: &str| profile.role_model(id).and_then(|reference| settings.target(reference, credentials).ok());
        self.roles = RoleTargets {
            aux: role("aux"),
            subagent: role("subagent"),
            compaction: role("compaction"),
            review: role("review"),
        };
    }

    /// Whether a model is configured — false only for the app server's
    /// set-up-first state.
    pub fn is_configured(&self) -> bool {
        !self.model.trim().is_empty() && !self.connection.base_url.is_empty()
    }

    /// `provider/model`, the name the model goes by everywhere a person sees it.
    pub fn model_ref(&self) -> String {
        if self.model.is_empty() {
            return String::new();
        }
        join_ref(&self.connection.provider, &self.model)
    }

    pub fn workspace_name(&self) -> &str {
        self.workspace.file_name().and_then(|name| name.to_str()).unwrap_or("workspace")
    }
}

/// Whether the agent's actions are reviewed before they run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    /// Reading, searching and edits inside the workspace run at once; anything
    /// else is judged first by a reviewer model against what you asked for,
    /// and blocked when it goes beyond it.
    #[default]
    Auto,
    /// Everything runs without review.
    Yolo,
}

impl Permission {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Yolo => "YOLO",
        }
    }

    /// The mode `name` spells, in any case. The names older versions used
    /// still parse: what they meant maps onto one of the two that remain.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "auto" | "plan" | "build" | "ask" => Some(Self::Auto),
            "yolo" | "always-approve" | "bypass" => Some(Self::Yolo),
            _ => None,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Auto => Self::Yolo,
            Self::Yolo => Self::Auto,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    /// The profile a new session runs, by name.
    pub default_profile: String,
    /// The API connections, by id. Model references name them.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// The profiles, by name: a `provider/model` and how to run it.
    pub profiles: BTreeMap<String, Profile>,
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
    #[serde(default)]
    pub remote: RemoteSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            default_profile: String::new(),
            providers: BTreeMap::new(),
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
            remote: RemoteSettings::default(),
        }
    }
}

/// A profile: a model, and the settings to run it with. Its name is the key
/// it is stored under; new ones are named after their model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// `provider/model`.
    pub model: String,
    /// Context window in tokens. Unset means detect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// Max output tokens, sent as `max_tokens`. Unset means detect, or leave
    /// it to the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<usize>,
    /// How hard the model should think. Unset leaves it to the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// How to read tool calls a model writes as text (`auto`, `none`,
    /// `hermes`, …). Unset means `auto`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_format: Option<String>,
    /// OpenRouter upstreams to route to, most preferred first, spelled as
    /// OpenRouter reports them (`Together`, `deepinfra/fp8`). One model is
    /// served by several suppliers that differ in context length,
    /// quantization and speed; pinning is how you stop landing on whichever
    /// is cheapest today. Ignored by every other provider.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<String>,
    /// Whether an upstream outside `upstreams` may serve a request none of
    /// them can. False makes the pin strict.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub allow_fallbacks: bool,
    /// Models for the jobs beyond the conversation, each `provider/model`:
    /// `aux`, `subagent`, `compaction`, `review` — see [`crate::roles`].
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, String>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            model: String::new(),
            context_window: None,
            max_output_tokens: None,
            reasoning_effort: None,
            tool_format: None,
            upstreams: Vec::new(),
            allow_fallbacks: true,
            roles: BTreeMap::new(),
        }
    }
}

impl Profile {
    pub fn for_model(reference: &str) -> Self {
        Self { model: reference.to_owned(), ..Self::default() }
    }

    /// The provider id this profile's model is on.
    pub fn provider(&self) -> &str {
        split_ref(&self.model).map_or("", |(provider, _)| provider)
    }
}

/// Profile names are free text — they are what pickers show — but one line,
/// and not so long a picker cannot show it.
pub fn validate_profile_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        bail!("a profile name is 1–80 characters on one line");
    }
    Ok(name)
}

fn is_true(value: &bool) -> bool {
    *value
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
        let content = fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?;
        toml::from_str(&content).with_context(|| format!("invalid project extension config: {}", path.display()))
    }
}

impl Settings {
    /// Read the settings. An older file is moved to the current shape first
    /// and saved straight away, with the original kept beside it as
    /// `config.toml.v<N>.bak`.
    pub fn load(paths: &AbacusPaths) -> Result<Self> {
        if !paths.config_file.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&paths.config_file)
            .with_context(|| format!("could not read {}", paths.config_file.display()))?;
        let mut table: toml::Table =
            toml::from_str(&content).with_context(|| format!("invalid config: {}", paths.config_file.display()))?;
        let version = table.get("version").and_then(toml::Value::as_integer).unwrap_or(1);
        if version > i64::from(SETTINGS_VERSION) {
            bail!("config version {version} is newer than this Abacus supports ({SETTINGS_VERSION})");
        }
        let migrated = version < i64::from(SETTINGS_VERSION);
        let mut credentials = None;
        if migrated {
            let mut loaded = Credentials::load(paths).unwrap_or_default();
            if migrate_v2(&mut table, &mut loaded, paths) {
                credentials = Some(loaded);
            }
        }
        let mut settings: Self = toml::Value::Table(table)
            .try_into()
            .with_context(|| format!("invalid config: {}", paths.config_file.display()))?;
        settings.version = SETTINGS_VERSION;
        if migrated {
            let backup = paths.root.join(format!("config.toml.v{version}.bak"));
            if !backup.exists() {
                atomic_write(&backup, content.as_bytes(), false)?;
            }
            settings.save(paths)?;
            if let Some(credentials) = credentials {
                credentials.save(paths)?;
            }
        }
        Ok(settings)
    }

    pub fn save(&self, paths: &AbacusPaths) -> Result<()> {
        paths.ensure()?;
        let content = toml::to_string_pretty(self).context("could not encode configuration")?;
        atomic_write(&paths.config_file, content.as_bytes(), false)
    }

    /// Whether the default profile exists and so does its provider.
    pub fn is_configured(&self) -> bool {
        self.profiles.get(&self.default_profile).is_some_and(|profile| self.providers.contains_key(profile.provider()))
    }

    pub fn active_profile(&self) -> Option<&Profile> {
        self.profiles.get(&self.default_profile)
    }

    /// `model` as a reference: as it is when its first segment names a
    /// provider, otherwise a model on `profile`'s own provider. So
    /// `openrouter/openai/gpt-6` and, on an OpenRouter profile, `openai/gpt-6`
    /// both mean the same model.
    pub fn qualify(&self, model: &str, profile: &Profile) -> String {
        let model = model.trim();
        match split_ref(model) {
            Some((provider, _)) if self.providers.contains_key(provider) => model.to_owned(),
            _ => join_ref(profile.provider(), model),
        }
    }

    /// Resolve a `provider/model` reference.
    pub fn target(&self, reference: &str, credentials: &Credentials) -> Result<Target> {
        let (provider, model) = split_ref(reference)
            .with_context(|| format!("`{reference}` names no provider; models are written `provider/model`"))?;
        let config = self.providers.get(provider).with_context(|| {
            format!("`{reference}` is on provider `{provider}`, which is not configured; `abacus providers` lists them")
        })?;
        Ok(Target { connection: Connection::resolve(provider, config, credentials), model: model.to_owned() })
    }

    /// The profile that runs exactly `reference`, if one does.
    pub fn profile_for(&self, reference: &str) -> Option<&str> {
        self.profiles.iter().find(|(_, profile)| profile.model == reference).map(|(name, _)| name.as_str())
    }

    /// The names of the profiles on `provider`.
    pub fn profiles_on(&self, provider: &str) -> Vec<String> {
        self.profiles
            .iter()
            .filter(|(_, profile)| profile.provider() == provider)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// `base`, or `base (2)`, `base (3)`… — the first no profile has.
    pub fn unused_profile_name(&self, base: &str) -> String {
        std::iter::once(base.to_owned())
            .chain((2..).map(|suffix| format!("{base} ({suffix})")))
            .find(|candidate| !self.profiles.contains_key(candidate))
            .expect("an unused suffix exists")
    }

    /// Add a profile for `reference` with every setting at its default, named
    /// after the model, and return its name.
    pub fn add_profile(&mut self, reference: &str) -> String {
        let name = self.unused_profile_name(reference);
        self.profiles.insert(name.clone(), Profile::for_model(reference));
        name
    }

    pub fn rename_profile(&mut self, from: &str, to: &str) -> Result<String> {
        let to = validate_profile_name(to)?.to_owned();
        if to == from {
            return Ok(to);
        }
        if self.profiles.contains_key(&to) {
            bail!("a profile named `{to}` already exists");
        }
        let profile = self.profiles.remove(from).with_context(|| format!("no profile named `{from}`"))?;
        self.profiles.insert(to.clone(), profile);
        if self.default_profile == from {
            self.default_profile = to.clone();
        }
        Ok(to)
    }

    /// Remove a profile. The last one stays: a session needs something to run.
    pub fn remove_profile(&mut self, name: &str) -> Result<()> {
        if !self.profiles.contains_key(name) {
            bail!("no profile named `{name}`");
        }
        if self.profiles.len() == 1 {
            bail!("that is the only profile; add another before removing it");
        }
        self.profiles.remove(name);
        if self.default_profile == name {
            self.default_profile = self.profiles.keys().next().cloned().unwrap_or_default();
        }
        Ok(())
    }

    /// Remove a provider and every profile on it, returning those profiles.
    /// Roles that pointed at it fall back to their profile's model.
    pub fn remove_provider(&mut self, id: &str) -> Vec<String> {
        self.providers.remove(id);
        let removed = self.profiles_on(id);
        for name in &removed {
            self.profiles.remove(name);
        }
        for profile in self.profiles.values_mut() {
            profile.roles.retain(|_, reference| split_ref(reference).is_none_or(|(provider, _)| provider != id));
        }
        if !self.profiles.contains_key(&self.default_profile) {
            self.default_profile = self.profiles.keys().next().cloned().unwrap_or_default();
        }
        removed
    }
}

/// Move a version 1 or 2 settings table to version 3, in place.
///
/// An old profile was an endpoint and a model at once. Each becomes a provider
/// — shared with any earlier one on the same endpoint and key — and a profile
/// of the same name on it, so `--profile` and scheduled jobs keep working. A
/// profile that named a scripted endpoint file has the file folded into its
/// provider. Returns whether `credentials` gained keys, which happens when a
/// provider got an id other than its old profile's, or a scripted endpoint
/// carried a literal token. Old entries are left in place, so restoring the
/// backup restores a working setup.
fn migrate_v2(table: &mut toml::Table, credentials: &mut Credentials, paths: &AbacusPaths) -> bool {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct OldProfile {
        name: String,
        base_url: String,
        model: String,
        protocol: ProviderProtocol,
        api_key_env: Option<String>,
        aux_model: Option<String>,
        reasoning_effort: Option<ReasoningEffort>,
        endpoint: Option<String>,
        providers: Vec<String>,
        #[serde(default = "default_true")]
        allow_fallbacks: bool,
        context_window: Option<usize>,
        max_output_tokens: Option<usize>,
        roles: BTreeMap<String, String>,
    }

    let old: BTreeMap<String, OldProfile> =
        table.remove("profiles").and_then(|value| value.try_into().ok()).unwrap_or_default();
    let mut agent = table.remove("agent").and_then(|value| value.as_table().cloned()).unwrap_or_default();
    let mut ui = table.remove("ui").and_then(|value| value.as_table().cloned()).unwrap_or_default();
    let tokens = |value: Option<toml::Value>| value.and_then(|value| value.as_integer()).map(|value| value as usize);
    let leftover_context = tokens(agent.remove("context_window"));
    let leftover_output = tokens(agent.remove("max_output_tokens"));
    let leftover_format = agent.remove("tool_format").and_then(|value| value.as_str().map(str::to_owned));
    let review_on_main = ui.remove("safety_uses_main").and_then(|value| value.as_bool()).unwrap_or(false);
    if ui.remove("permission_mode").and_then(|value| value.as_str().map(str::to_owned)).as_deref()
        == Some("always-approve")
    {
        agent.insert("mode".to_owned(), toml::Value::String("yolo".to_owned()));
    }

    let mut providers: BTreeMap<String, ProviderConfig> = BTreeMap::new();
    let mut profiles: BTreeMap<String, Profile> = BTreeMap::new();
    let mut credentials_changed = false;
    for (old_id, old) in old {
        let name = if old.name.trim().is_empty() { old_id.clone() } else { old.name.clone() };
        let mut provider = ProviderConfig {
            name: name.clone(),
            base_url: old.base_url.trim().trim_end_matches('/').to_owned(),
            protocol: old.protocol,
            api_key_env: old.api_key_env.clone(),
            ..ProviderConfig::default()
        };
        let mut model = old.model.trim().to_owned();
        let mut key = credentials.keys.get(&old_id).cloned();
        if let Some(reference) = &old.endpoint
            && let Ok(imported) = crate::providers::import_endpoint(reference, &paths.endpoints_dir)
        {
            provider = ProviderConfig { name, ..imported.provider };
            if model.is_empty() {
                model = imported.model.unwrap_or_default();
            }
            key = key.or(imported.token);
        }
        let shared = providers
            .iter()
            .find(|(id, existing)| {
                ProviderConfig { name: String::new(), ..(*existing).clone() }
                    == ProviderConfig { name: String::new(), ..provider.clone() }
                    && credentials.keys.get(*id) == key.as_ref()
            })
            .map(|(id, _)| id.clone());
        let provider_id = match shared {
            Some(id) => id,
            None => {
                // An id another profile's key is stored under is not reused,
                // so no key is ever written over.
                let id = crate::providers::unused_id(&preferred_provider_id(&old_id, &provider), |candidate| {
                    providers.contains_key(candidate)
                        || credentials.keys.get(candidate).is_some_and(|stored| Some(stored) != key.as_ref())
                });
                if let Some(key) = &key
                    && credentials.keys.get(&id) != Some(key)
                {
                    credentials.keys.insert(id.clone(), key.clone());
                    credentials_changed = true;
                }
                providers.insert(id.clone(), provider);
                id
            }
        };
        // A profile that never got a model was never runnable; its provider
        // is kept so the key is not lost.
        if model.is_empty() {
            continue;
        }
        let qualify = |model: &str| join_ref(&provider_id, model.trim());
        let mut roles = BTreeMap::new();
        if let Some(aux) = old.aux_model.as_deref().filter(|aux| !aux.trim().is_empty()) {
            roles.insert("aux".to_owned(), qualify(aux));
        }
        for (role, model) in &old.roles {
            if !model.trim().is_empty() && role != "default" {
                roles.insert(role.clone(), qualify(model));
            }
        }
        if review_on_main {
            roles.insert("review".to_owned(), qualify(&model));
        }
        profiles.insert(
            old_id,
            Profile {
                model: qualify(&model),
                context_window: old.context_window.or(leftover_context),
                max_output_tokens: old.max_output_tokens.or(leftover_output),
                reasoning_effort: old.reasoning_effort,
                tool_format: leftover_format.clone(),
                upstreams: old.providers,
                allow_fallbacks: old.allow_fallbacks,
                roles,
            },
        );
    }

    let default_profile = table.get("default_profile").and_then(toml::Value::as_str).unwrap_or_default().to_owned();
    if !profiles.contains_key(&default_profile) {
        let first = profiles.keys().next().cloned().unwrap_or_default();
        table.insert("default_profile".to_owned(), toml::Value::String(first));
    }
    let as_table =
        |value: Result<toml::Value, toml::ser::Error>| value.unwrap_or(toml::Value::Table(toml::Table::new()));
    table.insert("providers".to_owned(), as_table(toml::Value::try_from(&providers)));
    table.insert("profiles".to_owned(), as_table(toml::Value::try_from(&profiles)));
    table.insert("agent".to_owned(), toml::Value::Table(agent));
    table.insert("ui".to_owned(), toml::Value::Table(ui));
    table.insert("version".to_owned(), toml::Value::Integer(i64::from(SETTINGS_VERSION)));
    credentials_changed
}

/// The id a migrated provider would like: the preset it matches, else the old
/// profile's id unless that was a placeholder, else its name made into an id.
fn preferred_provider_id(old_id: &str, provider: &ProviderConfig) -> String {
    if let Some(preset) = crate::providers::PRESETS
        .iter()
        .find(|preset| preset.base_url == provider.base_url && preset.protocol == provider.protocol)
    {
        return preset.id.to_owned();
    }
    if !matches!(old_id, "default" | "cli" | "custom") && crate::providers::validate_provider_id(old_id).is_ok() {
        return old_id.to_owned();
    }
    Some(crate::providers::slug(&provider.name))
        .filter(|slug| !slug.is_empty())
        .unwrap_or_else(|| "provider".to_owned())
}

/// How hard the model should think before answering. Left unset, nothing is
/// sent and the provider's own default applies — the levels are only useful on
/// models that expose reasoning, and sending one to a model that does not is
/// how you collect 400s.
///
/// The ladder spans both worlds: the Anthropic Messages API knows
/// `low|medium|high|xhigh|max`, and OpenAI-shaped endpoints accept whatever
/// subset their models do — OpenAI's own stops at `high` on most models,
/// while others take `xhigh` or `max` and refuse `high`. So the chosen level
/// is sent as is, and a rejection settles what the endpoint accepts (see
/// `Provider::learn`). Ordered from least to most effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
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
    pub const ALL: [Self; 6] = [Self::Minimal, Self::Low, Self::Medium, Self::High, Self::XHigh, Self::Max];

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
    pub vim_mode: bool,
    pub animations: bool,
    pub show_tooltips: bool,
    pub theme: crate::theme::ThemeChoice,
    /// Which glyph table the interface draws with. `nerd` needs a patched
    /// font and is never selected automatically.
    #[serde(default)]
    pub glyphs: crate::ui::GlyphChoice,
    /// Which language the console commands speak: `auto` follows the operating
    /// system, `en` and `zh-CN` name one outright. `ABACUS_LANG` overrides it
    /// for a single run. The TUI is not translated yet, so this governs
    /// `abacus setup` and `abacus doctor`.
    #[serde(default)]
    pub language: Option<String>,
    /// Show the model's reasoning in the transcript, where the provider streams
    /// it apart from the answer.
    #[serde(default = "default_true")]
    pub show_thinking: bool,
    /// Check GitHub for a newer version tag on startup, at most once a day,
    /// and say so in the transcript. Never downloads or installs anything.
    #[serde(default = "default_true")]
    pub check_updates: bool,
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
    /// The widest the transcript and composer are drawn, in columns; a wider
    /// terminal centres them. 0 uses the whole terminal.
    pub content_width: u16,
    /// How many lines the composer grows to before it scrolls.
    pub composer_rows: u16,
}

impl UiSettings {
    /// The column cap to centre content at, with 0 meaning no cap.
    pub fn content_columns(&self) -> u16 {
        if self.content_width == 0 { u16::MAX } else { self.content_width }
    }
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
            vim_mode: true,
            animations: true,
            show_tooltips: true,
            theme: crate::theme::ThemeChoice::Auto,
            glyphs: crate::ui::GlyphChoice::Auto,
            language: None,
            show_thinking: true,
            check_updates: true,
            show_token_rate: false,
            draft_replies: true,
            content_width: 112,
            composer_rows: 10,
        }
    }
}

/// What `max_steps` may be set to. The default used to read 512 while every
/// load clamped it to 128; the default is now what runs, and the ceiling is
/// high enough for long unattended turns.
pub const MAX_STEPS: std::ops::RangeInclusive<usize> = 1..=1024;
/// What `tool_output_limit` may be set to, in characters.
pub const TOOL_OUTPUT_LIMIT: std::ops::RangeInclusive<usize> = 2_000..=200_000;

/// `max_steps` as the terminal's settings and the app server accept it.
pub fn check_max_steps(steps: usize) -> Result<()> {
    if !MAX_STEPS.contains(&steps) {
        bail!("max steps must be between {} and {}", MAX_STEPS.start(), MAX_STEPS.end());
    }
    Ok(())
}

/// `tool_output_limit` as the terminal's settings and the app server accept it.
pub fn check_tool_output_limit(characters: usize) -> Result<()> {
    if !TOOL_OUTPUT_LIMIT.contains(&characters) {
        bail!("tool output limit must be between {} and {}", TOOL_OUTPUT_LIMIT.start(), TOOL_OUTPUT_LIMIT.end());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// AUTO reviews what could do harm before it runs; YOLO runs everything.
    pub mode: Permission,
    /// Model round trips one turn may take before it stops, within
    /// [`MAX_STEPS`].
    pub max_steps: usize,
    /// Characters of tool output kept before truncation, within
    /// [`TOOL_OUTPUT_LIMIT`].
    pub tool_output_limit: usize,
    /// Timers the model may have waiting at once, within
    /// [`crate::timers::MAX_TIMERS`]. Keeps it from setting them by the
    /// hundred; 0 takes the timer tool away.
    pub max_timers: usize,
    /// Balanced high-savings mode for upstream tokens.
    #[serde(default)]
    pub token_compression: bool,
    /// Allow only one non-subagent upstream request at a time.
    #[serde(default)]
    pub one_stream: bool,
    /// Ask the provider to cache the request prefix (Anthropic
    /// `cache_control`). On by default; a repeated prefix is then billed at a
    /// fraction of its uncached rate. Turn it off only for an endpoint that
    /// claims the Anthropic protocol and chokes on the field.
    #[serde(default = "yes")]
    pub prompt_cache: bool,
    /// When a long session sits idle, summarise it while the provider's
    /// prompt cache is still warm, so the next message does not send the whole
    /// history again at full price. Claude Code's `idleCompaction`.
    #[serde(default = "yes")]
    pub idle_compaction: bool,
}

/// `serde(default)` for a bool that defaults to true.
fn yes() -> bool {
    true
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

/// Live sharing of interactive sessions through Abacus Sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteSettings {
    /// Share every interactive session automatically once it has received its
    /// first prompt, so it can be followed and steered from a phone. `/remote`
    /// still turns it off for one session.
    pub auto_share: bool,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self { auto_share: true }
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
        Self { enabled: true, endpoint: crate::activity::DEFAULT_ACTIVITY_ENDPOINT.to_owned() }
    }
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            mode: Permission::Auto,
            max_steps: 128,
            tool_output_limit: 30_000,
            max_timers: crate::timers::DEFAULT_MAX_TIMERS,
            token_compression: false,
            one_stream: false,
            prompt_cache: true,
            idle_compaction: true,
        }
    }
}

/// Secrets, kept apart from the settings with owner-only permissions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    /// API keys pasted into Abacus, by provider id.
    pub keys: BTreeMap<String, String>,
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
    pub memories_file: PathBuf,
    /// Where memories were kept, versioned, before they had a file of their
    /// own again; read once, when `memories_file` is imported.
    pub harness_dir: PathBuf,
    /// Where scripted endpoint files used to live; read once, when settings
    /// from before providers are migrated.
    pub endpoints_dir: PathBuf,
    /// Each provider's discovered model list, one JSON file per provider.
    pub models_dir: PathBuf,
    /// User-authored theme files, one JSON palette per name.
    pub themes_dir: PathBuf,
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
            themes_dir: root.join("themes"),
            traces_dir: root.join("traces"),
            attachments_dir: root.join("attachments"),
            memories_file: root.join("memories.json"),
            harness_dir: root.join("harness"),
            endpoints_dir: root.join("endpoints"),
            models_dir: root.join("models"),
            update_file: root.join("update.json"),
            recovery_file: root.join("recovered.md"),
            root,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.sessions_dir).with_context(|| format!("could not create {}", self.root.display()))?;
        Ok(())
    }

    /// A stable, anonymous per-install identifier, created on first use. It
    /// names this machine to activity counting, the sync device registry and
    /// remote control alike, so one install is one device everywhere.
    pub fn install_id(&self) -> String {
        let path = self.root.join("install_id");
        let read = || {
            fs::read_to_string(&path)
                .ok()
                .map(|existing| existing.trim().chars().take(100).collect::<String>())
                .filter(|existing| !existing.is_empty())
        };
        if let Some(existing) = read() {
            return existing;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let _ = atomic_write(&path, id.as_bytes(), false);
        // Another process may have created it first; whatever is on disk wins.
        read().unwrap_or(id)
    }

    /// What this device last exchanged with the sync server (see
    /// [`crate::sync_state`]).
    pub fn sync_state_file(&self) -> PathBuf {
        self.root.join("sync-state.json")
    }
}

/// A name a person recognises in a device list: the host name, or "Abacus CLI"
/// when the system will not say.
pub fn device_name() -> String {
    let host = ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .or_else(|| fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| fs::read_to_string("/etc/hostname").ok())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
        })
        .map(|host| host.trim().to_owned())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "Abacus CLI".to_owned());
    host.chars().take(100).collect()
}

fn resolve_workspace(path: Option<&Path>) -> Result<PathBuf> {
    let path = match path {
        Some(path) => path.to_owned(),
        None => std::env::current_dir().context("could not determine current directory")?,
    };
    if !path.is_dir() {
        bail!("workspace does not exist or is not a directory: {}", path.display());
    }
    path.canonicalize().with_context(|| format!("could not resolve workspace: {}", path.display()))
}

pub fn workspace_from_cli(cli: &Cli) -> Result<PathBuf> {
    resolve_workspace(cli.path.as_deref())
}

/// Reads a JSON state file, starting from the default when it is missing or
/// unreadable: these files hold learned history, never anything a run depends on.
pub fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    fs::read_to_string(path).ok().and_then(|content| serde_json::from_str(&content).ok()).unwrap_or_default()
}

/// Writes a JSON state file, best effort, for the same reason.
pub fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) {
    if let Ok(content) = serde_json::to_vec_pretty(value) {
        let _ = atomic_write(path, &content, false);
    }
}

pub fn atomic_write(path: &Path, content: &[u8], private: bool) -> Result<()> {
    // Unique per write, not just per process: sync writes from background
    // threads, and two writes sharing a temporary file could tear each other.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|name| name.to_str()).unwrap_or("abacus"),
        std::process::id(),
        WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = (|| -> Result<()> {
        let mut file =
            File::create(&temp).with_context(|| format!("could not create temporary file in {}", parent.display()))?;
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let _ = private;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        // `rename` replaces an existing file in one step on Windows too.
        // Removing it first would leave a moment with no file at all: a crash
        // then loses it, and another process reading meanwhile finds nothing.
        replace(&temp, path).with_context(|| format!("could not replace {}", path.display()))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written?;

    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Move `from` over `to`. On Windows the replace is refused while anything
/// holds the target open without delete sharing (another writer finishing its
/// own replace, a reader, an antivirus scan of the file just written), with
/// "access denied" or a sharing violation. Those holds last milliseconds, so a
/// few short retries turn a spurious failure into a slightly later success.
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        const ACCESS_DENIED: i32 = 5;
        const SHARING_VIOLATION: i32 = 32;
        let mut delay = std::time::Duration::from_millis(2);
        for _ in 0..10 {
            match fs::rename(from, to) {
                Err(error) if matches!(error.raw_os_error(), Some(ACCESS_DENIED | SHARING_VIOLATION)) => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(std::time::Duration::from_millis(100));
                }
                result => return result,
            }
        }
    }
    fs::rename(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Sync writes session files from background threads while the terminal
    /// saves its own: writes to one file from several threads each land
    /// whole, one after another, and leave no temporary file behind.
    #[test]
    fn concurrent_atomic_writes_land_whole() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let writers: Vec<_> = (0..8_u8)
            .map(|writer| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let content = vec![b'a' + writer; 256 * 1024];
                    for _ in 0..10 {
                        super::atomic_write(&path, &content, true).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let content = std::fs::read(&path).unwrap();
        assert_eq!(content.len(), 256 * 1024);
        assert!(content.iter().all(|byte| *byte == content[0]), "one write, whole");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1, "no temporary files left");
    }

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
        assert_eq!(ReasoningEffort::parse("xhigh"), Some(ReasoningEffort::XHigh));
        assert_eq!(ReasoningEffort::parse("x-high"), Some(ReasoningEffort::XHigh));

        // Anthropic takes a budget, and minimal means "do not enable thinking".
        assert_eq!(ReasoningEffort::Minimal.thinking_budget(), None);
        assert!(
            ReasoningEffort::Low.thinking_budget() < ReasoningEffort::High.thinking_budget(),
            "budgets rise with effort"
        );
    }

    /// Anthropic has no `minimal`, so it is the one level that is mapped
    /// before sending; everything else goes out as chosen.
    #[test]
    fn effort_levels_map_to_what_anthropic_accepts() {
        assert!(ReasoningEffort::ALL.windows(2).all(|pair| pair[0] < pair[1]), "ALL runs least to most");
        assert_eq!(ReasoningEffort::Minimal.anthropic_effort(), "low");
        assert_eq!(ReasoningEffort::XHigh.anthropic_effort(), "xhigh");
        assert_eq!(ReasoningEffort::Max.anthropic_effort(), "max");
        assert_eq!(ReasoningEffort::High.anthropic_effort(), "high");
    }

    #[test]
    fn routing_becomes_a_provider_object_only_when_pinned() {
        let none = Routing::default();
        assert!(!none.is_pinned());
        assert!(none.body().is_none(), "an unpinned request carries no field");

        let pinned = Routing { order: vec!["Together".into(), "Anthropic".into()], allow_fallbacks: false };
        let body = pinned.body().expect("a provider object");
        assert_eq!(body["order"][0], "Together");
        assert_eq!(body["order"][1], "Anthropic");
        assert_eq!(body["allow_fallbacks"], false);
    }

    #[test]
    fn an_order_can_be_written_with_commas_or_spaces() {
        assert_eq!(Routing::parse_order("Together, Anthropic"), vec!["Together", "Anthropic"]);
        assert_eq!(Routing::parse_order("  DeepInfra   Novita  "), vec!["DeepInfra", "Novita"]);
        assert_eq!(Routing::parse_order("Z.AI"), vec!["Z.AI"]);
        assert!(Routing::parse_order("  ,, ").is_empty());
    }

    #[test]
    fn the_old_mode_names_map_onto_auto_and_yolo() {
        for name in ["auto", "PLAN", "build", "ask"] {
            assert_eq!(Permission::parse(name), Some(Permission::Auto), "{name}");
        }
        for name in ["yolo", "always-approve"] {
            assert_eq!(Permission::parse(name), Some(Permission::Yolo), "{name}");
        }
        assert_eq!(Permission::parse("sometimes"), None);
        assert_eq!(Permission::Auto.toggled(), Permission::Yolo);
        assert_eq!(Permission::default(), Permission::Auto, "AUTO is the default");
    }

    /// Two providers and three profiles, the way setup and the pickers leave
    /// them.
    fn settings() -> Settings {
        let mut settings = Settings { default_profile: "kojo".into(), ..Settings::default() };
        settings
            .providers
            .insert("kojo".into(), ProviderConfig::from_preset(crate::providers::preset("kojo").unwrap()));
        settings
            .providers
            .insert("llama".into(), ProviderConfig::from_preset(crate::providers::preset("llama").unwrap()));
        settings.profiles.insert(
            "kojo".into(),
            Profile {
                context_window: Some(1_000_000),
                max_output_tokens: Some(64_000),
                ..Profile::for_model("kojo/kojo")
            },
        );
        settings.profiles.insert("llama/qwen-3-4b".into(), Profile::for_model("llama/qwen-3-4b"));
        settings
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        let mut settings = settings();
        settings.agent.token_compression = true;
        settings.agent.mode = Permission::Yolo;
        settings.providers.get_mut("kojo").unwrap().headers.insert("X-Title".into(), "abacus".into());
        settings.providers.get_mut("kojo").unwrap().body =
            crate::providers::parse_body_line(r#"{"store": false}"#).unwrap();
        settings.providers.get_mut("kojo").unwrap().remove = vec!["parallel_tool_calls".into()];
        settings.save(&paths).unwrap();
        let loaded = Settings::load(&paths).unwrap();
        assert_eq!(loaded.profiles, settings.profiles);
        assert_eq!(loaded.providers, settings.providers);
        assert_eq!(loaded.version, SETTINGS_VERSION);
        assert_eq!(loaded.agent.mode, Permission::Yolo);
        assert!(loaded.agent.token_compression);
        assert!(loaded.feedback.enabled);
    }

    #[test]
    fn resolve_runs_the_profile_with_cli_overrides_on_top() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        let settings = settings();
        let mut credentials = Credentials::default();
        credentials.keys.insert("kojo".into(), "sk-kojo".into());
        let resolve = |arguments: &[&str]| {
            let mut all = vec!["abacus"];
            all.extend_from_slice(arguments);
            all.push(workspace.to_str().unwrap());
            Config::resolve(&Cli::parse_from(all), &settings, &credentials, paths.clone()).unwrap()
        };

        let from_profile = resolve(&[]);
        assert_eq!(from_profile.profile, "kojo");
        assert_eq!(from_profile.model, "kojo");
        assert_eq!(from_profile.model_ref(), "kojo/kojo");
        assert_eq!(from_profile.connection.request_url(), "https://api.kojo.sh/v1/chat/completions");
        assert_eq!(from_profile.connection.api_key.as_deref(), Some("sk-kojo"));
        assert_eq!(from_profile.model_limits.context_window, 1_000_000);
        assert_eq!(from_profile.model_limits.configured_output_tokens, Some(64_000));
        assert_eq!(from_profile.permission, Permission::Auto);

        let other = resolve(&["--profile", "llama/qwen-3-4b"]);
        assert_eq!(other.model, "qwen-3-4b");
        assert_eq!(other.connection.base_url, "http://localhost:8080/v1");
        assert_eq!(other.connection.api_key, None, "a local server needs no key");

        // A bare model is a model on the profile's own provider; a qualified
        // one may be anywhere.
        assert_eq!(resolve(&["--model", "kojo-mini"]).model_ref(), "kojo/kojo-mini");
        assert_eq!(resolve(&["--model", "llama/other"]).model_ref(), "llama/other");

        let overridden = resolve(&["--context-window", "2m", "--max-output-tokens", "32k", "--yolo"]);
        assert_eq!(overridden.model_limits.context_window, 2_000_000);
        assert_eq!(overridden.model_limits.configured_output_tokens, Some(32_000));
        assert_eq!(overridden.permission, Permission::Yolo);
        assert_eq!(resolve(&["--always-approve"]).permission, Permission::Yolo, "the old flag still works");

        let inline = resolve(&["--base-url", "http://localhost:9000/v1", "--model", "tiny"]);
        assert_eq!(inline.profile, "cli");
        assert_eq!(inline.connection.request_url(), "http://localhost:9000/v1/chat/completions");
    }

    #[test]
    fn roles_resolve_across_providers_and_a_missing_one_inherits() {
        let mut settings = settings();
        let profile = settings.profiles.get_mut("kojo").unwrap();
        profile.roles.insert("aux".into(), "llama/qwen-3-4b".into());
        profile.roles.insert("subagent".into(), "gone/model".into());
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let config = Config::resolve(
            &Cli::parse_from(["abacus", workspace.to_str().unwrap()]),
            &settings,
            &Credentials::default(),
            AbacusPaths::under(dir.path().join("home")),
        )
        .unwrap();
        let aux = config.roles.aux.expect("aux is assigned");
        assert_eq!(aux.reference(), "llama/qwen-3-4b");
        assert_eq!(aux.connection.base_url, "http://localhost:8080/v1");
        assert!(config.roles.subagent.is_none(), "a role on a removed provider follows the main model");
        assert!(config.roles.compaction.is_none());
    }

    #[test]
    fn profile_names_are_free_text_on_one_line() {
        assert!(validate_profile_name("openrouter/openai/gpt-6-astra").is_ok());
        assert!(validate_profile_name("Fast local (4b)").is_ok());
        assert!(validate_profile_name("  ").is_err());
        assert!(validate_profile_name("two\nlines").is_err());
        let mut settings = settings();
        assert_eq!(settings.add_profile("kojo/kojo"), "kojo/kojo");
        assert_eq!(settings.add_profile("kojo/kojo"), "kojo/kojo (2)");
        assert_eq!(settings.profile_for("llama/qwen-3-4b"), Some("llama/qwen-3-4b"));
    }

    #[test]
    fn removing_a_provider_takes_its_profiles_and_roles_with_it() {
        let mut settings = settings();
        settings.profiles.get_mut("kojo").unwrap().roles.insert("aux".into(), "llama/qwen-3-4b".into());
        settings.default_profile = "llama/qwen-3-4b".into();
        assert_eq!(settings.remove_provider("llama"), ["llama/qwen-3-4b"]);
        assert_eq!(settings.default_profile, "kojo", "the default moves to what is left");
        assert!(settings.profiles["kojo"].roles.is_empty());
        assert!(settings.remove_profile("kojo").is_err(), "the last profile stays");
    }

    #[test]
    fn version_two_settings_become_providers_and_profiles() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        fs::create_dir_all(&paths.endpoints_dir).unwrap();
        fs::write(
            paths.endpoints_dir.join("codex.yaml"),
            "name: ChatGPT Codex\nurl: https://chatgpt.com/backend-api/codex/responses\nprotocol: responses\n\
             model: gpt-5.6-luna\nauth:\n  file: ~/.codex/auth.json\n  file_field: tokens.access_token\n",
        )
        .unwrap();
        let old = r#"
version = 2
default_profile = "default"

[profiles.default]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1/"
model = "anthropic/claude-sonnet-4.5"
api_key_env = "OPENROUTER_API_KEY"
aux_model = "openai/gpt-5-mini"
providers = ["Anthropic"]
allow_fallbacks = false

[profiles.cheap]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1"
model = "deepseek/deepseek-v4-flash"
api_key_env = "OPENROUTER_API_KEY"

[profiles.local]
name = "Ollama"
base_url = "http://localhost:11434/v1"
model = "qwen3-coder"

[profiles.codex]
name = "Codex"
endpoint = "codex"

[profiles.unfinished]
name = "Custom"
base_url = "http://localhost:8000/v1"

[ui]
permission_mode = "always-approve"
vim_mode = false

[agent]
max_steps = 64
context_window = 128000
tool_format = "hermes"
"#;
        fs::write(&paths.config_file, old).unwrap();
        let mut credentials = Credentials::default();
        credentials.keys.insert("default".into(), "sk-or-1".into());
        credentials.keys.insert("cheap".into(), "sk-or-1".into());
        credentials.save(&paths).unwrap();

        let settings = Settings::load(&paths).unwrap();
        assert_eq!(settings.version, SETTINGS_VERSION);
        // The two OpenRouter profiles shared an endpoint and a key, so they
        // share one provider, named after the preset it matches.
        assert_eq!(settings.providers.keys().collect::<Vec<_>>(), ["codex", "ollama", "openrouter", "unfinished"]);
        assert_eq!(settings.profiles["default"].model, "openrouter/anthropic/claude-sonnet-4.5");
        assert_eq!(settings.profiles["cheap"].model, "openrouter/deepseek/deepseek-v4-flash");
        assert_eq!(settings.profiles["default"].roles["aux"], "openrouter/openai/gpt-5-mini");
        assert_eq!(settings.profiles["default"].upstreams, ["Anthropic"]);
        assert!(!settings.profiles["default"].allow_fallbacks);
        assert_eq!(settings.profiles["local"].model, "ollama/qwen3-coder");
        // Leftover [agent] limits and the tool format move onto the profiles.
        assert_eq!(settings.profiles["local"].context_window, Some(128_000));
        assert_eq!(settings.profiles["local"].tool_format.as_deref(), Some("hermes"));
        // The endpoint file is folded into its provider.
        assert_eq!(settings.profiles["codex"].model, "codex/gpt-5.6-luna");
        assert_eq!(settings.providers["codex"].token_field.as_deref(), Some("tokens.access_token"));
        assert_eq!(settings.providers["codex"].base_url, "https://chatgpt.com/backend-api/codex");
        // A profile with no model was never runnable; its provider stays.
        assert!(!settings.profiles.contains_key("unfinished"));
        assert_eq!(settings.default_profile, "default");
        assert!(settings.is_configured());
        // Approvals were always on, so this becomes YOLO; AUTO otherwise.
        assert_eq!(settings.agent.mode, Permission::Yolo);
        assert_eq!(settings.agent.max_steps, 64);
        assert!(!settings.ui.vim_mode, "everything else is carried over");

        // The key is now under the provider's id too, and the old entries stay.
        let credentials = Credentials::load(&paths).unwrap();
        assert_eq!(credentials.keys.get("openrouter").map(String::as_str), Some("sk-or-1"));
        assert_eq!(credentials.keys.get("default").map(String::as_str), Some("sk-or-1"));
        // Saved in the new shape, with the original kept.
        assert_eq!(fs::read_to_string(paths.root.join("config.toml.v2.bak")).unwrap(), old);
        assert!(fs::read_to_string(&paths.config_file).unwrap().contains("[providers.openrouter]"));
        // And loading again changes nothing.
        let again = Settings::load(&paths).unwrap();
        assert_eq!(again.profiles, settings.profiles);
        assert_eq!(again.providers, settings.providers);
    }

    #[test]
    fn a_migrated_provider_never_takes_a_key_stored_for_another() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(
            &paths.config_file,
            "version = 2\ndefault_profile = \"default\"\n\
             [profiles.default]\nname = \"Work\"\nbase_url = \"https://openrouter.ai/api/v1\"\nmodel = \"a/b\"\n\
             [profiles.openrouter]\nname = \"Personal\"\nbase_url = \"https://openrouter.ai/api/v1\"\nmodel = \"c/d\"\n",
        )
        .unwrap();
        let mut credentials = Credentials::default();
        credentials.keys.insert("default".into(), "work-key".into());
        credentials.keys.insert("openrouter".into(), "personal-key".into());
        credentials.save(&paths).unwrap();

        let settings = Settings::load(&paths).unwrap();
        let credentials = Credentials::load(&paths).unwrap();
        let key_of = |profile: &str| credentials.keys[settings.profiles[profile].provider()].clone();
        assert_eq!(key_of("default"), "work-key");
        assert_eq!(key_of("openrouter"), "personal-key");
        assert_ne!(settings.profiles["default"].provider(), settings.profiles["openrouter"].provider());
    }

    #[test]
    fn credentials_are_separate_from_settings() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        let mut credentials = Credentials::default();
        credentials.keys.insert("default".into(), "secret".into());
        credentials.save(&paths).unwrap();
        assert!(!paths.config_file.exists());
        assert!(fs::read_to_string(&paths.credentials_file).unwrap().contains("secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&paths.credentials_file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
