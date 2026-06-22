use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64},
};
use std::time::Instant;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    activity::ActivityReporter,
    agent::{AgentEvent, AgentMode, ApprovalDecision, TurnOptions, run_turn},
    compaction::CompactionState,
    config::{Config, OutputFormat},
    goal::GoalState,
    provider::Provider,
    ralph::{RalphLoop, RalphStatus},
    services::AgentServices,
    session::{Session, SessionStore},
    task::TaskList,
};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: Config,
    format: OutputFormat,
    messages: Vec<Value>,
    session: Option<Session>,
    store: Option<SessionStore>,
    services: Arc<AgentServices>,
    loop_config: Option<RalphLoop>,
    reporter: Option<ActivityReporter>,
) -> Result<()> {
    let initial_tokens = session
        .as_ref()
        .map(|session| session.tokens_used)
        .unwrap_or(0);
    let provider = Provider::with_tokens(&config, Arc::new(AtomicU64::new(initial_tokens)))?;
    let session_id = session.as_ref().map(|session| session.id.to_string());
    services
        .run_hooks(
            "session_start",
            session_id.as_deref(),
            &json!({"workspace":config.workspace,"mode":"headless"}),
        )
        .await?;
    let started = Instant::now();
    let activity_session = session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if let Some(reporter) = &reporter {
        reporter
            .report_start(&activity_session, &config.model)
            .await;
    }
    // Keep long-running headless sessions (e.g. loops) visible with live tokens,
    // and let them drop off "active" if the process is killed.
    let heartbeat = reporter.clone().map(|reporter| {
        let provider = provider.clone();
        let session = activity_session.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
                crate::activity::HEARTBEAT_INTERVAL_SECS,
            ));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                reporter
                    .report_heartbeat(&session, provider.tokens_used())
                    .await;
            }
        })
    });
    let (events, mut receiver) = mpsc::unbounded_channel();
    let allow = Arc::new(AtomicBool::new(config.yes));
    let goal = GoalState::new(session.as_ref().and_then(|session| session.goal.clone()));
    let tasks = TaskList::new(
        session
            .as_ref()
            .map(|session| session.tasks.clone())
            .unwrap_or_default(),
    );
    let compaction = session
        .as_ref()
        .and_then(|session| session.compaction.clone())
        .unwrap_or_default();

    let mut ralph = loop_config;
    let mut text = String::new();
    let mut final_messages = messages;
    let mut failure: Option<String> = None;

    // Loop mode drives its own prompt replay; non-loop mode expects the caller to
    // have already pushed the user message onto `final_messages`.
    if let Some(state) = ralph.as_mut()
        && let Err(error) = state.begin_iteration()
    {
        failure = Some(format!("{error:#}"));
    } else if let Some(state) = ralph.as_ref() {
        final_messages.push(json!({"role": "user", "content": state.prompt.clone()}));
    }

    let mut current_task = if failure.is_none() {
        Some(tokio::spawn(run_turn(
            provider.clone(),
            final_messages.clone(),
            turn_options(
                &config,
                &allow,
                &services,
                &goal,
                &tasks,
                &compaction,
                session_id.clone(),
            ),
            events.clone(),
        )))
    } else {
        None
    };

    if failure.is_none() {
        'outer: loop {
            let mut next_messages: Option<Vec<Value>> = None;
            while let Some(event) = receiver.recv().await {
                match event {
                    AgentEvent::Delta(delta) => {
                        text.push_str(&delta);
                        match format {
                            OutputFormat::Plain => {
                                print!("{delta}");
                                io::stdout().flush()?;
                            }
                            OutputFormat::StreamingJson => emit(json!({
                                "type": "assistant.delta",
                                "text": delta
                            }))?,
                            OutputFormat::Json => {}
                        }
                    }
                    AgentEvent::Approval(request) => {
                        let tool = request.tool.clone();
                        let summary = request.summary.clone();
                        let _ = request.respond.send(ApprovalDecision::Reject);
                        match format {
                            OutputFormat::Plain => eprintln!(
                                "\n[rejected {tool}: {summary}; use --always-approve for headless mutations]"
                            ),
                            OutputFormat::StreamingJson => emit(json!({
                                "type": "approval.rejected",
                                "tool": tool,
                                "summary": summary
                            }))?,
                            OutputFormat::Json => {}
                        }
                    }
                    AgentEvent::ToolStarted { name, summary } => match format {
                        OutputFormat::Plain => eprintln!("\n[{name}: {summary}]"),
                        OutputFormat::StreamingJson => emit(json!({
                            "type": "tool.started",
                            "tool": name,
                            "summary": summary
                        }))?,
                        OutputFormat::Json => {}
                    },
                    AgentEvent::ToolFinished { name, output } => {
                        if format == OutputFormat::StreamingJson {
                            emit(json!({
                                "type": "tool.finished",
                                "tool": name,
                                "output": output
                            }))?;
                        }
                    }
                    AgentEvent::ModeChanged { mode, reason } => match format {
                        OutputFormat::Plain => eprintln!("\n[mode: {} · {reason}]", mode.label()),
                        OutputFormat::StreamingJson => emit(json!({
                            "type": "mode.changed",
                            "mode": mode.label().to_ascii_lowercase(),
                            "reason": reason
                        }))?,
                        OutputFormat::Json => {}
                    },
                    AgentEvent::Done { messages } => {
                        final_messages = messages;
                        if let Some(state) = ralph.as_mut() {
                            let completed =
                                state.observe_output(&latest_assistant_text(&final_messages));
                            if format == OutputFormat::Plain {
                                if completed {
                                    eprintln!(
                                        "\n[loop completed after {} iteration(s)]",
                                        state.iteration
                                    );
                                } else if state.status == RalphStatus::MaxIterations {
                                    eprintln!(
                                        "\n[loop stopped at {} iteration(s)]",
                                        state.iteration
                                    );
                                }
                            }
                            if state.is_active() {
                                match state.begin_iteration() {
                                    Ok(iteration) => {
                                        if format == OutputFormat::Plain {
                                            eprintln!("\n[loop · iteration {iteration}]");
                                        }
                                        let mut messages = final_messages.clone();
                                        messages.push(json!({"role": "user", "content": state.prompt.clone()}));
                                        next_messages = Some(messages);
                                    }
                                    Err(error) => {
                                        if format == OutputFormat::Plain {
                                            eprintln!("\n[loop stopped: {error}]");
                                        }
                                    }
                                }
                            }
                        }
                        break;
                    }
                    AgentEvent::Failed { error, messages } => {
                        final_messages = messages;
                        failure = Some(error.clone());
                        if let Some(state) = ralph.as_mut() {
                            let _ = state.pause();
                        }
                        if format == OutputFormat::Plain && ralph.is_some() {
                            eprintln!("\n[loop paused after failure]");
                        }
                        break;
                    }
                }
            }

            if let Some(messages) = next_messages {
                final_messages = messages.clone();
                current_task = Some(tokio::spawn(run_turn(
                    provider.clone(),
                    messages,
                    turn_options(
                        &config,
                        &allow,
                        &services,
                        &goal,
                        &tasks,
                        &compaction,
                        session_id.clone(),
                    ),
                    events.clone(),
                )));
                continue 'outer;
            }
            break 'outer;
        }
    }

    if let Some(task) = current_task {
        let _ = task.await;
    }

    let session_id = persist_session(
        session,
        store,
        PersistedRun {
            messages: final_messages,
            goal: &goal,
            tasks: &tasks,
            compaction: &compaction,
            ralph: &ralph,
            profile: &config.profile,
            model: &config.model,
            tokens_used: provider.tokens_used(),
            active_secs: started.elapsed().as_secs(),
        },
    )?;
    if let Err(error) = services
        .run_hooks(
            "session_end",
            session_id.as_deref(),
            &json!({
                "workspace":config.workspace,
                "mode":"headless",
                "status":if failure.is_some() { "failed" } else { "completed" }
            }),
        )
        .await
    {
        eprintln!("warning: session_end hook failed: {error:#}");
    }
    if let Some(handle) = heartbeat {
        handle.abort();
    }
    if let Some(reporter) = &reporter {
        reporter
            .report_end(
                &activity_session,
                provider.tokens_used(),
                started.elapsed().as_secs(),
            )
            .await;
    }
    if format == OutputFormat::Plain && !text.ends_with('\n') {
        println!();
    }

    if let Some(error) = failure {
        match format {
            OutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": false,
                    "error": error,
                    "text": text,
                    "session_id": session_id
                }))?
            ),
            OutputFormat::StreamingJson => emit(json!({
                "type": "error",
                "error": error,
                "session_id": session_id
            }))?,
            OutputFormat::Plain => eprintln!("error: {error}"),
        }
        bail!(error);
    }

    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "text": text,
                "session_id": session_id
            }))?
        ),
        OutputFormat::StreamingJson => emit(json!({
            "type": "done",
            "session_id": session_id
        }))?,
        OutputFormat::Plain => {}
    }
    Ok(())
}

