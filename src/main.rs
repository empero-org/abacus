use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use abacus_agent::{
    activity::ActivityReporter,
    agent::initial_messages,
    app_server,
    config::{
        AbacusPaths, Cli, Command, Config, Credentials, PluginsCommand, Settings, SkillsCommand, workspace_from_cli,
    },
    context::{self, expand_file_references},
    cron,
    extensions::PluginRegistry,
    headless, model_info,
    services::AgentServices,
    session::{Opened, SessionStore, WorkspaceSessions, opening_note},
    setup, tui,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Before anything is printed. A Windows console decodes the bytes it is
    // given with a codepage, not UTF-8, so on a console set to 936 every glyph
    // in this program would arrive as mojibake (issue #11). Switching it here
    // covers clap's help, the setup wizard, doctor and the TUI at once; if the
    // console refuses, the glyph tables fall back to ASCII.
    abacus_agent::terminal::ensure_utf8_console();
    // The language for anything printed before a settings file is read: the
    // environment's choice, then the operating system's. `setup` and `doctor`
    // refine it once they have the user's setting.
    abacus_agent::i18n::init(None);
    let cli = Cli::parse();
    if cli.goal && cli.prompt.is_none() {
        anyhow::bail!("--goal requires a prompt via -p/--prompt");
    }
    let paths = AbacusPaths::discover()?;

    // Commands that need no provider: copying files or listing sessions must
    // work on a machine that is not, or is no longer, configured.
    match &cli.command {
        Some(Command::Setup) => return setup::run(&paths).await,
        Some(Command::Completions { shell }) => {
            let out = &mut std::io::stdout();
            clap_complete::generate(*shell, &mut Cli::command(), "abacus", out);
            return Ok(());
        }
        Some(Command::Cron { action }) => {
            return cron::handle(action.clone(), &paths, workspace_from_cli(&cli)?).await;
        }
        Some(Command::Sync { action }) => {
            let workspace = workspace_from_cli(&cli)?;
            return abacus_agent::sync::handle(action.clone(), &paths, workspace).await;
        }
        Some(Command::Update { desktop, check }) => return abacus_agent::update::run(*desktop, *check).await,
        Some(Command::Sessions { all, json }) => {
            return print_sessions(&paths, workspace_from_cli(&cli)?, *all, *json);
        }
        Some(Command::Pull { destination, all }) => {
            // `abacus pull all` reads as a word, not a path. A directory
            // genuinely named `all` is still reachable as `--all ./all`.
            let keyword = destination.as_deref().is_some_and(|path| path.as_os_str() == "all");
            let destination = destination.clone().filter(|_| !keyword).unwrap_or_else(|| ".".into());
            return pull_traces(&paths, &destination, *all || keyword);
        }
        Some(Command::Skills { .. } | Command::Plugins { .. } | Command::Mcp | Command::Trust | Command::Untrust) => {
            return extension_command(&cli, &paths).await;
        }
        Some(Command::Models { provider, refresh, json }) => {
            return list_models(&paths, provider.as_deref(), *refresh, *json).await;
        }
        Some(Command::Providers { json }) => return list_providers(&paths, *json),
        Some(Command::Profiles { json }) => return list_profiles(&paths, *json),
        _ => {}
    }

    let mut settings = Settings::load(&paths)?;
    // The app server sets a provider up from the front end rather than
    // through the terminal wizard, so it starts either way.
    let app_server = matches!(cli.command, Some(Command::AppServer { .. }));
    if !settings.is_configured() && !cli.has_inline_provider() && !app_server {
        if matches!(cli.command, Some(Command::Doctor)) {
            println!("Abacus {}", env!("CARGO_PKG_VERSION"));
            println!("home       {}", paths.root.display());
            println!("config     no provider yet (run `abacus setup`)");
            return Ok(());
        }
        eprintln!("Abacus needs a provider before its first run. Starting setup…\n");
        setup::run(&paths).await?;
        settings = Settings::load(&paths)?;
    }
    let credentials = Credentials::load(&paths)?;
    let mut config = match Config::resolve(&cli, &settings, &credentials, paths.clone()) {
        Ok(config) => config,
        Err(error) if app_server => {
            eprintln!("app-server: no usable profile yet — {error:#}");
            Config::unconfigured(&cli, &settings, paths.clone())?
        }
        Err(error) => return Err(error),
    };

    // Commands that talk to the provider but open no session.
    match cli.command {
        Some(Command::Upstreams) => return list_upstreams(&config).await,
        Some(Command::Doctor) => return doctor(&config, &settings).await,
        // Everything else returned above; the app server wants the same
        // resolved config, session store, and services the TUI gets.
        _ => {}
    }

    // Best-effort: the model's real context window and output cap, from the
    // provider's cached model list or a quick look at its endpoint, so
    // compaction thresholds and output limits scale with the model. Non-fatal
    // — the heuristic/default estimates stand otherwise.
    if config.is_configured()
        && let Some(limits) = detect_limits(&config, config.model_limits).await
    {
        config.model_limits = limits;
    }

    let store = SessionStore::new(&paths, config.workspace.clone());
    let services = Arc::new(AgentServices::discover(&config.workspace, &config.paths, &settings).await?);
    // A headless run continues whatever it resumes from the newest copy, so it
    // syncs before reading it. The terminal and the app server sync in the
    // background and protect the session they have open instead.
    if cli.prompt.is_some() {
        headless::pull_before_run(&paths).await;
    }
    // Session creation is otherwise deferred until the first message is sent,
    // which avoids littering the store with empty sessions on every startup.
    let mut session = match (&cli.resume, cli.continue_last, config.no_session) {
        (_, _, true) => None,
        (Some(id), _, _) => Some(store.load(id)?),
        (None, true, _) => Some(store.latest()?),
        (None, false, _) => None,
    };
    let store = (!cli.no_session).then_some(store);

    let Some(prompt) = cli.prompt else {
        return if matches!(cli.command, Some(Command::AppServer { .. })) {
            app_server::run(config, settings, session, store, services).await
        } else {
            tui::run(
                config,
                settings,
                credentials,
                session,
                store,
                services,
                cli.mobile.map(|port| (port, cli.mobile_lan, cli.mobile_tunnel)),
            )
            .await
        };
    };
    for diagnostic in services.diagnostics() {
        eprintln!("warning: {diagnostic}");
    }
    let mut messages =
        session.as_ref().map(|value| value.messages.clone()).unwrap_or_else(|| initial_messages(&config.workspace));
    // With --goal the prompt is the goal too, as `/goal <prompt>` makes it.
    let goal = cli.goal.then(|| prompt.clone());
    let prompt = expand_file_references(&config.workspace, &prompt)?;
    // `@shot.png` in the prompt and `--image` files both reach the model as
    // images; a prompt with neither stays a plain string.
    let images = cli.images.iter().map(|path| context::image_data_url(path)).collect::<Result<Vec<_>>>()?;
    let content =
        context::with_images(context::user_content(&config.workspace, &config.paths.attachments_dir, &prompt), images);
    // Where the run happens, and for a resumed session how long it was left.
    messages.push(opening_note(Opened::of(session.as_ref()), &config.workspace));
    messages.push(json!({"role": "user", "content": content}));
    if let (Some(session), Some(store)) = (session.as_mut(), &store) {
        session.update_messages(messages.clone());
        store.save(session)?;
    }
    let reporter = ActivityReporter::new(settings.activity.enabled, &settings.activity.endpoint, &config.paths);
    headless::run(config, cli.output_format, messages, session, store, services, goal, reporter).await
}

