//! Live configuration: the `/config` panel, its tabs, the provider editor, and
//! applying a change.

use super::*;

/// The config panel, tab by tab: every setting with its label, how it is
/// changed, and one line on what it actually does — shown for whichever row
/// the cursor is on, because a settings screen that only lists names makes the
/// reader guess. One table, so display order, selection order, and the text
/// beside each row cannot drift apart.
///
/// The Providers tab has no fixed rows: it lists the providers, and opening
/// one shows [`PROVIDER_FIELDS`].
pub(super) const SETTINGS: &[(&str, &[Setting])] = &[
    (
        "Profile",
        &[
            toggled(
                ConfigKey::Profile,
                "Active profile",
                "The model this session runs, and how. Enter opens /profile: switch, rename, delete, or pick any model.",
            ),
            typed(ConfigKey::ProfileName, "Name", "What the profile is called in pickers and with --profile."),
            toggled(
                ConfigKey::Model,
                "Model",
                "provider/model this profile runs. Enter picks one from the providers' model lists.",
            ),
            toggled(
                ConfigKey::Effort,
                "Reasoning effort",
                "Enter steps auto → minimal → low → medium → high → xhigh → max. Auto leaves it to the provider; Ctrl+T steps it anywhere.",
            ),
            typed(
                ConfigKey::ContextWindow,
                "Context window",
                "In tokens (accepts 128k / 1m). Blank detects it from the provider.",
            ),
            typed(
                ConfigKey::MaxOutput,
                "Max output tokens",
                "Sent as max_tokens (accepts 8k / 64k). Blank detects it, or leaves it to the server — set it when the provider rejects the detected value.",
            ),
            toggled(
                ConfigKey::ToolFormat,
                "Tool-call format",
                "How to read tool calls a model writes as text instead of native calls. Enter steps through them; auto detects.",
            ),
            toggled(
                ConfigKey::AuxModel,
                "Auxiliary model",
                "Background calls — reading fetched pages and large outputs, drafts. Any provider. Enter picks, Del inherits the profile's model.",
            ),
            toggled(
                ConfigKey::ReviewModel,
                "Safety reviewer",
                "Judges in AUTO mode whether an action is what you asked for. Follows the auxiliary model unless set. Del inherits.",
            ),
            toggled(
                ConfigKey::SubagentModel,
                "Subagent model",
                "Delegated agents that name no model themselves. Del inherits the profile's model.",
            ),
            toggled(
                ConfigKey::CompactionModel,
                "Compaction model",
                "Writes the rolling summary of a long session — worth a capable model. Del inherits.",
            ),
            typed(
                ConfigKey::Upstreams,
                "Upstream routing",
                "OpenRouter only: which upstreams may serve the model, best first. `abacus upstreams` lists them.",
            ),
            toggled(
                ConfigKey::Fallbacks,
                "Allow other upstreams",
                "Off pins strictly — a request fails rather than landing on an unpinned upstream.",
            ),
        ],
    ),
    ("Providers", &[]),
    (
        "Agent",
        &[
            toggled(
                ConfigKey::Mode,
                "Mode",
                "AUTO: edits and inspection run, anything riskier is judged by the safety reviewer first. YOLO: everything runs. Shift+Tab switches one session.",
            ),
            typed(ConfigKey::MaxSteps, "Maximum agent steps", "How many tool calls one turn may make before it stops."),
            typed(ConfigKey::ToolOutputLimit, "Tool output limit", "Characters of tool output kept before truncation."),
            toggled(
                ConfigKey::ProjectTrust,
                "Trust this project",
                "Allow this project's own plugins, hooks, and MCP servers to run.",
            ),
            toggled(
                ConfigKey::TokenCompression,
                "Token compression",
                "Saves tokens: compacts sooner, keeps less recent history verbatim, and makes no draft calls.",
            ),
            toggled(
                ConfigKey::IdleCompaction,
                "Idle compaction",
                "When a long session sits idle for about 4½ minutes, summarise it while the prompt cache is still warm, so the next message does not resend the whole history. Not for local servers.",
            ),
            toggled(
                ConfigKey::OneStream,
                "One stream",
                "Serialize main and auxiliary upstream requests. Explicit subagents keep their own streams.",
            ),
            toggled(
                ConfigKey::TraceLogging,
                "Training traces",
                "Append every model call to ~/.abacus/traces as JSONL for fine-tuning. Stays local.",
            ),
        ],
    ),
    (
        "Search",
        &[
            toggled(
                ConfigKey::SearchEnabled,
                "Web search",
                "Whether the web_search and read_page tools are offered to the model at all.",
            ),
            toggled(
                ConfigKey::SearchBackend,
                "Search backend",
                "Enter cycles auto → searxng → brave → bing. Auto picks whatever is configured: your SearXNG instance, then Brave if its key is set, else Bing.",
            ),
            typed(
                ConfigKey::SearchInstanceUrl,
                "SearXNG URL",
                "Base URL of your SearXNG instance, e.g. http://localhost:8888. It must have the JSON format enabled in settings.yml. Blank to unset.",
            ),
            typed(
                ConfigKey::SearchApiKeyEnv,
                "Search key variable",
                "Environment variable holding the search API key. Blank uses the backend default (BRAVE_API_KEY for Brave).",
            ),
            toggled(
                ConfigKey::SearchSharedInstance,
                "Shared SearXNG",
                "Allow falling back to a public SearXNG instance when nothing else is configured. Off by default: queries would leave to a host neither you nor Abacus runs.",
            ),
        ],
    ),
    (
        "Interface",
        &[
            toggled(
                ConfigKey::Theme,
                "Theme",
                "Dark, light, auto, or a theme file from ~/.abacus/themes. `/theme export <name>` writes one to edit.",
            ),
            toggled(
                ConfigKey::Glyphs,
                "Glyphs",
                "Which glyph set to draw with. `nerd` needs a patched font installed, so it is never chosen for you.",
            ),
            typed(
                ConfigKey::ContentWidth,
                "Content width",
                "The widest the transcript and composer are drawn, in columns; a wider terminal centres them. `full` uses the whole terminal.",
            ),
            typed(
                ConfigKey::ComposerRows,
                "Input box height",
                "How many lines the composer grows to before it scrolls.",
            ),
            toggled(
                ConfigKey::VimMode,
                "Vim keybindings",
                "Esc enters normal mode in the composer instead of clearing it.",
            ),
            toggled(
                ConfigKey::ShowThinking,
                "Show thinking",
                "Show the model's reasoning, where the provider streams it apart from the answer. F3 toggles it.",
            ),
            toggled(
                ConfigKey::TokenRate,
                "Show tokens/second",
                "Show a live generation rate while a turn runs. Estimated.",
            ),
            toggled(ConfigKey::Animations, "Animations", "Spinners and the wave on running tool calls."),
            toggled(ConfigKey::Tooltips, "Welcome tips", "The guidance block on the welcome screen."),
            toggled(
                ConfigKey::DraftReplies,
                "Draft next message",
                "Predict a likely follow-up in the empty composer. One short model call per turn.",
            ),
            toggled(
                ConfigKey::CheckUpdates,
                "Update reminder",
                "Check GitHub daily for a newer version tag and say so. Never downloads anything.",
            ),
        ],
    ),
    (
        "Privacy",
        &[
            toggled(ConfigKey::FeedbackEnabled, "Feedback", "Whether /feedback is available at all."),
            toggled(
                ConfigKey::FeedbackDiagnostics,
                "Feedback diagnostics",
                "Attach extension diagnostics to feedback. Never your transcript.",
            ),
            typed(ConfigKey::FeedbackEndpoint, "Feedback endpoint", "Where /feedback submissions are sent."),
            toggled(
                ConfigKey::RemoteAutoShare,
                "Auto-share sessions",
                "While signed in to Abacus Sync, share each session live with your phone or browser once it has a prompt. /remote turns it off for one session.",
            ),
            toggled(
                ConfigKey::AdvancedToml,
                "Advanced configuration",
                "Open the raw TOML for settings without a row here.",
            ),
        ],
    ),
];