struct PersistedRun<'a> {
    messages: Vec<Value>,
    goal: &'a GoalState,
    tasks: &'a TaskList,
    compaction: &'a CompactionState,
    ralph: &'a Option<RalphLoop>,
    profile: &'a str,
    model: &'a str,
    tokens_used: u64,
    active_secs: u64,
}

fn persist_session(
    mut session: Option<Session>,
    store: Option<SessionStore>,
    run: PersistedRun<'_>,
) -> Result<Option<String>> {
    let Some(store) = store else {
        return Ok(None);
    };
    let mut session_value = if let Some(session_value) = session.take() {
        session_value
    } else {
        store.create(
            run.profile.to_owned(),
            run.model.to_owned(),
            run.messages.clone(),
        )?
    };
    session_value.update_messages(run.messages);
    session_value.goal = run.goal.snapshot();
    session_value.tasks = run.tasks.snapshot();
    session_value.compaction = Some(run.compaction.clone());
    session_value.ralph_loop = run.ralph.clone();
    session_value.tokens_used = run.tokens_used;
    session_value.active_secs = session_value.active_secs.saturating_add(run.active_secs);
    store.save(&session_value)?;
    Ok(Some(session_value.id.to_string()))
}

fn turn_options(
    config: &Config,
    allow: &Arc<AtomicBool>,
    services: &Arc<AgentServices>,
    goal: &GoalState,
    tasks: &TaskList,
    compaction: &CompactionState,
    session_id: Option<String>,
) -> TurnOptions {
    TurnOptions {
        workspace: config.workspace.clone(),
        max_steps: config.max_steps,
        tool_output_limit: config.tool_output_limit,
        mode: AgentMode::Auto,
        allow_mutations: allow.clone(),
        services: services.clone(),
        session_id,
        goal: goal.clone(),
        tasks: tasks.clone(),
        compaction: compaction.clone(),
        compaction_budget: config.model_limits.compaction_budget(),
        allow_subagents: true,
        web_search: config.web_search.clone(),
    }
}

