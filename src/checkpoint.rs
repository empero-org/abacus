//! What a turn did to the workspace through the file tools, kept so the
//! person can see it once the turn ends and take it back.
//!
//! The file tools tell the turn's [`Journal`] about a path before they first
//! change it, and it keeps what was there. When the turn ends, that is
//! compared with what is there now: lines added and removed per file, which a
//! front end shows under the turn, and a checkpoint on disk with both sides of
//! every file, which shows the turn's own diff and undoes it. What commands
//! change (`run_command`, `run_python`) is not seen.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};

/// Largest file a checkpoint keeps a copy of.
const MAX_FILE: u64 = 16 * 1024 * 1024;
/// Most a turn keeps across all its files.
const MAX_TURN: u64 = 128 * 1024 * 1024;
/// Most files a recursive delete or move keeps one by one.
const MAX_FILES: usize = 2_000;
/// Checkpoints kept per session; the oldest go first.
const KEEP: usize = 50;

/// The field a history message carries a turn's edits in. Such a message is
/// for the transcript; the model never reads it.
pub const MARKER_FIELD: &str = "edits";

/// Where a turn's file changes are recorded while it runs. The default
/// records nothing: a turn whose front end does not show edits pays nothing.
#[derive(Clone, Default)]
pub struct Journal(Arc<Mutex<Option<Recording>>>);

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let files = self.0.lock().unwrap().as_ref().map(|recording| recording.before.len());
        f.debug_struct("Journal").field("files", &files).finish()
    }
}

struct Recording {
    root: PathBuf,
    /// `root`, resolved, for paths the tools hand over resolved.
    canonical: Option<PathBuf>,
    /// What each path held before the turn first changed it, by
    /// workspace-relative path.
    before: BTreeMap<String, Kept>,
    /// Folders that were not there before the turn, which undoing it takes
    /// away again once they are empty.
    folders: BTreeSet<String>,
    kept: u64,
}

/// One side of a file.
#[derive(Clone, Debug, PartialEq)]
enum Kept {
    Missing,
    File(Vec<u8>),
    /// Too large to keep, or not a file: shown, but not undone. Its hash,
    /// when it could be read, says whether it changed.
    Lost(Option<String>),
}

/// What a turn did to one file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    /// `added`, `modified` or `deleted`.
    pub status: String,
    pub added: usize,
    pub removed: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
}

/// What a turn changed, under the checkpoint that can show and undo it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnEdits {
    pub checkpoint: String,
    pub files: Vec<FileChange>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub undone: bool,
}

impl TurnEdits {
    /// The history message that keeps these edits with the conversation, so a
    /// reopened thread shows them where the turn ended.
    pub fn marker(&self) -> Value {
        json!({"role": "system", "content": "", MARKER_FIELD: self})
    }

    /// The edits `message` keeps, if it is their marker.
    pub fn marked_by(message: &Value) -> Option<Self> {
        if message["role"] != "system" {
            return None;
        }
        serde_json::from_value(message.get(MARKER_FIELD)?.clone()).ok()
    }

    /// The transcript item a front end draws them as.
    pub fn item(&self, id: &str) -> Value {
        json!({
            "id": id,
            "type": "fileEdits",
            "checkpoint": self.checkpoint,
            "files": self.files,
            "undone": self.undone,
        })
    }
}

impl Journal {
    /// A journal that records, for a turn on `root`.
    pub fn recording(root: PathBuf) -> Self {
        let canonical = root.canonicalize().ok();
        Self(Arc::new(Mutex::new(Some(Recording {
            root,
            canonical,
            before: BTreeMap::new(),
            folders: BTreeSet::new(),
            kept: 0,
        }))))
    }

    /// Keep what `path` holds, unless this turn already did: a file tool is
    /// about to change it. A directory keeps each file under it.
    pub fn before(&self, path: &Path) {
        if let Some(recording) = self.0.lock().unwrap().as_mut() {
            recording.keep_tree(path);
        }
    }

    /// Keep both ends of a move: what `source` holds, and what each file of
    /// it will replace under `destination`.
    pub fn before_move(&self, source: &Path, destination: &Path) {
        let mut guard = self.0.lock().unwrap();
        let Some(recording) = guard.as_mut() else { return };
        recording.keep_tree(source);
        if destination.exists() {
            recording.keep_tree(destination);
        }
        if source.is_dir() && !source.is_symlink() {
            for file in files_under(source) {
                if let Ok(rest) = file.strip_prefix(source) {
                    recording.keep(&destination.join(rest));
                }
            }
        } else {
            recording.keep(destination);
        }
    }

