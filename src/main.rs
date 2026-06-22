use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use serde_json::json;
use std::sync::Arc;

use abacus_agent::{
    activity::ActivityReporter,
    agent::initial_messages,
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

    if let Some(Command::Setup { force }) = &cli.command {
        return setup::run(&paths, *force).await;
    }
    if let Some(Command::Completions { shell }) = &cli.command {
        clap_complete::generate(
            *shell,
            &mut Cli::command(),
            "abacus",
            &mut std::io::stdout(),
        );
        return Ok(());
    }

    if let Some(Command::Cron { action }) = cli.command.clone() {
        let workspace = workspace_from_cli(&cli)?;
        return cron::handle(action, &paths, workspace).await;
    }

    let mut settings = Settings::load(&paths)?;
    if matches!(
        cli.command,
        Some(Command::Skills { .. })
            | Some(Command::Plugins { .. })
            | Some(Command::Mcp)
            | Some(Command::Trust)
            | Some(Command::Untrust)
    ) {
        let workspace = workspace_from_cli(&cli)?;
        match &cli.command {
            Some(Command::Trust) => {
                settings.trust.set(&workspace, true);
                settings.save(&paths)?;
                println!("Trusted project extensions in {}", workspace.display());
            }
            Some(Command::Untrust) => {
                settings.trust.set(&workspace, false);
                settings.save(&paths)?;
                println!(
                    "Revoked project extension trust for {}",
                    workspace.display()
                );
            }
            Some(Command::Plugins {
                action: Some(PluginsCommand::Install { path, force }),
            }) => {
                let plugin = PluginRegistry::install(path, &paths, *force)?;
                settings.plugins.disabled.remove(&plugin.name);
                settings.save(&paths)?;
                println!("Installed {} {}", plugin.name, plugin.version);
            }
            Some(Command::Plugins {
                action: Some(PluginsCommand::Remove { name }),
            }) => {
                PluginRegistry::remove(name, &paths)?;
                settings.plugins.disabled.remove(name);
                settings.save(&paths)?;
                println!("Removed {name}");
            }
            Some(Command::Plugins {
                action: Some(PluginsCommand::Enable { name }),
            }) => {
                settings.plugins.disabled.remove(name);
                settings.save(&paths)?;
                println!("Enabled {name}");
            }
            Some(Command::Plugins {
                action: Some(PluginsCommand::Disable { name }),
            }) => {
                settings.plugins.disabled.insert(name.clone());
                settings.save(&paths)?;
                println!("Disabled {name}");
            }
            command => {
                let services = AgentServices::discover(&workspace, &paths, &settings).await?;
                match command {
                    Some(Command::Skills { action }) => match action {
                        Some(SkillsCommand::Inspect { name }) => {
                            let output = services
                                .skills
                                .execute("skill_load", &json!({"name":name}).to_string())
                                .context("skill tool unavailable")?;
                            println!("{output}");
                        }
                        _ => {
                            for skill in services.skills.list() {
                                println!("{}\t{}\t{}", skill.name, skill.source, skill.description);
                            }
                        }
                    },
                    Some(Command::Plugins { action }) => match action {
                        Some(PluginsCommand::Inspect { name }) => {
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
                        _ => {
                            for plugin in services.plugins.list() {
                                println!(
                                    "{}\t{}\t{}\t{}",
                                    plugin.name, plugin.version, plugin.source, plugin.description
                                );
                            }
                        }
                    },
                    Some(Command::Mcp) => {
                        for tool in services.mcp.tools() {
                            println!(
                                "{}\t{}\t{}",
                                tool.server, tool.exposed_name, tool.description
                            );
                        }
                    }
                    _ => {}
                }
                for diagnostic in services.diagnostics() {
                    eprintln!("warning: {diagnostic}");
                }
            }
        }
        return Ok(());
    }
    if matches!(cli.command, Some(Command::Sessions)) {
        let workspace = workspace_from_cli(&cli)?;
        let store = SessionStore::new(&paths, workspace);
        print_session_list(&store)?;
        return Ok(());
    }
    if matches!(cli.command, Some(Command::Doctor))
        && !settings.is_configured()
        && !cli.has_inline_provider()
    {
        println!("Abacus {}", env!("CARGO_PKG_VERSION"));
        println!("home       {}", paths.root.display());
        println!("config     missing (run `abacus setup`)");
        return Ok(());
    }
    if !settings.is_configured() && !cli.has_inline_provider() {
        eprintln!("Abacus needs a provider before its first run. Starting setup…\n");
        setup::run(&paths, false).await?;
        settings = Settings::load(&paths)?;
    }
    let credentials = Credentials::load(&paths)?;
    let mut config = Config::resolve(&cli, &settings, &credentials, paths.clone())?;

    match cli.command {
        Some(Command::Models) => {
            let models =
                setup::discover_models(&config.base_url, config.api_key.as_deref()).await?;
            for model in models {
                let marker = if model == config.model { "*" } else { " " };
                println!("{marker} {model}");
            }
            return Ok(());
        }
        Some(Command::Sessions) => {
            unreachable!()
        }
        Some(Command::Doctor) => {
            return doctor(&config, &settings).await;
        }
        Some(Command::Setup { .. }) => unreachable!(),
        Some(Command::Completions { .. }) => unreachable!(),
        Some(Command::Skills { .. })
        | Some(Command::Plugins { .. })
        | Some(Command::Mcp)
        | Some(Command::Trust)
        | Some(Command::Untrust)
        | Some(Command::Cron { .. }) => unreachable!(),
        None => {}
    }

    // Best-effort: ask the provider for the model's real context window and
    // output cap so compaction thresholds and output limits scale with the
    // model. Non-fatal — we fall back to the heuristic/default estimates.
    if config.model_limits.source != model_info::LimitSource::Override
        && let Some((context, output)) =
            model_info::detect_limits(&config.base_url, config.api_key.as_deref(), &config.model)
                .await
    {
        config.model_limits.apply_detected(context, output);
    }

    let store = SessionStore::new(&paths, config.workspace.clone());
    let services =
        Arc::new(AgentServices::discover(&config.workspace, &config.paths, &settings).await?);
    if cli.prompt.is_some() {
        for diagnostic in services.diagnostics() {
            eprintln!("warning: {diagnostic}");
        }
    }
    let mut session = if config.no_session {
        None
    } else if let Some(id) = &cli.resume {
        Some(store.load(id)?)
    } else if cli.continue_last {
        Some(store.latest()?)
    } else {
        // Defer session creation until the first message is sent — avoids
        // littering the store with empty sessions on every startup.
        None
    };

    if let Some(prompt) = cli.prompt {
        let mut messages = session
            .as_ref()
            .map(|value| value.messages.clone())
            .unwrap_or_else(|| initial_messages(&config.workspace));
        let mut loop_config = None;
        if cli.loop_run {
            let promise = cli
                .completion_promise
                .clone()
                .unwrap_or_else(|| abacus_agent::ralph::DEFAULT_COMPLETION_PROMISE.to_owned());
            loop_config = Some(abacus_agent::ralph::RalphLoop::new(
                prompt.clone(),
                promise,
                cli.max_iterations,
            )?);
        } else {
            let prompt = expand_file_references(&config.workspace, &prompt)?;
            messages.push(json!({"role": "user", "content": prompt}));
        }
        if let Some(value) = session.as_mut() {
            value.update_messages(messages.clone());
            if let Some(ralph) = &loop_config {
                value.ralph_loop = Some(ralph.clone());
            }
            store.save(value)?;
        }
        let reporter = ActivityReporter::new(
            settings.activity.enabled,
            &settings.activity.endpoint,
            &config.paths,
        );
        return headless::run(
            config,
            cli.output_format,
            messages,
            session,
            (!cli.no_session).then_some(store),
            services,
            loop_config,
            reporter,
        )
        .await;
    }

    tui::run(
        config,
        settings,
        credentials,
        session,
        (!cli.no_session).then_some(store),
        services,
    )
    .await
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
            session.updated_at.format("%Y-%m-%d %H:%M"),
            session.message_count,
            session.title
        );
    }
    Ok(())
}

