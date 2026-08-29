use std::io::{self, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, StatusCode, header};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    config::{AbacusPaths, Credentials, SyncCommand, SyncCredentials},
    session::{Session, SessionStore},
};

const PROTOCOL: &str = "1";
pub const AUTO_PUSH_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Deserialize)]
struct LoginResponse {
    access_token: String,
    user: Account,
}

#[derive(Debug, Deserialize)]
struct Account {
    email: String,
}

#[derive(Debug, Deserialize)]
struct DeviceStart {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct SessionList {
    items: Vec<RemoteSession>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RemoteSession {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) model: String,
    pub(crate) updated_at: String,
    pub(crate) revision: u64,
    #[serde(default)]
    pub(crate) remote_online: bool,
}

#[derive(Debug, Deserialize)]
struct PutResponse {
    meta: RemoteSession,
}

#[derive(Clone)]
pub struct SyncClient {
    http: Client,
    server: String,
    token: String,
}

impl SyncClient {
    pub fn new(credentials: &SyncCredentials) -> Result<Self> {
        let server = credentials.server.trim_end_matches('/').to_owned();
        let url = reqwest::Url::parse(&server).context("sync server is not a valid URL")?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("sync server must use HTTP or HTTPS");
        }
        Ok(Self {
            http: Client::builder()
                .user_agent(concat!("abacus-agent/", env!("CARGO_PKG_VERSION")))
                .build()?,
            server,
            token: credentials.token.clone(),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.server, path))
            .bearer_auth(&self.token)
            .header("Abacus-Protocol", PROTOCOL)
            .header(header::ACCEPT, "application/json")
    }

    async fn account(&self) -> Result<Account> {
        decode(
            self.request(reqwest::Method::GET, "/v1/auth/me")
                .send()
                .await?,
        )
        .await
    }

    pub(crate) async fn sessions(&self) -> Result<Vec<RemoteSession>> {
        let response: SessionList = decode(
            self.request(reqwest::Method::GET, "/v1/sync/sessions")
                .send()
                .await?,
        )
        .await?;
        Ok(response.items)
    }

    async fn revision(&self, id: &str) -> Result<Option<u64>> {
        let response = self
            .request(reqwest::Method::GET, "/v1/sync/sessions")
            .send()
            .await?;
        let sessions: SessionList = decode(response).await?;
        Ok(sessions
            .items
            .into_iter()
            .find(|item| item.id == id)
            .map(|item| item.revision))
    }

    pub async fn push(&self, session: &Session, trace: &[u8], force: bool) -> Result<u64> {
        let revision = self.revision(&session.id.to_string()).await?;
        if revision.is_some() && !force {
            bail!(
                "remote session {} already exists; use --force to replace revision {}",
                session.id,
                revision.unwrap_or_default()
            );
        }
        self.push_at_revision(session, trace, revision).await
    }

    async fn push_at_revision(
        &self,
        session: &Session,
        trace: &[u8],
        revision: Option<u64>,
    ) -> Result<u64> {
        let digest = format!("{:x}", Sha256::digest(trace));
        let body = json!({
            "session": session,
            "trace_base64": STANDARD.encode(trace),
            "trace_sha256": digest,
            "device_id": device_id(),
        });
        let mut request = self.request(
            reqwest::Method::PUT,
            &format!("/v1/sync/sessions/{}", session.id),
        );
        request = if let Some(revision) = revision {
            request.header(header::IF_MATCH, format!("\"{revision}\""))
        } else {
            request.header(header::IF_NONE_MATCH, "*")
        };
        let response: PutResponse = decode(request.json(&body).send().await?).await?;
        Ok(response.meta.revision)
    }
}

async fn password_login_flow(
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
        .header("Abacus-Protocol", PROTOCOL)
        .json(&json!({"email": email, "password": password, "device_name": device_id()}))
        .send()
        .await?;
    decode(response).await
}

