//! Incremental pull and push.
//!
//! Pull reads the account's change feed from the saved cursor and downloads
//! only sessions whose revision moved; push uploads only sessions whose
//! content changed since they last synced, conditional on the revision they
//! were based on. Neither direction overwrites the other side's work: when
//! both changed, the local copy is kept as a fork and the remote one
//! installed, and a refused upload waits for the next pull to do that. An
//! upload that finds its session deleted elsewhere settles it on the spot: the
//! work becomes a new session and goes up in the same pass.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::client::{ChangesPage, Precondition, RemoteDocument, SessionMeta, SyncClient, SyncError};
use crate::config::{AbacusPaths, atomic_write};
use crate::session::{Session, SessionStore};
use crate::sync_state::{
    Fingerprint, Hashes, Local, LocalEntry, Rejected, SessionRecord, Snapshot, SyncState, content_sha256, inspect,
    is_placeholder_document, local_sessions, sha256_hex, trace_path,
};

/// Changes requested per page of the feed.
const PAGE_SIZE: u32 = 200;
/// Transfers in flight at once.
const PARALLEL: usize = 4;
/// A feed that keeps claiming more pages than this is not trusted to end.
const MAX_PAGES: usize = 5_000;
/// Per-session upload backoff within one process: 5 s doubling to 5 min, and
/// no more tries after five failures until the process restarts.
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_CAP: Duration = Duration::from_secs(300);
const BACKOFF_ATTEMPTS: u32 = 5;

static FAILURES: LazyLock<Mutex<HashMap<Uuid, (u32, Instant)>>> = LazyLock::new(Mutex::default);

/// The server operations the engine needs; implemented by [`SyncClient`] and,
/// in tests, by an in-memory server.
pub(crate) trait Remote: Sync {
    fn changes(&self, cursor: u64, limit: u32) -> impl Future<Output = Result<ChangesPage, SyncError>> + Send;
    fn document(&self, id: &str) -> impl Future<Output = Result<RemoteDocument, SyncError>> + Send;
    fn trace(&self, id: &str, size_hint: u64) -> impl Future<Output = Result<Vec<u8>, SyncError>> + Send;
    fn put(
        &self,
        id: &str,
        document: &Value,
        trace: &[u8],
        hashes: &Hashes,
        precondition: Precondition,
    ) -> impl Future<Output = Result<SessionMeta, SyncError>> + Send;
    fn delete(&self, id: &str, revision: u64) -> impl Future<Output = Result<SessionMeta, SyncError>> + Send;
}

impl Remote for SyncClient {
    fn changes(&self, cursor: u64, limit: u32) -> impl Future<Output = Result<ChangesPage, SyncError>> + Send {
        SyncClient::changes(self, cursor, limit)
    }
    fn document(&self, id: &str) -> impl Future<Output = Result<RemoteDocument, SyncError>> + Send {
        SyncClient::document(self, id)
    }
    fn trace(&self, id: &str, size_hint: u64) -> impl Future<Output = Result<Vec<u8>, SyncError>> + Send {
        SyncClient::trace_sized(self, id, size_hint)
    }
    fn put(
        &self,
        id: &str,
        document: &Value,
        trace: &[u8],
        hashes: &Hashes,
        precondition: Precondition,
    ) -> impl Future<Output = Result<SessionMeta, SyncError>> + Send {
        SyncClient::put_document(self, id, document, trace, hashes, precondition)
    }
    fn delete(&self, id: &str, revision: u64) -> impl Future<Output = Result<SessionMeta, SyncError>> + Send {
        SyncClient::delete(self, id, revision)
    }
}

/// What one sync pass did.
#[derive(Debug, Default)]
pub struct SyncOutcome {
    /// Sessions written locally from the server.
    pub pulled: usize,
    /// Sessions uploaded.
    pub pushed: usize,
    /// Sessions that changed on both sides; the local copy was kept under a
    /// new id and the server's revision installed.
    pub forked: usize,
    /// Uploads the server refused because it moved on, unresolved until the
    /// next pull; one line per session.
    pub conflicts: Vec<String>,
    pub errors: Vec<String>,
    /// Changes that needed no transfer: already applied, or equal on both sides.
    pub skipped: usize,
    /// Sessions deleted on another device and settled here.
    pub deleted: usize,
    /// Newer revisions of sessions open in this process, not written over
    /// them; the front end decides whether to switch (see
    /// [`crate::sync::accept_held`]).
    pub held: Vec<HeldUpdate>,
    /// Forks and deletions, phrased for a person.
    pub notices: Vec<String>,
    /// One line per session that moved, for command output.
    pub(crate) lines: Vec<String>,
}

/// A newer revision of a session this process has open.
#[derive(Debug)]
pub struct HeldUpdate {
    pub session_id: Uuid,
    pub(crate) meta: SessionMeta,
    pub(crate) document: Value,
    pub(crate) trace: Option<Vec<u8>>,
}

impl SyncOutcome {
    /// A short status line, or `None` when nothing worth saying happened.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        let mut moved = Vec::new();
        if self.pulled > 0 {
            moved.push(format!("↓{}", self.pulled));
        }
        if self.pushed > 0 {
            moved.push(format!("↑{}", self.pushed));
        }
        if !moved.is_empty() {
            parts.push(format!("synced {}", moved.join(" ")));
        }
        if self.forked > 0 {
            parts.push(format!("{} forked", self.forked));
        }
        if !self.conflicts.is_empty() {
            parts.push(plural(self.conflicts.len(), "conflict"));
        }
        if self.deleted > 0 {
            parts.push(format!("{} deleted elsewhere", self.deleted));
        }
        match self.errors.as_slice() {
            [] => {}
            [only] if parts.is_empty() => parts.push(format!("sync failed: {}", crate::text::clip(only, 80, "…"))),
            errors => parts.push(plural(errors.len(), "sync error")),
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// One change to look at: the id, and the server's metadata when the feed
/// supplied it (a pending record is fetched without).
struct Work {
    id: Uuid,
    meta: Option<SessionMeta>,
}

/// A downloaded revision. `trace` is `None` when the local trace already has
/// the same hash, so it was not downloaded again.
struct Fetched {
    id: Uuid,
    meta: SessionMeta,
    document: Value,
    trace: Option<Vec<u8>>,
}

/// How two transcripts of the same session relate.
#[derive(Debug, PartialEq, Eq)]
enum Lineage {
    /// The remote transcript continues the local one.
    RemoteAhead,
    /// The local transcript continues the remote one.
    LocalAhead,
    Diverged,
}

fn lineage(local: &Value, remote: &Value) -> Lineage {
    let (Some(local), Some(remote)) = (local["messages"].as_array(), remote["messages"].as_array()) else {
        return Lineage::Diverged;
    };
    if remote.len() > local.len() && remote.starts_with(local) {
        Lineage::RemoteAhead
    } else if local.len() > remote.len() && local.starts_with(remote) {
        Lineage::LocalAhead
    } else {
        Lineage::Diverged
    }
}

/// Whether a local copy this device has no record of holds nothing that the
/// deleted revision `deleted` lacked: it is that revision byte for byte, a
/// placeholder, or last changed no later than it. Such a copy is a leftover —
/// typically of a delete that older versions of Abacus never applied — and
/// follows the delete into the sync trash instead of coming back as a new
/// session on every device that kept one.
fn superseded(snapshot: &Snapshot, deleted: &SessionMeta) -> bool {
    if is_placeholder_document(&snapshot.document) {
        return true;
    }
    if !deleted.session_sha256.is_empty()
        && snapshot.hashes.session == deleted.session_sha256
        && snapshot.hashes.trace == deleted.trace_sha256
    {
        return true;
    }
    match (timestamp(snapshot.document["updated_at"].as_str()), timestamp(Some(&deleted.updated_at))) {
        (Some(local), Some(deleted)) => local <= deleted,
        _ => false,
    }
}

/// An RFC 3339 time, or a naive one (servers on SQLite send those), as UTC.
fn timestamp(value: Option<&str>) -> Option<chrono::DateTime<Utc>> {
    let value = value?;
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f").map(|time| time.and_utc()))
        .ok()
}

fn title_of(document: &Value, fallback: &str) -> String {
    document["title"].as_str().filter(|title| !title.is_empty()).unwrap_or(fallback).to_owned()
}

fn short(id: &Uuid) -> String {
    id.to_string()[..8].to_owned()
}

pub(crate) struct Engine<'a, R: Remote> {
    remote: &'a R,
    paths: &'a AbacusPaths,
    pub state: SyncState,
    pub outcome: SyncOutcome,
    /// `--force`: pulls replace local copies without forking, pushes replace
    /// the server's revision.
    force: bool,
    /// A command the user is watching: per-session backoff does not apply.
    manual: bool,
    page_size: u32,
    /// Never-prompted sessions found on the server, retired after the pass.
    retire: Vec<(Uuid, u64)>,
    /// New sessions made from local work whose original was deleted elsewhere,
    /// waiting for their first upload.
    kept: Vec<Uuid>,
}

impl<'a, R: Remote> Engine<'a, R> {
    pub fn new(remote: &'a R, paths: &'a AbacusPaths, state: SyncState) -> Self {
        Self {
            remote,
            paths,
            state,
            outcome: SyncOutcome::default(),
            force: false,
            manual: false,
            page_size: PAGE_SIZE,
            retire: Vec::new(),
            kept: Vec::new(),
        }
    }

