//! Account-tied token usage. A signed-in install tells the Abacus server how
//! many tokens it spends, per model, so the account page and the admin overview
//! can show usage without anyone reading a transcript. A report holds counters
//! and identifiers only — never a prompt, a file, a path or any transcript text.
//!
//! Counters are **cumulative per (run, model)**. A run is one process working on
//! one session. The server keeps the latest figures for each `(run_id, model)`
//! and works out the delta itself, so a retried, duplicated or reordered report
//! can never count twice, and a lost one is simply covered by the next.
//!
//! The reporter is strictly best-effort, like [`crate::activity`]: every request
//! is bounded, nothing is ever printed, and a failure only means the next
//! report carries more. It exists only while the install is signed in to Abacus
//! Sync, so a signed-out install behaves exactly as before. `ABACUS_NO_USAGE=1`
//! turns it off for scripts. The anonymous activity pings are separate and
//! unchanged.
//!
//! One model per ledger is not guaranteed: `/model` and `/profile` switch the
//! provider while the session's [`TokenLedger`] keeps counting. The reporter
//! samples the ledger and credits each delta to the model that was current when
//! it was sampled, so a front end calls [`UsageReporter::note_model`] right
//! before it switches. Within one sampling interval that is approximate, which
//! is acceptable for a usage figure.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::{AbacusPaths, Credentials, device_name};
use crate::provider::{TokenLedger, TokenUsage};
use crate::sync::{SyncClient, SyncError};

/// How often an open session reports. Well inside the server's two-minute
/// "active now" window, so an open session stays visible.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// How long one report may take. Reports are cumulative, so a request that
/// does not finish is superseded by the next one rather than waited for.
const REPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// The server accepts 1 to 50 items per request. A run uses one to three
/// models, so the cap only exists to keep a request valid.
const MAX_REPORTS: usize = 50;

/// Model ids are short; the server stores up to 500 characters.
const MAX_MODEL_CHARS: usize = 200;

/// Token counters of one `(run, model)`, or the difference between two ledger
/// readings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counters {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

impl Counters {
    fn of(usage: &TokenUsage) -> Self {
        Self { input: usage.input, output: usage.output, cache_read: usage.cache_read, cache_write: usage.cache_write }
    }

    /// What was added since `earlier`. A counter that went down means the
    /// ledger was replaced by a fresh one; it contributes nothing rather than
    /// a negative number.
    fn since(self, earlier: Self) -> Self {
        Self {
            input: self.input.saturating_sub(earlier.input),
            output: self.output.saturating_sub(earlier.output),
            cache_read: self.cache_read.saturating_sub(earlier.cache_read),
            cache_write: self.cache_write.saturating_sub(earlier.cache_write),
        }
    }

    fn add(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
    }

    fn is_zero(self) -> bool {
        self == Self::default()
    }

    /// Input plus output: cached input is already part of the input figure.
    /// The ledger's own total is not used because it is rewritten when a
    /// session is resumed or replaced, which would make it go backwards.
    fn total(self) -> u64 {
        self.input.saturating_add(self.output)
    }
}

/// One model's cumulative counters within a run.
struct Entry {
    counters: Counters,
    /// Changed since it was last sent (or never sent).
    unsent: bool,
}

/// One process working on one session. A resumed or replaced session starts a
/// new run, so cumulative counters never span two of them.
struct Run {
    run_id: String,
    session_id: String,
    ledger: Arc<TokenLedger>,
    /// The model the next ledger delta is credited to.
    model: String,
    /// The ledger as it read at the last sample.
    sampled: Counters,
    models: BTreeMap<String, Entry>,
    /// Increases with every report of the run, across all its models.
    seq: u64,
}

impl Run {
    /// Start a run. The ledger may already hold figures, for example a
    /// session being resumed in a long-lived process, and those were spent
    /// before this run: they are the baseline, not usage.
    fn new(session_id: &str, model: &str, ledger: Arc<TokenLedger>) -> Self {
        Self {
            run_id: Uuid::new_v4().to_string(),
            session_id: session_id.to_owned(),
            sampled: Counters::of(&ledger.snapshot()),
            ledger,
            model: public_model_name(model),
            models: BTreeMap::new(),
            seq: 0,
        }
    }