/// Where the Providers tab sits in [`SETTINGS`].
pub(super) const PROVIDERS_TAB: usize = 1;

/// The provider editor's rows, for the provider the Providers tab opened.
/// Everything a request to it needs is here, so a provider never needs a
/// hand-edited file.
pub(super) const PROVIDER_FIELDS: &[Setting] = &[
    typed(ConfigKey::ProviderName, "Name", "What pickers call this provider. Its id, in model names, stays fixed."),
    typed(
        ConfigKey::ProviderUrl,
        "Base URL",
        "The API root, /v1 included where the API has one. Requests go to /chat/completions, /responses or /v1/messages under it.",
    ),
    toggled(
        ConfigKey::ProviderProtocol,
        "Protocol",
        "Chat Completions suits most providers; Responses is OpenAI's newer API; Anthropic is the Messages API.",
    ),
    typed(
        ConfigKey::ApiKey,
        "API key",
        "Stored in credentials.toml with owner-only permissions, never in the settings, and never shown back. Blank removes it.",
    ),
    typed(
        ConfigKey::ApiKeyEnv,
        "Key variable",
        "An environment variable to read the key from instead, such as KOJO_API_KEY. It wins over a stored key.",
    ),
    typed(
        ConfigKey::AuthHeader,
        "Auth header",
        "The header the key goes in. Blank: Authorization, or x-api-key for the Anthropic protocol.",
    ),
    typed(
        ConfigKey::AuthFormat,
        "Auth format",
        "How the key is written into it; {key} is replaced. Blank: Bearer {key}, or the bare key for x-api-key.",
    ),
    typed(
        ConfigKey::TokenCommand,
        "Token command",
        "A command whose output is the token, run before each request — for one that refreshes or lives in a password manager.",
    ),
    typed(
        ConfigKey::Headers,
        "Extra headers",
        "Sent on every request: `Name: value; Other: value`. {session} in a value becomes a per-session id.",
    ),
    typed(
        ConfigKey::Body,
        "Body overrides",
        "JSON merged over every request body, winning: {\"store\": false}. Nested objects merge.",
    ),
    typed(
        ConfigKey::RemoveFields,
        "Removed fields",
        "Body fields to drop from every request, comma-separated, for an endpoint that rejects one Abacus sends.",
    ),
    typed(
        ConfigKey::SystemPrefix,
        "System prefix",
        "Text sent as the first system block, ahead of Abacus's own prompt.",
    ),
    typed(
        ConfigKey::RequestUrl,
        "Request URL",
        "The full URL to post completions to, when it is not the usual path under the base URL. Blank derives it.",
    ),
    typed(
        ConfigKey::ModelsUrl,
        "Models URL",
        "Where to list models, when it is not {base URL}/models. Blank derives it.",
    ),
    typed(
        ConfigKey::ExtraModels,
        "Extra models",
        "Model ids to offer beside the listed ones, comma-separated — or instead of them, for an endpoint that lists none.",
    ),
    toggled(
        ConfigKey::Discover,
        "Models",
        "Enter asks the provider again and opens its list: pick a model to run it in a profile.",
    ),
    toggled(
        ConfigKey::RemoveProvider,
        "Remove provider",
        "Removes the provider, its stored key, and every profile on it. Enter twice.",
    ),
];

/// One row of the config panel.
pub(super) struct Setting {
    pub(super) key: ConfigKey,
    pub(super) label: &'static str,
    /// Whether Enter opens a text field, rather than cycling the value.
    pub(super) typed: bool,
    pub(super) help: &'static str,
}

/// One selectable row of the panel as it is showing now.
#[derive(Clone)]
pub(super) enum ConfigRow {
    Setting(&'static Setting),
    Provider(String),
    AddProvider,
}

/// The role a profile row assigns, for the rows that pick a model.
pub(super) fn row_role(key: ConfigKey) -> Option<&'static str> {
    match key {
        ConfigKey::Model => Some("default"),
        ConfigKey::AuxModel => Some("aux"),
        ConfigKey::ReviewModel => Some("review"),
        ConfigKey::SubagentModel => Some("subagent"),
        ConfigKey::CompactionModel => Some("compaction"),
        _ => None,
    }
}

const TOOL_FORMATS: &[&str] =
    &["auto", "none", "hermes", "qwen", "llama3_json", "mistral", "glm", "kimi", "deepseek", "json"];

pub(super) fn validate_settings(settings: &Settings) -> Result<()> {
    if let Some(profile) = settings.profiles.get(&settings.default_profile) {
        let (provider, _) = crate::providers::split_ref(&profile.model)
            .context("the active profile's model is not written provider/model")?;
        let provider = settings
            .providers
            .get(provider)
            .with_context(|| format!("the active profile's provider `{provider}` does not exist"))?;
        reqwest::Url::parse(&provider.base_url).context("the provider's base URL is invalid")?;
    }
    crate::config::check_max_steps(settings.agent.max_steps)?;
    crate::config::check_tool_output_limit(settings.agent.tool_output_limit)?;
    if settings.feedback.enabled {
        crate::feedback::FeedbackClient::new(&settings.feedback.endpoint)?;
    }
    Ok(())
}

/// Parse an optional token count for a limit override: blank clears it, a
/// number (with `k`/`m` suffixes) sets it.
pub(super) fn parse_optional_tokens(value: &str) -> Result<Option<usize>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    crate::model_info::parse_tokens(trimmed).map(Some)
}

pub(super) fn limit_source_label(source: crate::model_info::LimitSource) -> &'static str {
    match source {
        crate::model_info::LimitSource::Override => "override",
        crate::model_info::LimitSource::Detected => "detected",
        crate::model_info::LimitSource::Heuristic => "known model",
        crate::model_info::LimitSource::Default => "default",
    }
}

pub(super) fn on_off(value: bool) -> String {
    if value { "On" } else { "Off" }.to_owned()
}

/// A comma-separated list as typed into a field.
fn list(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_owned).collect()
}

fn optional(value: &str) -> Option<String> {
    Some(value.trim().to_owned()).filter(|value| !value.is_empty())
}

