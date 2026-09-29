use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

use abacus_agent::{
    activity::ActivityReporter,
    agent::initial_messages,
    app_server,
    config::{
        AbacusPaths, Cli, Command, Config, Credentials, PluginsCommand, Settings, SkillsCommand,
        workspace_from_cli,
    },
    context::expand_file_references,
    cron,
    extensions::PluginRegistry,
    headless, model_info,
    services::AgentServices,
    session::SessionStore,
    setup, tui,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.loop_run && cli.prompt.is_none() {
        anyhow::bail!("--loop requires a prompt via -p/--prompt");
    }
    let paths = AbacusPaths::discover()?;

    // Commands that need no provider: copying files or listing sessions must
    // work on a machine that is not, or is no longer, configured.
    match &cli.command {
        Some(Command::Setup { force }) => return setup::run(&paths, *force).await,
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
        Some(Command::Sessions) => {
            return print_session_list(&SessionStore::new(&paths, workspace_from_cli(&cli)?));
        }
        Some(Command::Pull { destination, all }) => {
            // `abacus pull all` reads as a word, not a path. A directory
            // genuinely named `all` is still reachable as `--all ./all`.
            let keyword = destination.as_deref().is_some_and(|path| path.as_os_str() == "all");
            let destination =
                destination.clone().filter(|_| !keyword).unwrap_or_else(|| ".".into());
            return pull_traces(&paths, &destination, *all || keyword);
        }
        Some(
            Command::Skills { .. }
            | Command::Plugins { .. }
            | Command::Mcp
            | Command::Trust
            | Command::Untrust,
        ) => return extension_command(&cli, &paths).await,
        _ => {}
    }

    let mut settings = Settings::load(&paths)?;
    if !settings.is_configured() && !cli.has_inline_provider() {
        if matches!(cli.command, Some(Command::Doctor)) {
            println!("Abacus {}", env!("CARGO_PKG_VERSION"));
            println!("home       {}", paths.root.display());
            println!("config     missing (run `abacus setup`)");
            return Ok(());
        }
        eprintln!("Abacus needs a provider before its first run. Starting setup…\n");
        setup::run(&paths, false).await?;
        settings = Settings::load(&paths)?;
    }
    let credentials = Credentials::load(&paths)?;
    let mut config = Config::resolve(&cli, &settings, &credentials, paths.clone())?;

    // Commands that talk to the provider but open no session.
    match cli.command {
        Some(Command::Eval { tasks, repeat, state, model, json }) => {
            let options =
                abacus_agent::eval::EvalOptions { filter: tasks, repeat, state, model, json };
            return abacus_agent::eval::run(config, settings, options).await;
        }
        Some(Command::Models) => {
            for model in setup::discover_models(&config.base_url, config.api_key.as_deref()).await?
            {
                println!("{} {model}", if model == config.model { "*" } else { " " });
            }
            return Ok(());
        }
        Some(Command::Providers) => return list_providers(&config).await,
        Some(Command::Doctor) => return doctor(&config, &settings).await,
        // Everything else returned above; the app server wants the same
        // resolved config, session store, and services the TUI gets.
        _ => {}
    }

    // Best-effort: ask the provider for the model's real context window and
    // output cap so compaction thresholds and output limits scale with the
    // model. Non-fatal — we fall back to the heuristic/default estimates.
    if let Some(limits) = detect_limits(&config, config.model_limits).await {
        config.model_limits = limits;
    }

    let store = SessionStore::new(&paths, config.workspace.clone());
    let services =
        Arc::new(AgentServices::discover(&config.workspace, &config.paths, &settings).await?);
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
        return if matches!(cli.command, Some(Command::AppServer)) {
            app_server::run(config, settings, credentials, session, store, services).await
        } else {
            tui::run(config, settings, credentials, session, store, services).await
        };
    };
    for diagnostic in services.diagnostics() {
        eprintln!("warning: {diagnostic}");
    }
    let mut messages = session
        .as_ref()
        .map(|value| value.messages.clone())
        .unwrap_or_else(|| initial_messages(&config.workspace));
    // A loop replays its own prompt each iteration; a plain run sends it once.
    let loop_config = if cli.loop_run {
        let promise = cli
            .completion_promise
            .clone()
            .unwrap_or_else(|| abacus_agent::ralph::DEFAULT_COMPLETION_PROMISE.to_owned());
        Some(abacus_agent::ralph::RalphLoop::new(prompt, promise, cli.max_iterations)?)
    } else {
        let prompt = expand_file_references(&config.workspace, &prompt)?;
        messages.push(json!({"role": "user", "content": prompt}));
        None
    };
    if let (Some(session), Some(store)) = (session.as_mut(), &store) {
        session.update_messages(messages.clone());
        if loop_config.is_some() {
            session.ralph_loop.clone_from(&loop_config);
        }
        store.save(session)?;
    }
    let reporter = ActivityReporter::new(
        settings.activity.enabled,
        &settings.activity.endpoint,
        &config.paths,
    );
    headless::run(
        config,
        cli.output_format,
        messages,
        session,
        store,
        services,
        loop_config,
        reporter,
    )
    .await
}