    /// Stop recording, and save what the turn changed as a checkpoint under
    /// `dir`. None when it changed nothing.
    pub fn finish(&self, dir: &Path) -> Result<Option<TurnEdits>> {
        let Some(recording) = self.0.lock().unwrap().take() else { return Ok(None) };
        let mut files = Vec::new();
        let mut entries = Vec::new();
        for (path, before) in recording.before {
            let now = recording.root.join(&path);
            let after = read_side(&now);
            // A directory now: the files under it are what changed.
            if before == after || (now.is_dir() && !now.is_symlink()) {
                continue;
            }
            files.push(change(&path, &before, &after));
            entries.push((path, before, after));
        }
        if files.is_empty() {
            return Ok(None);
        }
        let checkpoint = new_id();
        save(dir, &checkpoint, entries, recording.folders.into_iter().collect())?;
        prune(dir);
        Ok(Some(TurnEdits { checkpoint, files, undone: false }))
    }
}

impl Recording {
    fn relative(&self, path: &Path) -> Option<String> {
        let rest = path
            .strip_prefix(&self.root)
            .ok()
            .or_else(|| self.canonical.as_ref().and_then(|root| path.strip_prefix(root).ok()))?;
        let rest = rest.to_str()?.replace('\\', "/");
        (!rest.is_empty()).then_some(rest)
    }

    fn keep_tree(&mut self, path: &Path) {
        if path.is_dir() && !path.is_symlink() {
            for file in files_under(path) {
                self.keep(&file);
            }
        } else {
            self.keep(path);
        }
    }

    fn keep(&mut self, path: &Path) {
        let Some(relative) = self.relative(path) else { return };
        if self.before.contains_key(&relative) {
            return;
        }
        let kept = match fs::symlink_metadata(path) {
            Err(_) => {
                let mut folder = path.parent();
                while let Some(missing) = folder.filter(|folder| !folder.exists()) {
                    self.folders.extend(self.relative(missing));
                    folder = missing.parent();
                }
                Kept::Missing
            }
            Ok(meta) if !meta.is_file() => Kept::Lost(None),
            Ok(meta) if meta.len() > MAX_FILE || self.kept + meta.len() > MAX_TURN => Kept::Lost(hash_file(path)),
            Ok(meta) => match fs::read(path) {
                Ok(bytes) => {
                    self.kept += meta.len();
                    Kept::File(bytes)
                }
                Err(_) => Kept::Lost(None),
            },
        };
        self.before.insert(relative, kept);
    }
}

/// The files under `dir`, as many as a turn keeps.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(_) => files.push(path),
                Err(_) => {}
            }
            if files.len() >= MAX_FILES {
                return files;
            }
        }
    }
    files
}

fn read_side(path: &Path) -> Kept {
    match fs::symlink_metadata(path) {
        Err(_) => Kept::Missing,
        Ok(meta) if !meta.is_file() => Kept::Lost(None),
        Ok(meta) if meta.len() > MAX_FILE => Kept::Lost(hash_file(path)),
        Ok(_) => fs::read(path).map(Kept::File).unwrap_or(Kept::Lost(None)),
    }
}

/// What happened to `path`, from `before` to `after`, which differ.
fn change(path: &str, before: &Kept, after: &Kept) -> FileChange {
    let status = match (before, after) {
        (Kept::Missing, _) => "added",
        (_, Kept::Missing) => "deleted",
        _ => "modified",
    };
    let text = |side: &Kept| match side {
        Kept::Missing => Some(String::new()),
        Kept::File(bytes) => String::from_utf8(bytes.clone()).ok(),
        Kept::Lost(_) => None,
    };
    let (added, removed, binary) = match (text(before), text(after)) {
        (Some(old), Some(new)) => {
            let (added, removed) = count_lines(&old, &new);
            (added, removed, false)
        }
        _ => (0, 0, true),
    };
    FileChange { path: path.to_owned(), status: status.to_owned(), added, removed, binary }
}

fn count_lines(old: &str, new: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(old, new);
    diff.iter_all_changes().fold((0, 0), |(added, removed), change| match change.tag() {
        ChangeTag::Insert => (added + 1, removed),
        ChangeTag::Delete => (added, removed + 1),
        ChangeTag::Equal => (added, removed),
    })
}

/// Sortable by when it was made, so the oldest go first.
fn new_id() -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{}-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S%3f"), &suffix[..8])
}