impl App {
    pub(super) fn open_config(&mut self, argument: &str) {
        if self.running.is_some() {
            self.status = "finish or interrupt the active turn before changing configuration".to_owned();
            return;
        }
        let argument = argument.trim().to_ascii_lowercase();
        if argument == "raw" {
            return self.open_raw_config();
        }
        let tab = SETTINGS.iter().position(|(name, _)| name.to_ascii_lowercase() == argument).unwrap_or(0);
        self.open_config_tab(tab);
    }

    pub(super) fn open_config_tab(&mut self, tab: usize) {
        self.config_panel =
            Some(ConfigPanel { tab, selected: 0, editing: None, provider: None, confirm_remove: false });
    }

    /// Open the provider editor for `id`, on `field` when one is given.
    pub(super) fn open_provider_editor(&mut self, id: &str, field: Option<ConfigKey>) {
        self.config_panel = Some(ConfigPanel {
            tab: PROVIDERS_TAB,
            selected: 0,
            editing: None,
            provider: Some(id.to_owned()),
            confirm_remove: false,
        });
        if let Some(field) = field {
            if let Some(panel) = &mut self.config_panel {
                panel.selected = PROVIDER_FIELDS.iter().position(|setting| setting.key == field).unwrap_or(0);
            }
            self.begin_config_edit(field);
        }
    }

    /// The rows of the panel as it is showing now.
    pub(super) fn config_rows(&self) -> Vec<ConfigRow> {
        let Some(panel) = &self.config_panel else {
            return Vec::new();
        };
        if panel.tab == PROVIDERS_TAB {
            if panel.provider.is_some() {
                return PROVIDER_FIELDS.iter().map(ConfigRow::Setting).collect();
            }
            let mut rows: Vec<ConfigRow> = self.settings.providers.keys().cloned().map(ConfigRow::Provider).collect();
            rows.push(ConfigRow::AddProvider);
            return rows;
        }
        SETTINGS.get(panel.tab).map(|(_, rows)| rows.iter().map(ConfigRow::Setting).collect()).unwrap_or_default()
    }

    /// Step the panel's tab, closing an open provider editor on the way.
    pub(super) fn switch_config_tab(&mut self, delta: isize) {
        if let Some(panel) = &mut self.config_panel {
            let count = SETTINGS.len() as isize;
            panel.tab = (panel.tab as isize + delta).rem_euclid(count) as usize;
            panel.selected = 0;
            panel.provider = None;
            panel.editing = None;
            panel.confirm_remove = false;
        }
    }

    pub(super) fn open_raw_config(&mut self) {
        match toml::to_string_pretty(&self.settings) {
            Ok(text) => {
                let mut input = InputBuffer::new();
                input.insert_str(&text);
                self.config_panel = None;
                self.raw_config = Some(RawConfigEditor { input, error: None });
            }
            Err(error) => self.status = format!("could not encode configuration: {error}"),
        }
    }

    pub(super) fn save_raw_config(&mut self) {
        let Some(editor) = &self.raw_config else {
            return;
        };
        let content = editor.input.text();
        let result = (|| {
            let mut settings: Settings = toml::from_str(&content).context("configuration is not valid TOML")?;
            if settings.version > SETTINGS_VERSION {
                bail!("configuration version {} is newer than supported version {SETTINGS_VERSION}", settings.version);
            }
            settings.version = SETTINGS_VERSION;
            validate_settings(&settings)?;
            settings.save(&self.config.paths)?;
            self.settings = settings;
            self.apply_settings()?;
            Ok::<_, anyhow::Error>(())
        })();
        match result {
            Ok(()) => {
                self.raw_config = None;
                self.status = "configuration saved and applied".to_owned();
            }
            Err(error) => {
                if let Some(editor) = &mut self.raw_config {
                    editor.error = Some(format!("{error:#}"));
                }
            }
        }
    }

    /// Bring the running session in line with the settings: the active
    /// profile — its provider, model, limits and roles — and the agent and
    /// interface settings, without restarting.
    pub(super) fn apply_settings(&mut self) -> Result<()> {
        validate_settings(&self.settings)?;
        let before = (self.config.profile.clone(), self.config.model_ref(), self.config.model_limits);
        let name = self.settings.default_profile.clone();
        // A session started on an endpoint given on the command line has no
        // stored profile to follow until one is chosen in /profile.
        if self.follows_profile && self.settings.profiles.contains_key(&name) {
            self.config.use_profile(&self.settings, &self.credentials, &name)?;
        }
        // Limits detected at startup stand while the profile and its model
        // do and no override came or went.
        let same = before.0 == self.config.profile && before.1 == self.config.model_ref();
        if same
            && before.2.source != crate::model_info::LimitSource::Override
            && self.config.model_limits.source != crate::model_info::LimitSource::Override
        {
            self.config.model_limits = before.2;
        }
        let (steps, output) = (crate::config::MAX_STEPS, crate::config::TOOL_OUTPUT_LIMIT);
        self.config.max_steps = self.settings.agent.max_steps.clamp(*steps.start(), *steps.end());
        self.config.tool_output_limit = self.settings.agent.tool_output_limit.clamp(*output.start(), *output.end());
        self.config.token_compression = self.settings.agent.token_compression;
        self.config.one_stream = self.settings.agent.one_stream;
        self.config.prompt_cache = self.settings.agent.prompt_cache;
        self.config.idle_compaction = self.settings.agent.idle_compaction;
        self.config.trace_enabled = self.settings.trace.enabled;
        // The resolved search backend is what turns use; without this a
        // change here would only take effect on the next launch.
        self.config.web_search = self.settings.search.resolve();
        if !self.settings.ui.vim_mode {
            self.mode = InputMode::Insert;
        }
        // What was spent so far counts for the model it was spent on.
        if let Some(usage) = &self.usage
            && before.1 != self.config.model_ref()
        {
            usage.note_model(&self.config.model);
        }
        // Another model has none of this session cached.
        if before.1 != self.config.model_ref() {
            self.cancel_idle_compaction();
        }
        let mut provider = Provider::with_tokens(&self.config, self.tokens.clone())?;
        // Same conversation, new configuration: keep the session id so a host
        // that routes by it does not see a switch as a second conversation.
        provider.adopt_session(&self.provider);
        self.provider = provider;
        self.aux_provider = self.provider.for_role(self.config.roles.aux.as_ref());
        if let Some(session) = &mut self.session {
            session.profile = self.config.profile.clone();
            session.model = self.config.model_ref();
        }
        // The interface's own appearance travels with the settings too, so a
        // theme edited through the raw config file takes effect on save
        // rather than on next launch.
        let (theme, theme_error) = crate::theme::resolve(&self.settings.ui.theme, &self.config.paths.themes_dir);
        crate::theme::set_active(theme);
        crate::ui::set_glyphs(self.settings.ui.glyphs);
        if let Some(error) = theme_error {
            self.status = error;
        }
        self.persist_session();
        // Extensions come from a few settings only. Reloading them for any
        // other change (an effort, a theme) restarted every MCP server, and
        // its "configuration active" covered what the change itself said.
        let inputs = services_inputs(&self.settings);
        if inputs != self.services_inputs {
            self.services_inputs = inputs;
            self.reload_services = true;
        }
        Ok(())
    }

    pub(super) fn save_and_apply_settings(&mut self) -> Result<()> {
        self.settings.version = SETTINGS_VERSION;
        validate_settings(&self.settings)?;
        self.settings.save(&self.config.paths)?;
        self.apply_settings()
    }

