//! Live configuration: the `/config` panel, profiles, and applying a change.

use super::*;

/// The config panel, section by section: every setting with its label, how it
/// is changed, and one line on what it actually does — shown for whichever row
/// the cursor is on, because a settings screen that only lists names makes the
/// reader guess. One table, so display order, selection order, and the text
/// beside each row cannot drift apart.
pub(super) const SETTINGS: &[(&str, &[Setting])] = &[
    (
        "PROVIDER",
        &[
            toggled(
                ConfigKey::Profile,
                "Active profile",
                "Which stored provider profile this session talks to. Enter to switch, r to rename, d to delete, n to add.",
            ),
            typed(ConfigKey::Model, "Model", "Model ID sent to the provider. /model lists what the endpoint offers."),
            typed(
                ConfigKey::AuxModel,
                "Auxiliary model",
                "Cheaper model on this same endpoint for background calls (refine, drafts, tether, command checks). Blank = same as the main model.",
            ),
            typed(
                ConfigKey::Effort,
                "Reasoning effort",
                "How hard the model thinks: minimal, low, medium, high — or blank to leave it to the provider. Models without reasoning ignore it.",
            ),
            typed(ConfigKey::BaseUrl, "Provider URL", "OpenAI-compatible endpoint, including the /v1 suffix."),
            toggled(
                ConfigKey::Protocol,
                "Wire protocol",
                "Chat Completions suits most providers; Responses is OpenAI and xAI.",
            ),
            typed(
                ConfigKey::Providers,
                "Upstream providers",
                "OpenRouter only: which suppliers may serve this model, best first. `abacus providers` lists them.",
            ),
            toggled(
                ConfigKey::Fallbacks,
                "Allow other providers",
                "Off pins strictly — a request fails rather than landing on an unpinned provider.",
            ),
            typed(
                ConfigKey::ApiKey,
                "API key",
                "Stored in credentials.toml with owner-only permissions. Never shown back.",
            ),
            typed(
                ConfigKey::ContextWindow,
                "Context window",
                "This profile's context window in tokens (accepts 128k / 1m). Blank returns to auto-detection.",
            ),
            typed(
                ConfigKey::MaxOutput,
                "Max output tokens",
                "This profile's max output tokens sent as max_tokens (accepts 8k / 64k). Blank returns to auto — set this when the provider rejects the detected value.",
            ),
        ],
    ),
    (
        "AGENT",
        &[
            toggled(
                ConfigKey::Permission,
                "Permission mode",
                "Ask before every mutation, or allow them for the session.",
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
                "Token Compression",
                "Balanced high-savings mode: tighter context, fewer background checks, and no draft or routine refine calls.",
            ),
            toggled(
                ConfigKey::OneStream,
                "One Stream",
                "Serialize main and auxiliary upstream requests. Explicit subagents keep their own streams.",
            ),
        ],
    ),
    (
        "SEARCH",
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
        "INTERFACE",
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
            toggled(
                ConfigKey::VimMode,
                "Vim keybindings",
                "Esc enters normal mode in the composer instead of clearing it.",
            ),
            toggled(
                ConfigKey::ShowThinking,
                "Show thinking",
                "Show the model's reasoning, where the provider streams it apart from the answer.",
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
            toggled(
                ConfigKey::SafetyModel,
                "Safety classifier",
                "Which model judges whether a borderline command only inspects, in PLAN and AUTO.",
            ),
            toggled(
                ConfigKey::TraceLogging,
                "Training traces",
                "Append every model call to ~/.abacus/traces as JSONL for fine-tuning. Stays local.",
            ),
        ],
    ),
    (
        "PRIVACY",
        &[
            toggled(ConfigKey::FeedbackEnabled, "Feedback", "Whether /feedback is available at all."),
            toggled(
                ConfigKey::FeedbackDiagnostics,
                "Feedback diagnostics",
                "Attach extension diagnostics to feedback. Never your transcript.",
            ),
            typed(ConfigKey::FeedbackEndpoint, "Feedback endpoint", "Where /feedback submissions are sent."),
        ],
    ),
    (
        "ADVANCED",
        &[toggled(
            ConfigKey::AdvancedToml,
            "Advanced configuration",
            "Open the raw TOML for settings without a row here.",
        )],
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

pub(super) fn validate_settings(settings: &Settings) -> Result<()> {
    if !settings.profiles.contains_key(&settings.default_profile) {
        bail!("default profile `{}` does not exist", settings.default_profile);
    }
    let profile = &settings.profiles[&settings.default_profile];
    if profile.model.trim().is_empty() {
        bail!("the active profile needs a model");
    }
    reqwest::Url::parse(&profile.base_url).context("provider URL is invalid")?;
    if !(1..=128).contains(&settings.agent.max_steps) {
        bail!("max steps must be between 1 and 128");
    }
    if !(2_000..=200_000).contains(&settings.agent.tool_output_limit) {
        bail!("tool output limit must be between 2000 and 200000");
    }
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

impl App {
    pub(super) fn open_config(&mut self, argument: &str) {
        if self.running.is_some() {
            self.status = "finish or interrupt the active turn before changing configuration".to_owned();
            return;
        }
        if argument.trim() == "raw" {
            self.open_raw_config();
        } else {
            self.config_panel = Some(ConfigPanel { selected: 0, editing: None });
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

    pub(super) fn apply_settings(&mut self) -> Result<()> {
        validate_settings(&self.settings)?;
        let profile_name = self.settings.default_profile.clone();
        let profile = self.settings.profiles.get(&profile_name).context("default profile no longer exists")?.clone();
        let prior_profile = self.config.profile.clone();
        let prior_key = self.config.api_key.clone();
        self.config.profile = profile_name.clone();
        // Roles are read before `model` moves out of the clone below. Only
        // explicit assignments travel: an unassigned role stays `None` so the
        // turn falls back to whatever the main model is now, rather than
        // pinning the model this profile happened to have at load time.
        self.config.subagent_model = profile.role_model("subagent").map(str::to_owned);
        self.config.compaction_model = profile.role_model("compaction").map(str::to_owned);
        self.config.model = profile.model;
        self.config.aux_model = profile.aux_model.clone().filter(|model| !model.trim().is_empty());
        self.config.reasoning_effort = profile.reasoning_effort;
        // Pins take effect on the running session, not on the next launch.
        self.config.routing =
            crate::config::Routing { order: profile.providers.clone(), allow_fallbacks: profile.allow_fallbacks };
        self.config.base_url = profile.base_url.trim_end_matches('/').to_owned();
        self.config.protocol = profile.protocol;
        // Re-resolve the scripted endpoint for the new profile — without this a
        // switch away from a scripted profile (e.g. an Anthropic OAuth one)
        // left the old endpoint's URL, auth, and wire format attached, so the
        // "switched" profile kept talking to the previous endpoint.
        self.config.endpoint = match &profile.endpoint {
            Some(reference) => {
                match crate::endpoint::ScriptedEndpoint::resolve(reference, &self.config.paths.endpoints_dir) {
                    Ok(endpoint) => {
                        // A scripted endpoint is the authority on its own URL,
                        // protocol, and (when the profile omits it) model.
                        self.config.base_url = endpoint.url.trim_end_matches('/').to_owned();
                        self.config.protocol = endpoint.protocol;
                        if self.config.model.trim().is_empty()
                            && let Some(model) = &endpoint.model
                        {
                            self.config.model = model.clone();
                        }
                        Some(endpoint)
                    }
                    Err(error) => {
                        return Err(error).context("scripted endpoint for this profile");
                    }
                }
            }
            None => None,
        };
        self.config.api_key = profile
            .api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .or_else(|| self.credentials.keys.get(&profile_name).cloned())
            .or_else(|| (profile_name == prior_profile).then_some(prior_key).flatten());
        self.config.max_steps = self.settings.agent.max_steps.clamp(1, 128);
        self.config.tool_output_limit = self.settings.agent.tool_output_limit.clamp(2_000, 200_000);
        self.config.token_compression = self.settings.agent.token_compression;
        self.config.one_stream = self.settings.agent.one_stream;
        // Re-resolve the model limits when an override is in play — either
        // newly set (it must reach the provider and the compaction budget
        // immediately) or newly cleared (the Override source must not stick).
        // Profile switch is the other trigger: a 1M Claude profile must not
        // leak onto a 128k local one. With no override involved and the same
        // profile, startup detection is left alone.
        let (context_override, output_override) = self.settings.profile_limits(&profile_name);
        let profile_changed = prior_profile != profile_name;
        if context_override.is_some()
            || output_override.is_some()
            || profile_changed
            || self.config.model_limits.source == crate::model_info::LimitSource::Override
        {
            self.config.model_limits = crate::model_info::ModelLimits::resolve_from_name(
                &self.config.model, context_override, output_override,
            );
        }
        let always = self.settings.ui.permission_mode == PermissionMode::AlwaysApprove;
        self.config.yes = always;
        self.allow_mutations.store(always, std::sync::atomic::Ordering::Relaxed);
        if !self.settings.ui.vim_mode {
            self.mode = InputMode::Insert;
        }
        let mut provider = Provider::with_tokens(&self.config, self.tokens.clone())?;
        // Same conversation, new configuration: keep the session id so a host
        // that routes by it does not see a switch as a second conversation.
        provider.adopt_session(&self.provider);
        self.provider = provider;
        self.aux_provider = self.provider.for_role(self.config.aux_model.as_deref());
        if let Some(session) = &mut self.session {
            session.profile = self.config.profile.clone();
            session.model = self.config.model.clone();
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
        self.reload_services = true;
        Ok(())
    }

    pub(super) fn save_and_apply_settings(&mut self) -> Result<()> {
        self.settings.version = SETTINGS_VERSION;
        validate_settings(&self.settings)?;
        self.settings.save(&self.config.paths)?;
        self.apply_settings()
    }

    pub(super) fn cycle_config_value(&mut self, key: ConfigKey) -> Result<()> {
        match key {
            ConfigKey::Profile => {
                self.open_profile_picker();
                return Ok(());
            }
            ConfigKey::Fallbacks => {
                let profile = self.active_profile_mut()?;
                toggle(&mut profile.allow_fallbacks);
            }
            ConfigKey::Protocol => {
                let profile = self.active_profile_mut()?;
                // Rotate through all three, wrapping at the end:
                // chat-completions → responses → anthropic → chat-completions.
                profile.protocol = match profile.protocol {
                    ProviderProtocol::ChatCompletions => ProviderProtocol::Responses,
                    ProviderProtocol::Responses => ProviderProtocol::Anthropic,
                    ProviderProtocol::Anthropic => ProviderProtocol::ChatCompletions,
                };
            }
            ConfigKey::Permission => {
                self.settings.ui.permission_mode = if self.settings.ui.permission_mode == PermissionMode::Ask {
                    PermissionMode::AlwaysApprove
                } else {
                    PermissionMode::Ask
                };
            }
            ConfigKey::SearchEnabled => {
                toggle(&mut self.settings.search.enabled);
                self.apply_search_settings();
            }
            ConfigKey::SearchBackend => {
                use crate::web::SearchBackend;
                self.settings.search.backend = match self.settings.search.backend {
                    SearchBackend::Auto => SearchBackend::Searxng,
                    SearchBackend::Searxng => SearchBackend::Brave,
                    SearchBackend::Brave => SearchBackend::Bing,
                    SearchBackend::Bing => SearchBackend::Auto,
                };
                self.apply_search_settings();
            }
            ConfigKey::SearchSharedInstance => {
                toggle(&mut self.settings.search.use_shared_instance);
                self.apply_search_settings();
            }
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
            ConfigKey::SafetyModel => toggle(&mut self.settings.ui.safety_uses_main),
            ConfigKey::DraftReplies => {
                toggle(&mut self.settings.ui.draft_replies);
                if !self.settings.ui.draft_replies {
                    self.clear_draft();
                }
            }
            ConfigKey::TokenCompression => {
                toggle(&mut self.settings.agent.token_compression);
                self.config.token_compression = self.settings.agent.token_compression;
                if self.config.token_compression {
                    self.clear_draft();
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
            ConfigKey::AdvancedToml => {
                self.open_raw_config();
                return Ok(());
            }
            _ => return Ok(()),
        }
        self.save_and_apply_settings()?;
        self.status = format!("{} updated", setting(key).1.label);
        Ok(())
    }

    pub(super) fn active_profile_mut(&mut self) -> Result<&mut crate::config::ProviderProfile> {
        self.settings.profiles.get_mut(&self.settings.default_profile).context("default profile does not exist")
    }

    /// Re-resolve the search backend after a settings change.
    ///
    /// `config.web_search` is the resolved form the turn actually uses, built
    /// once at startup. Without this, a change here would be written to disk
    /// and take effect only on the next launch — the thing that made these
    /// settings setup-only in the first place.
    pub(super) fn apply_search_settings(&mut self) {
        self.config.web_search = self.settings.search.resolve();
    }

    pub(super) fn begin_config_edit(&mut self, key: ConfigKey) {
        // A secret is never seeded into the editor: `config_value` reports only
        // where the key came from, so pre-filling would put that description
        // into the field and, worse, invite showing the real value.
        if key == ConfigKey::ApiKey {
            if let Some(panel) = &mut self.config_panel {
                panel.editing = Some((key, InputBuffer::new()));
            }
            return;
        }
        let value = self.config_value(key);
        let mut input = InputBuffer::new();
        input.insert_str(&value);
        if let Some(panel) = &mut self.config_panel {
            panel.editing = Some((key, input));
        }
    }

    pub(super) fn commit_config_edit(&mut self) {
        let edit = self.config_panel.as_mut().and_then(|panel| panel.editing.take());
        let Some((key, input)) = edit else {
            return;
        };
        let value = input.text();
        let result = match key {
            ConfigKey::Model => self.active_profile_mut().map(|profile| {
                profile.model = value.trim().to_owned();
            }),
            ConfigKey::AuxModel => self.active_profile_mut().map(|profile| {
                let trimmed = value.trim();
                profile.aux_model = (!trimmed.is_empty()).then(|| trimmed.to_owned());
            }),
            ConfigKey::Effort => {
                let trimmed = value.trim();
                let parsed = crate::config::ReasoningEffort::parse(trimmed);
                if !trimmed.is_empty()
                    && !matches!(trimmed.to_ascii_lowercase().as_str(), "auto" | "default")
                    && parsed.is_none()
                {
                    Err(anyhow::anyhow!("effort must be minimal, low, medium, high, xhigh, max, or auto"))
                } else {
                    self.active_profile_mut().map(|profile| {
                        profile.reasoning_effort = parsed;
                    })
                }
            }
            ConfigKey::SearchInstanceUrl => {
                let trimmed = value.trim().trim_end_matches('/');
                if !trimmed.is_empty() && !trimmed.starts_with("http") {
                    Err(anyhow::anyhow!("a SearXNG URL needs its scheme, e.g. http://localhost:8888"))
                } else {
                    self.settings.search.instance_url = (!trimmed.is_empty()).then(|| trimmed.to_owned());
                    self.apply_search_settings();
                    Ok(())
                }
            }
            ConfigKey::SearchApiKeyEnv => {
                let trimmed = value.trim();
                self.settings.search.api_key_env = (!trimmed.is_empty()).then(|| trimmed.to_owned());
                self.apply_search_settings();
                Ok(())
            }
            ConfigKey::BaseUrl => self.active_profile_mut().map(|profile| {
                profile.base_url = value.trim().trim_end_matches('/').to_owned();
            }),
            ConfigKey::Providers => self.active_profile_mut().map(|profile| {
                profile.providers = crate::config::Routing::parse_order(&value);
            }),
            ConfigKey::MaxSteps => {
                value.trim().parse::<usize>().context("max steps must be a number").and_then(|number| {
                    if !(1..=128).contains(&number) {
                        bail!("max steps must be between 1 and 128");
                    }
                    self.settings.agent.max_steps = number;
                    Ok(())
                })
            }
            // Blank returns the limit to auto-resolution; a value (with `k`/`m`
            // suffixes accepted) becomes a hard override sent to the provider.
            ConfigKey::ContextWindow => parse_optional_tokens(&value).and_then(|tokens| {
                self.active_profile_mut()?.context_window = tokens;
                // A leftover [agent] override would otherwise leak back onto
                // every profile that has not set its own value — including
                // this one after a blank/auto clear.
                self.settings.agent.context_window = None;
                Ok(())
            }),
            ConfigKey::MaxOutput => parse_optional_tokens(&value).and_then(|tokens| {
                self.active_profile_mut()?.max_output_tokens = tokens;
                self.settings.agent.max_output_tokens = None;
                Ok(())
            }),
            ConfigKey::ToolOutputLimit => {
                value.trim().parse::<usize>().context("tool output limit must be a number").and_then(|number| {
                    if !(2_000..=200_000).contains(&number) {
                        bail!("tool output limit must be between 2000 and 200000");
                    }
                    self.settings.agent.tool_output_limit = number;
                    Ok(())
                })
            }
            ConfigKey::FeedbackEndpoint => crate::feedback::FeedbackClient::new(value.trim()).map(|_| {
                self.settings.feedback.endpoint = value.trim().to_owned();
            }),
            // The key goes to credentials.toml, which is written separately
            // from settings and kept owner-only.
            ConfigKey::ApiKey => {
                let profile = self.settings.default_profile.clone();
                let trimmed = value.trim().to_owned();
                if trimmed.is_empty() {
                    self.credentials.keys.remove(&profile);
                } else {
                    self.credentials.keys.insert(profile, trimmed);
                }
                self.credentials.save(&self.config.paths)
            }
            _ => Ok(()),
        }
        .and_then(|()| self.save_and_apply_settings());
        match result {
            Ok(()) => {
                if key == ConfigKey::Model {
                    self.pending_provider = None;
                }
                self.status = format!("{} saved", setting(key).1.label);
            }
            Err(error) => {
                self.status = format!("configuration error: {error:#}");
                self.begin_config_edit(key);
            }
        }
    }

    pub(super) fn config_value(&self, key: ConfigKey) -> String {
        let profile = self.settings.profiles.get(&self.settings.default_profile);
        match key {
            ConfigKey::Profile => self.settings.default_profile.clone(),
            ConfigKey::Model => profile.map(|value| value.model.clone()).unwrap_or_default(),
            ConfigKey::AuxModel => profile
                .and_then(|value| value.aux_model.clone())
                .filter(|model| !model.trim().is_empty())
                .unwrap_or_else(|| "(same as main)".to_owned()),
            ConfigKey::Effort => profile
                .and_then(|value| value.reasoning_effort)
                .map(|effort| effort.label().to_owned())
                .unwrap_or_else(|| "auto".to_owned()),
            ConfigKey::BaseUrl => profile.map(|value| value.base_url.clone()).unwrap_or_default(),
            ConfigKey::Protocol => profile.map(|value| format!("{:?}", value.protocol)).unwrap_or_default(),
            ConfigKey::Providers => {
                let pinned = profile.map(|profile| profile.providers.clone()).unwrap_or_default();
                if pinned.is_empty() { "any (endpoint chooses)".to_owned() } else { pinned.join(", ") }
            }
            ConfigKey::Fallbacks => on_off(profile.is_none_or(|profile| profile.allow_fallbacks)),
            // Report where the credential comes from, never the credential.
            ConfigKey::ApiKey => {
                let env = profile.and_then(|value| value.api_key_env.as_deref());
                let in_env = env.is_some_and(|name| std::env::var(name).is_ok_and(|v| !v.trim().is_empty()));
                if in_env {
                    format!("set · {} from environment", env.unwrap_or_default())
                } else if self
                    .credentials
                    .keys
                    .get(&self.settings.default_profile)
                    .is_some_and(|key| !key.trim().is_empty())
                {
                    "set · stored locally".to_owned()
                } else if env.is_some() {
                    format!("not set · export {} or press enter", env.unwrap_or_default())
                } else {
                    "not set".to_owned()
                }
            }
            ConfigKey::Permission => format!("{:?}", self.settings.ui.permission_mode),
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
            ConfigKey::VimMode => on_off(self.settings.ui.vim_mode),
            ConfigKey::ShowThinking => on_off(self.settings.ui.show_thinking),
            ConfigKey::TokenRate => on_off(self.settings.ui.show_token_rate),
            ConfigKey::Animations => on_off(self.settings.ui.animations),
            ConfigKey::Tooltips => on_off(self.settings.ui.show_tooltips),
            ConfigKey::DraftReplies => on_off(self.settings.ui.draft_replies),
            ConfigKey::SearchEnabled => on_off(self.settings.search.enabled),
            ConfigKey::SearchBackend => {
                let label = self.settings.search.backend.label().to_owned();
                // Auto is a decision deferred, so show what it currently
                // resolves to — otherwise the row says "auto" and the user has
                // no way to tell which engine is actually answering.
                if self.settings.search.backend == crate::web::SearchBackend::Auto {
                    format!("auto · {}", self.settings.search.resolve().backend.label())
                } else {
                    label
                }
            }
            ConfigKey::SearchInstanceUrl => self
                .settings
                .search
                .instance_url
                .clone()
                .filter(|url| !url.trim().is_empty())
                .unwrap_or_else(|| "(none)".to_owned()),
            ConfigKey::SearchApiKeyEnv => {
                let name = self
                    .settings
                    .search
                    .api_key_env
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "BRAVE_API_KEY".to_owned());
                // Report whether the variable is populated, never its contents.
                let present = std::env::var(&name).is_ok_and(|value| !value.trim().is_empty());
                format!("{name} · {}", if present { "set" } else { "unset" })
            }
            ConfigKey::SearchSharedInstance => on_off(self.settings.search.use_shared_instance),
            ConfigKey::TokenCompression => on_off(self.settings.agent.token_compression),
            ConfigKey::OneStream => on_off(self.settings.agent.one_stream),
            ConfigKey::CheckUpdates => on_off(self.settings.ui.check_updates),
            ConfigKey::SafetyModel => {
                if self.settings.ui.safety_uses_main { "Main model" } else { "Auxiliary model" }.to_owned()
            }
            ConfigKey::TraceLogging => match (&self.trace, self.settings.trace.enabled) {
                (Some(trace), true) => format!("On · {} records", trace.steps()),
                (None, true) => "On".to_owned(),
                (_, false) => "Off".to_owned(),
            },
            ConfigKey::MaxSteps => self.settings.agent.max_steps.to_string(),
            // The override when set; otherwise what auto-resolution landed on
            // and where it came from, so "auto" is never a mystery value.
            ConfigKey::ContextWindow => {
                let (context, _) = self.settings.profile_limits(&self.settings.default_profile);
                match context {
                    Some(tokens) => ui::format_count(tokens as u64),
                    None => format!(
                        "auto — {} ({})",
                        ui::format_count(self.config.model_limits.context_window as u64),
                        limit_source_label(self.config.model_limits.source),
                    ),
                }
            }
            ConfigKey::MaxOutput => {
                let (_, output) = self.settings.profile_limits(&self.settings.default_profile);
                match output {
                    Some(tokens) => ui::format_count(tokens as u64),
                    None => match self.config.model_limits.configured_output_tokens {
                        Some(tokens) => format!(
                            "auto — {} ({})",
                            ui::format_count(tokens as u64),
                            limit_source_label(self.config.model_limits.source),
                        ),
                        None => "auto — server default".to_owned(),
                    },
                }
            }
            ConfigKey::ToolOutputLimit => self.settings.agent.tool_output_limit.to_string(),
            ConfigKey::ProjectTrust => on_off(self.settings.trust.contains(&self.config.workspace)),
            ConfigKey::FeedbackEnabled => on_off(self.settings.feedback.enabled),
            ConfigKey::FeedbackDiagnostics => on_off(self.settings.feedback.include_diagnostics),
            ConfigKey::FeedbackEndpoint => self.settings.feedback.endpoint.clone(),
            ConfigKey::AdvancedToml => format!(
                "{} skills · {} plugins · {} MCP servers",
                self.settings.skills.paths.len(),
                self.settings.plugins.paths.len(),
                self.settings.mcp.len()
            ),
        }
    }

    /// Undo a provider that never got a model, restoring the previous profile.
    pub(super) fn cancel_pending_provider(&mut self) {
        let Some(pending) = self.pending_provider.take() else {
            return;
        };
        // Only roll back a profile that is still unusable; if the user gave it
        // a model by another route, leave it alone.
        let unfinished =
            self.settings.profiles.get(&pending.profile).is_some_and(|profile| profile.model.trim().is_empty());
        if !unfinished {
            return;
        }
        self.settings.profiles.remove(&pending.profile);
        self.settings.default_profile = pending.previous;
        let _ = self.settings.save(&self.config.paths);
        self.status = "provider discarded — no model was set".to_owned();
    }

    /// Offer the stored profiles, plus a way to add one. Cycling was the only
    /// way to switch before, which does nothing visible when there is a single
    /// profile and gives no way to create a second.
    pub(super) fn open_profile_picker(&mut self) {
        let mut items = self
            .settings
            .profiles
            .iter()
            .map(|(id, profile)| {
                let marker = if *id == self.settings.default_profile { "● " } else { "  " };
                (format!("{marker}{id}  —  {}  ·  {}", profile.name, profile.model), id.clone())
            })
            .collect::<Vec<_>>();
        items.push(("  + Add a provider…".to_owned(), NEW_PROVIDER_SENTINEL.to_owned()));
        self.picker = Some(Picker {
            title: "profile".to_owned(),
            items,
            selected: 0,
            action: PickerAction::SwitchProfile,
            prompt: None,
        });
    }

    /// The provider list from `abacus setup`, offered inside the TUI so a
    /// second provider does not require quitting and re-running the wizard.
    pub(super) fn open_provider_picker(&mut self) {
        let mut items = crate::setup::PRESETS
            .iter()
            .map(|preset| {
                let key = match preset.env_key {
                    Some(name) if crate::setup::key_in_env(preset) => format!("✓ {name}"),
                    Some(name) => format!("  {name}"),
                    None => "  no key needed".to_owned(),
                };
                (format!("  {:<18}{:<32}{key}", preset.name, preset.hint), preset.id.to_owned())
            })
            .collect::<Vec<_>>();
        // Scripted endpoints from ~/.abacus/endpoints, so a YAML-defined
        // provider (OAuth, custom headers, Anthropic protocol) is selectable
        // here rather than only by hand-editing config.toml.
        for name in self.scripted_endpoint_names() {
            items.push((
                format!("  {name:<18}scripted endpoint (~/.abacus/endpoints)"),
                format!("{ENDPOINT_SENTINEL_PREFIX}{name}"),
            ));
        }
        items.push(("  Custom OpenAI-compatible endpoint".to_owned(), CUSTOM_PROVIDER_SENTINEL.to_owned()));
        self.picker = Some(Picker {
            title: "provider".to_owned(),
            items,
            selected: 0,
            action: PickerAction::AddProvider,
            prompt: None,
        });
    }

    /// Names of the scripted endpoints defined under ~/.abacus/endpoints,
    /// sorted, for the provider picker.
    pub(super) fn scripted_endpoint_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.config.paths.endpoints_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let extension = path.extension().and_then(|value| value.to_str());
                if matches!(extension, Some("yaml") | Some("yml"))
                    && let Some(stem) = path.file_stem().and_then(|value| value.to_str())
                {
                    names.push(stem.to_owned());
                }
            }
        }
        names.sort();
        names
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
            PickerAction::SwitchProfile if value == NEW_PROVIDER_SENTINEL => self.open_provider_picker(),
            PickerAction::SwitchProfile => {
                self.settings.default_profile = value.clone();
                if let Err(error) = self.save_and_apply_settings() {
                    self.status = format!("could not switch profile: {error:#}");
                } else {
                    self.status = format!("profile {value}");
                }
            }
            PickerAction::AddProvider => self.add_provider(&value),
        }
    }

    pub(super) fn selected_profile_id(&self) -> Option<String> {
        let picker = self.picker.as_ref()?;
        if picker.action != PickerAction::SwitchProfile {
            return None;
        }
        let id = picker.items.get(picker.selected)?.1.clone();
        if id == NEW_PROVIDER_SENTINEL { None } else { Some(id) }
    }

    pub(super) fn begin_profile_rename(&mut self, id: &str) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        if picker.action != PickerAction::SwitchProfile {
            return;
        }
        let mut input = InputBuffer::new();
        input.insert_str(id);
        picker.prompt = Some(PickerPrompt::Rename { id: id.to_owned(), input });
    }

    pub(super) fn begin_profile_delete(&mut self, id: &str) {
        if self.settings.profiles.len() <= 1 {
            self.status = "cannot delete the last remaining profile".to_owned();
            return;
        }
        if let Some(picker) = self.picker.as_mut() {
            picker.prompt = Some(PickerPrompt::ConfirmDelete { id: id.to_owned() });
        }
    }

    pub(super) fn commit_profile_rename(&mut self, from: &str, to: &str) {
        match self.rename_profile(from, to) {
            Ok(id) => {
                self.status = format!("profile renamed to {id}");
                self.open_profile_picker();
                if let Some(picker) = self.picker.as_mut()
                    && let Some(index) = picker.items.iter().position(|(_, value)| value == &id)
                {
                    picker.selected = index;
                }
            }
            Err(error) => self.status = format!("could not rename profile: {error:#}"),
        }
    }

    pub(super) fn rename_profile(&mut self, from: &str, to: &str) -> Result<String> {
        let to = crate::config::validate_profile_id(to)?.to_owned();
        if to == from {
            return Ok(to);
        }
        if self.settings.profiles.contains_key(&to) {
            bail!("a profile named `{to}` already exists");
        }
        let mut profile = self.settings.profiles.remove(from).context("that profile no longer exists")?;
        if profile.name == from {
            profile.name = to.clone();
        }
        self.settings.profiles.insert(to.clone(), profile);
        if self.settings.default_profile == from {
            self.settings.default_profile = to.clone();
        }
        if let Some(key) = self.credentials.keys.remove(from) {
            self.credentials.keys.insert(to.clone(), key);
            self.credentials.save(&self.config.paths)?;
        }
        if let Some(pending) = &mut self.pending_provider
            && pending.profile == from
        {
            pending.profile = to.clone();
        }
        if let Some(pending) = &mut self.pending_provider
            && pending.previous == from
        {
            pending.previous = to.clone();
        }
        self.save_and_apply_settings()?;
        Ok(to)
    }

    pub(super) fn delete_profile(&mut self, id: &str) -> Result<()> {
        if !self.settings.profiles.contains_key(id) {
            bail!("no profile named `{id}`");
        }
        if self.settings.profiles.len() <= 1 {
            bail!("cannot delete the last remaining profile");
        }
        self.settings.profiles.remove(id);
        self.credentials.keys.remove(id);
        self.credentials.save(&self.config.paths)?;
        if self.settings.default_profile == id {
            let next = self.settings.profiles.keys().next().cloned().context("no remaining profile")?;
            self.settings.default_profile = next;
        }
        if let Some(pending) = &self.pending_provider
            && pending.profile == id
        {
            self.pending_provider = None;
        }
        self.save_and_apply_settings()?;
        Ok(())
    }

    pub(super) fn confirm_profile_delete(&mut self, id: &str) {
        match self.delete_profile(id) {
            Ok(()) => {
                self.status = format!("profile {id} deleted");
                self.open_profile_picker();
            }
            Err(error) => self.status = format!("could not delete profile: {error:#}"),
        }
    }

    /// Create a profile — from a preset, a scripted endpoint under
    /// `~/.abacus/endpoints`, or blank — and make it the active one.
    ///
    /// A scripted endpoint is copied onto the profile (URL, model, protocol)
    /// so it validates at once, while the `endpoint` reference keeps driving
    /// auth, headers, and body overrides.
    pub(super) fn add_provider(&mut self, id: &str) {
        use crate::config::ProviderProfile;
        let (base, profile) = if let Some(name) = id.strip_prefix(ENDPOINT_SENTINEL_PREFIX) {
            let endpoints = &self.config.paths.endpoints_dir;
            let endpoint = match crate::endpoint::ScriptedEndpoint::resolve(name, endpoints) {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    self.status = format!("could not load endpoint {name}: {error:#}");
                    return;
                }
            };
            let profile = ProviderProfile {
                name: endpoint.display_name().to_owned(),
                base_url: endpoint.url.clone(),
                model: endpoint.model.clone().unwrap_or_default(),
                protocol: endpoint.protocol,
                endpoint: Some(name.to_owned()),
                ..Default::default()
            };
            (name, profile)
        } else if let Some(preset) = crate::setup::PRESETS.iter().find(|preset| preset.id == id) {
            let profile = ProviderProfile {
                name: preset.name.to_owned(),
                base_url: preset.base_url.to_owned(),
                protocol: preset.protocol,
                api_key_env: preset.env_key.map(str::to_owned),
                ..Default::default()
            };
            (preset.id, profile)
        } else {
            let profile = ProviderProfile {
                name: "Custom".to_owned(),
                base_url: "http://localhost:8000/v1".to_owned(),
                ..Default::default()
            };
            ("custom", profile)
        };
        // Never silently replace an existing profile of the same name.
        let profile_id = std::iter::once(base.to_owned())
            .chain((2..).map(|suffix| format!("{base}-{suffix}")))
            .find(|candidate| !self.settings.profiles.contains_key(candidate))
            .expect("an unused suffix exists");
        let (name, ready) = (profile.name.clone(), !profile.model.trim().is_empty());
        let key =
            profile.api_key_env.clone().filter(|name| !std::env::var(name).is_ok_and(|value| !value.trim().is_empty()));
        self.settings.profiles.insert(profile_id.clone(), profile);
        let previous = std::mem::replace(&mut self.settings.default_profile, profile_id.clone());

        // With a model the profile is complete — apply it.
        if ready {
            let result = self.save_and_apply_settings();
            if !self.report(result, format!("{profile_id} active — {name}"), "could not apply") {
                self.settings.default_profile = previous;
            }
            return;
        }
        // Without one it is persisted but *not* applied: a profile with no
        // model fails validation, and applying a half-made provider would
        // break the running session. The live config keeps pointing at the
        // old profile until a model is committed.
        self.pending_provider = Some(PendingProvider { profile: profile_id.clone(), previous });
        if let Err(error) = self.settings.save(&self.config.paths) {
            self.status = format!("could not add provider: {error:#}");
            return;
        }
        self.status = match key {
            Some(key) => format!("{profile_id} added — set a model, then an API key ({key})"),
            None => format!("{profile_id} added — set a model"),
        };
        self.ask_for_model();
    }
    /// Put the cursor on the model field and open it. A new profile has no
    /// model, which is the one thing it cannot run without.
    pub(super) fn ask_for_model(&mut self) {
        if let Some(panel) = &mut self.config_panel {
            panel.selected = setting(ConfigKey::Model).0;
            self.begin_config_edit(ConfigKey::Model);
        }
    }

    /// Enter on a config row: open its text field, or cycle its value.
    pub(super) fn activate_setting(&mut self, index: usize) {
        let Some(setting) = settings().nth(index) else {
            return;
        };
        if setting.typed {
            self.begin_config_edit(setting.key);
        } else if let Err(error) = self.cycle_config_value(setting.key) {
            self.status = format!("configuration error: {error:#}");
        }
    }
}