// ---- on disk ----------------------------------------------------------------
//
// `<dir>/<checkpoint>.json` lists each file's two sides; the contents live in
// `<dir>/blobs/<sha256>`, shared between checkpoints.

#[derive(Serialize, Deserialize)]
struct Manifest {
    files: Vec<Entry>,
    /// Folders the turn made, by workspace-relative path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    folders: Vec<String>,
    #[serde(default)]
    undone: bool,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    path: String,
    before: Side,
    after: Side,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Side {
    Missing,
    /// Kept in `blobs/<hash>`.
    Kept {
        hash: String,
    },
    /// Not kept: its hash alone, which says whether it changed since.
    Hashed {
        hash: String,
    },
    /// Neither kept nor hashed.
    Lost,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hash_file(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            return Some(format!("{:x}", hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
    }
}

fn save(dir: &Path, checkpoint: &str, entries: Vec<(String, Kept, Kept)>, folders: Vec<String>) -> Result<()> {
    let blobs = dir.join("blobs");
    fs::create_dir_all(&blobs).with_context(|| format!("could not create {}", blobs.display()))?;
    let store = |kept: Kept| -> Result<Side> {
        Ok(match kept {
            Kept::Missing => Side::Missing,
            Kept::File(bytes) => {
                let hash = hash(&bytes);
                let blob = blobs.join(&hash);
                if !blob.exists() {
                    crate::config::atomic_write(&blob, &bytes, true)?;
                }
                Side::Kept { hash }
            }
            Kept::Lost(hash) => hash.map_or(Side::Lost, |hash| Side::Hashed { hash }),
        })
    };
    let mut files = Vec::new();
    for (path, before, after) in entries {
        files.push(Entry { path, before: store(before)?, after: store(after)? });
    }
    let manifest = serde_json::to_vec_pretty(&Manifest { files, folders, undone: false })?;
    crate::config::atomic_write(&dir.join(format!("{checkpoint}.json")), &manifest, true)
}

/// Keep the newest checkpoints, and the contents they still use.
fn prune(dir: &Path) {
    let mut manifests = manifests(dir);
    if manifests.len() <= KEEP {
        return;
    }
    manifests.sort();
    let (old, kept) = manifests.split_at(manifests.len() - KEEP);
    for path in old {
        let _ = fs::remove_file(path);
    }
    let mut used = HashSet::new();
    for path in kept {
        let Ok(manifest) = load(path) else { continue };
        for entry in manifest.files {
            for side in [entry.before, entry.after] {
                if let Side::Kept { hash } = side {
                    used.insert(hash);
                }
            }
        }
    }
    let Ok(blobs) = fs::read_dir(dir.join("blobs")) else { return };
    for blob in blobs.flatten() {
        if !used.contains(blob.file_name().to_string_lossy().as_ref()) {
            let _ = fs::remove_file(blob.path());
        }
    }
}

fn manifests(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect()
}

fn manifest_path(dir: &Path, checkpoint: &str) -> Result<PathBuf> {
    if checkpoint.is_empty() || !checkpoint.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("not a checkpoint: {checkpoint}");
    }
    let path = dir.join(format!("{checkpoint}.json"));
    if !path.exists() {
        bail!("that turn's edits are not kept on this computer");
    }
    Ok(path)
}

fn load(path: &Path) -> Result<Manifest> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn blob(dir: &Path, side: &Side) -> Option<Vec<u8>> {
    match side {
        Side::Missing => Some(Vec::new()),
        Side::Kept { hash } => fs::read(dir.join("blobs").join(hash)).ok(),
        Side::Hashed { .. } | Side::Lost => None,
    }
}

/// What the turn under `checkpoint` changed, as a unified diff with git's
/// headers. A file without text on both sides is named, not shown.
pub fn diff(dir: &Path, checkpoint: &str) -> Result<String> {
    let manifest = load(&manifest_path(dir, checkpoint)?)?;
    let mut out = String::new();
    for entry in manifest.files {
        let path = &entry.path;
        out.push_str(&format!("diff --git a/{path} b/{path}\n"));
        let old = blob(dir, &entry.before).and_then(|bytes| String::from_utf8(bytes).ok());
        let new = blob(dir, &entry.after).and_then(|bytes| String::from_utf8(bytes).ok());
        let (Some(old), Some(new)) = (old, new) else {
            out.push_str(&format!("Binary files a/{path} and b/{path} differ\n"));
            continue;
        };
        let from = if entry.before == Side::Missing { "/dev/null".to_owned() } else { format!("a/{path}") };
        let to = if entry.after == Side::Missing { "/dev/null".to_owned() } else { format!("b/{path}") };
        if entry.before == Side::Missing {
            out.push_str("new file mode 100644\n");
        } else if entry.after == Side::Missing {
            out.push_str("deleted file mode 100644\n");
        }
        let diff = TextDiff::from_lines(&old, &new);
        out.push_str(&diff.unified_diff().context_radius(3).header(&from, &to).to_string());
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

/// Put back what the turn under `checkpoint` changed in `root`, if nothing
/// changed those files since. Returns the files put back.
pub fn undo(dir: &Path, checkpoint: &str, root: &Path) -> Result<Vec<String>> {
    let path = manifest_path(dir, checkpoint)?;
    let mut manifest = load(&path)?;
    if manifest.undone {
        bail!("that turn's edits were already undone");
    }
    let mut changed = Vec::new();
    let mut lost = Vec::new();
    for entry in &manifest.files {
        let now = root.join(&entry.path);
        let current = if now.exists() { hash_file(&now) } else { None };
        let same = match &entry.after {
            Side::Missing => !now.exists(),
            Side::Kept { hash } | Side::Hashed { hash } => current.as_ref() == Some(hash),
            Side::Lost => false,
        };
        if !same {
            changed.push(entry.path.clone());
        } else if blob(dir, &entry.before).is_none() {
            lost.push(entry.path.clone());
        }
    }
    if !changed.is_empty() {
        bail!("changed since that turn: {}", changed.join(", "));
    }
    if !lost.is_empty() {
        bail!("too large to have been kept: {}", lost.join(", "));
    }
    let mut restored = Vec::new();
    for entry in &manifest.files {
        let now = root.join(&entry.path);
        if entry.before == Side::Missing {
            if now.exists() {
                fs::remove_file(&now).with_context(|| format!("could not remove {}", entry.path))?;
            }
        } else if let Some(bytes) = blob(dir, &entry.before) {
            if let Some(parent) = now.parent() {
                fs::create_dir_all(parent)?;
            }
            crate::config::atomic_write(&now, &bytes, false)
                .with_context(|| format!("could not restore {}", entry.path))?;
        }
        restored.push(entry.path.clone());
    }
    // The folders it made, deepest first, if nothing else is in them.
    let mut folders = manifest.folders.clone();
    folders.sort_by_key(|folder| std::cmp::Reverse(folder.matches('/').count()));
    for folder in folders {
        let _ = fs::remove_dir(root.join(folder));
    }
    manifest.undone = true;
    crate::config::atomic_write(&path, &serde_json::to_vec_pretty(&manifest)?, true)?;
    Ok(restored)
}

/// What the model is told when the person undoes a turn's edits, so it does
/// not go on as if they stood.
pub fn undo_note(files: &[String]) -> String {
    format!(
        "{UNDO_NOTE}{}. Those files are back to how they were before that turn; \
         reread them before relying on what you remember of them.",
        files.join(", ")
    )
}

const UNDO_NOTE: &str = "[The person undid the edits you made in an earlier turn to: ";

/// Whether `text` is an [`undo_note`]: said to the model, not by anyone.
pub fn is_undo_note(text: &str) -> bool {
    text.starts_with(UNDO_NOTE)
}

/// How a transcript shows an [`undo_note`].
pub fn undo_shown(note: &str) -> String {
    let files = note.strip_prefix(UNDO_NOTE).unwrap_or(note);
    format!("Edits undone: {}", files.split(". Those files").next().unwrap_or(files))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(root: &Path, change: impl FnOnce(&Journal)) -> (PathBuf, Option<TurnEdits>) {
        let dir = root.join(".checkpoints");
        let journal = Journal::recording(root.to_path_buf());
        change(&journal);
        let edits = journal.finish(&dir).unwrap();
        (dir, edits)
    }

    #[test]
    fn counts_what_a_turn_changed_and_undoes_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(root.join("gone.txt"), "x\ny\n").unwrap();
        let (dir, edits) = turn(root, |journal| {
            journal.before(&root.join("a.txt"));
            fs::write(root.join("a.txt"), "one\n2\nthree\nfour\n").unwrap();
            // A second change to the same file keeps the first original.
            journal.before(&root.join("a.txt"));
            journal.before(&root.join("new/b.txt"));
            fs::create_dir_all(root.join("new")).unwrap();
            fs::write(root.join("new/b.txt"), "b\n").unwrap();
            journal.before(&root.join("gone.txt"));
            fs::remove_file(root.join("gone.txt")).unwrap();
        });
        let edits = edits.unwrap();
        let shape: Vec<_> = edits
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.status.as_str(), file.added, file.removed))
            .collect();
        assert_eq!(shape, [("a.txt", "modified", 2, 1), ("gone.txt", "deleted", 0, 2), ("new/b.txt", "added", 1, 0)]);

        let diff = diff(&dir, &edits.checkpoint).unwrap();
        assert!(diff.contains("--- a/a.txt\n+++ b/a.txt\n"), "{diff}");
        assert!(diff.contains("-two\n+2\n"), "{diff}");
        assert!(diff.contains("--- /dev/null\n+++ b/new/b.txt\n"), "{diff}");

        let restored = undo(&dir, &edits.checkpoint, root).unwrap();
        assert_eq!(restored.len(), 3);
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "one\ntwo\nthree\n");
        assert_eq!(fs::read_to_string(root.join("gone.txt")).unwrap(), "x\ny\n");
        assert!(!root.join("new").exists(), "the folder the turn made goes too");
        assert!(undo(&dir, &edits.checkpoint, root).unwrap_err().to_string().contains("already undone"));
    }