    /// The active profile, to change. A session on an endpoint given on the
    /// command line has none.
    pub(super) fn active_profile_mut(&mut self) -> Result<&mut crate::config::Profile> {
        let name = self.settings.default_profile.clone();
        let profile = self.settings.profiles.get_mut(&name).filter(|_| self.follows_profile);
        profile.context("this session runs an endpoint given on the command line; pick a profile with /profile first")
    }

    /// The provider the editor has open.
    fn edited_provider_id(&self) -> Option<String> {
        self.config_panel.as_ref().and_then(|panel| panel.provider.clone())
    }

    fn edited_provider_mut(&mut self) -> Result<(String, &mut crate::providers::ProviderConfig)> {
        let id = self.edited_provider_id().context("no provider is open")?;
        let provider = self.settings.providers.get_mut(&id).context("that provider no longer exists")?;
        Ok((id, provider))
    }

    /// Enter on a row: open its text field, cycle its value, or do what it
    /// stands for.
    pub(super) fn activate_setting(&mut self, index: usize) {
        let Some(row) = self.config_rows().get(index).cloned() else {
            return;
        };
        match row {
            ConfigRow::Provider(id) => self.open_provider_editor(&id, None),
            ConfigRow::AddProvider => self.open_provider_picker(),
            ConfigRow::Setting(setting) if setting.typed => self.begin_config_edit(setting.key),
            ConfigRow::Setting(setting) => {
                if let Err(error) = self.cycle_config_value(setting.key) {
                    self.status = format!("configuration error: {error:#}");
                }
            }
        }
    }

    /// Del on a row: clear a role back to inheriting.
    pub(super) fn clear_setting(&mut self, index: usize) {
        let Some(ConfigRow::Setting(setting)) = self.config_rows().get(index).cloned() else {
            return;
        };
        match row_role(setting.key) {
            Some("default") | None => {}
            Some(role) => {
                let result = self.active_profile_mut().map(|profile| profile.set_role_model(role, None));
                let result = result.and_then(|()| self.save_and_apply_settings());
                self.report(result, format!("{}: inherits again", setting.label), "configuration error");
            }
        }
    }

    pub(super) fn cycle_config_value(&mut self, key: ConfigKey) -> Result<()> {
        if let Some(role) = row_role(key) {
            self.active_profile_mut()?;
            self.open_role_picker(role);
            return Ok(());
        }
        match key {
            ConfigKey::Profile => {
                self.open_model_hub();
                return Ok(());
            }
            ConfigKey::Effort => {
                use crate::config::ReasoningEffort::{High, Low, Max, Medium, Minimal, XHigh};
                let profile = self.active_profile_mut()?;
                profile.reasoning_effort = match profile.reasoning_effort {
                    None => Some(Minimal),
                    Some(Minimal) => Some(Low),
                    Some(Low) => Some(Medium),
                    Some(Medium) => Some(High),
                    Some(High) => Some(XHigh),
                    Some(XHigh) => Some(Max),
                    Some(Max) => None,
                };
            }
            ConfigKey::ToolFormat => {
                let profile = self.active_profile_mut()?;
                let current = profile.tool_format.clone().unwrap_or_else(|| "auto".to_owned());
                let next = TOOL_FORMATS.iter().position(|format| *format == current).map_or(0, |index| index + 1);
                profile.tool_format = TOOL_FORMATS.get(next % TOOL_FORMATS.len()).map(|format| (*format).to_owned());
                if profile.tool_format.as_deref() == Some("auto") {
                    profile.tool_format = None;
                }
            }
            ConfigKey::Fallbacks => toggle(&mut self.active_profile_mut()?.allow_fallbacks),
            ConfigKey::ProviderProtocol => {
                let (_, provider) = self.edited_provider_mut()?;
                provider.protocol = match provider.protocol {
                    ProviderProtocol::ChatCompletions => ProviderProtocol::Responses,
                    ProviderProtocol::Responses => ProviderProtocol::Anthropic,
                    ProviderProtocol::Anthropic => ProviderProtocol::ChatCompletions,
                };
            }
            ConfigKey::Discover => {
                let id = self.edited_provider_id().context("no provider is open")?;
                self.refresh_catalog(&id, true);
                self.open_provider_models(&id);
                return Ok(());
            }
            ConfigKey::RemoveProvider => return self.remove_edited_provider(),
            ConfigKey::Mode => {
                self.settings.agent.mode = self.settings.agent.mode.toggled();
                self.set_mode(self.settings.agent.mode);
            }
            ConfigKey::SearchEnabled => toggle(&mut self.settings.search.enabled),
            ConfigKey::SearchBackend => {
                use crate::web::SearchBackend;
                self.settings.search.backend = match self.settings.search.backend {
                    SearchBackend::Auto => SearchBackend::Searxng,
                    SearchBackend::Searxng => SearchBackend::Brave,
                    SearchBackend::Brave => SearchBackend::Bing,
                    SearchBackend::Bing => SearchBackend::Auto,
                };
            }
            ConfigKey::SearchSharedInstance => toggle(&mut self.settings.search.use_shared_instance),
            // Both cycle through the built-ins and whatever the user has
            // written, so a theme file is reachable without knowing its name.
            ConfigKey::Theme => {
                let mut names = vec!["auto".to_owned(), "dark".to_owned(), "light".to_owned()];
                names.extend(crate::theme::available(&self.config.paths.themes_dir));
                let current = self.settings.ui.theme.label().to_owned();
                let next = names.iter().position(|name| *name == current).map_or(0, |index| (index + 1) % names.len());
                let choice = crate::theme::ThemeChoice::parse(&names[next]);
                let (theme, error) = crate::theme::resolve(&choice, &self.config.paths.themes_dir);
                crate::theme::set_active(theme);
                if let Some(error) = error {
                    self.status = error;
                }
                self.settings.ui.theme = choice;
            }
            ConfigKey::Glyphs => {
                use crate::ui::GlyphChoice;
                self.settings.ui.glyphs = match self.settings.ui.glyphs {
                    GlyphChoice::Auto => GlyphChoice::Unicode,
                    GlyphChoice::Unicode => GlyphChoice::Nerd,
                    GlyphChoice::Nerd => GlyphChoice::Ascii,
                    GlyphChoice::Ascii => GlyphChoice::Auto,
                };
                crate::ui::set_glyphs(self.settings.ui.glyphs);
            }
            ConfigKey::VimMode => toggle(&mut self.settings.ui.vim_mode),
            ConfigKey::ShowThinking => toggle(&mut self.settings.ui.show_thinking),
            ConfigKey::TokenRate => toggle(&mut self.settings.ui.show_token_rate),
            ConfigKey::Animations => toggle(&mut self.settings.ui.animations),
            ConfigKey::Tooltips => toggle(&mut self.settings.ui.show_tooltips),
            ConfigKey::CheckUpdates => toggle(&mut self.settings.ui.check_updates),
            ConfigKey::DraftReplies => {
                toggle(&mut self.settings.ui.draft_replies);
                if !self.settings.ui.draft_replies {
                    self.clear_draft();
                }
            }
            ConfigKey::TokenCompression => {
                toggle(&mut self.settings.agent.token_compression);
                if self.settings.agent.token_compression {
                    self.clear_draft();
                }
            }
            ConfigKey::IdleCompaction => {
                toggle(&mut self.settings.agent.idle_compaction);
                if !self.settings.agent.idle_compaction {
                    self.cancel_idle_compaction();
                }
            }
            ConfigKey::OneStream => toggle(&mut self.settings.agent.one_stream),
            ConfigKey::TraceLogging => {
                toggle(&mut self.settings.trace.enabled);
                self.config.trace_enabled = self.settings.trace.enabled;
                if self.settings.trace.enabled {
                    // Opened on the next persist, which keeps one code path
                    // responsible for creating it.
                    self.persist_session();
                } else {
                    self.trace = None;
                }
            }
            ConfigKey::ProjectTrust => {
                let trusted = self.settings.trust.contains(&self.config.workspace);
                self.settings.trust.set(&self.config.workspace, !trusted);
            }
            ConfigKey::FeedbackEnabled => toggle(&mut self.settings.feedback.enabled),
            ConfigKey::FeedbackDiagnostics => toggle(&mut self.settings.feedback.include_diagnostics),
            ConfigKey::RemoteAutoShare => {
                toggle(&mut self.settings.remote.auto_share);
                // Turning it on shares the current session now; turning it
                // off leaves a live share alone — `/remote off` ends that.
                if self.settings.remote.auto_share {
                    self.remote_muted = false;
                    self.maybe_auto_share();
                }
            }
            ConfigKey::AdvancedToml => {
                self.open_raw_config();
                return Ok(());
            }
            _ => return Ok(()),
        }
        self.save_and_apply_settings()?;
        self.status = format!("{} updated", setting(key).label);
        Ok(())
    }