/// The model's limits as the provider reports them, unless the user pinned
/// them. `None` when there is nothing to improve on.
async fn detect_limits(config: &Config, mut limits: model_info::ModelLimits) -> Option<model_info::ModelLimits> {
    if limits.source == model_info::LimitSource::Override {
        return None;
    }
    let (context, output) = model_info::detect_limits(&config.paths, &config.connection, &config.model).await?;
    limits.apply_detected(context, output);
    Some(limits)
}

/// `abacus models`: every model every provider serves, from the cached lists
/// unless `--refresh` asks again (a list older than a day is asked again
/// anyway).
async fn list_models(paths: &AbacusPaths, only: Option<&str>, refresh: bool, json: bool) -> Result<()> {
    let settings = Settings::load(paths)?;
    let credentials = Credentials::load(paths)?;
    if let Some(only) = only
        && !settings.providers.contains_key(only)
    {
        anyhow::bail!("no provider named `{only}`; `abacus providers` lists them");
    }
    let active = settings.active_profile().map(|profile| profile.model.clone()).unwrap_or_default();
    let mut rows = Vec::new();
    for (id, provider) in settings.providers.iter().filter(|(id, _)| only.is_none_or(|only| only == id.as_str())) {
        let connection = abacus_agent::providers::Connection::resolve(id, provider, &credentials);
        match abacus_agent::providers::catalog(paths, &connection, refresh).await {
            Ok(catalog) => rows.extend(catalog.models.into_iter().map(|card| (id.clone(), card))),
            Err(error) => eprintln!("{id}: {error:#}"),
        }
    }
    if json {
        let models: Vec<_> = rows
            .iter()
            .map(|(provider, card)| {
                json!({
                    "id": abacus_agent::providers::join_ref(provider, &card.id),
                    "provider": provider,
                    "model": card.id,
                    "name": card.name,
                    "contextLength": card.context_length,
                    "maxOutputTokens": card.max_output_tokens,
                    "inputCost": card.input_cost,
                    "outputCost": card.output_cost,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&json!({"models": models}))?);
        return Ok(());
    }
    for (provider, card) in &rows {
        let reference = abacus_agent::providers::join_ref(provider, &card.id);
        let facts = [card.context_label(), card.cost_label()].into_iter().filter(|fact| !fact.is_empty());
        println!(
            "{} {reference}  {}",
            if reference == active { "*" } else { " " },
            facts.collect::<Vec<_>>().join("  ")
        );
    }
    Ok(())
}

/// `abacus providers`: the configured connections and where each one's key
/// comes from.
fn list_providers(paths: &AbacusPaths, json: bool) -> Result<()> {
    let settings = Settings::load(paths)?;
    let credentials = Credentials::load(paths)?;
    if json {
        let providers: Vec<_> = settings
            .providers
            .iter()
            .map(|(id, provider)| {
                json!({
                    "id": id,
                    "name": provider.display_name(id),
                    "baseUrl": provider.base_url,
                    "protocol": provider.protocol,
                    "key": provider.key_status(id, &credentials).label(),
                    "profiles": settings.profiles_on(id),
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&json!({"providers": providers}))?);
        return Ok(());
    }
    if settings.providers.is_empty() {
        println!("No providers yet. `abacus setup` connects one.");
        return Ok(());
    }
    let width = settings.providers.keys().map(String::len).max().unwrap_or(8);
    for (id, provider) in &settings.providers {
        println!(
            "{id:<width$}  {}  {}  key: {}",
            provider.display_name(id),
            provider.base_url,
            provider.key_status(id, &credentials).label()
        );
    }
    Ok(())
}

/// `abacus profiles`: what `--profile` and `/profile` can switch to.
fn list_profiles(paths: &AbacusPaths, json: bool) -> Result<()> {
    let settings = Settings::load(paths)?;
    if json {
        let profiles: Vec<_> = settings
            .profiles
            .iter()
            .map(|(name, profile)| {
                json!({
                    "name": name,
                    "model": profile.model,
                    "active": *name == settings.default_profile,
                    "contextWindow": profile.context_window,
                    "maxOutputTokens": profile.max_output_tokens,
                    "reasoningEffort": profile.reasoning_effort,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&json!({"profiles": profiles}))?);
        return Ok(());
    }
    if settings.profiles.is_empty() {
        println!("No profiles yet. `abacus setup` makes one.");
        return Ok(());
    }
    for (name, profile) in &settings.profiles {
        let marker = if *name == settings.default_profile { "*" } else { " " };
        if *name == profile.model {
            println!("{marker} {name}");
        } else {
            println!("{marker} {name}  ({})", profile.model);
        }
    }
    Ok(())
}

/// `abacus skills | plugins | mcp | trust | untrust`.
async fn extension_command(cli: &Cli, paths: &AbacusPaths) -> Result<()> {
    let mut settings = Settings::load(paths)?;
    let workspace = workspace_from_cli(cli)?;
    // The commands that only edit settings, and what they report.
    let changed = match &cli.command {
        Some(Command::Trust) => {
            settings.trust.set(&workspace, true);
            format!("Trusted project extensions in {}", workspace.display())
        }
        Some(Command::Untrust) => {
            settings.trust.set(&workspace, false);
            format!("Revoked project extension trust for {}", workspace.display())
        }
        Some(Command::Plugins { action: Some(PluginsCommand::Install { path, force }) }) => {
            let plugin = PluginRegistry::install(path, paths, *force)?;
            settings.plugins.disabled.remove(&plugin.name);
            format!("Installed {} {}", plugin.name, plugin.version)
        }
        Some(Command::Plugins { action: Some(PluginsCommand::Remove { name }) }) => {
            PluginRegistry::remove(name, paths)?;
            settings.plugins.disabled.remove(name);
            format!("Removed {name}")
        }
        Some(Command::Plugins { action: Some(PluginsCommand::Enable { name }) }) => {
            settings.plugins.disabled.remove(name);
            format!("Enabled {name}")
        }
        Some(Command::Plugins { action: Some(PluginsCommand::Disable { name }) }) => {
            settings.plugins.disabled.insert(name.clone());
            format!("Disabled {name}")
        }
        _ => String::new(),
    };
    if !changed.is_empty() {
        settings.save(paths)?;
        println!("{changed}");
        return Ok(());
    }

    let services = AgentServices::discover(&workspace, paths, &settings).await?;
    match &cli.command {
        Some(Command::Skills { action: Some(SkillsCommand::Inspect { name }) }) => {
            let skills = services.skills.read().expect("skill registry lock");
            let output = skills.execute("skill_load", &json!({"name":name}).to_string());
            println!("{}", output.context("skill tool unavailable")?);
        }
        Some(Command::Skills { .. }) => {
            for skill in services.skills.read().expect("skill registry lock").list() {
                println!("{}\t{}\t{}", skill.name, skill.source, skill.description);
            }
        }
        Some(Command::Plugins { action: Some(PluginsCommand::Inspect { name }) }) => {
            let plugin = services
                .plugins
                .list()
                .find(|plugin| plugin.name == *name)
                .with_context(|| format!("plugin `{name}` is not enabled"))?;
            println!("{} {}\n{}\nroot: {}", plugin.name, plugin.version, plugin.description, plugin.root.display());
            for command in &plugin.commands {
                println!("command: /{} — {}", command.name, command.description);
            }
            for hook in &plugin.hooks {
                println!("hook: {} — {}", hook.event, hook.command);
            }
            for name in plugin.mcp.keys() {
                println!("mcp: {name}");
            }
        }
        Some(Command::Plugins { .. }) => {
            for plugin in services.plugins.list() {
                println!("{}\t{}\t{}\t{}", plugin.name, plugin.version, plugin.source, plugin.description);
            }
        }
        _ => {
            for tool in services.mcp.tools() {
                println!("{}\t{}\t{}", tool.server, tool.exposed_name, tool.description);
            }
        }
    }
    for diagnostic in services.diagnostics() {
        eprintln!("warning: {diagnostic}");
    }
    Ok(())
}

/// `abacus upstreams`: the OpenRouter upstreams that can serve the active
/// model, with the ones the profile already pins marked — so the list doubles
/// as a view of the current routing.
async fn list_upstreams(config: &Config) -> Result<()> {
    use abacus_agent::console;
    let endpoints = abacus_agent::providers::discover_upstreams(&config.connection, &config.model).await?;
    console::banner(&format!("upstreams for {}", config.model_ref()));
    console::blank();
    if endpoints.is_empty() {
        console::note("The endpoint reported no upstream providers for this model.");
        console::blank();
        return Ok(());
    }
    let pinned = &config.routing.order;
    let width = endpoints.iter().map(|endpoint| endpoint.name.len()).max().unwrap_or(16);
    for endpoint in &endpoints {
        let is_pinned = pinned.iter().any(|entry| entry.eq_ignore_ascii_case(&endpoint.name) || *entry == endpoint.tag);
        let marker = if is_pinned { console::ok(console::marks().pass) } else { " ".to_owned() };
        println!(
            "  {marker} {}  {}  {}",
            console::pad(&endpoint.name, width),
            console::dim(&console::pad(&endpoint.tag, 22)),
            console::dim(&format!(
                "{:>9} ctx  {}",
                abacus_agent::ui::format_count(endpoint.context_length),
                endpoint.quantization
            )),
        );
    }
    console::blank();
    if pinned.is_empty() {
        console::note("Nothing pinned — OpenRouter chooses. Pin them under /config → Profile → Upstream routing.");
    } else {
        console::note(&format!(
            "Pinned: {}  ·  fallbacks {}",
            pinned.join(", "),
            if config.routing.allow_fallbacks { "allowed" } else { "off" }
        ));
    }
    console::blank();
    Ok(())
}

/// `abacus sessions`: this workspace's sessions, or with `--all` every
/// workspace's. `--json` is the shape a front end reads; it is the same list.
fn print_sessions(paths: &AbacusPaths, workspace: PathBuf, all: bool, json: bool) -> Result<()> {
    let workspaces = if all {
        abacus_agent::session::list_all(paths)?
    } else {
        let sessions = SessionStore::new(paths, workspace.clone()).list()?;
        if sessions.is_empty() { Vec::new() } else { vec![WorkspaceSessions { workspace, sessions }] }
    };
    if json {
        let live = abacus_agent::presence::live_sessions(paths);
        println!("{}", serde_json::to_string(&sessions_json(&workspaces, &live))?);
        return Ok(());
    }
    if workspaces.is_empty() {
        println!("{}", if all { "No saved sessions." } else { "No saved sessions for this workspace." });
        return Ok(());
    }
    for entry in &workspaces {
        if all {
            println!("{}", entry.workspace.display());
        }
        for session in &entry.sessions {
            println!(
                "{}{}  {}  {:>3} messages  {}",
                if all { "  " } else { "" },
                &session.id.to_string()[..8],
                session.updated_at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                session.message_count,
                session.title
            );
        }
    }
    Ok(())
}

/// `live` says which sessions are open somewhere right now; a front end shows
/// them as working or open, and nests forks under `forkedFrom`.
fn sessions_json(
    workspaces: &[WorkspaceSessions],
    live: &std::collections::HashMap<uuid::Uuid, abacus_agent::presence::Live>,
) -> serde_json::Value {
    let workspaces: Vec<_> = workspaces
        .iter()
        .map(|entry| {
            let sessions: Vec<_> = entry
                .sessions
                .iter()
                .map(|session| {
                    serde_json::json!({
                        "id": session.id.to_string(),
                        "title": session.title,
                        "model": session.model,
                        "updatedAt": session.updated_at.to_rfc3339(),
                        "messageCount": session.message_count,
                        "forkedFrom": session.forked_from.map(|id| id.to_string()),
                        "live": live.get(&session.id).map(abacus_agent::presence::Live::to_json),
                    })
                })
                .collect();
            serde_json::json!({
                "workspace": entry.workspace,
                "exists": entry.workspace.is_dir(),
                "sessions": sessions,
            })
        })
        .collect();
    serde_json::json!({ "workspaces": workspaces })
}

/// `abacus pull` — copy this machine's training traces into a directory.
fn pull_traces(paths: &AbacusPaths, destination: &Path, all: bool) -> Result<()> {
    use abacus_agent::console::{self, Health};
    use abacus_agent::sft::Pulled;

    let mut pulled = abacus_agent::sft::pull(&paths.traces_dir, destination)?;
    console::banner(if all { "training traces · all sessions" } else { "training traces" });
    console::blank();
    console::field("from", &paths.traces_dir.display().to_string());
    if all {
        console::field("and", &paths.sessions_dir.display().to_string());
    }
    console::field("into", &destination.display().to_string());
    console::blank();

    let mut rebuilt = 0usize;
    if all {
        // A live capture is strictly richer than a reconstruction, so sessions
        // that already have one are left alone.
        let captured = pulled.iter().map(|entry| entry.name.clone()).collect::<std::collections::HashSet<_>>();
        let from_sessions = abacus_agent::sft::pull_sessions(&paths.sessions_dir, destination, &captured)?;
        // Counts what was actually written, so a repeat run does not claim to
        // have rebuilt nine files it left untouched.
        rebuilt =
            from_sessions.iter().filter(|entry| matches!(entry.outcome, Pulled::Copied | Pulled::Updated)).count();
        pulled.extend(from_sessions);
        pulled.sort_by(|a, b| a.name.cmp(&b.name));
    }

    if pulled.is_empty() {
        console::check(Health::Warn, "traces", "none recorded yet — traces are written as you use Abacus");
        console::blank();
        return Ok(());
    }

    let width = pulled.iter().map(|entry| entry.name.len()).max().unwrap_or(20);
    let mut records = 0usize;
    let mut copied = 0usize;
    for entry in &pulled {
        records += entry.records;
        let (health, note) = match entry.outcome {
            Pulled::Copied => {
                copied += 1;
                (Health::Pass, "copied")
            }
            Pulled::Updated => {
                copied += 1;
                (Health::Pass, "updated")
            }
            Pulled::Unchanged => (Health::Pass, "already current"),
            Pulled::Empty => (Health::Warn, "no records — skipped"),
        };
        println!(
            "  {} {}  {}  {}",
            match health {
                Health::Pass => console::ok(console::marks().pass),
                _ => console::warn(console::marks().warn),
            },
            console::pad(&entry.name, width),
            console::dim(&format!("{:>6} records{:>10}", entry.records, human_bytes(entry.bytes))),
            console::dim(note),
        );
    }

    console::blank();
    println!("  {}", console::dim(&console::marks().rule.repeat(console::WIDTH)));
    println!(
        "  {} {} from {} · originals left in place",
        console::ok(console::marks().pass),
        console::bold(&console::count(records, "record")),
        console::count(copied, "file"),
    );
    if all && rebuilt > 0 {
        println!(
            "  {}",
            console::dim(&format!(
                "{} rebuilt from saved sessions — no reasoning or tool list, \
                 marked \"source\": \"session\"",
                console::count(rebuilt, "file")
            ))
        );
    } else if !all {
        println!("  {}", console::dim("run `abacus pull all` to include sessions recorded before tracing"));
    }
    console::blank();
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Environment and configuration diagnostics.
///
/// Grouped so the output can be read top to bottom as an answer to "why isn't
/// this working": identity first, then the provider round-trip, then the local
/// state. Anything that is merely worth knowing is a warning; only a genuine
/// failure sets the non-zero exit.
async fn doctor(config: &Config, settings: &Settings) -> Result<()> {
    use abacus_agent::console::{self, Health};
    use abacus_agent::i18n::{self, Key};

    // `doctor` is the command someone runs when nothing works, so it is read
    // in their language too. `main` has already set the language from the
    // environment; this is the settings file's say, which needs `Settings`.
    i18n::init(settings.ui.language.as_deref());

    let mut failures = 0usize;
    let mut warnings = 0usize;
    let mut record = |health: Health, label: &str, detail: &str| {
        match health {
            Health::Fail => failures += 1,
            Health::Warn => warnings += 1,
            Health::Pass => {}
        }
        console::check(health, label, detail);
    };

    console::banner(&i18n::t(Key::DoctorBanner).replace("{}", env!("CARGO_PKG_VERSION")));

    console::section(i18n::t(Key::DoctorSectionEnvironment));
    console::field(i18n::t(Key::DoctorFieldHome), &config.paths.root.display().to_string());
    console::field(i18n::t(Key::DoctorFieldWorkspace), &config.workspace.display().to_string());
    console::field(
        i18n::t(Key::DoctorFieldTerminal),
        &i18n::t(Key::DoctorTerminalDetail)
            .replacen("{}", &std::env::var("TERM").unwrap_or_else(|_| i18n::t(Key::WordUnset).to_owned()), 1)
            .replacen(
                "{}",
                match abacus_agent::theme::ColorDepth::detect() {
                    abacus_agent::theme::ColorDepth::None => i18n::t(Key::WordNo),
                    abacus_agent::theme::ColorDepth::Ansi16 => "16",
                    abacus_agent::theme::ColorDepth::Ansi256 => "256",
                    abacus_agent::theme::ColorDepth::TrueColor => "true",
                },
                1,
            )
            .replacen("{}", if abacus_agent::ui::glyphs().wordmark.is_some() { "unicode" } else { "ascii" }, 1),
    );

    console::section(i18n::t(Key::DoctorSectionProvider));
    console::field(i18n::t(Key::DoctorFieldProfile), &config.profile);
    console::field(i18n::t(Key::DoctorFieldModel), &config.model_ref());
    console::field(i18n::t(Key::DoctorFieldProvider), &config.connection.name);
    console::field(i18n::t(Key::DoctorFieldEndpoint), &config.connection.request_url());
    console::field(i18n::t(Key::DoctorFieldProtocol), &format!("{:?}", config.connection.protocol));
    console::field(i18n::t(Key::DoctorFieldMode), config.permission.label());
    // A token command or file is run here, so a broken one shows up as the
    // failure it will be on the first request.
    let credential = i18n::t(Key::DoctorCheckCredential);
    match config.connection.auth() {
        Ok(Some(_)) => record(Health::Pass, credential, i18n::t(Key::DoctorCredentialAvailable)),
        Ok(None) if config.connection.is_local() => {
            record(Health::Pass, credential, i18n::t(Key::DoctorCredentialLocal))
        }
        Ok(None) => record(Health::Fail, credential, i18n::t(Key::DoctorCredentialMissing)),
        Err(error) => record(Health::Fail, credential, &format!("{error:#}")),
    }
    let reachable = i18n::t(Key::DoctorCheckReachable);
    let discovered = abacus_agent::providers::refresh_catalog(&config.paths, &config.connection).await;
    match discovered.map(|catalog| catalog.models.into_iter().map(|card| card.id).collect::<Vec<_>>()) {
        Ok(models) if models.contains(&config.model) => record(
            Health::Pass,
            reachable,
            &i18n::t(Key::DoctorReachableFound)
                .replacen("{}", &models.len().to_string(), 1)
                .replacen("{}", &config.model, 1),
        ),
        // The endpoint answered but does not list the configured model. Some
        // gateways omit models they still serve, so this is a warning rather
        // than a failure.
        Ok(models) => record(
            Health::Warn,
            reachable,
            &i18n::t(Key::DoctorReachableMissing)
                .replacen("{}", &models.len().to_string(), 1)
                .replacen("{}", &config.model, 1),
        ),
        Err(error) => record(
            Health::Fail,
            reachable,
            &abacus_agent::text::clip(&abacus_agent::text::flat(&format!("{error:#}")), 160, "…"),
        ),
    }

    // Show the context budget a real run would use. Doctor is a diagnostic
    // command, so a best-effort /models probe is acceptable here and lets the
    // reported limits reflect the detected values rather than just the
    // heuristic/default estimate.
    let limits = detect_limits(config, config.model_limits).await.unwrap_or(config.model_limits);
    let output_cap =
        limits.configured_output_tokens.map(|tokens| tokens.to_string()).unwrap_or_else(|| "auto".to_owned());
    console::field(
        i18n::t(Key::DoctorFieldLimits),
        &i18n::t(Key::DoctorLimitsDetail)
            .replacen("{}", &limits.context_window.to_string(), 1)
            .replacen("{}", &output_cap, 1)
            .replacen(
                "{}",
                match limits.source {
                    model_info::LimitSource::Override => "override",
                    model_info::LimitSource::Detected => "detected",
                    model_info::LimitSource::Heuristic => "heuristic",
                    model_info::LimitSource::Default => "default",
                },
                1,
            )
            .replacen("{}", &limits.compaction_budget().compact_at_chars.to_string(), 1),
    );
    let tool_fmt = config.tool_format.as_arg();
    console::field(
        i18n::t(Key::DoctorFieldToolCalls),
        &if tool_fmt == "none" {
            i18n::t(Key::DoctorToolCallsNative).to_owned()
        } else {
            i18n::t(Key::DoctorToolCallsFallback).replace("{}", tool_fmt)
        },
    );

    console::section(i18n::t(Key::DoctorSectionLocalState));
    if config.paths.config_file.exists() {
        record(Health::Pass, i18n::t(Key::DoctorCheckSettings), &config.paths.config_file.display().to_string());
    } else {
        record(Health::Warn, i18n::t(Key::DoctorCheckSettings), i18n::t(Key::DoctorSettingsMissing));
    }
    record(
        Health::Pass,
        i18n::t(Key::DoctorCheckProviders),
        &i18n::t(Key::DoctorProvidersDetail)
            .replacen("{}", &i18n::count(settings.providers.len(), Key::WordProvider, Key::WordProviders), 1)
            .replacen("{}", &i18n::count(settings.profiles.len(), Key::WordProfile, Key::WordProfiles), 1),
    );
    let store = SessionStore::new(&config.paths, config.workspace.clone());
    match store.list().context("could not inspect sessions") {
        Ok(sessions) => record(
            Health::Pass,
            i18n::t(Key::DoctorCheckSessions),
            &i18n::t(Key::DoctorSessionsDetail)
                .replace("{}", &i18n::count(sessions.len(), Key::WordSession, Key::WordSessions)),
        ),
        Err(error) => record(Health::Fail, i18n::t(Key::DoctorCheckSessions), &format!("{error:#}")),
    }
    if config.trace_enabled {
        let count = std::fs::read_dir(&config.paths.traces_dir).map(|entries| entries.flatten().count()).unwrap_or(0);
        record(
            Health::Pass,
            i18n::t(Key::DoctorCheckTraces),
            &i18n::t(Key::DoctorTracesOn)
                .replacen("{}", &i18n::count(count, Key::WordSession, Key::WordSessions), 1)
                .replacen("{}", &config.paths.traces_dir.display().to_string(), 1),
        );
    } else {
        record(Health::Pass, i18n::t(Key::DoctorCheckTraces), i18n::t(Key::DoctorTracesOff));
    }
    match cron::CronStore::new(&config.paths).list() {
        Ok(jobs) if jobs.is_empty() => {
            record(Health::Pass, i18n::t(Key::DoctorCheckCron), i18n::t(Key::DoctorCronNone))
        }
        Ok(jobs) => record(
            Health::Pass,
            i18n::t(Key::DoctorCheckCron),
            &i18n::t(Key::DoctorCronScheduled).replace("{}", &jobs.len().to_string()),
        ),
        Err(error) => record(Health::Fail, i18n::t(Key::DoctorCheckCron), &format!("{error:#}")),
    }
    match AgentServices::discover(&config.workspace, &config.paths, settings).await {
        Ok(services) if services.diagnostics().is_empty() => {
            record(Health::Pass, i18n::t(Key::DoctorCheckExtensions), i18n::t(Key::DoctorExtensionsClean))
        }
        Ok(services) => {
            record(
                Health::Warn,
                i18n::t(Key::DoctorCheckExtensions),
                &i18n::count(services.diagnostics().len(), Key::WordWarning, Key::WordWarnings),
            );
            for diagnostic in services.diagnostics() {
                println!("               {}", console::dim(&diagnostic));
            }
        }
        Err(error) => record(Health::Fail, i18n::t(Key::DoctorCheckExtensions), &format!("{error:#}")),
    }
    #[cfg(unix)]
    if config.paths.credentials_file.exists() {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&config.paths.credentials_file)?.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            record(Health::Pass, i18n::t(Key::DoctorCheckKeyPerms), &format!("{mode:o}"));
        } else {
            record(
                Health::Fail,
                i18n::t(Key::DoctorCheckKeyPerms),
                &i18n::t(Key::DoctorKeyPermsInsecure).replace("{}", &format!("{mode:o}")),
            );
        }
    }

    console::blank();
    println!("  {}", console::dim(&console::marks().rule.repeat(console::WIDTH)));
    let summary = match (failures, warnings) {
        (0, 0) => console::ok(&i18n::t(Key::DoctorSummaryPassed).replace("{}", console::marks().pass)),
        (0, warnings) => {
            console::warn(&i18n::t(Key::DoctorSummaryWarnings).replacen("{}", console::marks().warn, 1).replacen(
                "{}",
                &i18n::count(warnings, Key::WordWarning, Key::WordWarnings),
                1,
            ))
        }
        (failures, _) => {
            console::err(&i18n::t(Key::DoctorSummaryFailures).replacen("{}", console::marks().fail, 1).replacen(
                "{}",
                &i18n::count(failures, Key::WordProblem, Key::WordProblems),
                1,
            ))
        }
    };
    println!("  {summary}");
    console::blank();

    // The last line a failing run prints, so it is in the reader's language too.
    if failures == 0 { Ok(()) } else { anyhow::bail!(i18n::t(Key::DoctorProblemsFound)) }
}
