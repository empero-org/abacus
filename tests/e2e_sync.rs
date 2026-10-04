//! Session sync against a fake server speaking the real HTTP contract: what
//! goes over the wire, and what each device ends up with.

#[path = "support/sync_server.rs"]
mod sync_server;

use std::path::PathBuf;

use abacus_agent::{
    config::{AbacusPaths, Credentials, SyncCredentials},
    session::{Session, SessionStore},
    sync::{self, SyncError},
    sync_state::SyncState,
};
use serde_json::{Value, json};
use sync_server::SyncServer;

/// One machine: an Abacus home signed in to `server`.
struct Device {
    _home: tempfile::TempDir,
    paths: AbacusPaths,
    workspace: PathBuf,
}

impl Device {
    fn new(server: &SyncServer, token: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let paths = AbacusPaths::under(home.path().to_path_buf());
        let credentials = Credentials {
            keys: Default::default(),
            sync: Some(SyncCredentials { server: server.url(), token: token.into(), email: "me@example.com".into() }),
        };
        credentials.save(&paths).unwrap();
        Self { _home: home, paths, workspace: PathBuf::from("/work/project") }
    }

    fn store(&self) -> SessionStore {
        SessionStore::new(&self.paths, self.workspace.clone())
    }

    fn session(&self, prompts: &[&str]) -> Session {
        let mut session =
            self.store().create("p".into(), "m".into(), vec![json!({"role": "system", "content": "s"})]).unwrap();
        self.say(&mut session, prompts);
        session
    }

    fn say(&self, session: &mut Session, prompts: &[&str]) {
        let mut messages = session.messages.clone();
        for prompt in prompts {
            messages.push(json!({"role": "user", "content": prompt}));
            messages.push(json!({"role": "assistant", "content": format!("re: {prompt}")}));
        }
        let trace = self.paths.traces_dir.join(format!("{}.jsonl", session.id));
        std::fs::create_dir_all(&self.paths.traces_dir).unwrap();
        let mut lines = std::fs::read(&trace).unwrap_or_default();
        lines.extend(format!("{}\n", json!({"step": messages.len()})).into_bytes());
        std::fs::write(&trace, lines).unwrap();
        session.update_messages(messages);
        self.store().save(session).unwrap();
    }

    fn sessions(&self) -> Vec<Session> {
        let store = self.store();
        store.list().unwrap().iter().map(|summary| store.load(&summary.id.to_string()).unwrap()).collect()
    }
}