    /// First Enter arms it, the second removes the provider, its key, and the
    /// profiles on it. The session's own provider goes only when another
    /// profile is left to switch to.
    fn remove_edited_provider(&mut self) -> Result<()> {
        let id = self.edited_provider_id().context("no provider is open")?;
        let armed = self.config_panel.as_ref().is_some_and(|panel| panel.confirm_remove);
        if !armed {
            if let Some(panel) = &mut self.config_panel {
                panel.confirm_remove = true;
            }
            let profiles = self.settings.profiles_on(&id).len();
            self.status = format!("enter again to remove {id} and its {}", console_count(profiles, "profile"));
            return Ok(());
        }
        let remaining = self.settings.profiles.len() - self.settings.profiles_on(&id).len();
        if self.settings.profiles.get(&self.settings.default_profile).is_some_and(|profile| profile.provider() == id)
            && remaining == 0
        {
            if let Some(panel) = &mut self.config_panel {
                panel.confirm_remove = false;
            }
            bail!("every profile runs on {id}; add another provider and pick a model on it first");
        }
        let removed = self.settings.remove_provider(&id);
        crate::providers::drop_catalog(&self.config.paths, &id);
        self.catalogs.remove(&id);
        if self.credentials.keys.remove(&id).is_some() {
            self.credentials.save(&self.config.paths)?;
        }
        self.save_and_apply_settings()?;
        if let Some(panel) = &mut self.config_panel {
            panel.provider = None;
            panel.selected = 0;
            panel.confirm_remove = false;
        }
        self.status = format!("removed {id} and {}", console_count(removed.len(), "profile"));
        Ok(())
    }

    pub(super) fn begin_config_edit(&mut self, key: ConfigKey) {
        // A secret is never seeded into the editor: `config_value` reports only
        // where the key came from, so pre-filling would put that description
        // into the field and, worse, invite showing the real value.
        let value = if key == ConfigKey::ApiKey { String::new() } else { self.config_edit_seed(key) };
        let mut input = InputBuffer::new();
        input.insert_str(&value);
        if let Some(panel) = &mut self.config_panel {
            panel.editing = Some((key, input));
        }
    }

    /// What a text field opens with: the value as it can be typed back, which
    /// is not always how the row shows it.
    fn config_edit_seed(&self, key: ConfigKey) -> String {
        let provider = self.edited_provider_id().and_then(|id| self.settings.providers.get(&id).cloned());
        let profile = self.settings.active_profile().filter(|_| self.follows_profile);
        match key {
            ConfigKey::ProfileName => self.settings.default_profile.clone(),
            ConfigKey::ContextWindow => {
                profile.and_then(|profile| profile.context_window).map(|tokens| tokens.to_string()).unwrap_or_default()
            }
            ConfigKey::MaxOutput => profile
                .and_then(|profile| profile.max_output_tokens)
                .map(|tokens| tokens.to_string())
                .unwrap_or_default(),
            ConfigKey::Upstreams => profile.map(|profile| profile.upstreams.join(", ")).unwrap_or_default(),
            ConfigKey::ProviderName => provider.map(|provider| provider.name).unwrap_or_default(),
            ConfigKey::ProviderUrl => provider.map(|provider| provider.base_url).unwrap_or_default(),
            ConfigKey::ApiKeyEnv => provider.and_then(|provider| provider.api_key_env).unwrap_or_default(),
            ConfigKey::AuthHeader => provider.and_then(|provider| provider.auth_header).unwrap_or_default(),
            ConfigKey::AuthFormat => provider.and_then(|provider| provider.auth_format).unwrap_or_default(),
            ConfigKey::TokenCommand => {
                provider.map(|provider| crate::providers::command_line(&provider.token_command)).unwrap_or_default()
            }
            ConfigKey::Headers => {
                provider.map(|provider| crate::providers::headers_line(&provider.headers)).unwrap_or_default()
            }
            ConfigKey::Body => provider.map(|provider| crate::providers::body_line(&provider.body)).unwrap_or_default(),
            ConfigKey::RemoveFields => provider.map(|provider| provider.remove.join(", ")).unwrap_or_default(),
            ConfigKey::SystemPrefix => provider.and_then(|provider| provider.system_prefix).unwrap_or_default(),
            ConfigKey::RequestUrl => provider.and_then(|provider| provider.request_url).unwrap_or_default(),
            ConfigKey::ModelsUrl => provider.and_then(|provider| provider.models_url).unwrap_or_default(),
            ConfigKey::ExtraModels => provider.map(|provider| provider.models.join(", ")).unwrap_or_default(),
            ConfigKey::SearchInstanceUrl => self.settings.search.instance_url.clone().unwrap_or_default(),
            ConfigKey::SearchApiKeyEnv => self.settings.search.api_key_env.clone().unwrap_or_default(),
            other => self.config_value(other),
        }
    }