async fn doctor(config: &Config, settings: &Settings) -> Result<()> {
    let mut healthy = true;
    println!("Abacus {}", env!("CARGO_PKG_VERSION"));
    println!("home       {}", config.paths.root.display());
    println!("workspace  {}", config.workspace.display());
    println!("profile    {}", config.profile);
    println!("model      {}", config.model);
    println!("endpoint   {}", config.base_url);
    println!("protocol   {:?}", config.protocol);
    println!(
        "api key    {}",
        if config.api_key.is_some() {
            "available"
        } else {
            "missing"
        }
    );
    print!("provider   ");
    match setup::discover_models(&config.base_url, config.api_key.as_deref()).await {
        Ok(models) => println!("ok ({} models)", models.len()),
        Err(error) => {
            healthy = false;
            println!(
                "error ({})",
                format!("{error:#}").lines().next().unwrap_or("unknown")
            );
        }
    }
    // Show the context budget a real run would use. Doctor is a diagnostic
    // command, so a best-effort /models probe is acceptable here and lets the
    // reported limits reflect the detected values rather than just the
    // heuristic/default estimate.
    let mut limits = config.model_limits;
    if limits.source != model_info::LimitSource::Override
        && let Some((context, output)) =
            model_info::detect_limits(&config.base_url, config.api_key.as_deref(), &config.model)
                .await
    {
        limits.apply_detected(context, output);
    }
    let output_cap = limits
        .configured_output_tokens
        .map(|tokens| tokens.to_string())
        .unwrap_or_else(|| "auto".to_owned());
    println!(
        "limits     {} context, {} output ({}); compacts at ~{} chars",
        limits.context_window,
        output_cap,
        match limits.source {
            model_info::LimitSource::Override => "override",
            model_info::LimitSource::Detected => "detected",
            model_info::LimitSource::Heuristic => "heuristic",
            model_info::LimitSource::Default => "default",
        },
        limits.compaction_budget().compact_at_chars,
    );
    let tool_fmt = config.tool_format.as_arg();
    println!(
        "tool fmt   {}{}",
        tool_fmt,
        if tool_fmt == "none" {
            " (native only)"
        } else {
            ""
        }
    );
    print!("sessions   ");
    let store = SessionStore::new(&config.paths, config.workspace.clone());
    match store.list().context("could not inspect sessions") {
        Ok(sessions) => println!("ok ({})", sessions.len()),
        Err(error) => {
            healthy = false;
            println!("error ({error})");
        }
    }
    print!("extensions ");
    match AgentServices::discover(&config.workspace, &config.paths, settings).await {
        Ok(services) if services.diagnostics().is_empty() => println!(
            "ok ({} skills, {} plugins, {} MCP tools; project trusted: {})",
            services.skills.list().count(),
            services.plugins.list().count(),
            services.mcp.tools().count(),
            services.project_trusted()
        ),
        Ok(services) => {
            healthy = false;
            println!("warnings");
            for diagnostic in services.diagnostics() {
                println!("             {diagnostic}");
            }
        }
        Err(error) => {
            healthy = false;
            println!("error ({error:#})");
        }
    }
    print!("cron       ");
    match cron::CronStore::new(&config.paths).list() {
        Ok(jobs) => println!("ok ({} jobs)", jobs.len()),
        Err(error) => {
            healthy = false;
            println!("error ({error:#})");
        }
    }
    #[cfg(unix)]
    if config.paths.credentials_file.exists() {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&config.paths.credentials_file)?
            .permissions()
            .mode()
            & 0o777;
        print!("key perms  ");
        if mode & 0o077 == 0 {
            println!("ok ({mode:o})");
        } else {
            healthy = false;
            println!("unsafe ({mode:o}; expected 600)");
        }
    }
    if healthy {
        Ok(())
    } else {
        anyhow::bail!("doctor found one or more problems")
    }
}