    pub fn manual(mut self, force: bool) -> Self {
        self.manual = true;
        self.force = force;
        self
    }

    /// Close the pass: save the state, and hand back what happened or the
    /// error that ended it.
    pub fn finish(mut self, result: Result<(), SyncError>) -> Result<SyncOutcome, SyncError> {
        self.save();
        result.map(|()| self.outcome)
    }

    fn save(&mut self) {
        if let Err(error) = self.state.save() {
            let message = format!("could not save sync state: {error:#}");
            if !self.outcome.errors.contains(&message) {
                self.outcome.errors.push(message);
            }
        }
    }

    /// Note a failed item; a fatal error ends the pass instead.
    fn note(&mut self, title: &str, error: SyncError) -> Result<(), SyncError> {
        if error.is_fatal() {
            return Err(error);
        }
        self.outcome.errors.push(format!("{title}: {error}"));
        Ok(())
    }

    /// Bring every session that changed on the server since the last pull
    /// down to this device. `full` re-reads the whole feed, which costs one
    /// listing; revisions already held are still skipped.
    pub async fn pull(&mut self, full: bool) -> Result<(), SyncError> {
        let mut index = local_sessions(self.paths);
        self.settle_deleted(&mut index);
        // Revisions known to be ahead are fetched wherever the cursor is.
        let pending = self
            .state
            .sessions
            .iter()
            .filter(|(_, record)| record.pending() && !record.deleted)
            .filter_map(|(id, _)| Uuid::parse_str(id).ok())
            .map(|id| Work { id, meta: None })
            .collect::<Vec<_>>();
        if !pending.is_empty() {
            self.process(pending, &mut index).await?;
            self.save();
        }
        let mut cursor = if full { 0 } else { self.state.cursor };
        let mut complete = true;
        for _ in 0..MAX_PAGES {
            let page = match self.remote.changes(cursor, self.page_size).await {
                Ok(page) => page,
                Err(error) => {
                    complete = false;
                    self.note("change feed", error)?;
                    break;
                }
            };
            let work = page
                .items
                .into_iter()
                .filter_map(|meta| Uuid::parse_str(&meta.id).ok().map(|id| Work { id, meta: Some(meta) }))
                .collect();
            // The cursor moves past a page only once all of it is applied, so
            // a failed download is retried next time instead of skipped.
            if !self.process(work, &mut index).await? {
                complete = false;
                break;
            }
            let advanced = page.next_cursor > cursor;
            cursor = cursor.max(page.next_cursor);
            self.state.set_cursor(cursor);
            self.save();
            if !page.has_more || !advanced {
                break;
            }
        }
        if complete {
            self.state.last_pull_at = Some(Utc::now());
        }
        self.save();
        self.retire_placeholders().await;
        Ok(())
    }

    /// Download one session, whatever the cursor says.
    pub async fn pull_one(&mut self, id: Uuid) -> Result<(), SyncError> {
        let mut index = local_sessions(self.paths);
        self.process(vec![Work { id, meta: None }], &mut index).await?;
        self.retire_placeholders().await;
        Ok(())
    }

    /// Plan, download (concurrently) and apply a batch of changes. Returns
    /// whether every change was applied.
    async fn process(&mut self, work: Vec<Work>, index: &mut HashMap<Uuid, LocalEntry>) -> Result<bool, SyncError> {
        let mut downloads = Vec::new();
        for item in work {
            if let Some(local_trace) = self.plan(&item, index).await {
                downloads.push((item, local_trace));
            }
        }
        let remote = self.remote;
        let offline = AtomicBool::new(false);
        let offline = &offline;
        let mut results = stream::iter(downloads)
            .map(|(item, local_trace)| async move {
                let id = item.id;
                if offline.load(Ordering::Relaxed) {
                    return (id, Err(SyncError::Transient("skipped while offline".into())));
                }
                let fetched = fetch(remote, item, local_trace).await;
                if matches!(fetched, Err(SyncError::Transient(_))) {
                    offline.store(true, Ordering::Relaxed);
                }
                (id, fetched)
            })
            .buffer_unordered(PARALLEL);
        let mut complete = true;
        let mut reported_offline = false;
        while let Some((id, fetched)) = results.next().await {
            match fetched {
                Ok(fetched) => {
                    if let Err(error) = self.apply(fetched, index).await {
                        self.outcome.errors.push(format!("{}: {error:#}", short(&id)));
                        complete = false;
                    }
                }
                // Deleted between the listing and the download.
                Err(SyncError::Gone) => {
                    self.tombstone(id, None, index);
                }
                Err(SyncError::Transient(_)) if reported_offline => complete = false,
                Err(error) => {
                    reported_offline |= matches!(error, SyncError::Transient(_));
                    self.note(&short(&id), error)?;
                    complete = false;
                }
            }
        }
        Ok(complete)
    }

    /// Decide what a change needs without touching the network. `None` means
    /// it was handled here; otherwise it needs a download, and the value is
    /// the local trace's hash, if known, so an identical trace is not fetched.
    async fn plan(&mut self, item: &Work, index: &mut HashMap<Uuid, LocalEntry>) -> Option<Option<String>> {
        let record = self.state.record(&item.id).cloned();
        if let Some(meta) = &item.meta {
            if meta.deleted {
                self.tombstone(item.id, Some(meta), index);
                return None;
            }
            // `--force` decides conflicts; a revision this copy already has is
            // not one, so it is skipped all the same.
            if let Some(record) = &record
                && record.revision == Some(meta.revision)
                && !record.pending()
            {
                self.outcome.skipped += 1;
                return None;
            }
        }
        let Some(entry) = index.get(&item.id).cloned() else {
            return Some(None);
        };
        match inspect_off(entry, record.clone()).await {
            Ok(Local::Clean(_)) => Some(record.map(|record| record.local_trace_sha256)),
            Ok(Local::Dirty(snapshot) | Local::Untracked(snapshot)) => {
                // Byte-identical on both sides (an upload whose reply was
                // lost, or a state file that was): adopt the revision.
                if let Some(meta) = &item.meta
                    && snapshot.hashes.session == meta.session_sha256
                    && snapshot.hashes.trace == meta.trace_sha256
                {
                    self.state.mark_synced(&item.id, meta, &snapshot.hashes, Some(snapshot.fingerprint));
                    self.outcome.skipped += 1;
                    return None;
                }
                Some(Some(snapshot.hashes.trace))
            }
            Err(_) => Some(None),
        }
    }

    /// Put a downloaded revision in place, keeping anything local it would
    /// otherwise replace.
    async fn apply(&mut self, fetched: Fetched, index: &mut HashMap<Uuid, LocalEntry>) -> Result<()> {
        let Fetched { id, meta, document, trace } = fetched;
        // The file is named after the document's id: one that names another
        // session would be written over that session's file.
        if document["id"].as_str() != Some(id.to_string().as_str()) {
            bail!("the server sent a different session's document");
        }
        let title = title_of(&document, &meta.title);
        let record = self.state.record(&id).cloned();
        if record.as_ref().is_some_and(|record| record.revision == Some(meta.revision)) {
            // A pending flag that outlived its cause: nothing is newer.
            self.state.update(&id, |record| {
                record.conflict = false;
                record.behind = None;
            });
            self.outcome.skipped += 1;
            return Ok(());
        }
        let entry = index.get(&id).cloned();
        if is_placeholder_document(&document) && entry.is_none() {
            // Screens opened but never used are noise on every device.
            self.retire.push((id, meta.revision));
            return Ok(());
        }
        if super::is_open(&id) {
            let revision = meta.revision;
            self.state.update(&id, |record| record.behind = Some(revision));
            self.outcome.held.push(HeldUpdate { session_id: id, meta, document, trace });
            return Ok(());
        }
        let local = match &entry {
            Some(entry) => Some(inspect_off(entry.clone(), record).await?),
            None => None,
        };
        let snapshot = match local {
            None | Some(Local::Clean(_)) => None,
            Some(Local::Dirty(snapshot) | Local::Untracked(snapshot)) => Some(snapshot),
        };
        let Some(snapshot) = snapshot else {
            let installed = self.install(&meta, document, trace, entry).await?;
            index.insert(id, installed);
            self.outcome.pulled += 1;
            self.outcome.lines.push(format!("↓ {title} ({}) — revision {}", short(&id), meta.revision));
            return Ok(());
        };
        if content_sha256(&document) == snapshot.hashes.content && meta.trace_sha256 == snapshot.hashes.trace {
            // The same conversation; only bookkeeping such as time spent
            // differs. Keep the local file as it is.
            self.state.mark_synced(&id, &meta, &snapshot.hashes, Some(snapshot.fingerprint));
            self.outcome.skipped += 1;
            return Ok(());
        }
        let entry = entry.context("a changed local copy has a file")?;
        let relation = if self.force { Lineage::RemoteAhead } else { lineage(&snapshot.document, &document) };
        match relation {
            Lineage::RemoteAhead => {
                let installed = self.install(&meta, document, trace, Some(entry)).await?;
                index.insert(id, installed);
                self.outcome.pulled += 1;
                self.outcome.lines.push(format!("↓ {title} ({}) — revision {}", short(&id), meta.revision));
            }
            Lineage::LocalAhead => {
                // The server holds an earlier point of this conversation.
                // Base the local copy on that revision; it stays changed, so
                // the push that follows uploads it over exactly that revision.
                let remote = Hashes::of(&document, meta.trace_sha256.clone());
                self.state.mark_synced(&id, &meta, &remote, None);
                self.outcome.skipped += 1;
            }
            Lineage::Diverged => {
                let (paths, local, mine) = (self.paths.clone(), entry.clone(), snapshot.document);
                let kept = off_runtime(move || fork(&paths, &local, mine, Some("(local fork)"))).await?;
                let fork_id = kept.id;
                index.insert(fork_id, kept);
                let installed = self.install(&meta, document, trace, Some(entry)).await?;
                index.insert(id, installed);
                self.outcome.pulled += 1;
                self.outcome.forked += 1;
                let notice = format!(
                    "“{title}” changed here and on another device; your copy was kept as “{title} (local fork)” ({})",
                    short(&fork_id)
                );
                self.outcome.lines.push(format!("! {notice}"));
                self.outcome.notices.push(notice);
            }
        }
        Ok(())
    }