fn last_message(document: &Value) -> String {
    document["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_owned()
}

fn last(session: &Session) -> String {
    last_message(&serde_json::to_value(session).unwrap())
}

#[tokio::test]
async fn sync_pushes_once_then_conditionally_and_never_twice() {
    let server = SyncServer::start("t0k3n").await;
    let device = Device::new(&server, "t0k3n");
    let mut session = device.session(&["hello"]);
    let id = session.id.to_string();

    let outcome = sync::push_dirty(&device.paths).await.unwrap();
    assert_eq!((outcome.pushed, outcome.errors.len()), (1, 0), "{outcome:?}");
    let [create] = server.seen("PUT", "/v1/sync/sessions/").try_into().unwrap();
    assert_eq!(create.header("if-none-match"), Some("*"));
    assert_eq!(create.header("if-match"), None);
    assert_eq!(create.header("abacus-protocol"), Some("1"));
    assert_eq!(create.header("authorization"), Some("Bearer t0k3n"));
    assert_eq!(create.json()["device_id"], device.paths.install_id());
    let state = SyncState::load(&device.paths);
    assert_eq!(state.record(&session.id).unwrap().revision, Some(1));
    assert_eq!(device.paths.sync_state_file(), device.paths.root.join("sync-state.json"));

    // Nothing changed: nothing is sent, not even a listing.
    let before = server.seen("GET", "/").len();
    assert_eq!(sync::push_dirty(&device.paths).await.unwrap().pushed, 0);
    assert_eq!(server.seen("PUT", "/").len(), 1);
    assert_eq!(server.seen("GET", "/").len(), before);

    // A new turn goes up against the revision it was based on.
    device.say(&mut session, &["more"]);
    sync::push_session(&device.paths, &session).await.unwrap();
    let puts = server.seen("PUT", "/v1/sync/sessions/");
    assert_eq!(puts[1].header("if-match"), Some("\"1\""));
    assert_eq!(server.revision(&id), Some(2));
    assert_eq!(last_message(&server.stored(&id)), "re: more");
}

#[tokio::test]
async fn sync_pull_downloads_only_what_changed() {
    let server = SyncServer::start("t").await;
    let (laptop, desktop) = (Device::new(&server, "t"), Device::new(&server, "t"));
    let first = laptop.session(&["one"]);
    let mut second = laptop.session(&["two"]);
    sync::reconcile(&laptop.paths).await.unwrap();

    let outcome = sync::pull_changes(&desktop.paths).await.unwrap();
    assert_eq!(outcome.pulled, 2, "{outcome:?}");
    assert_eq!(desktop.sessions().len(), 2);

    laptop.say(&mut second, &["three"]);
    sync::push_dirty(&laptop.paths).await.unwrap();
    let documents = server.seen("GET", "/v1/sync/sessions/").len();
    let outcome = sync::pull_changes(&desktop.paths).await.unwrap();
    assert_eq!(outcome.pulled, 1, "{outcome:?}");
    let fetched = server.seen("GET", "/v1/sync/sessions/");
    assert!(fetched[documents..].iter().all(|seen| seen.path.contains(&second.id.to_string())));
    assert!(!fetched[documents..].iter().any(|seen| seen.path.contains(&first.id.to_string())));
    let changes = server.seen("GET", "/v1/sync/changes");
    assert!(changes.last().unwrap().path.contains("cursor=2"), "{:?}", changes.last().unwrap().path);
}

#[tokio::test]
async fn sync_a_remote_delete_removes_a_clean_local_copy() {
    let server = SyncServer::start("t").await;
    let device = Device::new(&server, "t");
    let session = device.session(&["short-lived"]);
    sync::reconcile(&device.paths).await.unwrap();
    server.delete(&session.id.to_string());

    let outcome = sync::pull_changes(&device.paths).await.unwrap();
    assert_eq!(outcome.deleted, 1, "{outcome:?}");
    assert!(device.sessions().is_empty());
    assert!(device.paths.root.join("sync-trash").join(format!("{}.json", session.id)).exists());
    assert!(SyncState::load(&device.paths).record(&session.id).is_none());
}

#[tokio::test]
async fn sync_a_newer_remote_revision_forks_a_changed_local_copy() {
    let server = SyncServer::start("t").await;
    let device = Device::new(&server, "t");
    let mut session = device.session(&["start"]);
    sync::reconcile(&device.paths).await.unwrap();

    // Another device continues the session differently…
    let mut theirs = serde_json::to_value(&session).unwrap();
    theirs["messages"].as_array_mut().unwrap().push(json!({"role": "user", "content": "their turn"}));
    server.write(theirs, b"{\"other\":true}\n");
    // …while this one continues it too.
    device.say(&mut session, &["my turn"]);

    let outcome = sync::pull_changes(&device.paths).await.unwrap();
    assert_eq!(outcome.forked, 1, "{outcome:?}");
    let sessions = device.sessions();
    let original = sessions.iter().find(|candidate| candidate.id == session.id).unwrap();
    let fork = sessions.iter().find(|candidate| candidate.id != session.id).unwrap();
    assert_eq!(last(original), "their turn");
    assert_eq!(last(fork), "re: my turn");
    assert!(fork.title.ends_with("(local fork)"));
}

#[tokio::test]
async fn sync_a_stale_revision_with_unchanged_content_is_retried() {
    let server = SyncServer::start("t").await;
    let device = Device::new(&server, "t");
    let mut session = device.session(&["start"]);
    let id = session.id.to_string();
    sync::reconcile(&device.paths).await.unwrap();
    server.touch(&id);
    device.say(&mut session, &["more"]);

    let outcome = sync::push_dirty(&device.paths).await.unwrap();
    assert_eq!((outcome.pushed, outcome.conflicts.len()), (1, 0), "{outcome:?}");
    let puts = server.seen("PUT", "/v1/sync/sessions/");
    let tried: Vec<_> = puts[1..].iter().map(|seen| seen.header("if-match").unwrap_or("").to_owned()).collect();
    assert_eq!(tried, ["\"1\"", "\"2\""]);
    assert_eq!(server.revision(&id), Some(3));
}

#[tokio::test]
async fn sync_a_real_conflict_is_reported_and_never_overwritten() {
    let server = SyncServer::start("t").await;
    let device = Device::new(&server, "t");
    let mut session = device.session(&["start"]);
    let id = session.id.to_string();
    sync::reconcile(&device.paths).await.unwrap();
    let mut theirs = serde_json::to_value(&session).unwrap();
    theirs["messages"].as_array_mut().unwrap().push(json!({"role": "user", "content": "their turn"}));
    server.write(theirs, b"");
    device.say(&mut session, &["my turn"]);

    let outcome = sync::push_dirty(&device.paths).await.unwrap();
    assert_eq!((outcome.pushed, outcome.conflicts.len()), (0, 1), "{outcome:?}");
    assert_eq!(last_message(&server.stored(&id)), "their turn");
    assert!(SyncState::load(&device.paths).record(&session.id).unwrap().conflict);
}

#[tokio::test]
async fn sync_a_rejected_token_is_an_unauthorized_error() {
    let server = SyncServer::start("right").await;
    let device = Device::new(&server, "wrong");
    device.session(&["hello"]);
    let error = sync::reconcile(&device.paths).await.unwrap_err();
    assert!(matches!(error.downcast_ref::<SyncError>(), Some(SyncError::Unauthorized)), "{error:#}");
    assert!(server.seen("PUT", "/").is_empty());
}

#[tokio::test]
async fn sync_client_covers_remote_pairing_and_usage() {
    let server = SyncServer::start("t").await;
    let device = Device::new(&server, "t");
    let session = device.session(&["share me"]);
    sync::push_dirty(&device.paths).await.unwrap();
    let id = session.id.to_string();
    let client = sync::configured_client(&device.paths).unwrap();

    client.enable_remote(&id).await.unwrap();
    let [enable] = server.seen("POST", "/v1/remote/sessions/").try_into().unwrap();
    assert_eq!(enable.json()["install_id"], device.paths.install_id());
    let ticket = client.agent_ticket(&id).await.unwrap();
    assert_eq!(ticket.ws_url, format!("/v1/remote/agent/{id}"), "filled in for older servers");
    assert_eq!(
        client.agent_socket_url(&ticket).unwrap(),
        format!("ws://{}/v1/remote/agent/{id}?ticket=one%2Fuse%2Bticket", server.address)
    );
    client.disable_remote(&id).await.unwrap();

    let pairing = client.pairing_url(Some(&id)).await.unwrap();
    assert_eq!((pairing.expires_in, pairing.session_id.as_deref()), (300, Some(id.as_str())));
    let reply = client.report_usage(&json!({"reports": [{"run_id": "r", "seq": 1}]})).await.unwrap();
    assert_eq!(reply["accepted"], 1);

    let (fetched, meta) = client.session(&id).await.unwrap();
    assert_eq!((fetched.id, meta.revision), (session.id, 1));
    assert_eq!(
        client.trace(&id).await.unwrap(),
        std::fs::read(device.paths.traces_dir.join(format!("{id}.jsonl"))).unwrap()
    );
    let changes = client.changes(0, 10).await.unwrap();
    assert_eq!((changes.items.len(), changes.next_cursor, changes.has_more), (1, 1, false));
}