async fn device_login_flow(server: &str) -> Result<LoginResponse> {
    let start: DeviceStart = decode(
        anonymous_client()?
            .post(format!("{server}/v1/auth/device"))
            .header("Abacus-Protocol", PROTOCOL)
            .json(&json!({"device_name": device_id()}))
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
            .header("Abacus-Protocol", PROTOCOL)
            .json(&json!({"device_code": start.device_code}))
            .send()
            .await?;
        match response.status() {
            StatusCode::OK => return decode(response).await,
            StatusCode::PRECONDITION_REQUIRED => continue,
            StatusCode::TOO_MANY_REQUESTS => {
                interval += Duration::from_secs(1);
            }
            other => {
                let detail = response.text().await.unwrap_or_default();
                bail!(
                    "sync server returned {other}: {}",
                    detail.chars().take(1000).collect::<String>()
                );
            }
        }
    }
}

fn anonymous_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("abacus-agent/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

async fn decode<T: for<'de> Deserialize<'de>>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    if response
        .headers()
        .get("Abacus-Protocol")
        .and_then(|value| value.to_str().ok())
        != Some(PROTOCOL)
    {
        bail!("sync server did not confirm Abacus protocol version {PROTOCOL}");
    }
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail: String = body.chars().take(1000).collect();
        bail!("sync server returned {status}: {detail}");
    }
    serde_json::from_str(&body).context("sync server returned invalid JSON")
}

pub async fn handle(
    action: SyncCommand,
    paths: &AbacusPaths,
    workspace: std::path::PathBuf,
) -> Result<()> {
    let mut credentials = Credentials::load(paths)?;
    match action {
        SyncCommand::Login {
            server,
            email,
            password,
            password_login,
        } => {
            let server = server.trim_end_matches('/').to_owned();
            let login = if password_login || password.is_some() {
                password_login_flow(&server, email, password).await?
            } else {
                device_login_flow(&server).await?
            };
            credentials.sync = Some(SyncCredentials {
                server,
                token: login.access_token,
                email: login.user.email.clone(),
            });
            credentials.save(paths)?;
            println!(
                "Signed in as {}. Sessions now sync automatically across devices.",
                login.user.email
            );
        }
        SyncCommand::Logout => {
            credentials.sync = None;
            credentials.save(paths)?;
            println!("Signed out of session sync.");
        }
        SyncCommand::Status => {
            let client = configured(&credentials)?;
            let account = client.account().await?;
            println!("Signed in as {} at {}", account.email, client.server);
        }
        SyncCommand::Sessions => {
            for session in configured(&credentials)?.sessions().await? {
                let online = if session.remote_online { " remote" } else { "" };
                println!(
                    "{}\t{}\t{}\t{}\tr{}{}",
                    &session.id[..session.id.len().min(8)],
                    session.title,
                    session.model,
                    session.updated_at,
                    session.revision,
                    online
                );
            }
        }
        SyncCommand::Push { session, force } => {
            let client = configured(&credentials)?;
            let store = SessionStore::new(paths, workspace);
            let sessions = if let Some(id) = session {
                vec![store.load(&id)?]
            } else {
                store
                    .list()?
                    .into_iter()
                    .map(|summary| store.load(&summary.id.to_string()))
                    .collect::<Result<Vec<_>>>()?
            };
            for session in sessions {
                let trace_path = paths.traces_dir.join(format!("{}.jsonl", session.id));
                let trace = std::fs::read(&trace_path).unwrap_or_default();
                let revision = client.push(&session, &trace, force).await?;
                println!(
                    "Synced {} ({}) at revision {revision}",
                    session.title, session.id
                );
            }
        }
        SyncCommand::Pull { session, force } => {
            let client = configured(&credentials)?;
            let ids = if let Some(id) = session {
                vec![id]
            } else {
                client
                    .sessions()
                    .await?
                    .into_iter()
                    .map(|item| item.id)
                    .collect()
            };
            for id in ids {
                let (session, trace) = client.pull(&id).await?;
                install_pulled(paths, session.clone(), &trace, force)?;
                println!("Downloaded {} ({})", session.title, session.id);
            }
        }
    }
    Ok(())
}