    /// [`install`] with the file work off the runtime.
    async fn install(
        &mut self,
        meta: &SessionMeta,
        document: Value,
        trace: Option<Vec<u8>>,
        existing: Option<LocalEntry>,
    ) -> Result<LocalEntry> {
        let (paths, revision) = (self.paths.clone(), meta.clone());
        let written =
            off_runtime(move || write_revision(&paths, &revision, &document, trace.as_deref(), existing.as_ref()))
                .await?;
        self.state.mark_synced(&written.entry.id, meta, &written.hashes, Some(written.fingerprint));
        Ok(written.entry)
    }

    /// The server deleted a session. A copy unchanged since it last synced is
    /// moved to the sync trash, so the delete reaches every device; a copy
    /// with work the server never saw is kept under a new id, so that work
    /// survives and syncs without resurrecting the deleted one. A session
    /// open in this process is left alone until a later pull.
    fn tombstone(
        &mut self,
        id: Uuid,
        meta: Option<&SessionMeta>,
        index: &mut HashMap<Uuid, LocalEntry>,
    ) -> Option<Uuid> {
        let Some(entry) = index.get(&id).cloned() else {
            if self.state.record(&id).is_some() {
                self.state.forget(&id);
            }
            self.outcome.skipped += 1;
            return None;
        };
        let title = meta.map(|meta| meta.title.clone()).filter(|title| !title.is_empty()).unwrap_or_else(|| short(&id));
        if super::is_open(&id) {
            self.state.update(&id, |record| {
                record.deleted = true;
                record.conflict = false;
                record.behind = None;
            });
            return None;
        }
        match self.settle(&entry, meta, index) {
            Ok(Some(kept)) => {
                self.outcome.deleted += 1;
                let notice = format!(
                    "“{title}” was deleted on another device; the changes made here were kept as a new session ({})",
                    short(&kept)
                );
                self.outcome.lines.push(format!("– {notice}"));
                self.outcome.notices.push(notice);
                Some(kept)
            }
            Ok(None) => {
                self.outcome.deleted += 1;
                let notice = format!("“{title}” was deleted on another device");
                self.outcome.lines.push(format!("– {notice} ({})", short(&id)));
                self.outcome.notices.push(notice);
                None
            }
            Err(error) => {
                self.outcome.errors.push(format!("{}: {error:#}", short(&id)));
                None
            }
        }
    }

    /// Apply a remote delete to one local copy; returns the new id when
    /// unsynced work was kept. `deleted` is the tombstone, when known.
    fn settle(
        &mut self,
        entry: &LocalEntry,
        deleted: Option<&SessionMeta>,
        index: &mut HashMap<Uuid, LocalEntry>,
    ) -> Result<Option<Uuid>> {
        let record = self.state.record(&entry.id).cloned();
        let kept = match inspect(entry, record.as_ref())? {
            Local::Clean(_) => None,
            Local::Untracked(snapshot) if deleted.is_some_and(|deleted| superseded(&snapshot, deleted)) => None,
            Local::Dirty(snapshot) | Local::Untracked(snapshot) => {
                let kept = fork(self.paths, entry, snapshot.document, None)?;
                let kept_id = kept.id;
                index.insert(kept_id, kept);
                Some(kept_id)
            }
        };
        trash(self.paths, entry)?;
        index.remove(&entry.id);
        self.state.forget(&entry.id);
        Ok(kept)
    }

    /// Deletes that arrived while their session was open, now that it is not.
    fn settle_deleted(&mut self, index: &mut HashMap<Uuid, LocalEntry>) {
        let waiting = self
            .state
            .sessions
            .iter()
            .filter(|(_, record)| record.deleted)
            .filter_map(|(id, _)| Uuid::parse_str(id).ok())
            .filter(|id| !super::is_open(id))
            .collect::<Vec<_>>();
        for id in waiting {
            self.tombstone(id, None, index);
        }
    }

    async fn retire_placeholders(&mut self) {
        for (id, revision) in std::mem::take(&mut self.retire) {
            let _ = self.remote.delete(&id.to_string(), revision).await;
        }
    }

    /// Upload every local session that changed since it last synced — or just
    /// `only` — conditional on the revision it was based on.
    pub async fn push(&mut self, only: Option<Uuid>) -> Result<(), SyncError> {
        let mut index = local_sessions(self.paths);
        if let Some(id) = only {
            index.retain(|candidate, _| *candidate == id);
            if index.is_empty() {
                self.outcome.errors.push(format!("no local session {id}"));
                return Ok(());
            }
        }
        let mut entries = index.values().cloned().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.id);
        let mut uploads = Vec::new();
        for entry in entries {
            let named = only == Some(entry.id);
            if let Some(plan) = self.plan_upload(&entry, named).await {
                uploads.push((entry, plan));
            }
        }
        let mut complete = self.upload_all(uploads, &mut index).await?;
        // Work kept from a session that turned out to be deleted elsewhere is
        // a new session: it goes up now, not at the next sync.
        let kept = std::mem::take(&mut self.kept);
        if !kept.is_empty() {
            let mut uploads = Vec::new();
            for entry in kept.iter().filter_map(|id| index.get(id).cloned()) {
                if let Some(plan) = self.plan_upload(&entry, false).await {
                    uploads.push((entry, plan));
                }
            }
            complete &= self.upload_all(uploads, &mut index).await?;
        }
        if complete {
            self.state.last_push_at = Some(Utc::now());
        }
        self.save();
        Ok(())
    }

    /// Upload `uploads` (a few at a time) and record each result. Returns
    /// whether every one ended in a way that needs no later retry.
    async fn upload_all(
        &mut self,
        uploads: Vec<(LocalEntry, UploadPlan)>,
        index: &mut HashMap<Uuid, LocalEntry>,
    ) -> Result<bool, SyncError> {
        let (remote, force) = (self.remote, self.force);
        let offline = AtomicBool::new(false);
        let offline = &offline;
        let mut results = stream::iter(uploads)
            .map(|(entry, plan)| async move {
                if offline.load(Ordering::Relaxed) {
                    return Upload::skipped(entry);
                }
                let upload = upload(remote, entry, plan, force).await;
                if matches!(upload.result, Err(UploadError::Remote(SyncError::Transient(_)))) {
                    // Offline: stop starting uploads that would wait out the
                    // same timeout one after another.
                    offline.store(true, Ordering::Relaxed);
                }
                upload
            })
            .buffer_unordered(PARALLEL);
        let mut complete = true;
        while let Some(outcome) = results.next().await {
            complete &= matches!(
                outcome.result,
                Ok(_) | Err(UploadError::Skipped | UploadError::Remote(SyncError::Deleted(_)))
            );
            self.record_upload(outcome, index)?;
        }
        Ok(complete)
    }

    /// Whether `entry` needs uploading, and against which revision.
    async fn plan_upload(&mut self, entry: &LocalEntry, named: bool) -> Option<UploadPlan> {
        let record = self.state.record(&entry.id).cloned();
        if let Some(record) = &record
            && !self.force
            && (record.deleted || record.pending())
        {
            // A delete waiting to be settled, or a server known to be ahead:
            // an upload now would only be refused. The next pull resolves it.
            return None;
        }
        if !self.manual && backing_off(&entry.id) {
            return None;
        }
        let local = match inspect_off(entry.clone(), record.clone()).await {
            Ok(local) => local,
            Err(error) => {
                self.outcome.errors.push(format!("{}: {error:#}", short(&entry.id)));
                return None;
            }
        };
        match local {
            Local::Clean(fresh) => {
                if let Some(fingerprint) = fresh {
                    self.state.update(&entry.id, |record| record.fingerprint = Some(fingerprint));
                }
                if !(self.force && named) {
                    return None;
                }
            }
            Local::Dirty(snapshot) | Local::Untracked(snapshot) => {
                if is_placeholder_document(&snapshot.document) {
                    return None;
                }
                let refused = record.as_ref().and_then(|record| record.rejected.as_ref()).is_some_and(|rejected| {
                    rejected.local_sha256 == snapshot.hashes.content
                        && rejected.local_trace_sha256 == snapshot.hashes.trace
                });
                if refused && !self.force {
                    return None;
                }
            }
        }
        let record = record.unwrap_or_default();
        Some(UploadPlan {
            precondition: record.revision.map_or(Precondition::Create, Precondition::Revision),
            known_session: record.session_sha256,
            known_trace: record.trace_sha256,
        })
    }

