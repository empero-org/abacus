//! Session sync against a fake server speaking the real HTTP contract: what
//! goes over the wire, and what each device ends up with.

#[path = "support/sync_server.rs"]
mod sync_server;

use std::path::PathBuf;

use abacus_agent::{
    config::{AbacusPaths, Credentials, SyncCommand, SyncCredentials},
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

    fn trace_file(&self, session: &Session) -> PathBuf {
        self.paths.traces_dir.join(format!("{}.jsonl", session.id))
    }

    /// Grow the trace until its upload is well past the compression threshold.
    fn pad_trace(&self, session: &Session) -> Vec<u8> {
        let mut lines = std::fs::read(self.trace_file(session)).unwrap_or_default();
        for step in 0..2_000 {
            let line = json!({"step": step, "output": "the quick brown fox jumps over the lazy dog"});
            lines.extend(format!("{line}\n").into_bytes());
        }
        std::fs::write(self.trace_file(session), &lines).unwrap();
        lines
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

fn keys(puts: &[sync_server::Seen]) -> Vec<String> {
    puts.iter().map(|seen| seen.header("idempotency-key").expect("every write carries a key").to_owned()).collect()
}

#[tokio::test]
async fn sync_a_retried_write_keeps_its_key_and_is_replayed() {
    let server = SyncServer::start("t").await;
    let device = Device::new(&server, "t");
    let mut session = device.session(&["hello"]);
    let id = session.id.to_string();
    // The upload lands, but its reply never arrives.
    server.lose_replies(1);

    let outcome = sync::push_dirty(&device.paths).await.unwrap();
    assert_eq!((outcome.pushed, outcome.errors.len(), outcome.conflicts.len()), (1, 0, 0), "{outcome:?}");
    let sent = keys(&server.seen("PUT", "/v1/sync/sessions/"));
    assert_eq!(sent.len(), 2, "one retry");
    assert_eq!(sent[0], sent[1], "the retry is the same write");
    assert_eq!((server.replays(), server.revision(&id)), (1, Some(1)), "answered from the record, applied once");
    assert_eq!(SyncState::load(&device.paths).record(&session.id).unwrap().revision, Some(1));

    // The next write is another write: reusing the key would be refused.
    device.say(&mut session, &["more"]);
    sync::push_session(&device.paths, &session).await.unwrap();
    let sent = keys(&server.seen("PUT", "/v1/sync/sessions/"));
    assert_eq!(sent.len(), 3);
    assert_ne!(sent[2], sent[0]);
    assert_eq!(server.revision(&id), Some(2));
}

#[tokio::test]
async fn sync_a_write_without_an_answer_is_sent_again_under_its_key() {
    let server = SyncServer::start("t").await;
    let device = Device::new(&server, "t");
    let session = device.session(&["hello"]);
    let id = session.id.to_string();
    // Every attempt of one automatic pass goes unanswered.
    server.lose_replies(3);

    let outcome = sync::push_dirty(&device.paths).await.unwrap();
    assert_eq!((outcome.pushed, outcome.errors.len()), (0, 1), "{outcome:?}");
    assert!(SyncState::load(&device.paths).record(&session.id).is_none(), "nothing was confirmed");

    // `abacus sync push` later: the same write, so the same key.
    sync::handle(SyncCommand::Push { session: None, force: false }, &device.paths, device.workspace.clone())
        .await
        .unwrap();
    let sent = keys(&server.seen("PUT", "/v1/sync/sessions/"));
    assert_eq!(sent.len(), 4);
    assert!(sent.iter().all(|key| *key == sent[0]), "{sent:?}");
    assert_eq!((server.replays(), server.revision(&id)), (3, Some(1)));
    assert_eq!(SyncState::load(&device.paths).record(&session.id).unwrap().revision, Some(1));
}

#[tokio::test]
async fn sync_a_write_to_a_deleted_session_keeps_the_work_as_a_new_session() {
    let server = SyncServer::start("t").await;
    let device = Device::new(&server, "t");
    let mut session = device.session(&["start"]);
    let id = session.id.to_string();
    sync::reconcile(&device.paths).await.unwrap();
    server.delete(&id);
    device.say(&mut session, &["after the delete"]);

    // No pull in between: the upload is what finds out.
    let outcome = sync::push_dirty(&device.paths).await.unwrap();
    assert_eq!((outcome.deleted, outcome.pushed), (1, 1), "{outcome:?}");
    assert!(outcome.conflicts.is_empty() && outcome.errors.is_empty(), "{outcome:?}");
    let puts = server.seen("PUT", "/v1/sync/sessions/");
    assert_eq!(puts.len(), 3, "the create, the write the tombstone refused, the kept work as a new session");
    assert_eq!(puts[1].header("if-match"), Some("\"1\""));
    assert_eq!(puts[2].header("if-none-match"), Some("*"));

    let [kept] = device.sessions().try_into().unwrap();
    assert_ne!(kept.id, session.id);
    assert_eq!(last(&kept), "re: after the delete");
    assert_eq!(server.revision(&kept.id.to_string()), Some(1));
    assert_eq!(last_message(&server.stored(&kept.id.to_string())), "re: after the delete");
    assert!(device.paths.root.join("sync-trash").join(format!("{id}.json")).exists());
    assert!(SyncState::load(&device.paths).record(&session.id).is_none());
    assert!(outcome.notices[0].contains("kept as a new session"), "{:?}", outcome.notices);
}

#[tokio::test]
async fn sync_large_bodies_travel_gzipped_in_both_directions() {
    let server = SyncServer::start("t").await;
    let (laptop, desktop) = (Device::new(&server, "t"), Device::new(&server, "t"));
    let small = laptop.session(&["tiny"]);
    let big = laptop.session(&["big"]);
    let trace = laptop.pad_trace(&big);

    let outcome = sync::push_dirty(&laptop.paths).await.unwrap();
    assert_eq!((outcome.pushed, outcome.errors.len()), (2, 0), "{outcome:?}");
    let puts = server.seen("PUT", "/v1/sync/sessions/");
    let upload = |session: &Session| puts.iter().find(|seen| seen.path.ends_with(&session.id.to_string())).unwrap();
    assert_eq!(upload(&small).header("content-encoding"), None, "small bodies stay plain");
    let large = upload(&big);
    assert_eq!(large.header("content-encoding"), Some("gzip"));
    assert!(large.body.len() * 10 < trace.len(), "{} bytes sent for a {} byte trace", large.body.len(), trace.len());
    assert_eq!(large.json()["device_id"], laptop.paths.install_id());
    assert_eq!(server.trace(&big.id.to_string()), trace, "the server holds the original bytes");

    let outcome = sync::pull_changes(&desktop.paths).await.unwrap();
    assert_eq!(outcome.pulled, 2, "{outcome:?}");
    for get in server.seen("GET", "/v1/sync/sessions/") {
        assert!(get.header("accept-encoding").is_some_and(|value| value.contains("gzip")), "{get:?}");
    }
    assert!(server.gzipped_responses() >= 1, "the trace came down compressed");
    assert_eq!(std::fs::read(desktop.trace_file(&big)).unwrap(), trace);
    let client = sync::configured_client(&desktop.paths).unwrap();
    assert_eq!(client.trace(&big.id.to_string()).await.unwrap(), trace);
    assert_eq!(client.session(&big.id.to_string()).await.unwrap().0.id, big.id);
}

#[tokio::test]
async fn sync_a_server_that_refuses_gzip_is_sent_plain_bodies_from_then_on() {
    let server = SyncServer::start("t").await;
    server.refuse_gzip();
    let device = Device::new(&server, "t");
    let mut session = device.session(&["big"]);
    let id = session.id.to_string();
    let trace = device.pad_trace(&session);

    let outcome = sync::push_dirty(&device.paths).await.unwrap();
    assert_eq!((outcome.pushed, outcome.errors.len()), (1, 0), "{outcome:?}");
    let puts = server.seen("PUT", "/v1/sync/sessions/");
    assert_eq!(puts.len(), 2, "tried compressed, then plain");
    assert_eq!((puts[0].header("content-encoding"), puts[1].header("content-encoding")), (Some("gzip"), None));
    let sent = keys(&puts);
    assert_eq!(sent[0], sent[1], "the same write");
    assert_eq!(server.trace(&id), trace);

    // Remembered: the next large write does not try again.
    device.say(&mut session, &["more"]);
    sync::push_session(&device.paths, &session).await.unwrap();
    let puts = server.seen("PUT", "/v1/sync/sessions/");
    assert_eq!(puts.len(), 3);
    assert_eq!(puts[2].header("content-encoding"), None);
    assert_eq!(server.revision(&id), Some(2));
}

/// A session resumed at every start is open at every pull, so a newer copy
/// from another device is held back each time and never settled. Leaving it
/// settles it: both copies are kept, and nothing is left waiting.
#[tokio::test]
async fn sync_settles_a_held_session_once_it_is_closed() {
    let server = SyncServer::start("t0k3n").await;
    let device = Device::new(&server, "t0k3n");
    let mut session = device.session(&["start"]);
    let id = session.id.to_string();
    sync::push_dirty(&device.paths).await.unwrap();

    // Another device answers one more prompt; this one, with the session open, too.
    let mut elsewhere = server.stored(&id);
    let messages = elsewhere["messages"].as_array_mut().unwrap();
    messages.push(json!({"role": "user", "content": "asked there"}));
    messages.push(json!({"role": "assistant", "content": "answered there"}));
    server.write(elsewhere, &server.trace(&id));
    sync::session_opened(session.id);
    device.say(&mut session, &["here"]);
    let outcome = sync::reconcile(&device.paths).await.unwrap();
    assert_eq!((outcome.held.len(), outcome.pushed), (1, 0), "{outcome:?}");
    assert_eq!(last(&device.store().load(&id).unwrap()), "re: here", "the open copy is left alone");

    let requests = server.seen("GET", "/").len();
    let other = device.session(&["unrelated"]);
    let quiet = sync::settle_closed(&device.paths, other.id).await.unwrap();
    assert_eq!((quiet.pulled, server.seen("GET", "/").len()), (0, requests), "nothing waits on it");

    let outcome = sync::settle_closed(&device.paths, session.id).await.unwrap();
    assert_eq!(outcome.forked, 1, "{outcome:?}");
    assert_eq!(last(&device.store().load(&id).unwrap()), "answered there");
    let fork = device.sessions().into_iter().find(|kept| kept.title.ends_with("(local fork)")).unwrap();
    assert_eq!(last(&fork), "re: here");
    assert!(!SyncState::load(&device.paths).record(&session.id).unwrap().pending());
    // The kept work goes up with the next upload, and the session is in step.
    assert_eq!(sync::push_dirty(&device.paths).await.unwrap().pushed, 2, "the fork and the unrelated session");
    assert_eq!(server.revision(&id), Some(2));
}