/// The model's limits as the provider reports them, unless the user pinned
/// them. `None` when there is nothing to improve on.
async fn detect_limits(
    config: &Config,
    mut limits: model_info::ModelLimits,
) -> Option<model_info::ModelLimits> {
    if limits.source == model_info::LimitSource::Override {
        return None;
    }
    let (context, output) = model_info::detect_limits(
        &config.models_endpoint()?,
        config.api_key.as_deref(),
        &config.model,
    )
    .await?;
    limits.apply_detected(context, output);
    Some(limits)
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
            println!(
                "{} {}\n{}\nroot: {}",
                plugin.name,
                plugin.version,
                plugin.description,
                plugin.root.display()
            );
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
                println!(
                    "{}\t{}\t{}\t{}",
                    plugin.name, plugin.version, plugin.source, plugin.description
                );
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

/// `abacus providers`: the upstreams that can serve the active model, with the
/// ones the profile already pins marked — so the list doubles as a view of the
/// current routing.
async fn list_providers(config: &Config) -> Result<()> {
    use abacus_agent::console;
    let endpoints =
        setup::discover_endpoints(&config.base_url, config.api_key.as_deref(), &config.model)
            .await?;
    console::banner(&format!("providers for {}", config.model));
    console::blank();
    if endpoints.is_empty() {
        console::note("The endpoint reported no upstream providers for this model.");
        console::blank();
        return Ok(());
    }
    let pinned = &config.routing.order;
    let width = endpoints.iter().map(|endpoint| endpoint.name.len()).max().unwrap_or(16);
    for endpoint in &endpoints {
        let is_pinned = pinned
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(&endpoint.name) || *entry == endpoint.tag);
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
        console::note("Nothing pinned — the endpoint chooses. Pin with /providers <name, name>.");
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
fn print_session_list(store: &SessionStore) -> Result<()> {
    let sessions = store.list()?;
    if sessions.is_empty() {
        println!("No saved sessions for this workspace.");
        return Ok(());
    }
    for session in sessions {
        println!(
            "{}  {}  {:>3} messages  {}",
            &session.id.to_string()[..8],
            session.updated_at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
            session.message_count,
            session.title
        );
    }
    Ok(())
}

/// Environment and configuration diagnostics.
///
/// Grouped so the output can be read top to bottom as an answer to "why isn't
/// this working": identity first, then the provider round-trip, then the local
/// state. Anything that is merely worth knowing is a warning; only a genuine
/// failure sets the non-zero exit.
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
        let captured =
            pulled.iter().map(|entry| entry.name.clone()).collect::<std::collections::HashSet<_>>();
        let from_sessions =
            abacus_agent::sft::pull_sessions(&paths.sessions_dir, destination, &captured)?;
        // Counts what was actually written, so a repeat run does not claim to
        // have rebuilt nine files it left untouched.
        rebuilt = from_sessions
            .iter()
            .filter(|entry| matches!(entry.outcome, Pulled::Copied | Pulled::Updated))
            .count();
        pulled.extend(from_sessions);
        pulled.sort_by(|a, b| a.name.cmp(&b.name));
    }

    if pulled.is_empty() {
        console::check(
            Health::Warn,
            "traces",
            "none recorded yet — traces are written as you use Abacus",
        );
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
        println!(
            "  {}",
            console::dim("run `abacus pull all` to include sessions recorded before tracing")
        );
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

async fn doctor(config: &Config, settings: &Settings) -> Result<()> {
    use abacus_agent::console::{self, Health};

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

    console::banner(&format!("diagnostics · v{}", env!("CARGO_PKG_VERSION")));

    console::section("Environment");
    console::field("home", &config.paths.root.display().to_string());
    console::field("workspace", &config.workspace.display().to_string());
    console::field(
        "terminal",
        &format!(
            "{} · {} colour · {} glyphs",
            std::env::var("TERM").unwrap_or_else(|_| "unset".to_owned()),
            match abacus_agent::theme::ColorDepth::detect() {
                abacus_agent::theme::ColorDepth::None => "no",
                abacus_agent::theme::ColorDepth::Ansi16 => "16",
                abacus_agent::theme::ColorDepth::Ansi256 => "256",
                abacus_agent::theme::ColorDepth::TrueColor => "true",
            },
            if abacus_agent::ui::glyphs().wordmark.is_some() { "unicode" } else { "ascii" }
        ),
    );

    console::section("Provider");
    console::field("profile", &config.profile);
    console::field("model", &config.model);
    console::field("endpoint", &config.endpoint());
    if let Some(scripted) = &config.endpoint {
        console::field("scripted endpoint", scripted.display_name());
    }
    console::field("protocol", &format!("{:?}", config.protocol));
    // A scripted endpoint with an auth block carries its own token — resolve
    // it to report health rather than flagging the absent standard credential.
    let scripted_auth = config.endpoint.as_ref().map(|endpoint| endpoint.auth_header());
    match (&config.api_key, scripted_auth) {
        (_, Some(Ok(Some(_)))) => record(Health::Pass, "credential", "scripted endpoint auth"),
        (_, Some(Ok(None))) if config.endpoint.is_some() => {
            record(Health::Pass, "credential", "none (scripted endpoint)")
        }
        (_, Some(Err(error))) => record(
            Health::Fail,
            "credential",
            &format!("scripted endpoint auth failed — {error:#}"),
        ),
        (Some(_), _) => record(Health::Pass, "credential", "available"),
        (None, _)
            if config.base_url.contains("localhost") || config.base_url.contains("127.0.0.1") =>
        {
            record(Health::Pass, "credential", "none (local endpoint)")
        }
        (None, _) => record(
            Health::Fail,
            "credential",
            "missing — export the profile's key or run `abacus setup`",
        ),
    }
    match setup::discover_models(&config.base_url, config.api_key.as_deref()).await {
        Ok(models) if models.contains(&config.model) => record(
            Health::Pass,
            "reachable",
            &format!("{} models, including {}", models.len(), config.model),
        ),
        // The endpoint answered but does not list the configured model. Some
        // gateways omit models they still serve, so this is a warning rather
        // than a failure.
        Ok(models) => record(
            Health::Warn,
            "reachable",
            &format!("{} models, but {} is not among them", models.len(), config.model),
        ),
        Err(error) => record(Health::Fail, "reachable", &one_line(&format!("{error:#}"), 160)),
    }

    // Show the context budget a real run would use. Doctor is a diagnostic
    // command, so a best-effort /models probe is acceptable here and lets the
    // reported limits reflect the detected values rather than just the
    // heuristic/default estimate.
    let limits = detect_limits(config, config.model_limits).await.unwrap_or(config.model_limits);
    let output_cap = limits
        .configured_output_tokens
        .map(|tokens| tokens.to_string())
        .unwrap_or_else(|| "auto".to_owned());
    console::field(
        "limits",
        &format!(
            "{} context · {} output · {} · compacts near {} chars",
            limits.context_window,
            output_cap,
            match limits.source {
                model_info::LimitSource::Override => "override",
                model_info::LimitSource::Detected => "detected",
                model_info::LimitSource::Heuristic => "heuristic",
                model_info::LimitSource::Default => "default",
            },
            limits.compaction_budget().compact_at_chars,
        ),
    );
    let tool_fmt = config.tool_format.as_arg();
    console::field(
        "tool calls",
        &if tool_fmt == "none" {
            "native only".to_owned()
        } else {
            format!("{tool_fmt} (text fallback enabled)")
        },
    );

    console::section("Local state");
    if config.paths.config_file.exists() {
        record(Health::Pass, "settings", &config.paths.config_file.display().to_string());
    } else {
        record(Health::Warn, "settings", "not written yet — run `abacus setup`");
    }
    let store = SessionStore::new(&config.paths, config.workspace.clone());
    match store.list().context("could not inspect sessions") {
        Ok(sessions) => {
            record(Health::Pass, "sessions", &format!("{} for this workspace", sessions.len()))
        }
        Err(error) => record(Health::Fail, "sessions", &format!("{error:#}")),
    }
    if config.trace_enabled {
        let count = std::fs::read_dir(&config.paths.traces_dir)
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        record(
            Health::Pass,
            "traces",
            &format!(
                "on · {} in {}",
                console::count(count, "session"),
                config.paths.traces_dir.display()
            ),
        );
    } else {
        record(Health::Pass, "traces", "off");
    }
    match cron::CronStore::new(&config.paths).list() {
        Ok(jobs) if jobs.is_empty() => record(Health::Pass, "cron", "no scheduled jobs"),
        Ok(jobs) => record(Health::Pass, "cron", &format!("{} scheduled", jobs.len())),
        Err(error) => record(Health::Fail, "cron", &format!("{error:#}")),
    }
    match AgentServices::discover(&config.workspace, &config.paths, settings).await {
        Ok(services) if services.diagnostics().is_empty() => {
            record(Health::Pass, "extensions", "no problems")
        }
        Ok(services) => {
            record(
                Health::Warn,
                "extensions",
                &console::count(services.diagnostics().len(), "warning"),
            );
            for diagnostic in services.diagnostics() {
                println!("               {}", console::dim(&diagnostic));
            }
        }
        Err(error) => record(Health::Fail, "extensions", &format!("{error:#}")),
    }
    #[cfg(unix)]
    if config.paths.credentials_file.exists() {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&config.paths.credentials_file)?.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            record(Health::Pass, "key perms", &format!("{mode:o}"));
        } else {
            record(
                Health::Fail,
                "key perms",
                &format!("{mode:o} — readable by others; expected 600"),
            );
        }
    }

    console::blank();
    println!("  {}", console::dim(&console::marks().rule.repeat(console::WIDTH)));
    let summary = match (failures, warnings) {
        (0, 0) => console::ok(&format!("{} All checks passed", console::marks().pass)),
        (0, warnings) => console::warn(&format!(
            "{} {}, nothing blocking",
            console::marks().warn,
            console::count(warnings, "warning")
        )),
        (failures, _) => console::err(&format!(
            "{} {} found",
            console::marks().fail,
            console::count(failures, "problem")
        )),
    };
    println!("  {summary}");
    console::blank();

    if failures == 0 { Ok(()) } else { anyhow::bail!("doctor found one or more problems") }
}

/// Collapse a multi-line error into something that fits on a diagnostic row.
fn one_line(value: &str, max: usize) -> String {
    let flat = value.replace(['\n', '\r'], " ");
    if flat.chars().count() <= max {
        return flat;
    }
    format!("{}…", flat.chars().take(max).collect::<String>())
}
