//! Abacus Sync: account sign-in, and keeping sessions and their traces in
//! step across devices.
//!
//! * [`client`] speaks the server's REST API (typed errors, timeouts, retries).
//! * [`engine`] pulls the change feed and pushes changed sessions.
//! * [`crate::sync_state`] remembers what this device last exchanged.
//!
//! Front ends call [`reconcile`] / [`pull_changes`] when a session opens,
//! [`push_session`] after a turn or when idle, and [`push_dirty`] on close.
//! Every pass is incremental: an unchanged session costs one `stat`, an
//! unchanged server costs one request.

mod client;
mod engine;

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub use client::{Account, ChangesPage, Pairing, Precondition, SessionMeta, SyncClient, SyncError, TicketResponse};
pub use engine::{HeldUpdate, SyncOutcome};

use crate::config::{AbacusPaths, Credentials, Settings, SyncCommand, SyncCredentials, device_name};
use crate::session::{Session, SessionStore};
use crate::sync_state::{Local, SyncState, inspect, is_placeholder_document, local_sessions};
use client::{Retry, anonymous_client, decode};
use engine::Engine;

pub const AUTO_PUSH_IDLE: Duration = Duration::from_secs(60);

/// One sync pass at a time per process: the open-time pull, the idle push and
/// the close-time push would otherwise race each other for the same files.
static SYNC_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Sessions an interactive front end has open in this process. A pull never
/// writes over one of these: the front end would save its in-memory copy over
/// the download and the next push would quietly undo the other device's work.
/// The newer revision is handed over in [`SyncOutcome::held`] instead.
static OPEN_SESSIONS: Mutex<Vec<Uuid>> = Mutex::new(Vec::new());

pub fn session_opened(id: Uuid) {
    OPEN_SESSIONS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(id);
}

pub fn session_closed(id: Uuid) {
    let mut open = OPEN_SESSIONS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(index) = open.iter().position(|candidate| *candidate == id) {
        open.swap_remove(index);
    }
}

pub(crate) fn is_open(id: &Uuid) -> bool {
    OPEN_SESSIONS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(id)
}

pub fn is_configured(credentials: &Credentials) -> bool {
    credentials.sync.is_some()
}

/// A session that has never received a prompt is not a session yet. The
/// transcript always opens with the system prompt, so "empty" means no user
/// message has ever been added.
pub fn is_placeholder(session: &Session) -> bool {
    session.title == "New session" && !session.messages.iter().any(|message| message["role"] == "user")
}

pub fn configured_client(paths: &AbacusPaths) -> Result<SyncClient> {
    configured(&Credentials::load(paths)?, paths)
}

fn configured(credentials: &Credentials, paths: &AbacusPaths) -> Result<SyncClient> {
    let credentials = credentials.sync.as_ref().context("sync is not configured; run `abacus sync login`")?;
    Ok(SyncClient::new(credentials)?.with_home(paths))
}

enum Job {
    Reconcile,
    Pull,
    PushAll,
    PushOne(Uuid),
}

/// Run one automatic pass. Signed out is not an error: there is nothing to do.
async fn run(paths: &AbacusPaths, job: Job) -> Result<SyncOutcome> {
    let credentials = Credentials::load(paths)?;
    if credentials.sync.is_none() {
        return Ok(SyncOutcome::default());
    }
    let client = configured(&credentials, paths)?;
    let _guard = SYNC_LOCK.lock().await;
    let state = SyncState::load_for(paths, &client.server, &client.email);
    let mut engine = Engine::new(&client, paths, state);
    let result = match job {
        Job::Reconcile => match engine.pull(false).await {
            Ok(()) => engine.push(None).await,
            error => error,
        },
        Job::Pull => engine.pull(false).await,
        Job::PushAll => engine.push(None).await,
        Job::PushOne(id) => engine.push(Some(id)).await,
    };
    Ok(engine.finish(result)?)
}

/// Download what changed on the server since the last pull.
pub async fn pull_changes(paths: &AbacusPaths) -> Result<SyncOutcome> {
    run(paths, Job::Pull).await
}

/// Upload every local session that changed since it last synced.
pub async fn push_dirty(paths: &AbacusPaths) -> Result<SyncOutcome> {
    run(paths, Job::PushAll).await
}

/// [`pull_changes`], then [`push_dirty`]: what a front end does when it opens,
/// so uploads a previous close could not finish go out too.
pub async fn reconcile(paths: &AbacusPaths) -> Result<SyncOutcome> {
    run(paths, Job::Reconcile).await
}