    fn record_upload(&mut self, upload: Upload, index: &mut HashMap<Uuid, LocalEntry>) -> Result<(), SyncError> {
        let Upload { entry, title, hashes, fingerprint, result } = upload;
        let id = entry.id;
        match result {
            Ok(meta) => {
                clear_backoff(&id);
                self.state.mark_synced(&id, &meta, &hashes, Some(fingerprint));
                self.outcome.pushed += 1;
                self.outcome.lines.push(format!("↑ {title} ({}) — revision {}", short(&id), meta.revision));
                self.save();
            }
            Err(UploadError::Skipped) => {}
            Err(UploadError::Local(error)) => self.outcome.errors.push(format!("{}: {error:#}", short(&id))),
            Err(UploadError::Remote(SyncError::Deleted(current))) => {
                // Deleted on another device meanwhile: settled as a pull
                // would settle it, keeping what was changed here under a new
                // id. A session open in this process only gets flagged, and
                // is settled by a later pull once it is closed.
                if let Some(kept) = self.tombstone(id, current.as_deref(), index) {
                    self.kept.push(kept);
                }
            }
            Err(UploadError::Remote(SyncError::Conflict(current))) => {
                let behind = current.map(|current| current.revision);
                self.state.update(&id, |record| {
                    record.conflict = true;
                    record.behind = behind.or(record.behind);
                });
                self.outcome.conflicts.push(format!("{title} ({})", short(&id)));
                self.outcome.lines.push(format!(
                    "! {title} ({}) changed on another device since this one last synced; `abacus sync pull` keeps \
                     both copies, `abacus sync push --force` replaces the server's",
                    short(&id)
                ));
            }
            Err(UploadError::Remote(SyncError::Rejected { status, detail })) => {
                let reason = format!("{status}: {detail}");
                self.state.update(&id, |record| {
                    record.rejected = Some(Rejected {
                        local_sha256: hashes.content.clone(),
                        local_trace_sha256: hashes.trace.clone(),
                        reason: reason.clone(),
                    });
                });
                self.outcome.errors.push(format!(
                    "{title}: the server refused it ({reason}); it will not be retried until it changes"
                ));
            }
            Err(UploadError::Remote(error)) => {
                if !error.is_fatal() {
                    note_failure(&id);
                }
                self.note(&title, error)?;
            }
        }
        Ok(())
    }
}

fn backing_off(id: &Uuid) -> bool {
    let failures = FAILURES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    failures.get(id).is_some_and(|(attempts, next)| *attempts >= BACKOFF_ATTEMPTS || Instant::now() < *next)
}

fn note_failure(id: &Uuid) {
    let mut failures = FAILURES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (attempts, next) = failures.entry(*id).or_insert((0, Instant::now()));
    *attempts += 1;
    let delay = BACKOFF_BASE.saturating_mul(1_u32 << (*attempts - 1).min(16)).min(BACKOFF_CAP);
    *next = Instant::now() + delay;
}

fn clear_backoff(id: &Uuid) {
    FAILURES.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(id);
}

/// Download a revision, skipping the trace when the local one already has
/// its hash. A trace that does not match the document's hash was replaced
/// between the two requests; the pair is fetched again once.
async fn fetch<R: Remote>(remote: &R, item: Work, local_trace: Option<String>) -> Result<Fetched, SyncError> {
    let id = item.id.to_string();
    let mut attempts = 0;
    loop {
        attempts += 1;
        let RemoteDocument { meta, session } = remote.document(&id).await?;
        if meta.deleted {
            return Err(SyncError::Gone);
        }
        if local_trace.as_deref() == Some(meta.trace_sha256.as_str()) {
            return Ok(Fetched { id: item.id, meta, document: session, trace: None });
        }
        let trace = remote.trace(&id, meta.size_bytes).await?;
        let expected = meta.trace_sha256.clone();
        let (trace, intact) = off_runtime(move || {
            let intact = expected.is_empty() || sha256_hex(&trace) == expected;
            Ok((trace, intact))
        })
        .await
        .map_err(|error| SyncError::Transient(format!("{error:#}")))?;
        if intact {
            return Ok(Fetched { id: item.id, meta, document: session, trace: Some(trace) });
        }
        if attempts >= 2 {
            return Err(SyncError::Transient("the trace changed while it was downloading".into()));
        }
    }
}

/// What an upload is conditional on, and what the server held when this
/// device last agreed with it.
struct UploadPlan {
    precondition: Precondition,
    known_session: String,
    known_trace: String,
}

struct Upload {
    entry: LocalEntry,
    title: String,
    hashes: Hashes,
    fingerprint: Fingerprint,
    result: Result<SessionMeta, UploadError>,
}

impl Upload {
    fn skipped(entry: LocalEntry) -> Self {
        let fingerprint = Fingerprint::take(&entry.path, &entry.trace);
        let hashes = Hashes { session: String::new(), content: String::new(), trace: String::new() };
        Self { title: short(&entry.id), entry, hashes, fingerprint, result: Err(UploadError::Skipped) }
    }
}

enum UploadError {
    Skipped,
    Local(anyhow::Error),
    Remote(SyncError),
}

/// Read a session and its trace as they are now and upload exactly that.
async fn upload<R: Remote>(remote: &R, entry: LocalEntry, plan: UploadPlan, force: bool) -> Upload {
    let source = entry.clone();
    let read = off_runtime(move || {
        let fingerprint = Fingerprint::take(&source.path, &source.trace);
        let (document, trace) = read_for_upload(&source)?;
        let hashes = Hashes::of(&document, sha256_hex(&trace));
        Ok((fingerprint, document, trace, hashes))
    })
    .await;
    let (fingerprint, document, trace, hashes) = match read {
        Ok(read) => read,
        Err(error) => {
            let mut upload = Upload::skipped(entry);
            upload.result = Err(UploadError::Local(error));
            return upload;
        }
    };
    let title = title_of(&document, &short(&entry.id));
    if is_placeholder_document(&document) {
        return Upload { entry, title, hashes, fingerprint, result: Err(UploadError::Skipped) };
    }
    let id = entry.id.to_string();
    let mut result = remote.put(&id, &document, &trace, &hashes, plan.precondition).await;
    match &result {
        // The server moved on without the content moving (a remote toggle, a
        // re-upload of the same revision): nothing of theirs would be lost.
        Err(SyncError::Conflict(Some(current)))
            if !current.deleted
                && !plan.known_session.is_empty()
                && current.session_sha256 == plan.known_session
                && current.trace_sha256 == plan.known_trace =>
        {
            result = remote.put(&id, &document, &trace, &hashes, Precondition::Revision(current.revision)).await;
        }
        Err(SyncError::Conflict(Some(current))) if force => {
            result = remote.put(&id, &document, &trace, &hashes, Precondition::Revision(current.revision)).await;
        }
        // The record names a revision the server does not have at all.
        Err(SyncError::Conflict(None)) if plan.precondition != Precondition::Create => {
            result = remote.put(&id, &document, &trace, &hashes, Precondition::Create).await;
        }
        _ => {}
    }
    Upload { entry, title, hashes, fingerprint, result: result.map_err(UploadError::Remote) }
}

fn read_for_upload(entry: &LocalEntry) -> Result<(Value, Vec<u8>)> {
    let content = std::fs::read(&entry.path).with_context(|| format!("could not read {}", entry.path.display()))?;
    let document: Value = serde_json::from_slice(&content).context("invalid session file")?;
    if document["id"].as_str() != Some(entry.id.to_string().as_str()) {
        bail!("session file {} names a different id", entry.path.display());
    }
    let trace = match std::fs::read(&entry.trace) {
        Ok(trace) => trace,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).with_context(|| format!("could not read {}", entry.trace.display())),
    };
    Ok((document, trace))
}

/// Write a server revision to disk and record it as synced. The trace goes
/// first: if the process dies between the two writes, the session file still
/// matches its old record and the trace mismatch makes it "changed", which
/// the next sync resolves through the conflict path rather than losing it.
pub(crate) fn install(
    paths: &AbacusPaths,
    state: &mut SyncState,
    meta: &SessionMeta,
    document: &Value,
    trace: Option<&[u8]>,
    existing: Option<&LocalEntry>,
) -> Result<LocalEntry> {
    let written = write_revision(paths, meta, document, trace, existing)?;
    state.mark_synced(&written.entry.id, meta, &written.hashes, Some(written.fingerprint));
    Ok(written.entry)
}

/// A revision on disk, and what to record about it.
struct Written {
    entry: LocalEntry,
    hashes: Hashes,
    fingerprint: Fingerprint,
}