fn configured(credentials: &Credentials) -> Result<SyncClient> {
    let credentials = credentials
        .sync
        .as_ref()
        .context("sync is not configured; run `abacus sync login`")?;
    SyncClient::new(credentials)
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

fn device_id() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "Abacus CLI".to_owned())
}

pub fn is_configured(credentials: &Credentials) -> bool {
    credentials.sync.is_some()
}

pub fn spawn_session_sync(paths: &AbacusPaths, session: &Session) {
    let paths = paths.clone();
    let session = session.clone();
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            let _ = push_session_best_effort(&paths, &session).await;
        });
    }
}

pub async fn push_all_updated_local(paths: &AbacusPaths) -> Result<usize> {
    push_all_updated_local_inner(paths, false).await
}

pub async fn push_all_local_force(paths: &AbacusPaths) -> Result<usize> {
    push_all_updated_local_inner(paths, true).await
}

async fn push_all_updated_local_inner(paths: &AbacusPaths, force: bool) -> Result<usize> {
    let Some(client) = optional_client(paths)? else {
        return Ok(0);
    };
    let base = &paths.sessions_dir;
    if !base.exists() {
        return Ok(0);
    }
    let mut pushed = 0;
    for workspace_dir in std::fs::read_dir(base)? {
        let workspace_dir = workspace_dir?;
        let directory = workspace_dir.path();
        if !directory.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Ok(content) = std::fs::read(&path) else {
                continue;
            };
            let Ok(session) = serde_json::from_slice::<Session>(&content) else {
                continue;
            };
            if !force && is_placeholder(&session) {
                continue;
            }
            let trace = std::fs::read(paths.traces_dir.join(format!("{}.jsonl", session.id)))
                .unwrap_or_default();
            let revision = client
                .revision(&session.id.to_string())
                .await
                .ok()
                .flatten();
            let result = if force {
                client.push(&session, &trace, true).await
            } else {
                client.push_at_revision(&session, &trace, revision).await
            };
            match result {
                Ok(_) => pushed += 1,
                Err(error) => {
                    let message = format!("{error:#}");
                    if message.contains("409") || message.contains("conflict") {
                        eprintln!(
                            "sync push conflict for {}: use `abacus sync push --force`",
                            session.id
                        );
                    }
                }
            }
        }
    }
    Ok(pushed)
}

pub async fn pull_workspace(paths: &AbacusPaths, workspace: &std::path::Path) -> Result<usize> {
    let Some(client) = optional_client(paths)? else {
        return Ok(0);
    };
    let mut pulled = 0;
    for remote in client.sessions().await? {
        let (session, trace) = client.pull(&remote.id).await?;
        if is_placeholder(&session) {
            // Screens that were opened but never used are noise on every
            // device; retire them from the server instead of reinstalling.
            let _ = client.delete_session(&remote.id, remote.revision).await;
            continue;
        }
        install_pulled(paths, session, &trace, true)?;
        pulled += 1;
    }
    Ok(pulled)
}

pub async fn push_session_now(paths: &AbacusPaths, session: &Session) -> Result<()> {
    let Some(client) = optional_client(paths)? else {
        return Ok(());
    };
    push_one(&client, paths, session).await?;
    Ok(())
}

fn optional_client(paths: &AbacusPaths) -> Result<Option<SyncClient>> {
    let credentials = Credentials::load(paths)?;
    credentials.sync.as_ref().map(SyncClient::new).transpose()
}

async fn push_session_best_effort(paths: &AbacusPaths, session: &Session) -> Result<()> {
    let Some(client) = optional_client(paths)? else {
        return Ok(());
    };
    let _ = push_one(&client, paths, session).await;
    Ok(())
}

pub fn is_placeholder(session: &Session) -> bool {
    // A session that has never received a prompt is not a session yet. The
    // transcript always opens with the system prompt, so "empty" means no user
    // message has ever been added.
    session.title == "New session"
        && !session
            .messages
            .iter()
            .any(|message| message["role"] == "user")
}