    pub(super) fn commit_config_edit(&mut self) {
        let edit = self.config_panel.as_mut().and_then(|panel| panel.editing.take());
        let Some((key, input)) = edit else {
            return;
        };
        let value = input.text();
        let trimmed = value.trim();
        let result: Result<()> = (|| {
            match key {
                ConfigKey::ProfileName => {
                    let from = self.settings.default_profile.clone();
                    self.active_profile_mut()?;
                    self.settings.rename_profile(&from, trimmed)?;
                }
                ConfigKey::ContextWindow => {
                    let tokens = parse_optional_tokens(trimmed)?;
                    self.active_profile_mut()?.context_window = tokens;
                }
                ConfigKey::MaxOutput => {
                    let tokens = parse_optional_tokens(trimmed)?;
                    self.active_profile_mut()?.max_output_tokens = tokens;
                }
                ConfigKey::Upstreams => {
                    self.active_profile_mut()?.upstreams = crate::config::Routing::parse_order(trimmed)
                }
                ConfigKey::ProviderName => self.edited_provider_mut()?.1.name = trimmed.to_owned(),
                ConfigKey::ProviderUrl => {
                    let url = crate::providers::normalise_base_url(trimmed)?;
                    self.edited_provider_mut()?.1.base_url = url;
                }
                ConfigKey::ApiKey => {
                    let (id, _) = self.edited_provider_mut()?;
                    if trimmed.is_empty() {
                        self.credentials.keys.remove(&id);
                    } else {
                        self.credentials.keys.insert(id, trimmed.to_owned());
                    }
                    self.credentials.save(&self.config.paths)?;
                }
                ConfigKey::ApiKeyEnv => self.edited_provider_mut()?.1.api_key_env = optional(trimmed),
                ConfigKey::AuthHeader => self.edited_provider_mut()?.1.auth_header = optional(trimmed),
                ConfigKey::AuthFormat => self.edited_provider_mut()?.1.auth_format = optional(trimmed),
                ConfigKey::TokenCommand => {
                    self.edited_provider_mut()?.1.token_command = crate::providers::parse_command_line(trimmed)
                }
                ConfigKey::Headers => {
                    let headers = crate::providers::parse_headers_line(trimmed)?;
                    self.edited_provider_mut()?.1.headers = headers;
                }
                ConfigKey::Body => {
                    let body = crate::providers::parse_body_line(trimmed)?;
                    self.edited_provider_mut()?.1.body = body;
                }
                ConfigKey::RemoveFields => self.edited_provider_mut()?.1.remove = list(trimmed),
                ConfigKey::SystemPrefix => self.edited_provider_mut()?.1.system_prefix = optional(trimmed),
                ConfigKey::RequestUrl => self.edited_provider_mut()?.1.request_url = optional(trimmed),
                ConfigKey::ModelsUrl => self.edited_provider_mut()?.1.models_url = optional(trimmed),
                ConfigKey::ExtraModels => self.edited_provider_mut()?.1.models = list(trimmed),
                ConfigKey::SearchInstanceUrl => {
                    let url = trimmed.trim_end_matches('/');
                    if !url.is_empty() && !url.starts_with("http") {
                        bail!("a SearXNG URL needs its scheme, e.g. http://localhost:8888");
                    }
                    self.settings.search.instance_url = optional(url);
                }
                ConfigKey::SearchApiKeyEnv => self.settings.search.api_key_env = optional(trimmed),
                ConfigKey::MaxSteps => {
                    let number = trimmed.parse::<usize>().context("max steps must be a number")?;
                    crate::config::check_max_steps(number)?;
                    self.settings.agent.max_steps = number;
                }
                ConfigKey::ToolOutputLimit => {
                    let number = trimmed.parse::<usize>().context("tool output limit must be a number")?;
                    crate::config::check_tool_output_limit(number)?;
                    self.settings.agent.tool_output_limit = number;
                }
                ConfigKey::ContentWidth => {
                    if matches!(trimmed.to_ascii_lowercase().as_str(), "full" | "0") {
                        self.settings.ui.content_width = 0;
                    } else {
                        let columns =
                            trimmed.parse::<u16>().context("content width must be a number of columns, or full")?;
                        if !(40..=1000).contains(&columns) {
                            bail!("content width must be between 40 and 1000 columns, or full");
                        }
                        self.settings.ui.content_width = columns;
                    }
                }
                ConfigKey::ComposerRows => {
                    let rows = trimmed.parse::<u16>().context("input box height must be a number of lines")?;
                    if !(1..=100).contains(&rows) {
                        bail!("input box height must be between 1 and 100 lines");
                    }
                    self.settings.ui.composer_rows = rows;
                }
                ConfigKey::FeedbackEndpoint => {
                    crate::feedback::FeedbackClient::new(trimmed)?;
                    self.settings.feedback.endpoint = trimmed.to_owned();
                }
                _ => {}
            }
            self.save_and_apply_settings()
        })();
        match result {
            Ok(()) => {
                self.status = format!("{} saved", setting(key).label);
                self.after_provider_edit(key);
            }
            Err(error) => {
                self.status = format!("configuration error: {error:#}");
                self.begin_config_edit(key);
            }
        }
    }

    /// A provider's connection changed. Its model list may now be different,
    /// so it is asked again — and a provider no profile uses yet goes straight
    /// to its list, which is the step setting one up is for.
    fn after_provider_edit(&mut self, key: ConfigKey) {
        let connection_field = matches!(
            key,
            ConfigKey::ProviderUrl
                | ConfigKey::ApiKey
                | ConfigKey::ApiKeyEnv
                | ConfigKey::AuthHeader
                | ConfigKey::AuthFormat
                | ConfigKey::TokenCommand
                | ConfigKey::Headers
                | ConfigKey::ModelsUrl
                | ConfigKey::ExtraModels
        );
        let Some(id) = self.edited_provider_id().filter(|_| connection_field) else {
            return;
        };
        self.refresh_catalog(&id, true);
        if self.settings.profiles_on(&id).is_empty() {
            self.open_provider_models(&id);
        }
    }

