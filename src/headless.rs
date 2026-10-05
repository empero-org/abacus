use std::io::{self, Write};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Instant;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    activity::ActivityReporter,
    agent::{AgentEvent, ApprovalDecision, TurnOptions, run_turn},
    config::{AbacusPaths, Config, OutputFormat},
    provider::Provider,
    ralph::{RalphLoop, RalphStatus},
    services::AgentServices,
    session::{Session, SessionState, SessionStore},
    usage::UsageReporter,
};

/// How long a headless run waits on session sync at either end.
const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long the final usage report may hold up the exit.
const USAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Download what changed on other devices. Called before the run reads the
/// session it resumes, so it continues the newest copy: a pull after the read
/// would have the run build on the older copy, and its upload at the end would
/// replace the other device's turns. Bounded: a scripted run must not hang on
/// an unreachable sync server.
pub async fn pull_before_run(paths: &AbacusPaths) {
    let credentials = crate::config::Credentials::load(paths).unwrap_or_default();
    if crate::sync::is_configured(&credentials) {
        let _ = tokio::time::timeout(SYNC_TIMEOUT, crate::sync::pull_changes(paths)).await;
    }
}

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
    let initial_tokens = session.as_ref().map(|session| session.tokens_used).unwrap_or(0);
    let tokens = Arc::new(crate::provider::TokenLedger::new(initial_tokens));
    let provider = Provider::with_tokens(&config, tokens.clone())?;
    let session_id = session.as_ref().map(|session| session.id.to_string());
    services
        .run_hooks("session_start", session_id.as_deref(), &json!({"workspace":config.workspace,"mode":"headless"}))
        .await?;
    let started = Instant::now();
    let activity_session = session_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let credentials = crate::config::Credentials::load(&config.paths).unwrap_or_default();
    if let Some(reporter) = &reporter {
        reporter.report_start(&activity_session, &config.model).await;
    }
    let heartbeat = reporter.as_ref().map(|reporter| reporter.heartbeat(activity_session.clone(), tokens.clone()));
    let (events, mut receiver) = mpsc::unbounded_channel();
    let allow = Arc::new(AtomicBool::new(config.yes));
    // Keyed by the session id so promotion counts distinct sessions rather
    // than distinct processes.
    let state = SessionState::open(&config, session.as_ref(), activity_session.clone());

    let mut ralph = loop_config;
    let mut text = String::new();
    let mut final_messages = messages;

    // A headless run normally has no session until it finishes, so there is no
    // id to key a trace on while the turn is running. Since the run is going to
    // save one anyway, create it up front when tracing — the alternative is
    // recording nothing at all for exactly the runs most worth recording.
    let mut session = session;
    if config.trace_enabled
        && session.is_none()
        && let Some(store) = &store
    {
        match store.create(config.profile.clone(), config.model.clone(), final_messages.clone()) {
            Ok(created) => session = Some(created),
            Err(error) => eprintln!("warning: could not create session — {error:#}"),
        }
    }
    let session_id = session.as_ref().map(|session| session.id.to_string());
    // Opened only now that a trace may have created the session, so the usage
    // is filed under the id the session ends up with.
    let usage = UsageReporter::new(&config.paths, &credentials, "headless");
    if let Some(usage) = &usage {
        usage.open_session(session_id.as_deref().unwrap_or(&activity_session), &config.model, tokens.clone());
    }
    let usage_task = usage.as_ref().map(UsageReporter::spawn_periodic);
    let trace = match (config.trace_enabled, session_id.as_deref()) {
        (true, Some(id)) => match crate::sft::TraceWriter::open(&config.paths.traces_dir, id) {
            Ok(writer) => Some(writer),
            Err(error) => {
                eprintln!("warning: training trace disabled — {error:#}");
                None
            }
        },
        _ => None,
    };

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

    let start = |messages: Vec<Value>| {
        let options = turn_options(&config, &allow, &services, &state, session_id.clone(), trace.clone());
        tokio::spawn(run_turn(provider.clone(), messages, options, events.clone()))
    };
    let mut current_task = failure.is_none().then(|| start(final_messages.clone()));

    while failure.is_none()
        && let Some(event) = receiver.recv().await
    {
        match event {
            AgentEvent::Delta(delta) => {
                text.push_str(&delta);
                match format {
                    OutputFormat::Plain => {
                        print!("{delta}");
                        io::stdout().flush()?;
                    }
                    OutputFormat::StreamingJson => emit(json!({"type": "assistant.delta", "text": delta}))?,
                    OutputFormat::Json => {}
                }
            }
            AgentEvent::Approval(request) => {
                let _ = request.respond.send(ApprovalDecision::Reject);
                let (tool, summary) = (request.tool, request.summary);
                announce(
                    format,
                    &format!("rejected {tool}: {summary}; use --always-approve for headless mutations"),
                    json!({"type": "approval.rejected", "tool": tool, "summary": summary}),
                )?;
            }
            AgentEvent::UserQuestion(request) => {
                // Headless mode can't show a modal — auto-pick the first
                // option so the agent loop can continue without blocking.
                let first = request.options.first().filter(|option| !option.is_empty()).cloned();
                let _ = request
                    .respond
                    .send(crate::agent::UserAnswer { selected_labels: first.into_iter().collect(), custom_text: None });
                announce(
                    format,
                    &format!("auto-answered question: {}", request.header),
                    json!({"type": "user_question.auto_answered", "header": request.header}),
                )?;
            }
            AgentEvent::ToolStarted { name, summary } => announce(
                format,
                &format!("{name}: {summary}"),
                json!({"type": "tool.started", "tool": name, "summary": summary}),
            )?,
            AgentEvent::ToolFinished { name, output } => {
                if format == OutputFormat::StreamingJson {
                    emit(json!({"type": "tool.finished", "tool": name, "output": output}))?;
                }
            }
            AgentEvent::ModeChanged { mode, reason } => announce(
                format,
                &format!("mode: {} · {reason}", mode.label()),
                json!({
                    "type": "mode.changed",
                    "mode": mode.label().to_ascii_lowercase(),
                    "reason": reason
                }),
            )?,
            AgentEvent::Done { messages, .. } => {
                final_messages = messages;
                let Some(state) = ralph.as_mut() else { break };
                if state.observe_output(crate::text::last_reply(&final_messages)) {
                    aside(format, &format!("loop completed after {} iteration(s)", state.iteration));
                } else if state.status == RalphStatus::MaxIterations {
                    aside(format, &format!("loop stopped at {} iteration(s)", state.iteration));
                }
                if !state.is_active() {
                    break;
                }
                match state.begin_iteration() {
                    Ok(iteration) => {
                        aside(format, &format!("loop · iteration {iteration}"));
                        final_messages.push(json!({"role": "user", "content": state.prompt.clone()}));
                        current_task = Some(start(final_messages.clone()));
                    }
                    Err(error) => {
                        aside(format, &format!("loop stopped: {error}"));
                        break;
                    }
                }
            }
            // Headless output is the answer, not the deliberation.
            AgentEvent::Reasoning(_) => {}
            AgentEvent::Notice(notice) => eprintln!("note: {notice}"),
            AgentEvent::TraceFailed { error } => {
                eprintln!("warning: training trace disabled — {error}");
            }
            AgentEvent::Failed { error, messages } => {
                final_messages = messages;
                failure = Some(error);
                if let Some(state) = ralph.as_mut() {
                    let _ = state.pause();
                    aside(format, "loop paused after failure");
                }
            }
        }
    }
    if let Some(task) = current_task {
        let _ = task.await;
    }

    if let Some(task) = usage_task {
        task.abort();
    }
    let saved = persist_session(
        session,
        store,
        PersistedRun {
            messages: final_messages,
            state: &state,
            ralph: &ralph,
            profile: &config.profile,
            model: &config.model,
            tokens_used: provider.tokens_used(),
            active_secs: started.elapsed().as_secs(),
        },
    );
    // Awaited, not spawned: the process exits right after, which would cancel
    // a background upload. The two go out together so neither adds to the
    // other's wait, and the usage is reported even when the save failed.
    let upload = async {
        if let Ok(Some(session)) = &saved {
            let _ = tokio::time::timeout(SYNC_TIMEOUT, crate::sync::push_session(&config.paths, session)).await;
        }
    };
    let report = async {
        if let Some(usage) = &usage {
            usage.finish(USAGE_TIMEOUT).await;
        }
    };
    tokio::join!(upload, report);
    let saved = saved?;
    let session_id = saved.as_ref().map(|session| session.id.to_string());
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
        reporter.report_end(&activity_session, provider.tokens_used(), started.elapsed().as_secs()).await;
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
    state: &'a SessionState,
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
) -> Result<Option<Session>> {
    let Some(store) = store else {
        return Ok(None);
    };
    let mut session_value = if let Some(session_value) = session.take() {
        session_value
    } else {
        store.create(run.profile.to_owned(), run.model.to_owned(), run.messages.clone())?
    };
    session_value.update_messages(run.messages);
    run.state.save(&mut session_value);
    session_value.ralph_loop = run.ralph.clone();
    session_value.tokens_used = run.tokens_used;
    session_value.active_secs = session_value.active_secs.saturating_add(run.active_secs);
    store.save(&session_value)?;
    Ok(Some(session_value))
}