    /// Credit whatever the ledger gained since the last sample to the current
    /// model.
    fn sample(&mut self) {
        let now = Counters::of(&self.ledger.snapshot());
        let gained = now.since(self.sampled);
        self.sampled = now;
        if gained.is_zero() {
            return;
        }
        let entry =
            self.models.entry(self.model.clone()).or_insert(Entry { counters: Counters::default(), unsent: true });
        entry.counters.add(gained);
        entry.unsent = true;
    }

    fn switch_model(&mut self, model: &str) {
        self.sample();
        self.model = public_model_name(model);
    }

    fn has_unsent(&self) -> bool {
        self.models.values().any(|entry| entry.unsent)
    }

    /// Mark everything unsent so the next report goes out even though no
    /// tokens were spent: it tells the server the session is still open.
    fn keepalive(&mut self) {
        for entry in self.models.values_mut() {
            entry.unsent = true;
        }
    }
}

/// Who is reporting. Fixed for the life of the process.
struct Identity {
    install_id: String,
    kind: &'static str,
    name: String,
    os: &'static str,
    arch: &'static str,
    app_version: &'static str,
}

#[derive(Default)]
struct Inner {
    run: Option<Run>,
    /// Why reporting stopped for good: the server does not take reports from
    /// this install (signed out, no such endpoint). Retrying every minute
    /// would only add load, so nothing more is sent until the next start.
    stopped: Option<String>,
}

/// A report on its way, with what is needed to take it back if it is lost.
struct Outgoing {
    body: Value,
    run_id: String,
    models: Vec<String>,
}

/// Reports a signed-in install's token usage to the account. Cheap to clone;
/// every clone shares one state.
#[derive(Clone)]
pub struct UsageReporter {
    client: SyncClient,
    identity: Arc<Identity>,
    inner: Arc<Mutex<Inner>>,
}

impl UsageReporter {
    /// A reporter for `kind` (`"tui"`, `"headless"` or `"app-server"`), or
    /// `None` when the install is signed out or `ABACUS_NO_USAGE` is set. A
    /// `None` reporter makes every call site a no-op, as with
    /// [`crate::activity::ActivityReporter`].
    pub fn new(paths: &AbacusPaths, credentials: &Credentials, kind: &'static str) -> Option<Self> {
        if opted_out(std::env::var_os("ABACUS_NO_USAGE").as_deref()) {
            return None;
        }
        let sync = credentials.sync.as_ref().filter(|sync| !sync.token.trim().is_empty())?;
        let client = SyncClient::new(sync).ok()?.with_home(paths);
        let install_id = client.install_id.clone();
        Some(Self::with_client(
            client,
            Identity {
                install_id,
                kind,
                name: device_name().chars().take(100).collect(),
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
                app_version: env!("CARGO_PKG_VERSION"),
            },
        ))
    }

    fn with_client(client: SyncClient, identity: Identity) -> Self {
        Self { client, identity: Arc::new(identity), inner: Arc::default() }
    }

    /// Begin a new run for `session_id` on `model`, counting from the
    /// ledger's current reading. Call it when a session opens, and again when
    /// another is resumed or started in the same process. A run still open is
    /// reported one last time in the background first.
    pub fn open_session(&self, session_id: &str, model: &str, ledger: Arc<TokenLedger>) {
        let mut inner = self.lock();
        let previous = inner.run.replace(Run::new(session_id, model, ledger));
        let closing = match previous {
            Some(mut previous) if inner.stopped.is_none() => self.outgoing(&mut previous, true),
            _ => None,
        };
        drop(inner);
        self.spawn_delivery(closing);
    }

    /// Credit what has been spent so far to the current model, and credit what
    /// follows to `model`. Call it right before switching model or profile.
    pub fn note_model(&self, model: &str) {
        if let Some(run) = self.lock().run.as_mut() {
            run.switch_model(model);
        }
    }

    /// Send the run's counters if they changed since the last report. With
    /// `final_` the run is closed instead: every model is sent once more,
    /// marked final, and the reporter is idle until the next
    /// [`UsageReporter::open_session`]. Never fails and never takes longer
    /// than the request timeout.
    pub async fn report(&self, final_: bool) {
        if let Some(outgoing) = self.prepare(final_) {
            self.deliver(outgoing).await;
        }
    }

