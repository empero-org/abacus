use std::io::{self, Write};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, header};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    config::{AbacusPaths, Credentials, SyncCommand, SyncCredentials},
    session::{Session, SessionStore},
};

const PROTOCOL: &str = "1";

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
        } => {
            let email = match email {
                Some(value) => value,
                None => prompt("Email: ")?,
            };
            let password = match password {
                Some(value) => value,
                None => prompt_password()?,
            };
            let server = server.trim_end_matches('/').to_owned();
            let response = Client::builder()
                .user_agent(concat!("abacus-agent/", env!("CARGO_PKG_VERSION")))
                .build()?
                .post(format!("{server}/v1/auth/login"))
                .header("Abacus-Protocol", PROTOCOL)
                .json(&json!({"email": email, "password": password, "device_name": device_id()}))
                .send()
                .await?;
            let login: LoginResponse = decode(response).await?;
            credentials.sync = Some(SyncCredentials {
                server,
                token: login.access_token,
                email: login.user.email.clone(),
            });
            credentials.save(paths)?;
            println!(
                "Signed in as {}. Session sync is now configured.",
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

pub fn spawn_session_sync(paths: &AbacusPaths, session: &Session) {
    let paths = paths.clone();
    let session = session.clone();
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            push_session_best_effort(&paths, &session).await;
        });
    }
}

async fn push_session_best_effort(paths: &AbacusPaths, session: &Session) {
    let Ok(credentials) = Credentials::load(paths) else {
        return;
    };
    let Some(sync) = credentials.sync else {
        return;
    };
    let Ok(client) = SyncClient::new(&sync) else {
        return;
    };
    let trace =
        std::fs::read(paths.traces_dir.join(format!("{}.jsonl", session.id))).unwrap_or_default();
    // Read immediately before the conditional write. A concurrent writer still
    // produces a conflict; automatic sync never retries by overwriting it.
    let revision = client
        .revision(&session.id.to_string())
        .await
        .ok()
        .flatten();
    let _ = client.push_at_revision(session, &trace, revision).await;
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