    #[test]
    fn refuses_to_undo_over_later_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("a.txt"), "old\n").unwrap();
        let (dir, edits) = turn(root, |journal| {
            journal.before(&root.join("a.txt"));
            fs::write(root.join("a.txt"), "new\n").unwrap();
        });
        let edits = edits.unwrap();
        fs::write(root.join("a.txt"), "newer\n").unwrap();
        let error = undo(&dir, &edits.checkpoint, root).unwrap_err().to_string();
        assert!(error.contains("changed since that turn: a.txt"), "{error}");
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "newer\n");
    }

    #[test]
    fn a_turn_that_changed_nothing_leaves_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("a.txt"), "same\n").unwrap();
        let (dir, edits) = turn(root, |journal| {
            journal.before(&root.join("a.txt"));
            fs::write(root.join("a.txt"), "same\n").unwrap();
        });
        assert_eq!(edits, None);
        assert!(!dir.exists());
        // And one that records nothing records nothing.
        Journal::default().before(&root.join("a.txt"));
        assert_eq!(Journal::default().finish(&dir).unwrap(), None);
    }

    #[test]
    fn a_move_keeps_both_ends_and_a_directory_each_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::write(root.join("src/a.rs"), "a\n").unwrap();
        fs::write(root.join("src/deep/b.rs"), "b\n").unwrap();
        let (dir, edits) = turn(root, |journal| {
            journal.before_move(&root.join("src"), &root.join("lib"));
            fs::rename(root.join("src"), root.join("lib")).unwrap();
        });
        let edits = edits.unwrap();
        let shape: Vec<_> = edits.files.iter().map(|file| (file.path.as_str(), file.status.as_str())).collect();
        assert_eq!(
            shape,
            [("lib/a.rs", "added"), ("lib/deep/b.rs", "added"), ("src/a.rs", "deleted"), ("src/deep/b.rs", "deleted")]
        );
        undo(&dir, &edits.checkpoint, root).unwrap();
        assert_eq!(fs::read_to_string(root.join("src/deep/b.rs")).unwrap(), "b\n");
        assert!(!root.join("lib").exists());
    }

    #[test]
    fn the_marker_round_trips_and_names_the_edits() {
        let edits = TurnEdits {
            checkpoint: "20261104T120000000-abcd1234".into(),
            files: vec![FileChange {
                path: "a.rs".into(),
                status: "modified".into(),
                added: 3,
                removed: 1,
                binary: false,
            }],
            undone: false,
        };
        let marker = edits.marker();
        assert!(crate::compaction::is_marker(&marker));
        assert_eq!(TurnEdits::marked_by(&marker), Some(edits.clone()));
        assert_eq!(TurnEdits::marked_by(&json!({"role": "user", "content": ""})), None);
        assert_eq!(edits.item("i")["type"], "fileEdits");
        let note = undo_note(&["a.rs".into(), "b.rs".into()]);
        assert!(is_undo_note(&note));
        assert_eq!(undo_shown(&note), "Edits undone: a.rs, b.rs");
        assert!(manifest_path(Path::new("/tmp"), "../x").is_err());
    }
}