    pub(super) fn config_value(&self, key: ConfigKey) -> String {
        let profile = self.settings.active_profile().filter(|_| self.follows_profile);
        let provider_id = self.edited_provider_id();
        let provider = provider_id.as_ref().and_then(|id| self.settings.providers.get(id));
        let unset = |value: Option<String>, fallback: &str| {
            value.filter(|value| !value.is_empty()).unwrap_or_else(|| fallback.to_owned())
        };
        let role = |id: &str| -> String {
            match profile.and_then(|profile| profile.resolve_role(id)) {
                Some(resolved) if resolved.inherited => format!("{} (inherited)", resolved.model),
                Some(resolved) => {
                    let known = crate::providers::split_ref(&resolved.model)
                        .is_some_and(|(provider, _)| self.settings.providers.contains_key(provider));
                    if known { resolved.model } else { format!("{} (provider missing)", resolved.model) }
                }
                None => "—".to_owned(),
            }
        };
        match key {
            ConfigKey::Profile => {
                if profile.is_some() {
                    self.settings.default_profile.clone()
                } else {
                    format!("command line · {}", self.config.model_ref())
                }
            }
            ConfigKey::ProfileName => self.settings.default_profile.clone(),
            ConfigKey::Model => profile.map(|profile| profile.model.clone()).unwrap_or_else(|| self.config.model_ref()),
            ConfigKey::Effort => profile
                .and_then(|profile| profile.reasoning_effort)
                .map_or("auto".to_owned(), |effort| effort.label().to_owned()),
            // The override when set; otherwise what auto-resolution landed on
            // and where it came from, so "auto" is never a mystery value.
            ConfigKey::ContextWindow => match profile.and_then(|profile| profile.context_window) {
                Some(tokens) => ui::format_count(tokens as u64),
                None => format!(
                    "auto — {} ({})",
                    ui::format_count(self.config.model_limits.context_window as u64),
                    limit_source_label(self.config.model_limits.source),
                ),
            },
            ConfigKey::MaxOutput => match profile.and_then(|profile| profile.max_output_tokens) {
                Some(tokens) => ui::format_count(tokens as u64),
                None => match self.config.model_limits.configured_output_tokens {
                    Some(tokens) => format!(
                        "auto — {} ({})",
                        ui::format_count(tokens as u64),
                        limit_source_label(self.config.model_limits.source),
                    ),
                    None => "auto — server default".to_owned(),
                },
            },
            ConfigKey::ToolFormat => unset(profile.and_then(|profile| profile.tool_format.clone()), "auto"),
            ConfigKey::AuxModel => role("aux"),
            ConfigKey::ReviewModel => role("review"),
            ConfigKey::SubagentModel => role("subagent"),
            ConfigKey::CompactionModel => role("compaction"),
            ConfigKey::Upstreams => {
                let pinned = profile.map(|profile| profile.upstreams.clone()).unwrap_or_default();
                if pinned.is_empty() { "any (OpenRouter chooses)".to_owned() } else { pinned.join(", ") }
            }
            ConfigKey::Fallbacks => on_off(profile.is_none_or(|profile| profile.allow_fallbacks)),
            ConfigKey::ProviderName => provider
                .map(|provider| provider.display_name(provider_id.as_deref().unwrap_or_default()).to_owned())
                .unwrap_or_default(),
            ConfigKey::ProviderUrl => provider.map(|provider| provider.base_url.clone()).unwrap_or_default(),
            // The words `abacus setup` offers them by.
            ConfigKey::ProviderProtocol => provider
                .map(|provider| match provider.protocol {
                    ProviderProtocol::ChatCompletions => "chat completions",
                    ProviderProtocol::Responses => "responses",
                    ProviderProtocol::Anthropic => "anthropic",
                })
                .unwrap_or_default()
                .to_owned(),
            // Where the credential comes from, never the credential.
            ConfigKey::ApiKey => match (provider, &provider_id) {
                (Some(provider), Some(id)) => provider.key_status(id, &self.credentials).label(),
                _ => String::new(),
            },
            ConfigKey::ApiKeyEnv => match provider.and_then(|provider| provider.api_key_env.clone()) {
                Some(name) => {
                    let present = std::env::var(&name).is_ok_and(|value| !value.trim().is_empty());
                    format!("{name} · {}", if present { "set" } else { "not set" })
                }
                None => "(none)".to_owned(),
            },
            ConfigKey::AuthHeader => {
                let anthropic = provider.is_some_and(|provider| provider.protocol == ProviderProtocol::Anthropic);
                unset(
                    provider.and_then(|provider| provider.auth_header.clone()),
                    if anthropic { "x-api-key (default)" } else { "Authorization (default)" },
                )
            }
            ConfigKey::AuthFormat => unset(provider.and_then(|provider| provider.auth_format.clone()), "(default)"),
            ConfigKey::TokenCommand => {
                unset(provider.map(|provider| crate::providers::command_line(&provider.token_command)), "(none)")
            }
            ConfigKey::Headers => {
                unset(provider.map(|provider| crate::providers::headers_line(&provider.headers)), "(none)")
            }
            ConfigKey::Body => unset(provider.map(|provider| crate::providers::body_line(&provider.body)), "(none)"),
            ConfigKey::RemoveFields => unset(provider.map(|provider| provider.remove.join(", ")), "(none)"),
            ConfigKey::SystemPrefix => unset(provider.and_then(|provider| provider.system_prefix.clone()), "(none)"),
            ConfigKey::RequestUrl => match (provider, &provider_id) {
                (Some(provider), Some(id)) if provider.request_url.is_none() => format!(
                    "{} (derived)",
                    crate::providers::Connection::resolve(id, provider, &Credentials::default()).request_url()
                ),
                _ => unset(provider.and_then(|provider| provider.request_url.clone()), ""),
            },
            ConfigKey::ModelsUrl => match (provider, &provider_id) {
                (Some(provider), Some(id)) if provider.models_url.is_none() => format!(
                    "{} (derived)",
                    crate::providers::Connection::resolve(id, provider, &Credentials::default()).models_url()
                ),
                _ => unset(provider.and_then(|provider| provider.models_url.clone()), ""),
            },
            ConfigKey::ExtraModels => unset(provider.map(|provider| provider.models.join(", ")), "(none)"),
            ConfigKey::Discover => match provider_id.as_ref().and_then(|id| self.catalogs.get(id)) {
                Some(crate::model_hub::Catalog::Ready(cards)) => format!("{} · enter to browse", cards.len()),
                Some(crate::model_hub::Catalog::Loading) => "asking…".to_owned(),
                Some(crate::model_hub::Catalog::Failed(error)) => format!("failed — {error}"),
                _ => "not listed yet · enter to ask".to_owned(),
            },
            ConfigKey::RemoveProvider => {
                if self.config_panel.as_ref().is_some_and(|panel| panel.confirm_remove) {
                    "enter again to remove".to_owned()
                } else {
                    let count = provider_id.as_ref().map_or(0, |id| self.settings.profiles_on(id).len());
                    format!("with {}", console_count(count, "profile"))
                }
            }
            ConfigKey::Mode => {
                let live = self.mode();
                let default = self.settings.agent.mode;
                if live == default {
                    default.label().to_owned()
                } else {
                    format!("{} (this session: {})", default.label(), live.label())
                }
            }
            ConfigKey::Theme => {
                let resolved = self.settings.ui.theme.resolve();
                let polarity = if resolved == ThemeMode::Dark { "dark" } else { "light" };
                match &self.settings.ui.theme {
                    crate::theme::ThemeChoice::Auto => format!("auto · {polarity}"),
                    other => other.label().to_owned(),
                }
            }
            ConfigKey::Glyphs => {
                let choice = self.settings.ui.glyphs;
                if choice == crate::ui::GlyphChoice::Auto {
                    // `auto` alone does not say what you are looking at, and
                    // the difference is the whole reason to open this row.
                    let resolved =
                        if std::ptr::eq(choice.resolve(), &crate::ui::Glyphs::ASCII) { "ascii" } else { "unicode" };
                    format!("auto · {resolved}")
                } else {
                    choice.label().to_owned()
                }
            }
            ConfigKey::ContentWidth => match self.settings.ui.content_width {
                0 => "full".to_owned(),
                columns => columns.to_string(),
            },
            ConfigKey::ComposerRows => self.settings.ui.composer_rows.to_string(),
            ConfigKey::VimMode => on_off(self.settings.ui.vim_mode),
            ConfigKey::ShowThinking => on_off(self.settings.ui.show_thinking),
            ConfigKey::TokenRate => on_off(self.settings.ui.show_token_rate),
            ConfigKey::Animations => on_off(self.settings.ui.animations),
            ConfigKey::Tooltips => on_off(self.settings.ui.show_tooltips),
            ConfigKey::DraftReplies => on_off(self.settings.ui.draft_replies),
            ConfigKey::CheckUpdates => on_off(self.settings.ui.check_updates),
            ConfigKey::SearchEnabled => on_off(self.settings.search.enabled),
            ConfigKey::SearchBackend => {
                // Auto is a decision deferred, so show what it currently
                // resolves to — otherwise the row says "auto" and the user has
                // no way to tell which engine is actually answering.
                if self.settings.search.backend == crate::web::SearchBackend::Auto {
                    format!("auto · {}", self.settings.search.resolve().backend.label())
                } else {
                    self.settings.search.backend.label().to_owned()
                }
            }
            ConfigKey::SearchInstanceUrl => unset(self.settings.search.instance_url.clone(), "(none)"),
            ConfigKey::SearchApiKeyEnv => {
                let name = unset(self.settings.search.api_key_env.clone(), "BRAVE_API_KEY");
                // Report whether the variable is populated, never its contents.
                let present = std::env::var(&name).is_ok_and(|value| !value.trim().is_empty());
                format!("{name} · {}", if present { "set" } else { "unset" })
            }
            ConfigKey::SearchSharedInstance => on_off(self.settings.search.use_shared_instance),
            ConfigKey::TokenCompression => on_off(self.settings.agent.token_compression),
            ConfigKey::IdleCompaction => on_off(self.settings.agent.idle_compaction),
            ConfigKey::OneStream => on_off(self.settings.agent.one_stream),
            ConfigKey::TraceLogging => match (&self.trace, self.settings.trace.enabled) {
                (Some(trace), true) => format!("On · {} records", trace.steps()),
                (None, true) => "On".to_owned(),
                (_, false) => "Off".to_owned(),
            },
            ConfigKey::MaxSteps => self.settings.agent.max_steps.to_string(),
            ConfigKey::ToolOutputLimit => self.settings.agent.tool_output_limit.to_string(),
            ConfigKey::ProjectTrust => on_off(self.settings.trust.contains(&self.config.workspace)),
            ConfigKey::FeedbackEnabled => on_off(self.settings.feedback.enabled),
            ConfigKey::FeedbackDiagnostics => on_off(self.settings.feedback.include_diagnostics),
            ConfigKey::FeedbackEndpoint => self.settings.feedback.endpoint.clone(),
            ConfigKey::RemoteAutoShare => on_off(self.settings.remote.auto_share),
            ConfigKey::AdvancedToml => format!(
                "{} skills · {} plugins · {} MCP servers",
                self.settings.skills.paths.len(),
                self.settings.plugins.paths.len(),
                self.settings.mcp.len()
            ),
        }
    }