fn latest_assistant_text(messages: &[Value]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant" && message["content"].is_string())
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn emit(value: Value) -> Result<()> {
    println!("{}", serde_json::to_string(&value)?);
    io::stdout().flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AbacusPaths;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn headless_persistence_creates_session_with_usage_totals() {
        let directory = tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = SessionStore::new(
            &AbacusPaths::under(directory.path().join("home")),
            workspace.canonicalize().unwrap(),
        );
        let messages = vec![
            json!({"role":"system","content":"x"}),
            json!({"role":"user","content":"count this"}),
            json!({"role":"assistant","content":"done"}),
        ];

        let id = persist_session(
            None,
            Some(store.clone()),
            PersistedRun {
                messages,
                goal: &GoalState::default(),
                tasks: &TaskList::default(),
                compaction: &CompactionState::default(),
                ralph: &None,
                profile: "local",
                model: "model",
                tokens_used: 150_000_000,
                active_secs: 42,
            },
        )
        .unwrap()
        .unwrap();

        let loaded = store.load(&id[..8]).unwrap();
        assert_eq!(loaded.tokens_used, 150_000_000);
        assert_eq!(loaded.active_secs, 42);
        assert_eq!(loaded.title, "count this");
    }

    #[test]
    fn headless_persistence_keeps_resumed_usage_cumulative() {
        let directory = tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = SessionStore::new(
            &AbacusPaths::under(directory.path().join("home")),
            workspace.canonicalize().unwrap(),
        );
        let mut session = store
            .create(
                "local".into(),
                "old-model".into(),
                vec![json!({"role":"system","content":"x"})],
            )
            .unwrap();
        session.tokens_used = 12_000;
        session.active_secs = 30;
        store.save(&session).unwrap();

        let id = persist_session(
            Some(session),
            Some(store.clone()),
            PersistedRun {
                messages: vec![
                    json!({"role":"system","content":"x"}),
                    json!({"role":"user","content":"continue"}),
                ],
                goal: &GoalState::default(),
                tasks: &TaskList::default(),
                compaction: &CompactionState::default(),
                ralph: &None,
                profile: "ignored",
                model: "ignored",
                tokens_used: 15_500,
                active_secs: 10,
            },
        )
        .unwrap()
        .unwrap();

        let loaded = store.load(&id[..8]).unwrap();
        assert_eq!(loaded.tokens_used, 15_500);
        assert_eq!(loaded.active_secs, 40);
        assert_eq!(loaded.profile, "local");
        assert_eq!(loaded.model, "old-model");
    }
}