/// The file half of [`install`].
fn write_revision(
    paths: &AbacusPaths,
    meta: &SessionMeta,
    document: &Value,
    trace: Option<&[u8]>,
    existing: Option<&LocalEntry>,
) -> Result<Written> {
    let session = Session::deserialize(document).context("the server sent a session this version cannot read")?;
    let target = SessionStore::new(paths, session.workspace.clone()).path(session.id);
    let trace_target = trace_path(paths, &session.id);
    if let Some(trace) = trace {
        atomic_write(&trace_target, trace, true)?;
    }
    let content = serde_json::to_vec_pretty(document).context("could not encode session")?;
    atomic_write(&target, &content, true)?;
    if let Some(existing) = existing
        && existing.path != target
    {
        let _ = std::fs::remove_file(&existing.path);
    }
    let trace_sha = trace.map(sha256_hex).unwrap_or_else(|| meta.trace_sha256.clone());
    let hashes = Hashes::of(document, trace_sha);
    let fingerprint = Fingerprint::take(&target, &trace_target);
    Ok(Written { entry: LocalEntry { id: session.id, path: target, trace: trace_target }, hashes, fingerprint })
}

/// Run disk and CPU work — reading, hashing or writing a session and its
/// trace — on the blocking pool. A large session is seconds of it: on a
/// runtime worker that would hold up every task queued behind it, and a pass
/// abandoned at exit could not stop until the work was done.
async fn off_runtime<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work).await.context("sync work stopped")?
}

/// [`inspect`] off the runtime: a changed session is read and hashed whole.
async fn inspect_off(entry: LocalEntry, record: Option<SessionRecord>) -> Result<Local> {
    off_runtime(move || inspect(&entry, record.as_ref())).await
}

/// Keep a local copy under a new id, next to the original, with its trace.
fn fork(paths: &AbacusPaths, entry: &LocalEntry, mut document: Value, suffix: Option<&str>) -> Result<LocalEntry> {
    if !document.is_object() {
        bail!("session file {} is not a session", entry.path.display());
    }
    let id = Uuid::new_v4();
    document["id"] = json!(id.to_string());
    if let Some(suffix) = suffix {
        document["title"] = json!(format!("{} {suffix}", title_of(&document, "Session")));
    }
    let path = entry.path.with_file_name(format!("{id}.json"));
    atomic_write(&path, &serde_json::to_vec_pretty(&document)?, true)?;
    let trace = trace_path(paths, &id);
    if entry.trace.exists() {
        std::fs::copy(&entry.trace, &trace).with_context(|| format!("could not copy {}", entry.trace.display()))?;
    }
    Ok(LocalEntry { id, path, trace })
}

/// Move a session deleted elsewhere out of the session store. Kept on disk
/// rather than removed: a delete that arrives through sync should never be
/// the only copy of anything going away for good.
fn trash(paths: &AbacusPaths, entry: &LocalEntry) -> Result<()> {
    let bin = paths.root.join("sync-trash");
    std::fs::create_dir_all(&bin).with_context(|| format!("could not create {}", bin.display()))?;
    std::fs::rename(&entry.path, bin.join(format!("{}.json", entry.id)))
        .with_context(|| format!("could not move {}", entry.path.display()))?;
    if entry.trace.exists() {
        let _ = std::fs::rename(&entry.trace, bin.join(format!("{}.jsonl", entry.id)));
    }
    Ok(())
}