fn turn_options(
    config: &Config,
    allow: &Arc<AtomicBool>,
    services: &Arc<AgentServices>,
    state: &SessionState,
    session_id: Option<String>,
    trace: Option<crate::sft::TraceWriter>,
) -> TurnOptions {
    // A headless run defaults to AUTO so the model chooses; `--mode` pins it,
    // which is what makes a read-only CI check expressible.
    TurnOptions {
        trace,
        allow_mutations: allow.clone(),
        session_id,
        ..state.turn(config, services.clone()).with_workspace_stores(config)
    }
}

/// A bracketed aside on stderr, in the plain format only.
fn aside(format: OutputFormat, text: &str) {
    if format == OutputFormat::Plain {
        eprintln!("\n[{text}]");
    }
}

/// Reports something that happened alongside the answer: an aside in the plain
/// format, an event line when streaming, nothing when only the result is wanted.
fn announce(format: OutputFormat, text: &str, event: Value) -> Result<()> {
    aside(format, text);
    if format == OutputFormat::StreamingJson {
        emit(event)?;
    }
    Ok(())
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
        let store =
            SessionStore::new(&AbacusPaths::under(directory.path().join("home")), workspace.canonicalize().unwrap());
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
                state: &SessionState::default(),
                ralph: &None,
                profile: "local",
                model: "model",
                tokens_used: 150_000_000,
                active_secs: 42,
            },
        )
        .unwrap()
        .unwrap()
        .id
        .to_string();

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
        let store =
            SessionStore::new(&AbacusPaths::under(directory.path().join("home")), workspace.canonicalize().unwrap());
        let mut session =
            store.create("local".into(), "old-model".into(), vec![json!({"role":"system","content":"x"})]).unwrap();
        session.tokens_used = 12_000;
        session.active_secs = 30;
        store.save(&session).unwrap();

        let id = persist_session(
            Some(session),
            Some(store.clone()),
            PersistedRun {
                messages: vec![json!({"role":"system","content":"x"}), json!({"role":"user","content":"continue"})],
                state: &SessionState::default(),
                ralph: &None,
                profile: "ignored",
                model: "ignored",
                tokens_used: 15_500,
                active_secs: 10,
            },
        )
        .unwrap()
        .unwrap()
        .id
        .to_string();

        let loaded = store.load(&id[..8]).unwrap();
        assert_eq!(loaded.tokens_used, 15_500);
        assert_eq!(loaded.active_secs, 40);
        assert_eq!(loaded.profile, "local");
        assert_eq!(loaded.model, "old-model");
    }
}