async fn push_one(client: &SyncClient, paths: &AbacusPaths, session: &Session) -> Result<u64> {
    if is_placeholder(session) {
        return Ok(
            match client
                .revision(&session.id.to_string())
                .await
                .ok()
                .flatten()
            {
                Some(revision) => revision,
                None => 0,
            },
        );
    }
    let trace =
        std::fs::read(paths.traces_dir.join(format!("{}.jsonl", session.id))).unwrap_or_default();
    // Read immediately before the conditional write. A concurrent writer still
    // produces a conflict; automatic sync never retries by overwriting it.
    let revision = client
        .revision(&session.id.to_string())
        .await
        .ok()
        .flatten();
    client.push_at_revision(session, &trace, revision).await
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
}

#[derive(Debug, Deserialize)]
struct TicketResponse {
    ticket: String,
}

impl SyncClient {
    pub async fn enable_remote(&self, session_id: &str) -> Result<String> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v1/remote/sessions/{session_id}/enable"),
            )
            .send()
            .await?;
        let _: serde_json::Value = decode(response).await?;
        let ticket: TicketResponse = decode(
            self.request(reqwest::Method::POST, "/v1/remote/tickets")
                .json(&json!({"session_id": session_id, "role": "agent"}))
                .send()
                .await?,
        )
        .await?;
        let mut url = reqwest::Url::parse(&self.server)?;
        url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
            .map_err(|_| anyhow::anyhow!("could not create remote WebSocket URL"))?;
        url.set_path(&format!("/v1/remote/agent/{session_id}"));
        url.set_query(Some(&format!("ticket={}", ticket.ticket)));
        Ok(url.to_string())
    }

    pub async fn disable_remote(&self, session_id: &str) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/v1/remote/sessions/{session_id}/disable"),
            )
            .send()
            .await?;
        let _: serde_json::Value = decode(response).await?;
        Ok(())
    }
}

pub fn configured_client(paths: &AbacusPaths) -> Result<SyncClient> {
    configured(&Credentials::load(paths)?)
}

#[derive(Debug, Deserialize)]
struct GetResponse {
    session: Session,
}

impl SyncClient {
    async fn pull(&self, session_id: &str) -> Result<(Session, Vec<u8>)> {
        let document: GetResponse = decode(
            self.request(
                reqwest::Method::GET,
                &format!("/v1/sync/sessions/{session_id}"),
            )
            .send()
            .await?,
        )
        .await?;
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("/v1/sync/sessions/{session_id}/trace"),
            )
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            bail!("sync trace download returned {status}");
        }
        Ok((document.session, response.bytes().await?.to_vec()))
    }

    async fn delete_session(&self, session_id: &str, revision: u64) -> Result<()> {
        let response = self
            .request(
                reqwest::Method::DELETE,
                &format!("/v1/sync/sessions/{session_id}"),
            )
            .header(header::IF_MATCH, format!("\"{revision}\""))
            .send()
            .await?;
        let _: serde_json::Value = decode(response).await?;
        Ok(())
    }
}

fn install_pulled(paths: &AbacusPaths, session: Session, trace: &[u8], force: bool) -> Result<()> {
    let store = SessionStore::for_session(paths, session.workspace.clone());
    if let Ok(local) = store.load(&session.id.to_string())
        && !force
    {
        let mut fork = local;
        fork.id = uuid::Uuid::new_v4();
        fork.title = format!("{} (local fork)", fork.title);
        store.save(&fork)?;
        let source = paths.traces_dir.join(format!("{}.jsonl", session.id));
        let destination = paths.traces_dir.join(format!("{}.jsonl", fork.id));
        if source.exists() {
            std::fs::copy(source, destination)?;
        }
        eprintln!("Preserved the previous local copy as {}", fork.id);
    }
    store.save(&session)?;
    std::fs::create_dir_all(&paths.traces_dir)?;
    crate::config::atomic_write(
        &paths.traces_dir.join(format!("{}.jsonl", session.id)),
        trace,
        true,
    )?;
    Ok(())
}