/// Upload one session if it changed since it last synced, and say what
/// happened (a refused upload shows up in [`SyncOutcome::conflicts`]).
pub async fn sync_session(paths: &AbacusPaths, session: &Session) -> Result<SyncOutcome> {
    if is_placeholder(session) || !SessionStore::new(paths, session.workspace.clone()).path(session.id).exists() {
        return Ok(SyncOutcome::default());
    }
    run(paths, Job::PushOne(session.id)).await
}

/// Upload one session if it changed since it last synced.
pub async fn push_session(paths: &AbacusPaths, session: &Session) -> Result<()> {
    let outcome = sync_session(paths, session).await?;
    match outcome.errors.into_iter().next() {
        Some(error) => Err(anyhow!(error)),
        None => Ok(()),
    }
}

/// [`push_session`] in the background, best effort.
pub fn spawn_session_sync(paths: &AbacusPaths, session: &Session) {
    let paths = paths.clone();
    let session = session.clone();
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            let _ = push_session(&paths, &session).await;
        });
    }
}

/// Install a newer revision of a session this process has open, provided the
/// open copy has not changed since it last synced, and return it as loaded
/// from disk. `Ok(None)` means it has changed; the next sync resolves that as
/// a conflict, keeping both.
pub fn accept_held(paths: &AbacusPaths, update: HeldUpdate) -> Result<Option<Session>> {
    let Some(sync) = Credentials::load(paths)?.sync else {
        return Ok(None);
    };
    let workspace = Session::deserialize(&update.document).context("unreadable session")?.workspace;
    let mut state = SyncState::load_for(paths, &sync.server, &sync.email);
    let Some(id) = engine::accept(paths, &mut state, update)? else {
        return Ok(None);
    };
    state.save()?;
    SessionStore::new(paths, workspace).load(&id.to_string()).map(Some)
}

#[deprecated(note = "use `pull_changes`, which downloads only what changed")]
pub async fn pull_workspace(paths: &AbacusPaths, _workspace: &std::path::Path) -> Result<usize> {
    Ok(pull_changes(paths).await?.pulled)
}

#[deprecated(note = "use `push_dirty`")]
pub async fn push_all_updated_local(paths: &AbacusPaths) -> Result<usize> {
    Ok(push_dirty(paths).await?.pushed)
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    access_token: String,
    user: Account,
}