    /// [`UsageReporter::report`] in the background, for callers that must not
    /// wait (the end of a turn).
    pub fn spawn_report(&self, final_: bool) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let reporter = self.clone();
            runtime.spawn(async move { reporter.report(final_).await });
        }
    }

    /// Close the run with a final report, waiting at most `bound`. For the
    /// exit path, where an unbounded wait would hold the user's shell.
    pub async fn finish(&self, bound: Duration) {
        let _ = tokio::time::timeout(bound, self.report(true)).await;
    }

    /// Report every [`REPORT_INTERVAL`] while the session is open, until the
    /// returned task is aborted. Samples the ledger first, so tokens spent by
    /// a turn that is still running are counted too. An idle but open session
    /// sends a report without new tokens, which keeps its device "active".
    pub fn spawn_periodic(&self) -> JoinHandle<()> {
        self.spawn_every(REPORT_INTERVAL)
    }

    fn spawn_every(&self, period: Duration) -> JoinHandle<()> {
        let reporter = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // the first tick fires immediately; skip it
            loop {
                ticker.tick().await;
                reporter.tick().await;
            }
        })
    }

    async fn tick(&self) {
        if let Some(run) = self.lock().run.as_mut() {
            run.sample();
            if !run.has_unsent() {
                run.keepalive();
            }
        }
        self.report(false).await;
    }

    /// Why reporting stopped, if it did. For a status line or a debug view;
    /// nothing prints this by itself.
    pub fn problem(&self) -> Option<String> {
        self.lock().stopped.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Decide what to send, and mark it sent. `None` means there is nothing
    /// to say.
    fn prepare(&self, final_: bool) -> Option<Outgoing> {
        let mut inner = self.lock();
        if final_ {
            let mut run = inner.run.take()?;
            return if inner.stopped.is_some() { None } else { self.outgoing(&mut run, true) };
        }
        if inner.stopped.is_some() {
            return None;
        }
        let run = inner.run.as_mut()?;
        run.sample();
        if run.has_unsent() { self.outgoing(run, false) } else { None }
    }

    /// The request body for `run`: the models that changed, or all of them
    /// when closing. Each gets this report's `seq`, so a later report always
    /// outranks an earlier one for the server's duplicate check.
    fn outgoing(&self, run: &mut Run, final_: bool) -> Option<Outgoing> {
        run.sample();
        let models: Vec<String> = run
            .models
            .iter()
            .filter(|(_, entry)| final_ || entry.unsent)
            .map(|(model, _)| model.clone())
            .take(MAX_REPORTS)
            .collect();
        if models.is_empty() {
            return None;
        }
        run.seq += 1;
        let reports: Vec<Value> = models
            .iter()
            .filter_map(|model| {
                let entry = run.models.get_mut(model)?;
                entry.unsent = false;
                Some(report_item(run.seq, &run.run_id, &run.session_id, model, entry.counters, final_))
            })
            .collect();
        let body = request_body(&self.identity, &Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true), reports);
        Some(Outgoing { body, run_id: run.run_id.clone(), models })
    }

    fn spawn_delivery(&self, outgoing: Option<Outgoing>) {
        if let (Some(outgoing), Ok(runtime)) = (outgoing, tokio::runtime::Handle::try_current()) {
            let reporter = self.clone();
            runtime.spawn(async move { reporter.deliver(outgoing).await });
        }
    }

    /// Send one report. A transient failure puts its models back as unsent so
    /// the next report carries them; anything else means the server will not
    /// take reports from this install, and they stop.
    async fn deliver(&self, outgoing: Outgoing) {
        match tokio::time::timeout(REPORT_TIMEOUT, self.client.report_usage(&outgoing.body)).await {
            Ok(Ok(_)) => {}
            Ok(Err(SyncError::Transient(_))) | Err(_) => self.restore(&outgoing),
            Ok(Err(error)) => self.lock().stopped = Some(error.to_string()),
        }
    }

    fn restore(&self, outgoing: &Outgoing) {
        if let Some(run) = self.lock().run.as_mut().filter(|run| run.run_id == outgoing.run_id) {
            for model in &outgoing.models {
                if let Some(entry) = run.models.get_mut(model) {
                    entry.unsent = true;
                }
            }
        }
    }
}