    /// One row of the Providers tab's list.
    pub(super) fn provider_row_label(&self, id: &str) -> String {
        let Some(provider) = self.settings.providers.get(id) else {
            return id.to_owned();
        };
        let models = match self.catalogs.get(id) {
            Some(crate::model_hub::Catalog::Ready(cards)) => format!(" · {} models", cards.len()),
            _ => String::new(),
        };
        let profiles = self.settings.profiles_on(id).len();
        format!(
            "{}  {}  key: {}{models} · {}",
            provider.display_name(id),
            provider.base_url,
            provider.key_status(id, &self.credentials).label(),
            console_count(profiles, "profile"),
        )
    }

    /// The preset list, offered inside the TUI so a second provider does not
    /// require quitting and re-running the wizard.
    pub(super) fn open_provider_picker(&mut self) {
        let mut items = crate::providers::PRESETS
            .iter()
            .map(|preset| {
                let key = match preset.env_key {
                    Some(name) if crate::providers::key_in_env(preset) => format!("✓ {name}"),
                    Some(name) => format!("  {name}"),
                    None => "  no key needed".to_owned(),
                };
                (format!("  {:<14}{:<40}{key}", preset.name, preset.hint), preset.id.to_owned())
            })
            .collect::<Vec<_>>();
        items.push((
            "  Custom endpoint — OpenAI-compatible, Responses, or Anthropic".to_owned(),
            CUSTOM_PROVIDER.to_owned(),
        ));
        self.picker =
            Some(Picker { title: "add a provider".to_owned(), items, selected: 0, action: PickerAction::AddProvider });
    }

    /// Accept the highlighted picker row, or `index` when a click named one.
    pub(super) fn accept_picker(&mut self, index: Option<usize>) {
        let Some(picker) = &self.picker else {
            return;
        };
        let index = index.unwrap_or(picker.selected);
        let Some((_, value)) = picker.items.get(index).cloned() else {
            return;
        };
        let action = picker.action;
        self.picker = None;
        match action {
            PickerAction::ResumeSession => self.resume_session(&value),
            PickerAction::AddProvider => self.add_provider(&value),
        }
    }

    /// Add a provider from a preset or blank, and walk on to what it still
    /// needs: a URL for a custom one, a key for a hosted one without one,
    /// otherwise straight to its model list.
    pub(super) fn add_provider(&mut self, choice: &str) {
        use crate::providers::{ProviderConfig, unused_id};
        let preset = crate::providers::preset(choice);
        // The same endpoint twice is the same provider: open the one there is.
        if let Some(preset) = preset
            && let Some(existing) = self
                .settings
                .providers
                .iter()
                .find(|(_, provider)| provider.base_url == preset.base_url && provider.protocol == preset.protocol)
                .map(|(id, _)| id.clone())
        {
            self.open_provider_editor(&existing, None);
            self.status = format!("{} is set up already as {existing}", preset.name);
            return;
        }
        let (base, provider) = match preset {
            Some(preset) => (preset.id, ProviderConfig::from_preset(preset)),
            None => (
                "custom",
                ProviderConfig {
                    name: "Custom".to_owned(),
                    base_url: "http://localhost:8000/v1".to_owned(),
                    ..ProviderConfig::default()
                },
            ),
        };
        let id = unused_id(base, |candidate| self.settings.providers.contains_key(candidate));
        let missing_key = provider.key_status(&id, &self.credentials).is_missing();
        self.settings.providers.insert(id.clone(), provider);
        if let Err(error) = self.save_and_apply_settings() {
            self.settings.providers.remove(&id);
            self.status = format!("could not add the provider: {error:#}");
            return;
        }
        if preset.is_none() {
            self.open_provider_editor(&id, Some(ConfigKey::ProviderUrl));
            self.status = format!("{id} added — its base URL, then a key if it needs one");
        } else if missing_key {
            self.open_provider_editor(&id, Some(ConfigKey::ApiKey));
            self.status = format!("{id} added — paste its API key");
        } else {
            self.open_provider_editor(&id, None);
            self.refresh_catalog(&id, true);
            self.open_provider_models(&id);
            self.status = format!("{id} added — pick a model to run");
        }
    }
}

/// `n thing` / `n things`.
fn console_count(count: usize, singular: &str) -> String {
    crate::console::count(count, singular)
}

/// What extension discovery reads from the settings (see
/// `AgentServices::discover`): only a change to these needs the extensions
/// loaded again.
pub(super) fn services_inputs(settings: &Settings) -> String {
    serde_json::to_string(&(&settings.trust, &settings.plugins, &settings.skills, &settings.mcp)).unwrap_or_default()
}