#[derive(Debug, Deserialize)]
struct DeviceStart {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

/// Who is signing in, so the account's device list can name this machine and
/// usage can be tied to it.
fn identity(paths: &AbacusPaths) -> Value {
    json!({
        "device_name": device_name(),
        "install_id": paths.install_id(),
        "client": {
            "kind": "cli",
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "app_version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn with_identity(paths: &AbacusPaths, mut body: Value) -> Value {
    if let (Some(body), Value::Object(identity)) = (body.as_object_mut(), identity(paths)) {
        body.extend(identity);
    }
    body
}

async fn password_login_flow(
    paths: &AbacusPaths,
    server: &str,
    email: Option<String>,
    password: Option<String>,
) -> Result<LoginResponse> {
    let email = match email {
        Some(value) => value,
        None => prompt("Email: ")?,
    };
    let password = match password {
        Some(value) => value,
        None => prompt_password()?,
    };
    let response = anonymous_client()?
        .post(format!("{server}/v1/auth/login"))
        .header("Abacus-Protocol", client::PROTOCOL)
        .json(&with_identity(paths, json!({"email": email, "password": password})))
        .send()
        .await?;
    decode(response).await
}

async fn device_login_flow(paths: &AbacusPaths, server: &str) -> Result<LoginResponse> {
    let start: DeviceStart = decode(
        anonymous_client()?
            .post(format!("{server}/v1/auth/device"))
            .header("Abacus-Protocol", client::PROTOCOL)
            .json(&identity(paths))
            .send()
            .await?,
    )
    .await?;
    println!("Open this page in a browser and sign in with your magic link:");
    println!("  {}", start.verification_uri);
    println!();
    println!("Then enter this code:");
    println!("  {}", start.user_code);
    println!();
    println!("Waiting for approval…");
    let deadline = std::time::Instant::now() + Duration::from_secs(start.expires_in.max(30));
    let mut interval = Duration::from_secs(start.interval.max(1));
    loop {
        if std::time::Instant::now() >= deadline {
            bail!("device login expired; run `abacus sync login` again");
        }
        tokio::time::sleep(interval).await;
        let response = anonymous_client()?
            .post(format!("{server}/v1/auth/device/token"))
            .header("Abacus-Protocol", client::PROTOCOL)
            .json(&json!({"device_code": start.device_code}))
            .send()
            .await?;
        match response.status() {
            reqwest::StatusCode::OK => return decode(response).await,
            reqwest::StatusCode::PRECONDITION_REQUIRED => continue,
            reqwest::StatusCode::TOO_MANY_REQUESTS => interval += Duration::from_secs(1),
            other => {
                let detail = response.text().await.unwrap_or_default();
                bail!("sync server returned {other}: {}", detail.chars().take(1000).collect::<String>());
            }
        }
    }
}

pub async fn handle(action: SyncCommand, paths: &AbacusPaths, _workspace: PathBuf) -> Result<()> {
    let mut credentials = Credentials::load(paths)?;
    match action {
        SyncCommand::Login { server, email, password, password_login } => {
            let server = server.trim_end_matches('/').to_owned();
            let login = if password_login || password.is_some() {
                password_login_flow(paths, &server, email, password).await?
            } else {
                device_login_flow(paths, &server).await?
            };
            credentials.sync =
                Some(SyncCredentials { server, token: login.access_token, email: login.user.email.clone() });
            credentials.save(paths)?;
            println!(
                "Signed in as {} on {}. Sessions now sync automatically across devices.",
                login.user.email,
                device_name()
            );
        }
        SyncCommand::Logout => {
            credentials.sync = None;
            credentials.save(paths)?;
            println!("Signed out of session sync.");
        }
        SyncCommand::Status => print_status(paths, &credentials).await?,
        SyncCommand::Sessions => print_sessions(paths, &credentials).await?,
        SyncCommand::Push { session, force } => {
            let client = configured(&credentials, paths)?.with_retry(Retry::MANUAL);
            let only = session.as_deref().map(|prefix| resolve_local(paths, prefix)).transpose()?;
            let _guard = SYNC_LOCK.lock().await;
            let state = SyncState::load_for(paths, &client.server, &client.email);
            let mut engine = Engine::new(&client, paths, state).manual(force);
            let result = engine.push(only).await;
            print_outcome(&engine.finish(result)?, "Nothing to upload: every local session is in sync.")?;
        }
        SyncCommand::Pull { session, force } => {
            let client = configured(&credentials, paths)?.with_retry(Retry::MANUAL);
            let only = match session.as_deref() {
                Some(prefix) => Some(resolve_remote(&client, prefix).await?),
                None => None,
            };
            let _guard = SYNC_LOCK.lock().await;
            let state = SyncState::load_for(paths, &client.server, &client.email);
            let mut engine = Engine::new(&client, paths, state).manual(force);
            // A command re-reads the whole feed: one listing, and nothing
            // already here is downloaded again.
            let result = match only {
                Some(id) => engine.pull_one(id).await,
                None => engine.pull(true).await,
            };
            print_outcome(&engine.finish(result)?, "Nothing to download: this device has every session.")?;
        }
        SyncCommand::Pair { session, url_only } => {
            let client = configured(&credentials, paths)?.with_retry(Retry::MANUAL);
            let session = match session.as_deref() {
                Some(prefix) => Some(resolve_remote(&client, prefix).await?.to_string()),
                None => None,
            };
            let pairing = match client.pairing_url(session.as_deref()).await {
                Err(SyncError::Gone) => bail!("{} does not offer phone pairing yet", client.server),
                pairing => pairing?,
            };
            if url_only {
                println!("{}", pairing.pairing_url);
            } else {
                println!("Open this link on your phone to sign in to {}:", client.server);
                println!();
                println!("  {}", pairing.pairing_url);
                println!();
                let minutes = pairing.expires_in.div_ceil(60).max(1);
                println!(
                    "It works once and expires in {minutes} minute{}. Anyone with it can use your account until then.",
                    if minutes == 1 { "" } else { "s" }
                );
            }
        }
    }
    Ok(())
}

/// What a sync command did, one line per session. An error or an unresolved
/// conflict makes the command fail, so scripts notice what did not sync.
fn print_outcome(outcome: &SyncOutcome, nothing: &str) -> Result<()> {
    for line in &outcome.lines {
        println!("{line}");
    }
    for error in &outcome.errors {
        eprintln!("error: {error}");
    }
    if outcome.lines.is_empty() && outcome.errors.is_empty() {
        println!("{nothing}");
    }
    match outcome.errors.len() + outcome.conflicts.len() {
        0 => Ok(()),
        count => bail!("{} could not be synced", plural(count, "session")),
    }
}

/// A local session by id or unique prefix, in any workspace.
fn resolve_local(paths: &AbacusPaths, prefix: &str) -> Result<Uuid> {
    let matches =
        local_sessions(paths).into_keys().filter(|id| id.to_string().starts_with(prefix.trim())).collect::<Vec<_>>();
    match matches.as_slice() {
        [] => bail!("no local session matches `{prefix}`"),
        [id] => Ok(*id),
        _ => bail!("session prefix `{prefix}` is ambiguous"),
    }
}

/// A server session by id or unique prefix.
async fn resolve_remote(client: &SyncClient, prefix: &str) -> Result<Uuid> {
    if let Ok(id) = Uuid::parse_str(prefix.trim()) {
        return Ok(id);
    }
    let matches = client
        .sessions()
        .await?
        .into_iter()
        .filter(|meta| meta.id.starts_with(prefix.trim()))
        .filter_map(|meta| Uuid::parse_str(&meta.id).ok())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => bail!("no session on the server matches `{prefix}`"),
        [id] => Ok(*id),
        _ => bail!("session prefix `{prefix}` is ambiguous"),
    }
}

async fn print_status(paths: &AbacusPaths, credentials: &Credentials) -> Result<()> {
    let Some(sync) = &credentials.sync else {
        println!("Not signed in. Run `abacus sync login` to sync sessions across devices.");
        return Ok(());
    };
    let client = configured(credentials, paths)?;
    let (account, shared) = match client.account().await {
        Ok(account) => {
            let shared = client.sessions().await.map(|items| items.iter().filter(|meta| meta.remote_online).count());
            (format!("{}  ({})", account.email, client.server), shared.ok())
        }
        Err(SyncError::Unauthorized) => {
            (format!("{}  ({}) — sign-in expired; run `abacus sync login`", sync.email, client.server), None)
        }
        Err(error) => (format!("{}  ({}) — {error}", sync.email, client.server), None),
    };
    let state = SyncState::load_for(paths, &client.server, &client.email);
    let install_id = paths.install_id();
    let short_install = match install_id.char_indices().nth(4) {
        Some((cut, _)) if install_id.len() > 8 => {
            format!("{}…{}", &install_id[..cut], &install_id[install_id.len() - 3..])
        }
        _ => install_id.clone(),
    };
    println!("Account      {account}");
    println!(
        "Device       {} · install {short_install} · abacus {} {}/{}",
        device_name(),
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "Sync         cursor {} · last pull {} · last push {}",
        state.cursor,
        ago(state.last_pull_at),
        ago(state.last_push_at)
    );

    let mut counts = BTreeMap::<&str, usize>::new();
    let local = local_sessions(paths);
    for entry in local.values() {
        let record = state.record(&entry.id);
        if let Some(record) = record {
            if record.deleted {
                *counts.entry("deleted").or_default() += 1;
                continue;
            }
            if record.conflict {
                *counts.entry("conflicts").or_default() += 1;
            }
            if record.behind.is_some() {
                *counts.entry("behind").or_default() += 1;
            }
            if record.rejected.is_some() {
                *counts.entry("refused").or_default() += 1;
            }
        }
        match inspect(entry, record) {
            Ok(Local::Clean(_)) => *counts.entry("synced").or_default() += 1,
            Ok(Local::Dirty(_)) => *counts.entry("dirty").or_default() += 1,
            Ok(Local::Untracked(snapshot)) if is_placeholder_document(&snapshot.document) => {
                *counts.entry("placeholders").or_default() += 1
            }
            Ok(Local::Untracked(_)) => *counts.entry("dirty").or_default() += 1,
            Err(_) => *counts.entry("unreadable").or_default() += 1,
        }
    }
    let count = |key: &str| counts.get(key).copied().unwrap_or(0);
    let mut line = format!(
        "{} · {} dirty · {}",
        plural(local.len() - count("placeholders"), "session"),
        count("dirty"),
        plural(count("conflicts"), "conflict")
    );
    for (key, label) in [
        ("behind", "newer on the server"),
        ("deleted", "deleted elsewhere, waiting to settle"),
        ("refused", "refused by the server"),
        ("unreadable", "unreadable"),
    ] {
        if count(key) > 0 {
            line.push_str(&format!(" · {} {label}", count(key)));
        }
    }
    println!("Local        {line}");
    let auto_share = Settings::load(paths).map(|settings| settings.remote.auto_share).unwrap_or(true);
    let shared = shared.map_or_else(String::new, |shared| format!(" · {} shared now", plural(shared, "session")));
    println!("Remote       auto-share {}{shared}", if auto_share { "on" } else { "off" });
    if count("conflicts") > 0 {
        println!();
        println!(
            "`abacus sync pull` resolves conflicts by keeping both copies; `abacus sync push --force` keeps this device's."
        );
    }
    Ok(())
}

async fn print_sessions(paths: &AbacusPaths, credentials: &Credentials) -> Result<()> {
    let client = configured(credentials, paths)?;
    let mut remote = client.sessions().await?;
    if remote.is_empty() {
        println!("No sessions on the server yet.");
        return Ok(());
    }
    remote.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let state = SyncState::load_for(paths, &client.server, &client.email);
    let local = local_sessions(paths);
    println!("{:<8}  {:>4}  {:<16}  {:>8}  {:<12}  TITLE", "ID", "REV", "UPDATED", "SIZE", "THIS DEVICE");
    for meta in &remote {
        let here = Uuid::parse_str(&meta.id).ok().map_or("—", |id| {
            let record = state.record(&id);
            match (local.get(&id), record) {
                (None, _) => "not here",
                (Some(_), Some(record)) if record.conflict => "conflict",
                (Some(entry), Some(record)) if record.revision == Some(meta.revision) => {
                    match inspect(entry, Some(record)) {
                        Ok(Local::Clean(_)) => "in sync",
                        _ => "changed here",
                    }
                }
                (Some(_), _) => "behind",
            }
        });
        let live = if meta.remote_online { "  · live" } else { "" };
        println!(
            "{:<8}  {:>4}  {:<16}  {:>8}  {:<12}  {}{live}",
            &meta.id[..meta.id.len().min(8)],
            meta.revision,
            local_time(&meta.updated_at),
            human_size(meta.size_bytes),
            here,
            crate::text::clip(&meta.title, 60, "…"),
        );
    }
    Ok(())
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

fn ago(time: Option<DateTime<Utc>>) -> String {
    let Some(time) = time else {
        return "never".to_owned();
    };
    let seconds = (Utc::now() - time).num_seconds().max(0);
    match seconds {
        0..60 => format!("{seconds} s ago"),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86400),
    }
}

/// A server timestamp in local time. Older servers on SQLite send naive
/// timestamps, which are UTC.
fn local_time(value: &str) -> String {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f").map(|naive| naive.and_utc()));
    match parsed {
        Ok(time) => time.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string(),
        Err(_) => crate::text::clip(value, 16, ""),
    }
}

fn human_size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_owned())
}

fn prompt_password() -> Result<String> {
    // Avoid accepting a password through argv in normal use. Terminal echo is
    // disabled and restored by stty on Unix; other platforms fall back to input.
    print!("Password: ");
    io::stdout().flush()?;
    #[cfg(unix)]
    let _ = std::process::Command::new("stty").arg("-echo").status();
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("stty").arg("echo").status();
        println!();
    }
    Ok(value.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_client_rejects_non_http_urls() {
        let result = SyncClient::new(&SyncCredentials {
            server: "file:///tmp/server".into(),
            token: "secret".into(),
            email: "test@example.com".into(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn configured_credentials_enable_auto_sync() {
        assert!(!is_configured(&Credentials::default()));
        let credentials = Credentials {
            keys: Default::default(),
            sync: Some(SyncCredentials {
                server: "https://abacus.empero.org".into(),
                token: "secret".into(),
                email: "person@example.com".into(),
            }),
        };
        assert!(is_configured(&credentials));
        assert_eq!(AUTO_PUSH_IDLE, std::time::Duration::from_secs(60));
    }

    #[test]
    fn sign_in_identifies_the_install() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().to_path_buf());
        let body = with_identity(&paths, json!({"email": "a@b"}));
        assert_eq!(body["email"], "a@b");
        assert_eq!(body["install_id"], paths.install_id());
        assert_eq!(body["install_id"], paths.install_id(), "stable across calls");
        assert_eq!(body["client"]["kind"], "cli");
        assert_eq!(body["client"]["app_version"], env!("CARGO_PKG_VERSION"));
        assert!(!body["device_name"].as_str().unwrap().is_empty());
    }

    #[test]
    fn signed_out_sync_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().to_path_buf());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(reconcile(&paths)).unwrap();
        assert_eq!((outcome.pulled, outcome.pushed), (0, 0));
    }

    #[test]
    fn status_helpers_read_well() {
        assert_eq!(ago(None), "never");
        assert_eq!(ago(Some(Utc::now() - chrono::Duration::seconds(125))), "2 min ago");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1_572_864), "1.5 MB");
        assert_eq!(local_time("not a time"), "not a time");
        assert!(local_time("2026-10-04T21:13:00.123456").starts_with("2026-10-0"));
    }
}