/// Accept a held revision for a session that is open here, if this copy has
/// not changed since it last synced. `Ok(None)` means it has; the next sync
/// resolves that as a conflict.
pub(crate) fn accept(paths: &AbacusPaths, state: &mut SyncState, update: HeldUpdate) -> Result<Option<Uuid>> {
    let HeldUpdate { session_id, meta, document, trace } = update;
    let index = local_sessions(paths);
    let entry = index.get(&session_id);
    if let Some(entry) = entry
        && !matches!(inspect(entry, state.record(&session_id))?, Local::Clean(_))
    {
        return Ok(None);
    }
    install(paths, state, &meta, &document, trace.as_deref(), entry)?;
    Ok(Some(session_id))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sync_state::session_sha256;
    use std::collections::BTreeMap;
    use tempfile::{TempDir, tempdir};

    /// The server's sync semantics, in memory.
    #[derive(Default)]
    pub(crate) struct FakeServer {
        inner: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        rows: BTreeMap<String, Row>,
        change: u64,
        calls: Vec<String>,
        /// Fail the next request whose call name starts with this.
        fail: Vec<(String, SyncError)>,
    }

    #[derive(Clone)]
    struct Row {
        meta: SessionMeta,
        document: Value,
        trace: Vec<u8>,
    }

    impl FakeServer {
        fn call(&self, name: String) -> Result<(), SyncError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(name.clone());
            if let Some(index) = inner.fail.iter().position(|(prefix, _)| name.starts_with(prefix.as_str())) {
                return Err(inner.fail.remove(index).1);
            }
            Ok(())
        }

        fn calls(&self, prefix: &str) -> usize {
            self.inner.lock().unwrap().calls.iter().filter(|call| call.starts_with(prefix)).count()
        }

        fn fail_once(&self, prefix: &str, error: SyncError) {
            self.inner.lock().unwrap().fail.push((prefix.to_owned(), error));
        }

        fn revision(&self, id: &Uuid) -> Option<u64> {
            self.inner.lock().unwrap().rows.get(&id.to_string()).map(|row| row.meta.revision)
        }

        fn stored(&self, id: &Uuid) -> Value {
            self.inner.lock().unwrap().rows[&id.to_string()].document.clone()
        }

        /// A revision bump that leaves the content alone, like a remote toggle.
        fn touch(&self, id: &Uuid) {
            let mut inner = self.inner.lock().unwrap();
            inner.change += 1;
            let change = inner.change;
            let row = inner.rows.get_mut(&id.to_string()).unwrap();
            row.meta.revision += 1;
            row.meta.change_id = change;
        }

        fn delete_now(&self, id: &Uuid) {
            self.touch(id);
            self.inner.lock().unwrap().rows.get_mut(&id.to_string()).unwrap().meta.deleted = true;
        }

        fn store(
            &self,
            id: &str,
            document: &Value,
            trace: &[u8],
            hashes: &Hashes,
            precondition: Precondition,
        ) -> Result<SessionMeta, SyncError> {
            let mut inner = self.inner.lock().unwrap();
            let revision = match (inner.rows.get(id), precondition) {
                (Some(row), _) if row.meta.deleted => return Err(SyncError::Deleted(Some(Box::new(row.meta.clone())))),
                (Some(row), Precondition::Create) => return Err(SyncError::Conflict(Some(Box::new(row.meta.clone())))),
                (Some(row), Precondition::Revision(revision)) if row.meta.revision != revision => {
                    return Err(SyncError::Conflict(Some(Box::new(row.meta.clone()))));
                }
                (Some(row), _) => row.meta.revision + 1,
                (None, Precondition::Create) => 1,
                (None, Precondition::Revision(_)) => return Err(SyncError::Conflict(None)),
            };
            inner.change += 1;
            let meta = SessionMeta {
                id: id.to_owned(),
                title: document["title"].as_str().unwrap_or_default().to_owned(),
                // The document's own time, which a tombstone keeps.
                updated_at: document["updated_at"].as_str().unwrap_or_default().to_owned(),
                revision,
                change_id: inner.change,
                session_sha256: session_sha256(document),
                trace_sha256: hashes.trace.clone(),
                size_bytes: trace.len() as u64,
                ..SessionMeta::default()
            };
            inner
                .rows
                .insert(id.to_owned(), Row { meta: meta.clone(), document: document.clone(), trace: trace.to_vec() });
            Ok(meta)
        }
    }

    impl Remote for FakeServer {
        fn changes(&self, cursor: u64, limit: u32) -> impl Future<Output = Result<ChangesPage, SyncError>> + Send {
            let result = self.call(format!("changes {cursor}")).map(|()| {
                let inner = self.inner.lock().unwrap();
                let mut rows = inner.rows.values().filter(|row| row.meta.change_id > cursor).collect::<Vec<_>>();
                rows.sort_by_key(|row| row.meta.change_id);
                let has_more = rows.len() > limit as usize;
                rows.truncate(limit as usize);
                ChangesPage {
                    next_cursor: rows.last().map_or(cursor, |row| row.meta.change_id),
                    items: rows.into_iter().map(|row| row.meta.clone()).collect(),
                    has_more,
                }
            });
            async move { result }
        }

        fn document(&self, id: &str) -> impl Future<Output = Result<RemoteDocument, SyncError>> + Send {
            let result = self.call(format!("document {id}")).and_then(|()| {
                let inner = self.inner.lock().unwrap();
                match inner.rows.get(id) {
                    Some(row) if !row.meta.deleted => {
                        Ok(RemoteDocument { meta: row.meta.clone(), session: row.document.clone() })
                    }
                    _ => Err(SyncError::Gone),
                }
            });
            async move { result }
        }

        fn trace(&self, id: &str, _size_hint: u64) -> impl Future<Output = Result<Vec<u8>, SyncError>> + Send {
            let result = self.call(format!("trace {id}")).and_then(|()| {
                let inner = self.inner.lock().unwrap();
                inner.rows.get(id).map(|row| row.trace.clone()).ok_or(SyncError::Gone)
            });
            async move { result }
        }

        fn put(
            &self,
            id: &str,
            document: &Value,
            trace: &[u8],
            hashes: &Hashes,
            precondition: Precondition,
        ) -> impl Future<Output = Result<SessionMeta, SyncError>> + Send {
            let result =
                self.call(format!("put {id}")).and_then(|()| self.store(id, document, trace, hashes, precondition));
            async move { result }
        }

        fn delete(&self, id: &str, revision: u64) -> impl Future<Output = Result<SessionMeta, SyncError>> + Send {
            let result = self.call(format!("delete {id}")).and_then(|()| {
                let mut inner = self.inner.lock().unwrap();
                inner.change += 1;
                let change = inner.change;
                let row = inner.rows.get_mut(id).ok_or(SyncError::Gone)?;
                if row.meta.revision != revision {
                    return Err(SyncError::Conflict(Some(Box::new(row.meta.clone()))));
                }
                row.meta.revision += 1;
                row.meta.deleted = true;
                row.meta.change_id = change;
                Ok(row.meta.clone())
            });
            async move { result }
        }
    }

    /// One device: an Abacus home with sessions in a workspace.
    pub(crate) struct Device {
        _dir: TempDir,
        pub paths: AbacusPaths,
        workspace: std::path::PathBuf,
    }

    impl Device {
        pub fn new() -> Self {
            let dir = tempdir().unwrap();
            let paths = AbacusPaths::under(dir.path().join("home"));
            // The same project on every device, as when one repository is
            // checked out on two machines; sessions are filed by it.
            let workspace = std::path::PathBuf::from("/work/project");
            Self { _dir: dir, paths, workspace }
        }

        fn store(&self) -> SessionStore {
            SessionStore::new(&self.paths, self.workspace.clone())
        }

        /// A session with one exchange per prompt, and a trace line each.
        pub fn session(&self, prompts: &[&str]) -> Session {
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
                let trace = trace_path(&self.paths, &session.id);
                std::fs::create_dir_all(trace.parent().unwrap()).unwrap();
                let mut existing = std::fs::read(&trace).unwrap_or_default();
                existing.extend(serde_json::to_vec(&json!({"prompt": prompt})).unwrap());
                existing.push(b'\n');
                std::fs::write(&trace, existing).unwrap();
            }
            session.update_messages(messages);
            self.store().save(session).unwrap();
        }

        fn load(&self, id: &Uuid) -> Option<Session> {
            self.store().load(&id.to_string()).ok()
        }

        fn sessions(&self) -> Vec<Session> {
            self.store().list().unwrap().iter().map(|summary| self.load(&summary.id).unwrap()).collect()
        }

        fn state(&self) -> SyncState {
            SyncState::load_for(&self.paths, "https://sync.test", "me@example.com")
        }

        fn forget_everything(&self) {
            std::fs::remove_file(self.paths.sync_state_file()).unwrap();
        }

        async fn reconcile(&self, server: &FakeServer) -> SyncOutcome {
            self.try_reconcile(server).await.unwrap()
        }

        async fn try_reconcile(&self, server: &FakeServer) -> Result<SyncOutcome, SyncError> {
            let mut engine = Engine::new(server, &self.paths, self.state());
            let result = match engine.pull(false).await {
                Ok(()) => engine.push(None).await,
                error => error,
            };
            engine.finish(result)
        }

        async fn pull(&self, server: &FakeServer, page_size: u32) -> SyncOutcome {
            let mut engine = Engine::new(server, &self.paths, self.state());
            engine.page_size = page_size;
            let result = engine.pull(false).await;
            engine.finish(result).unwrap()
        }

        async fn push(&self, server: &FakeServer, force: bool) -> SyncOutcome {
            let mut engine = Engine::new(server, &self.paths, self.state()).manual(force);
            let result = engine.push(None).await;
            engine.finish(result).unwrap()
        }
    }

    fn texts(session: &Session) -> Vec<String> {
        session.messages.iter().filter_map(|message| message["content"].as_str().map(str::to_owned)).collect()
    }

    fn last(session: &Session) -> String {
        texts(session).pop().unwrap()
    }

    #[tokio::test]
    async fn sync_uploads_once_and_then_has_nothing_to_do() {
        let server = FakeServer::default();
        let device = Device::new();
        let session = device.session(&["hello"]);
        let _placeholder = device.store().create("p".into(), "m".into(), vec![json!({"role": "system"})]).unwrap();

        let outcome = device.reconcile(&server).await;
        assert_eq!((outcome.pushed, outcome.errors.len()), (1, 0), "{outcome:?}");
        assert_eq!(server.revision(&session.id), Some(1));
        assert_eq!(server.calls("put"), 1, "the never-prompted placeholder stays local");
        assert!(device.state().last_push_at.is_some() && device.state().last_pull_at.is_some());

        // Our own upload shows up in the feed; it must not be downloaded back.
        let outcome = device.reconcile(&server).await;
        assert_eq!((outcome.pushed, outcome.pulled), (0, 0), "{outcome:?}");
        assert_eq!(server.calls("put"), 1);
        assert_eq!(server.calls("document") + server.calls("trace"), 0);

        // Reopening a session moves bookkeeping only: still nothing to upload.
        let mut reopened = device.load(&session.id).unwrap();
        reopened.update_messages(reopened.messages.clone());
        reopened.active_secs += 30;
        device.store().save(&reopened).unwrap();
        assert_eq!(device.reconcile(&server).await.pushed, 0);

        // A new turn is a change, uploaded against the revision it was based on.
        device.say(&mut reopened, &["more"]);
        assert_eq!(device.reconcile(&server).await.pushed, 1);
        assert_eq!(server.revision(&session.id), Some(2));
    }

    #[tokio::test]
    async fn pull_pages_through_the_feed_and_downloads_only_what_moved() {
        let server = FakeServer::default();
        let (laptop, desktop) = (Device::new(), Device::new());
        let ids = (0..5).map(|n| laptop.session(&[&format!("task {n}")]).id).collect::<Vec<_>>();
        laptop.reconcile(&server).await;

        let outcome = desktop.pull(&server, 2).await;
        assert_eq!(outcome.pulled, 5, "{outcome:?}");
        assert_eq!(server.calls("changes"), 1 + 3, "the laptop's pull, then three pages for five items");
        assert_eq!(desktop.state().cursor, 5);
        assert!(ids.iter().all(|id| desktop.load(id).is_some()));

        // One session moves on the laptop; the desktop fetches exactly that.
        let mut moved = laptop.load(&ids[2]).unwrap();
        laptop.say(&mut moved, &["follow-up"]);
        laptop.reconcile(&server).await;
        let documents = server.calls("document");
        assert_eq!(desktop.pull(&server, 2).await.pulled, 1);
        assert_eq!(server.calls("document"), documents + 1);
        assert_eq!(last(&desktop.load(&ids[2]).unwrap()), "re: follow-up");
    }

    /// A download is filed under the id inside the document. One that names
    /// another session must not be written over that session's file.
    #[tokio::test]
    async fn a_download_that_names_another_session_is_refused() {
        let server = FakeServer::default();
        let (laptop, desktop) = (Device::new(), Device::new());
        let mine = desktop.session(&["only here"]);
        let theirs = laptop.session(&["from the laptop"]);
        laptop.reconcile(&server).await;

        // The server answers for `theirs` with a document claiming to be `mine`.
        let mut impostor = serde_json::to_value(laptop.load(&theirs.id).unwrap()).unwrap();
        impostor["id"] = json!(mine.id.to_string());
        let hashes = Hashes::of(&impostor, sha256_hex(b""));
        server.touch(&theirs.id);
        let revision = server.revision(&theirs.id).unwrap();
        server.store(&theirs.id.to_string(), &impostor, b"", &hashes, Precondition::Revision(revision)).unwrap();

        let outcome = desktop.pull(&server, 10).await;
        assert_eq!((outcome.pulled, outcome.errors.len()), (0, 1), "{outcome:?}");
        assert_eq!(last(&desktop.load(&mine.id).unwrap()), "re: only here");
        assert!(desktop.load(&theirs.id).is_none());
    }

    #[tokio::test]
    async fn a_failed_download_holds_the_cursor_until_it_succeeds() {
        let server = FakeServer::default();
        let (laptop, desktop) = (Device::new(), Device::new());
        let first = laptop.session(&["one"]).id;
        let second = laptop.session(&["two"]).id;
        laptop.reconcile(&server).await;

        server.fail_once(&format!("document {second}"), SyncError::Rejected { status: 500, detail: "x".into() });
        let outcome = desktop.pull(&server, 10).await;
        assert_eq!((outcome.pulled, outcome.errors.len()), (1, 1), "{outcome:?}");
        assert_eq!(desktop.state().cursor, 0, "the page is not complete");
        assert!(desktop.state().last_pull_at.is_none());

        let outcome = desktop.pull(&server, 10).await;
        assert_eq!(outcome.pulled, 1, "only the missing one is fetched again: {outcome:?}");
        assert!(desktop.load(&first).is_some() && desktop.load(&second).is_some());
        assert_eq!(desktop.state().cursor, 2);
    }

    #[tokio::test]
    async fn going_offline_ends_the_pass_early() {
        let server = FakeServer::default();
        let device = Device::new();
        for prompt in ["a", "b", "c", "d", "e", "f"] {
            device.session(&[prompt]);
        }
        for _ in 0..PARALLEL {
            server.fail_once("put", SyncError::Transient("could not connect".into()));
        }
        let outcome = device.reconcile(&server).await;
        assert_eq!(outcome.pushed, 0);
        assert!(server.calls("put") <= PARALLEL, "no new uploads start once offline");
    }

    #[tokio::test]
    async fn both_sides_changed_keeps_the_local_copy_as_a_fork() {
        let server = FakeServer::default();
        let (laptop, desktop) = (Device::new(), Device::new());
        let session = laptop.session(&["start"]);
        laptop.reconcile(&server).await;
        desktop.reconcile(&server).await;

        let mut there = laptop.load(&session.id).unwrap();
        laptop.say(&mut there, &["laptop work"]);
        laptop.reconcile(&server).await;
        let mut here = desktop.load(&session.id).unwrap();
        desktop.say(&mut here, &["desktop work"]);

        let outcome = desktop.reconcile(&server).await;
        assert_eq!(outcome.forked, 1, "{outcome:?}");
        assert_eq!(last(&desktop.load(&session.id).unwrap()), "re: laptop work");
        let fork = desktop.sessions().into_iter().find(|candidate| candidate.id != session.id).unwrap();
        assert_eq!(last(&fork), "re: desktop work");
        assert!(fork.title.ends_with("(local fork)"));
        assert!(trace_path(&desktop.paths, &fork.id).exists());
        // The fork is a new session, uploaded in the same pass.
        assert_eq!(server.revision(&fork.id), Some(1));
        assert_eq!(outcome.pushed, 1);
    }

    #[tokio::test]
    async fn a_refused_upload_never_overwrites_and_the_next_pull_resolves_it() {
        let server = FakeServer::default();
        let (laptop, desktop) = (Device::new(), Device::new());
        let session = laptop.session(&["start"]);
        laptop.reconcile(&server).await;
        desktop.reconcile(&server).await;

        let mut there = laptop.load(&session.id).unwrap();
        laptop.say(&mut there, &["laptop work"]);
        laptop.reconcile(&server).await;
        let remote_before = server.stored(&session.id);
        let mut here = desktop.load(&session.id).unwrap();
        desktop.say(&mut here, &["desktop work"]);

        // Push alone (the close-time path): refused, recorded, nothing lost.
        let mut engine = Engine::new(&server, &desktop.paths, desktop.state());
        let result = engine.push(None).await;
        let outcome = engine.finish(result).unwrap();
        assert_eq!((outcome.pushed, outcome.conflicts.len()), (0, 1), "{outcome:?}");
        assert_eq!(server.stored(&session.id), remote_before);
        assert!(desktop.state().record(&session.id).unwrap().conflict);

        // Pushing again does not hammer the server with doomed uploads.
        let puts = server.calls("put");
        let mut engine = Engine::new(&server, &desktop.paths, desktop.state());
        let result = engine.push(None).await;
        engine.finish(result).unwrap();
        assert_eq!(server.calls("put"), puts);

        // The pull resolves it by forking, even though the cursor is past it.
        let outcome = desktop.reconcile(&server).await;
        assert_eq!(outcome.forked, 1, "{outcome:?}");
        assert_eq!(last(&desktop.load(&session.id).unwrap()), "re: laptop work");
        assert!(!desktop.state().record(&session.id).unwrap().pending());
    }

    #[tokio::test]
    async fn a_stale_revision_with_known_content_is_retried() {
        let server = FakeServer::default();
        let device = Device::new();
        let mut session = device.session(&["start"]);
        device.reconcile(&server).await;
        server.touch(&session.id);
        device.say(&mut session, &["more"]);

        let mut engine = Engine::new(&server, &device.paths, device.state());
        let result = engine.push(None).await;
        let outcome = engine.finish(result).unwrap();
        assert_eq!((outcome.pushed, outcome.conflicts.len()), (1, 0), "{outcome:?}");
        assert_eq!(server.revision(&session.id), Some(3));
    }

    #[tokio::test]
    async fn force_push_replaces_the_server_copy() {
        let server = FakeServer::default();
        let (laptop, desktop) = (Device::new(), Device::new());
        let session = laptop.session(&["start"]);
        laptop.reconcile(&server).await;
        desktop.reconcile(&server).await;
        let mut there = laptop.load(&session.id).unwrap();
        laptop.say(&mut there, &["laptop work"]);
        laptop.reconcile(&server).await;
        let mut here = desktop.load(&session.id).unwrap();
        desktop.say(&mut here, &["desktop work"]);

        let outcome = desktop.push(&server, true).await;
        assert_eq!(outcome.pushed, 1, "{outcome:?}");
        let stored = server.stored(&session.id);
        assert_eq!(stored["messages"].as_array().unwrap().last().unwrap()["content"], "re: desktop work");
    }

    #[tokio::test]
    async fn one_side_extending_the_other_needs_no_fork() {
        let server = FakeServer::default();
        let (laptop, desktop) = (Device::new(), Device::new());
        let session = laptop.session(&["start"]);
        laptop.reconcile(&server).await;
        desktop.reconcile(&server).await;

        // The desktop's state is lost and its copy is behind: fast-forward.
        let mut there = laptop.load(&session.id).unwrap();
        laptop.say(&mut there, &["more"]);
        laptop.reconcile(&server).await;
        desktop.forget_everything();
        let outcome = desktop.reconcile(&server).await;
        assert_eq!(outcome.forked, 0, "{outcome:?}");
        assert_eq!(last(&desktop.load(&session.id).unwrap()), "re: more");

        // The laptop's copy is ahead of the server with no record: uploaded
        // over the revision it extends.
        let mut there = laptop.load(&session.id).unwrap();
        laptop.say(&mut there, &["even more"]);
        laptop.forget_everything();
        let outcome = laptop.reconcile(&server).await;
        assert_eq!((outcome.forked, outcome.pushed), (0, 1), "{outcome:?}");
        assert_eq!(server.revision(&session.id), Some(3));
        assert_eq!(desktop.sessions().len(), 1, "no forks anywhere");
    }

    #[tokio::test]
    async fn losing_the_state_file_costs_no_transfers_for_equal_copies() {
        let server = FakeServer::default();
        let device = Device::new();
        for prompt in ["a", "b", "c"] {
            device.session(&[prompt]);
        }
        device.reconcile(&server).await;
        device.forget_everything();

        let outcome = device.reconcile(&server).await;
        assert_eq!((outcome.pulled, outcome.pushed, outcome.skipped), (0, 0, 3), "{outcome:?}");
        assert_eq!(server.calls("document") + server.calls("trace"), 0);
        assert_eq!(server.calls("put"), 3);
    }

    #[tokio::test]
    async fn a_remote_delete_reaches_clean_copies_and_keeps_unsynced_work() {
        let server = FakeServer::default();
        let device = Device::new();
        let clean = device.session(&["finished"]);
        let mut busy = device.session(&["in progress"]);
        device.reconcile(&server).await;
        server.delete_now(&clean.id);
        server.delete_now(&busy.id);
        device.say(&mut busy, &["unsynced work"]);

        let outcome = device.reconcile(&server).await;
        assert_eq!(outcome.deleted, 2, "{outcome:?}");
        assert!(device.load(&clean.id).is_none(), "the delete reached this device");
        assert!(device.paths.root.join("sync-trash").join(format!("{}.json", clean.id)).exists());
        assert!(device.load(&busy.id).is_none());
        let [kept] = device.sessions().try_into().unwrap();
        assert_eq!(last(&kept), "re: unsynced work");
        assert_eq!(kept.title, busy.title);
        // The kept work syncs as a new session; the deleted id stays deleted.
        assert_eq!(server.revision(&kept.id), Some(1));
        assert_eq!(outcome.pushed, 1);
    }

    /// A device that synced with an older Abacus has no record of anything,
    /// and kept its copies of sessions deleted elsewhere (deletes never
    /// reached it). Such a copy follows the delete unless it holds work the
    /// deleted revision lacks; it must not come back as a new session.
    #[tokio::test]
    async fn a_delete_settles_unrecorded_copies_unless_they_are_newer() {
        let server = FakeServer::default();
        let (laptop, desktop) = (Device::new(), Device::new());
        let same = laptop.session(&["same everywhere"]);
        let mut older = laptop.session(&["older here"]);
        let newer = laptop.session(&["newer here"]);
        laptop.reconcile(&server).await;
        desktop.reconcile(&server).await;

        // The laptop takes one further; the desktop another, unsynced.
        laptop.say(&mut older, &["only on the laptop"]);
        laptop.reconcile(&server).await;
        let mut there = desktop.load(&newer.id).unwrap();
        desktop.say(&mut there, &["only on the desktop"]);
        for id in [same.id, older.id, newer.id] {
            server.delete_now(&id);
        }
        desktop.forget_everything();

        let outcome = desktop.reconcile(&server).await;
        assert_eq!(outcome.deleted, 3, "{outcome:?}");
        let [kept] = desktop.sessions().try_into().unwrap();
        assert_eq!(last(&kept), "re: only on the desktop", "work the server never saw survives");
        assert_eq!(outcome.pushed, 1, "and only that comes back: {outcome:?}");
        for id in [same.id, older.id, newer.id] {
            assert!(desktop.paths.root.join("sync-trash").join(format!("{id}.json")).exists());
        }
    }

    #[tokio::test]
    async fn an_upload_that_finds_the_session_deleted_keeps_the_work_as_a_new_session() {
        let server = FakeServer::default();
        let device = Device::new();
        let mut session = device.session(&["start"]);
        device.reconcile(&server).await;
        server.delete_now(&session.id);
        device.say(&mut session, &["unsynced work"]);

        // No pull in between: the upload itself learns of the delete.
        let outcome = device.push(&server, false).await;
        assert_eq!((outcome.deleted, outcome.pushed, outcome.forked), (1, 1, 0), "{outcome:?}");
        assert!(outcome.conflicts.is_empty() && outcome.errors.is_empty(), "not a revision conflict: {outcome:?}");
        assert!(outcome.notices[0].contains("kept as a new session"), "{:?}", outcome.notices);
        assert!(device.load(&session.id).is_none());
        assert!(device.paths.root.join("sync-trash").join(format!("{}.json", session.id)).exists());
        let [kept] = device.sessions().try_into().unwrap();
        assert_eq!(last(&kept), "re: unsynced work");
        assert_eq!(server.revision(&kept.id), Some(1), "uploaded in the same pass");
        assert_eq!(server.calls("put"), 3, "the first upload, the refused one, and the new session");
        assert!(device.state().last_push_at.is_some());
        assert!(device.state().record(&session.id).is_none());

        // Nothing is left to do.
        let outcome = device.reconcile(&server).await;
        assert_eq!((outcome.pushed, outcome.deleted), (0, 0), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_delete_found_by_an_upload_waits_while_the_session_is_open() {
        let server = FakeServer::default();
        let device = Device::new();
        let mut session = device.session(&["start"]);
        device.reconcile(&server).await;
        server.delete_now(&session.id);
        device.say(&mut session, &["unsynced work"]);

        crate::sync::session_opened(session.id);
        let outcome = device.push(&server, false).await;
        crate::sync::session_closed(session.id);
        assert_eq!((outcome.deleted, outcome.pushed), (0, 0), "{outcome:?}");
        assert!(outcome.errors.is_empty() && outcome.conflicts.is_empty());
        assert_eq!(last(&device.load(&session.id).unwrap()), "re: unsynced work", "the open file is left alone");
        assert!(device.state().record(&session.id).unwrap().deleted);
        assert_eq!(server.calls("put"), 2);

        // Closed: the next sync settles it.
        let outcome = device.reconcile(&server).await;
        assert_eq!((outcome.deleted, outcome.pushed), (1, 1), "{outcome:?}");
        assert!(device.load(&session.id).is_none());
        let [kept] = device.sessions().try_into().unwrap();
        assert_eq!(server.revision(&kept.id), Some(1));
    }

    #[tokio::test]
    async fn an_open_session_is_held_for_the_front_end() {
        let server = FakeServer::default();
        let (laptop, desktop) = (Device::new(), Device::new());
        let session = laptop.session(&["start"]);
        laptop.reconcile(&server).await;
        desktop.reconcile(&server).await;
        let mut there = laptop.load(&session.id).unwrap();
        laptop.say(&mut there, &["more"]);
        laptop.reconcile(&server).await;

        crate::sync::session_opened(session.id);
        let mut outcome = desktop.reconcile(&server).await;
        crate::sync::session_closed(session.id);
        assert_eq!(outcome.pulled, 0);
        let held = outcome.held.pop().expect("the newer revision is handed over");
        assert_eq!(last(&desktop.load(&session.id).unwrap()), "re: start", "not written over");
        assert!(desktop.state().record(&session.id).unwrap().pending());

        let mut state = desktop.state();
        assert_eq!(accept(&desktop.paths, &mut state, held).unwrap(), Some(session.id));
        state.save().unwrap();
        assert_eq!(last(&desktop.load(&session.id).unwrap()), "re: more");
        assert!(!desktop.state().record(&session.id).unwrap().pending());
        assert_eq!(desktop.reconcile(&server).await.pushed, 0);
    }

    #[tokio::test]
    async fn a_rejected_sign_in_stops_the_pass() {
        let server = FakeServer::default();
        let device = Device::new();
        device.session(&["a"]);
        server.fail_once("changes", SyncError::Unauthorized);
        assert!(matches!(device.try_reconcile(&server).await, Err(SyncError::Unauthorized)));
        assert_eq!(server.calls("put"), 0);
    }

    #[tokio::test]
    async fn a_permanently_refused_upload_is_not_retried_until_it_changes() {
        let server = FakeServer::default();
        let device = Device::new();
        let mut session = device.session(&["huge"]);
        server.fail_once("put", SyncError::Rejected { status: 413, detail: "trace too large".into() });
        assert_eq!(device.reconcile(&server).await.errors.len(), 1);
        device.reconcile(&server).await;
        assert_eq!(server.calls("put"), 1);
        device.say(&mut session, &["smaller"]);
        assert_eq!(device.reconcile(&server).await.pushed, 1);
    }

    /// Checking, reading, hashing and installing a large session is seconds of
    /// disk and CPU. It runs off the runtime: on a worker it holds up every
    /// task queued behind it, and a pass abandoned at exit could not stop until
    /// it was done.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "timing on Windows is dominated by its 15.6 ms timer and scheduler noise; Linux and macOS cover this"
    )]
    fn large_sessions_are_read_hashed_and_written_off_the_runtime() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        let server = FakeServer::default();
        let (laptop, desktop) = (Device::new(), Device::new());
        let session = laptop.session(&["big"]);
        // Noise, which no shortcut makes quick to hash.
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let trace: Vec<u8> = (0..8 * 1024 * 1024)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 24) as u8
            })
            .collect();
        std::fs::write(trace_path(&laptop.paths, &session.id), &trace).unwrap();

        // What hashing it once costs on this machine, done where it is seen.
        let started = Instant::now();
        assert!(!sha256_hex(&trace).is_empty());
        let work = started.elapsed();
        if work < Duration::from_millis(40) {
            // An optimised build hashes this too fast for a stall to show.
            eprintln!("skipped: hashing the trace takes only {work:?} here");
            return;
        }

        // One thread runs everything, so work done on it stops the ticker,
        // which stands for everything else the runtime runs.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (idle, longest) = runtime.block_on(async {
            let worst = Arc::new(AtomicU64::new(0));
            let ticking = worst.clone();
            let ticker = tokio::spawn(async move {
                let mut last = Instant::now();
                loop {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    let now = Instant::now();
                    ticking.fetch_max(now.duration_since(last).as_micros() as u64, Ordering::Relaxed);
                    last = now;
                }
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            // The gap the ticker shows with nothing in its way: about 1 ms on
            // Linux, the 15.6 ms timer tick on Windows.
            worst.store(0, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(80)).await;
            let idle = Duration::from_micros(worst.swap(0, Ordering::Relaxed));
            assert_eq!(laptop.push(&server, false).await.pushed, 1);
            // The fake server never waits, so give the ticker its turn to
            // see how long it was kept waiting.
            tokio::time::sleep(Duration::from_millis(5)).await;
            assert_eq!(desktop.pull(&server, 10).await.pulled, 1);
            tokio::time::sleep(Duration::from_millis(5)).await;
            ticker.abort();
            (idle, Duration::from_micros(worst.load(Ordering::Relaxed)))
        });
        assert_eq!(std::fs::read(trace_path(&desktop.paths, &session.id)).unwrap(), trace);
        if work < idle * 4 {
            eprintln!("skipped: hashing takes {work:?}, too close to this timer's {idle:?} tick to tell a stall");
            return;
        }
        assert!(
            longest < idle + work / 2,
            "the runtime was held for {longest:?}; hashing the trace once takes {work:?} (idle tick {idle:?})"
        );
    }

    #[test]
    fn summaries_are_short() {
        let mut outcome = SyncOutcome { pulled: 2, pushed: 1, ..SyncOutcome::default() };
        assert_eq!(outcome.summary().unwrap(), "synced ↓2 ↑1");
        outcome.conflicts.push("t (00000000)".into());
        assert_eq!(outcome.summary().unwrap(), "synced ↓2 ↑1 · 1 conflict");
        assert_eq!(SyncOutcome::default().summary(), None);
        let offline =
            SyncOutcome { errors: vec!["sync server unavailable: could not connect".into()], ..SyncOutcome::default() };
        assert_eq!(offline.summary().unwrap(), "sync failed: sync server unavailable: could not connect");
    }

    #[test]
    fn lineage_compares_transcripts() {
        let a = json!({"messages": [1, 2]});
        let b = json!({"messages": [1, 2, 3]});
        let c = json!({"messages": [1, 9, 3]});
        assert_eq!(lineage(&a, &b), Lineage::RemoteAhead);
        assert_eq!(lineage(&b, &a), Lineage::LocalAhead);
        assert_eq!(lineage(&b, &c), Lineage::Diverged);
        assert_eq!(lineage(&a, &a), Lineage::Diverged);
    }
}