/// One `(run, model)` in a request, field for field as the server's
/// `POST /v1/usage/report` documents it.
fn report_item(seq: u64, run_id: &str, session_id: &str, model: &str, counters: Counters, final_: bool) -> Value {
    json!({
        "run_id": run_id,
        "session_id": session_id,
        "model": model,
        "seq": seq,
        "input_tokens": counters.input,
        "output_tokens": counters.output,
        "cache_read_tokens": counters.cache_read,
        "cache_write_tokens": counters.cache_write,
        "total_tokens": counters.total(),
        "final": final_,
    })
}

fn request_body(identity: &Identity, sent_at: &str, reports: Vec<Value>) -> Value {
    json!({
        "install_id": identity.install_id,
        "client": {
            "kind": identity.kind,
            "name": identity.name,
            "os": identity.os,
            "arch": identity.arch,
            "app_version": identity.app_version,
        },
        "sent_at": sent_at,
        "reports": reports,
    })
}

/// Whether `ABACUS_NO_USAGE` asks for reporting to stay off. Set to anything
/// but an explicit "no" (`0`, `false`, `off`, empty).
fn opted_out(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| {
        !matches!(value.to_string_lossy().trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "no" | "off")
    })
}

/// The model as the server should see it. A local server may name its model
/// by file path (`/home/me/models/x.gguf`); the path is the user's business,
/// the file name is the model. Ids like `vendor/model-x` are not paths and
/// pass through.
fn public_model_name(model: &str) -> String {
    let model = model.trim();
    let mut chars = model.chars();
    let drive = matches!((chars.next(), chars.next(), chars.next()), (Some(letter), Some(':'), Some('\\' | '/')) if letter.is_ascii_alphabetic());
    let is_path = drive || model.starts_with(['/', '\\', '~']) || model.starts_with("./") || model.starts_with("../");
    let name =
        if is_path { model.rsplit(['/', '\\']).find(|part| !part.is_empty()).unwrap_or_default() } else { model };
    if name.is_empty() { "unknown".to_owned() } else { crate::text::clip(name, MAX_MODEL_CHARS, "") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SyncCredentials;
    use crate::provider::Usage;
    use std::sync::atomic::{AtomicU16, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A server that records every usage report and answers with a chosen status.
    struct Server {
        url: String,
        bodies: Arc<Mutex<Vec<Value>>>,
        status: Arc<AtomicU16>,
        task: JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Server {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
            let status = Arc::new(AtomicU16::new(202));
            let (recorded, answer) = (bodies.clone(), status.clone());
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else { return };
                    let (recorded, answer) = (recorded.clone(), answer.clone());
                    tokio::spawn(async move {
                        let mut buffer = Vec::new();
                        let mut chunk = [0_u8; 8192];
                        let (body_start, length) = loop {
                            let read = stream.read(&mut chunk).await.unwrap_or(0);
                            if read == 0 {
                                return;
                            }
                            buffer.extend_from_slice(&chunk[..read]);
                            let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
                                continue;
                            };
                            let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
                            let length = head
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            break (end + 4, length);
                        };
                        while buffer.len() < body_start + length {
                            let read = stream.read(&mut chunk).await.unwrap_or(0);
                            if read == 0 {
                                break;
                            }
                            buffer.extend_from_slice(&chunk[..read]);
                        }
                        recorded.lock().unwrap().push(serde_json::from_slice(&buffer[body_start..]).unwrap());
                        let code = answer.load(Ordering::Relaxed);
                        let reply = format!(
                            "HTTP/1.1 {code} X\r\nabacus-protocol: 1\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
                        );
                        let _ = stream.write_all(reply.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    });
                }
            });
            Self { url, bodies, status, task }
        }

        fn reporter(&self) -> UsageReporter {
            let client = SyncClient::new(&SyncCredentials {
                server: self.url.clone(),
                token: "token".into(),
                email: "me@example.com".into(),
            })
            .unwrap();
            UsageReporter::with_client(client, identity("tui"))
        }

        fn reports(&self) -> Vec<Value> {
            self.bodies.lock().unwrap().clone()
        }

        /// Reports arrive on background tasks; wait for `count` of them.
        async fn reports_after(&self, count: usize) -> Vec<Value> {
            for _ in 0..200 {
                if self.reports().len() >= count {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            self.reports()
        }
    }

    fn identity(kind: &'static str) -> Identity {
        Identity {
            install_id: "install-1".into(),
            kind,
            name: "laptop".into(),
            os: "linux",
            arch: "x86_64",
            app_version: "0.6.4",
        }
    }

    fn spend(ledger: &TokenLedger, input: u64, output: u64, cache_read: u64) {
        ledger.record(&Usage { total: input + output, prompt: input, completion: output, cache_read, cache_write: 0 });
    }

    fn item<'a>(body: &'a Value, model: &str) -> &'a Value {
        body["reports"].as_array().unwrap().iter().find(|item| item["model"] == model).unwrap()
    }

    #[test]
    fn the_request_matches_the_documented_example() {
        let mut run =
            Run::new("5d1c2f3a-0000-4000-8000-00000000aaaa", "empero/model-x", Arc::new(TokenLedger::default()));
        run.run_id = "0b9f0d7c-0000-4000-8000-00000000bbbb".into();
        run.seq = 6;
        run.models.insert(
            "empero/model-x".into(),
            Entry {
                counters: Counters { input: 120_345, output: 5_120, cache_read: 90_000, cache_write: 0 },
                unsent: true,
            },
        );
        let reporter = UsageReporter::with_client(
            SyncClient::new(&SyncCredentials {
                server: "http://127.0.0.1:1".into(),
                token: "t".into(),
                email: "e".into(),
            })
            .unwrap(),
            identity("tui"),
        );
        let outgoing = reporter.outgoing(&mut run, false).unwrap();
        let mut body = outgoing.body;
        assert!(body["sent_at"].as_str().is_some_and(|stamp| stamp.ends_with('Z') && stamp.len() == 20), "{body}");
        body["sent_at"] = json!("2026-10-04T12:00:00Z");
        assert_eq!(
            body,
            json!({
                "install_id": "install-1",
                "client": {"kind": "tui", "name": "laptop", "os": "linux", "arch": "x86_64", "app_version": "0.6.4"},
                "sent_at": "2026-10-04T12:00:00Z",
                "reports": [{
                    "run_id": "0b9f0d7c-0000-4000-8000-00000000bbbb",
                    "session_id": "5d1c2f3a-0000-4000-8000-00000000aaaa",
                    "model": "empero/model-x",
                    "seq": 7,
                    "input_tokens": 120_345,
                    "output_tokens": 5_120,
                    "cache_read_tokens": 90_000,
                    "cache_write_tokens": 0,
                    "total_tokens": 125_465,
                    "final": false,
                }],
            })
        );
        // Nothing but counters and identifiers: no field that could hold text from a session.
        let text = body.to_string();
        for forbidden in ["prompt", "message", "content", "path", "workspace", "cwd"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
    }

    #[tokio::test]
    async fn a_model_switch_splits_the_ledger_between_the_models() {
        let server = Server::start().await;
        let reporter = server.reporter();
        let ledger = Arc::new(TokenLedger::default());
        reporter.open_session("session-1", "model-a", ledger.clone());

        spend(&ledger, 1_000, 100, 800);
        reporter.note_model("model-b");
        spend(&ledger, 500, 50, 0);
        reporter.report(false).await;

        let reports = server.reports();
        assert_eq!(reports.len(), 1);
        let (a, b) = (item(&reports[0], "model-a"), item(&reports[0], "model-b"));
        assert_eq!((a["input_tokens"].as_u64(), a["output_tokens"].as_u64()), (Some(1_000), Some(100)));
        assert_eq!(a["cache_read_tokens"], 800);
        assert_eq!((b["input_tokens"].as_u64(), b["output_tokens"].as_u64()), (Some(500), Some(50)));
        assert_eq!(b["cache_read_tokens"], 0);
        assert_eq!(a["seq"], b["seq"]);
        assert_eq!(a["run_id"], b["run_id"]);
    }

    #[tokio::test]
    async fn figures_are_cumulative_and_seq_only_grows() {
        let server = Server::start().await;
        let reporter = server.reporter();
        let ledger = Arc::new(TokenLedger::default());
        reporter.open_session("session-1", "model-a", ledger.clone());

        let mut seqs = Vec::new();
        for turn in 1..=3_u64 {
            spend(&ledger, 100, 10, 0);
            reporter.report(false).await;
            let sent = server.reports().pop().unwrap();
            let sent = item(&sent, "model-a");
            assert_eq!(sent["input_tokens"], 100 * turn, "cumulative, not a delta");
            assert_eq!(sent["total_tokens"], 110 * turn);
            assert_eq!(sent["final"], false);
            seqs.push(sent["seq"].as_u64().unwrap());
        }
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");

        reporter.report(true).await;
        let last = server.reports().pop().unwrap();
        assert_eq!(item(&last, "model-a")["final"], true);
        assert!(item(&last, "model-a")["seq"].as_u64().unwrap() > *seqs.last().unwrap());
        assert_eq!(item(&last, "model-a")["input_tokens"], 300);
    }

    #[tokio::test]
    async fn nothing_is_sent_when_nothing_changed() {
        let server = Server::start().await;
        let reporter = server.reporter();
        let ledger = Arc::new(TokenLedger::default());
        reporter.open_session("session-1", "model-a", ledger.clone());

        reporter.report(false).await;
        assert!(server.reports().is_empty(), "no tokens yet");

        spend(&ledger, 10, 1, 0);
        reporter.report(false).await;
        reporter.report(false).await;
        assert_eq!(server.reports().len(), 1, "the second report has nothing new");

        // A run that never spent anything has nothing to close, either.
        let idle = server.reporter();
        idle.open_session("session-2", "model-a", Arc::new(TokenLedger::default()));
        idle.report(true).await;
        assert_eq!(server.reports().len(), 1);
    }

    #[tokio::test]
    async fn a_resumed_ledger_is_a_baseline_not_usage() {
        let server = Server::start().await;
        let reporter = server.reporter();
        let ledger = Arc::new(TokenLedger::new(50_000));
        spend(&ledger, 7_000, 700, 0);
        reporter.open_session("session-1", "model-a", ledger.clone());
        reporter.report(false).await;
        assert!(server.reports().is_empty(), "what was spent before the run is not reported");

        spend(&ledger, 20, 2, 0);
        reporter.report(false).await;
        let sent = server.reports().pop().unwrap();
        assert_eq!(item(&sent, "model-a")["input_tokens"], 20);
        assert_eq!(item(&sent, "model-a")["total_tokens"], 22);
    }

    #[tokio::test]
    async fn opening_another_session_closes_the_previous_run() {
        let server = Server::start().await;
        let reporter = server.reporter();
        let ledger = Arc::new(TokenLedger::default());
        reporter.open_session("session-1", "model-a", ledger.clone());
        spend(&ledger, 40, 4, 0);

        reporter.open_session("session-2", "model-a", ledger.clone());
        let reports = server.reports_after(1).await;
        let first = item(&reports[0], "model-a");
        assert_eq!((first["session_id"].as_str(), first["final"].as_bool()), (Some("session-1"), Some(true)));

        spend(&ledger, 5, 1, 0);
        reporter.report(false).await;
        let second = server.reports().pop().unwrap();
        let second = item(&second, "model-a");
        assert_eq!(second["session_id"], "session-2");
        assert_ne!(second["run_id"], first["run_id"], "a new session is a new run");
        assert_eq!(second["input_tokens"], 5, "the new run starts from zero");
    }

    #[tokio::test]
    async fn a_lost_report_is_covered_by_the_next() {
        let server = Server::start().await;
        let reporter = server.reporter();
        let ledger = Arc::new(TokenLedger::default());
        reporter.open_session("session-1", "model-a", ledger.clone());
        spend(&ledger, 100, 10, 0);

        server.status.store(503, Ordering::Relaxed);
        reporter.report(false).await;
        assert_eq!(server.reports().len(), 1);
        assert!(reporter.problem().is_none(), "a busy server is not the end of reporting");

        server.status.store(202, Ordering::Relaxed);
        reporter.report(false).await;
        let reports = server.reports();
        assert_eq!(reports.len(), 2, "the unsent figures go out again");
        assert_eq!(item(&reports[1], "model-a")["input_tokens"], 100);
        assert!(item(&reports[1], "model-a")["seq"].as_u64() > item(&reports[0], "model-a")["seq"].as_u64());
    }

    #[tokio::test]
    async fn a_server_that_refuses_us_is_left_alone() {
        for refusal in [401_u16, 404, 422] {
            let server = Server::start().await;
            let reporter = server.reporter();
            let ledger = Arc::new(TokenLedger::default());
            reporter.open_session("session-1", "model-a", ledger.clone());
            spend(&ledger, 100, 10, 0);
            server.status.store(refusal, Ordering::Relaxed);

            reporter.report(false).await;
            spend(&ledger, 100, 10, 0);
            reporter.report(false).await;
            reporter.report(true).await;
            assert_eq!(server.reports().len(), 1, "{refusal}: one refusal ends the attempts");
            assert!(reporter.problem().is_some());
        }
    }

    #[tokio::test]
    async fn an_idle_open_session_keeps_reporting_so_the_device_stays_active() {
        let server = Server::start().await;
        let reporter = server.reporter();
        let ledger = Arc::new(TokenLedger::default());
        reporter.open_session("session-1", "model-a", ledger.clone());

        reporter.tick().await;
        assert!(server.reports().is_empty(), "before any tokens there is nothing to keep alive");

        spend(&ledger, 100, 10, 0);
        reporter.tick().await;
        reporter.tick().await;
        let reports = server.reports();
        assert_eq!(reports.len(), 2);
        assert_eq!(item(&reports[1], "model-a")["input_tokens"], item(&reports[0], "model-a")["input_tokens"]);
        assert!(item(&reports[1], "model-a")["seq"].as_u64() > item(&reports[0], "model-a")["seq"].as_u64());
    }

    #[tokio::test]
    async fn the_periodic_task_reports_what_a_running_turn_has_spent() {
        let server = Server::start().await;
        let reporter = server.reporter();
        let ledger = Arc::new(TokenLedger::default());
        reporter.open_session("session-1", "model-a", ledger.clone());
        let task = reporter.spawn_every(Duration::from_millis(30));

        spend(&ledger, 123, 4, 0);
        let reports = server.reports_after(1).await;
        task.abort();
        assert_eq!(item(&reports[0], "model-a")["input_tokens"], 123);
    }

    #[test]
    fn signed_out_installs_do_not_report() {
        let home = tempfile::tempdir().unwrap();
        let paths = AbacusPaths::under(home.path().to_path_buf());
        assert!(UsageReporter::new(&paths, &Credentials::default(), "tui").is_none());

        let signed_in = Credentials {
            sync: Some(SyncCredentials {
                server: "https://sync.example".into(),
                token: "token".into(),
                email: "me@example.com".into(),
            }),
            ..Credentials::default()
        };
        // The test environment may itself set the opt-out; the parsing is tested below.
        if std::env::var_os("ABACUS_NO_USAGE").is_none() {
            let reporter = UsageReporter::new(&paths, &signed_in, "headless").unwrap();
            assert_eq!(reporter.identity.kind, "headless");
            assert_eq!(reporter.identity.install_id, paths.install_id());
        }
        let empty_token = Credentials {
            sync: Some(SyncCredentials { server: "https://sync.example".into(), token: " ".into(), email: "e".into() }),
            ..Credentials::default()
        };
        assert!(UsageReporter::new(&paths, &empty_token, "tui").is_none());
    }

    #[test]
    fn the_opt_out_is_off_unless_asked_for() {
        assert!(!opted_out(None));
        for off in ["", "0", "false", "No", " off "] {
            assert!(!opted_out(Some(OsStr::new(off))), "{off:?}");
        }
        for on in ["1", "true", "yes", "anything"] {
            assert!(opted_out(Some(OsStr::new(on))), "{on:?}");
        }
    }

    #[test]
    fn model_names_never_carry_a_path() {
        assert_eq!(public_model_name("empero/model-x"), "empero/model-x");
        assert_eq!(public_model_name(" gpt-5 "), "gpt-5");
        assert_eq!(public_model_name("/home/me/models/qwen-7b.Q4.gguf"), "qwen-7b.Q4.gguf");
        assert_eq!(public_model_name("~/models/qwen.gguf"), "qwen.gguf");
        assert_eq!(public_model_name("../models/qwen.gguf"), "qwen.gguf");
        assert_eq!(public_model_name(r"C:\Users\me\models\qwen.gguf"), "qwen.gguf");
        assert_eq!(public_model_name(""), "unknown");
        assert_eq!(public_model_name("/"), "unknown");
        assert_eq!(public_model_name(&"m".repeat(500)).len(), MAX_MODEL_CHARS);
    }
}
