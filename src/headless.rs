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
    goal: Option<String>,
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
    let yolo = Arc::new(AtomicBool::new(config.permission == crate::config::Permission::Yolo));
    // Keyed by the session id so promotion counts distinct sessions rather
    // than distinct processes.
    let state = SessionState::open(&config, session.as_ref());

    // A goal run carries on, turn after turn, until the goal is no longer
    // active; any other run is the one turn.
    let goal_run = goal.is_some();
    if let Some(objective) = &goal {
        state.goal.create(objective)?;
    }
    let mut worked = false;
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
        match store.create(config.profile.clone(), config.model_ref(), final_messages.clone()) {
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

    // The caller has already pushed the prompt onto `final_messages`.
    let start = |messages: Vec<Value>| {
        let options = turn_options(&config, &yolo, &services, &state, session_id.clone(), trace.clone());
        tokio::spawn(run_turn(provider.clone(), messages, options, events.clone()))
    };
    let mut current_task = Some(start(final_messages.clone()));

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
            // AUTO asks only when its reviewer could not decide, and there is
            // nobody here to answer.
            AgentEvent::Approval(request) => {
                let _ = request.respond.send(ApprovalDecision::Reject);
                let (tool, summary) = (request.tool, request.summary);
                announce(
                    format,
                    &format!("not run, the safety review could not decide: {tool}: {summary} (--yolo runs everything)"),
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
            AgentEvent::ToolStarted { name, summary } => {
                worked = true;
                announce(
                    format,
                    &format!("{name}: {summary}"),
                    json!({"type": "tool.started", "tool": name, "summary": summary}),
                )?
            }
            AgentEvent::ToolFinished { name, output, refused } => {
                if format == OutputFormat::StreamingJson {
                    emit(json!({"type": "tool.finished", "tool": name, "output": output, "refused": refused}))?;
                }
            }
            AgentEvent::Done { messages, .. } => {
                final_messages = messages;
                // The model ended its turn to be woken by a timer it set: the
                // run is not over while one waits. A goal carries on once no
                // timer does.
                if let Some(fired) = next_timer(&state, format).await {
                    announce(
                        format,
                        &fired.shown(),
                        json!({"type": "timer.fired", "id": fired.id, "after": fired.after, "note": fired.note}),
                    )?;
                    final_messages.push(json!({"role": "user", "content": fired.prompt()}));
                    current_task = Some(start(final_messages.clone()));
                    continue;
                }
                if !goal_run {
                    break;
                }
                match state.goal.after_turn(std::mem::take(&mut worked)) {
                    Some(crate::goal::Next::Turn(prompt)) => {
                        aside(format, "goal not done yet, carrying on");
                        final_messages.push(json!({"role": "user", "content": prompt}));
                        current_task = Some(start(final_messages.clone()));
                    }
                    Some(crate::goal::Next::Stalled) => {
                        aside(format, "goal paused: two turns in a row ended without doing any work");
                        break;
                    }
                    None => {
                        if let Some(goal) = state.goal.snapshot() {
                            aside(format, &format!("goal {:?} after {} turn(s)", goal.status, goal.iterations));
                        }
                        break;
                    }
                }
            }
            // Headless output is the answer, not the deliberation.
            AgentEvent::Reasoning(_) => {}
            AgentEvent::Notice(notice) => eprintln!("note: {notice}"),
            AgentEvent::SubagentReport(_) => eprintln!("note: {}", crate::agent::REPORT_NOTICE),
            AgentEvent::AgentMessage { from, text } => {
                eprintln!("note: {}", crate::agent::message_notice(&from, &text))
            }
            AgentEvent::MessageSent { .. } | AgentEvent::ToolDescribed { .. } => {}
            AgentEvent::PeerMessage(message) => eprintln!("message {}: {}", message.origin(), message.text),
            AgentEvent::TimerFired(fired) => eprintln!("note: {}", fired.shown()),
            AgentEvent::TraceFailed { error } => {
                eprintln!("warning: training trace disabled — {error}");
            }
            AgentEvent::Failed { error, messages } => {
                final_messages = messages;
                failure = Some(error);
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
            profile: &config.profile,
            model: &config.model_ref(),
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

    // A goal run says where its goal ended up: complete, or paused.
    let goal_status = goal_run.then(|| state.goal.snapshot().map(|goal| goal.status)).flatten();
    match format {
        OutputFormat::Json => {
            let mut result = json!({
                "ok": true,
                "text": text,
                "session_id": session_id
            });
            if let Some(status) = goal_status {
                result["goal"] = json!(status);
            }
            println!("{}", serde_json::to_string_pretty(&result)?)
        }
        OutputFormat::StreamingJson => {
            let mut done = json!({
                "type": "done",
                "session_id": session_id
            });
            if let Some(status) = goal_status {
                done["goal"] = json!(status);
            }
            emit(done)?
        }
        OutputFormat::Plain => {}
    }
    Ok(())
}

struct PersistedRun<'a> {
    messages: Vec<Value>,
    state: &'a SessionState,
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
    session_value.tokens_used = run.tokens_used;
    session_value.active_secs = session_value.active_secs.saturating_add(run.active_secs);
    store.save(&session_value)?;
    Ok(Some(session_value))
}

fn turn_options(
    config: &Config,
    yolo: &Arc<AtomicBool>,
    services: &Arc<AgentServices>,
    state: &SessionState,
    session_id: Option<String>,
    trace: Option<crate::sft::TraceWriter>,
) -> TurnOptions {
    // AUTO unless `--yolo`: what the reviewer clears runs, the rest is
    // refused, and nothing waits on a person who is not there.
    // A run with a session can list, read and message the others. Nothing
    // reads its own inbox: a one-shot run is not there to answer, so messages
    // to it wait for the terminal or the desktop to open the session.
    let peers = match (config.no_session, session_id.as_deref().and_then(|id| id.parse().ok())) {
        (false, Some(id)) => crate::peers::Peers::new(config.paths.clone(), config.workspace.clone(), Some(id)),
        _ => Default::default(),
    };
    TurnOptions { trace, yolo: yolo.clone(), session_id, peers, ..state.turn(config, services.clone()) }
}

/// The next timer the model set, once it comes due; None when none waits.
async fn next_timer(state: &SessionState, format: OutputFormat) -> Option<crate::timers::Fired> {
    let mut told = false;
    loop {
        // Pending first: a timer gone from there is already in the queue.
        let due = state.timers.next_due();
        if let Some(fired) = state.injections.take_timer() {
            return Some(fired);
        }
        let due = due?;
        if !told {
            told = true;
            let count = state.timers.pending();
            let what = if count == 1 { "a timer".to_owned() } else { format!("{count} timers") };
            aside(
                format,
                &format!(
                    "waiting for {what}, the next due in {}",
                    crate::timers::span(due.as_secs_f64().ceil() as u64)
                ),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
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
