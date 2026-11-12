use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Checkpoint {
    pub id: String,
    pub session_id: String,
    pub turn_index: usize,
    #[serde(default)]
    pub step_index: usize,
    #[serde(default)]
    pub tool_name: String,
    pub timestamp: u64,
    pub prompt_summary: String,
    pub commit_sha: Option<String>,
    pub touched_files: Vec<String>,
    pub created_files: Vec<String>,
    pub is_fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRevision {
    pub checkpoint_id: String,
    pub turn_index: usize,
    pub step_index: usize,
    pub timestamp: u64,
    pub tool_name: String,
    pub prompt_summary: String,
    pub was_created: bool,
    pub commit_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileHistory {
    pub file_path: String,
    pub status: String,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub revisions: Vec<FileRevision>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileDiffResponse {
    pub checkpoint_id: String,
    pub file_path: String,
    pub status: String,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub diff_text: String,
    pub before_content: Option<String>,
    pub after_content: Option<String>,
}

fn checkpoint_dir_key(turn_index: usize, step_index: usize) -> String {
    if step_index == 0 { turn_index.to_string() } else { format!("{}_{}", turn_index, step_index) }
}

fn normalize_rel_path(relative_path: &str) -> String {
    relative_path.replace('\\', "/").trim_start_matches("./").trim_start_matches('/').to_string()
}

fn compute_unified_diff(file_path: &str, before: Option<&str>, after: Option<&str>) -> (usize, usize, String) {
    let before_norm = before.map(|s| s.replace("\r\n", "\n"));
    let after_norm = after.map(|s| s.replace("\r\n", "\n"));

    let old_lines: Vec<&str> = match before_norm.as_deref() {
        Some(s) if !s.is_empty() => s.lines().collect(),
        _ => Vec::new(),
    };
    let new_lines: Vec<&str> = match after_norm.as_deref() {
        Some(s) if !s.is_empty() => s.lines().collect(),
        _ => Vec::new(),
    };

    let n = old_lines.len();
    let m = new_lines.len();

    // Guard against huge files for LCS table
    if n * m > 4_000_000 {
        let mut out = String::new();
        out.push_str(&format!("--- a/{}\n+++ b/{}\n@@ -1,{} +1,{} @@\n", file_path, file_path, n, m));
        for l in &old_lines {
            out.push_str(&format!("-{}\n", l));
        }
        for l in &new_lines {
            out.push_str(&format!("+{}\n", l));
        }
        return (m, n, out);
    }

    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            if old_lines[i] == new_lines[j] {
                dp[i][j] = dp[i + 1][j + 1] + 1;
            } else {
                dp[i][j] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum DiffOp<'a> {
        Equal(&'a str),
        Delete(&'a str),
        Insert(&'a str),
    }

    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0, 0);
    let mut added = 0usize;
    let mut removed = 0usize;

    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            ops.push(DiffOp::Equal(old_lines[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(DiffOp::Delete(old_lines[i]));
            removed += 1;
            i += 1;
        } else {
            ops.push(DiffOp::Insert(new_lines[j]));
            added += 1;
            j += 1;
        }
    }
    while i < n {
        ops.push(DiffOp::Delete(old_lines[i]));
        removed += 1;
        i += 1;
    }
    while j < m {
        ops.push(DiffOp::Insert(new_lines[j]));
        added += 1;
        j += 1;
    }

    if added == 0 && removed == 0 {
        return (0, 0, String::new());
    }

    let mut out = String::new();
    let old_header = if before.is_none() { "/dev/null".to_string() } else { format!("a/{}", file_path) };
    let new_header = if after.is_none() { "/dev/null".to_string() } else { format!("b/{}", file_path) };
    out.push_str(&format!("--- {}\n+++ {}\n", old_header, new_header));

    // Include context around changes (up to 3 context lines)
    let mut include = vec![false; ops.len()];
    for (idx, op) in ops.iter().enumerate() {
        if !matches!(op, DiffOp::Equal(_)) {
            let start = idx.saturating_sub(3);
            let end = (idx + 3).min(ops.len().saturating_sub(1));
            for slot in include.iter_mut().take(end + 1).skip(start) {
                *slot = true;
            }
        }
    }

    let mut old_ln = 1usize;
    let mut new_ln = 1usize;
    let mut in_hunk = false;

    for (idx, op) in ops.iter().enumerate() {
        if include[idx] {
            if !in_hunk {
                out.push_str(&format!("@@ -{},{} +{},{} @@\n", old_ln, n, new_ln, m));
                in_hunk = true;
            }
            match op {
                DiffOp::Equal(line) => {
                    out.push_str(&format!(" {}\n", line));
                    old_ln += 1;
                    new_ln += 1;
                }
                DiffOp::Delete(line) => {
                    out.push_str(&format!("-{}\n", line));
                    old_ln += 1;
                }
                DiffOp::Insert(line) => {
                    out.push_str(&format!("+{}\n", line));
                    new_ln += 1;
                }
            }
        } else {
            in_hunk = false;
            match op {
                DiffOp::Equal(_) => {
                    old_ln += 1;
                    new_ln += 1;
                }
                DiffOp::Delete(_) => old_ln += 1,
                DiffOp::Insert(_) => new_ln += 1,
            }
        }
    }

    (added, removed, out)
}

#[derive(Debug, Clone)]
pub struct CheckpointManager {
    pub root: PathBuf,
    pub checkpoints: HashMap<String, Vec<Checkpoint>>,
}

/// Sanitizes session ID to ensure valid Git ref names and safe directory names.
fn safe_session_name(session_id: &str) -> String {
    let s: String =
        session_id.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if s.is_empty() { "session".to_string() } else { s }
}

impl CheckpointManager {
    pub fn new(root: PathBuf) -> Self {
        let root = {
            let s = root.to_string_lossy();
            if let Some(stripped) = s.strip_prefix(r"\\?\") { PathBuf::from(stripped) } else { root }
        };
        Self { root, checkpoints: HashMap::new() }
    }

    /// Check if the root directory is inside a Git work tree.
    fn is_git_repo(&self) -> bool {
        let output = Command::new("git").args(["rev-parse", "--is-inside-work-tree"]).current_dir(&self.root).output();
        match output {
            Ok(out) if out.status.success() => {
                let s = String::from_utf8_lossy(&out.stdout);
                s.trim() == "true"
            }
            _ => false,
        }
    }

    /// Get the absolute path to the `.git` directory.
    fn get_git_dir(&self) -> Option<PathBuf> {
        let output = Command::new("git").args(["rev-parse", "--git-dir"]).current_dir(&self.root).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if s.is_empty() {
            return None;
        }
        let p = Path::new(&s);
        if p.is_absolute() { Some(p.to_path_buf()) } else { Some(self.root.join(p)) }
    }

    /// Prepares a checkpoint before any file-mutating tool executes in an agent turn.
    /// Uses an isolated Git tree commit via a temporary index file (`GIT_INDEX_FILE`),
    /// falling back to fast on-demand file copying for non-Git workspaces.
    pub fn prepare_turn_checkpoint(
        &mut self,
        session_id: &str,
        turn_index: usize,
        prompt: &str,
    ) -> anyhow::Result<Checkpoint> {
        self.prepare_tool_checkpoint(session_id, turn_index, 0, "", prompt)
    }

    /// Prepares a granular checkpoint before a specific file-mutating tool call (`step_index`).
    pub fn prepare_tool_checkpoint(
        &mut self,
        session_id: &str,
        turn_index: usize,
        step_index: usize,
        tool_name: &str,
        prompt: &str,
    ) -> anyhow::Result<Checkpoint> {
        if let Some(list) = self.checkpoints.get(session_id)
            && let Some(existing) = list.iter().find(|c| c.turn_index == turn_index && c.step_index == step_index)
        {
            return Ok(existing.clone());
        }

        let prompt_summary = prompt.lines().next().unwrap_or("").trim().chars().take(80).collect::<String>();
        let prompt_summary = if prompt_summary.is_empty() { format!("Turn {}", turn_index) } else { prompt_summary };

        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

        let dir_key = checkpoint_dir_key(turn_index, step_index);
        let id = if step_index == 0 {
            format!("chk_{}_{}", session_id, turn_index)
        } else {
            format!("chk_{}_{}_{}", session_id, turn_index, step_index)
        };
        let safe_session = safe_session_name(session_id);

        // Attempt Git shadow checkpoint if root is inside a git repo
        let mut git_commit_sha: Option<String> = None;
        let mut is_fallback = true;

        if self.is_git_repo()
            && let Some(git_dir) = self.get_git_dir()
        {
            let index_file = git_dir.join(format!("abacus_idx_{}_{}", safe_session, dir_key));

            if index_file.exists() {
                let _ = fs::remove_file(&index_file);
            }

            struct IndexCleanup<'a>(&'a Path);
            impl<'a> Drop for IndexCleanup<'a> {
                fn drop(&mut self) {
                    let _ = fs::remove_file(self.0);
                }
            }
            let _cleanup = IndexCleanup(&index_file);

            let git_attempt = (|| -> anyhow::Result<String> {
                // 1. git add -A using temporary index
                let add_status = Command::new("git")
                    .args(["add", "-A"])
                    .current_dir(&self.root)
                    .env("GIT_INDEX_FILE", &index_file)
                    .status()?;
                if !add_status.success() {
                    anyhow::bail!("git add -A failed: {:?}", add_status);
                }

                // 2. git write-tree
                let write_tree_out = Command::new("git")
                    .arg("write-tree")
                    .current_dir(&self.root)
                    .env("GIT_INDEX_FILE", &index_file)
                    .output()?;
                if !write_tree_out.status.success() {
                    anyhow::bail!("git write-tree failed: {}", String::from_utf8_lossy(&write_tree_out.stderr));
                }
                let tree_sha = String::from_utf8_lossy(&write_tree_out.stdout).trim().to_string();

                // 3. Find parent HEAD commit (if any)
                let head_out =
                    Command::new("git").args(["rev-parse", "--verify", "HEAD"]).current_dir(&self.root).output();
                let parent_sha = head_out.ok().and_then(|out| {
                    if out.status.success() {
                        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if !s.is_empty() { Some(s) } else { None }
                    } else {
                        None
                    }
                });

                // 4. git commit-tree
                let mut commit_cmd = Command::new("git");
                commit_cmd.args(["commit-tree", &tree_sha]);
                if let Some(ref parent) = parent_sha {
                    commit_cmd.args(["-p", parent]);
                }
                let msg = format!("abacus checkpoint {} turn {} step {}", session_id, turn_index, step_index);
                commit_cmd.args(["-m", &msg]);
                commit_cmd
                    .current_dir(&self.root)
                    .env("GIT_INDEX_FILE", &index_file)
                    .env("GIT_AUTHOR_NAME", "Abacus Checkpoint")
                    .env("GIT_AUTHOR_EMAIL", "checkpoint@abacus.internal")
                    .env("GIT_COMMITTER_NAME", "Abacus Checkpoint")
                    .env("GIT_COMMITTER_EMAIL", "checkpoint@abacus.internal");

                let commit_out = commit_cmd.output()?;
                if !commit_out.status.success() {
                    anyhow::bail!("git commit-tree failed: {}", String::from_utf8_lossy(&commit_out.stderr));
                }
                let commit_sha = String::from_utf8_lossy(&commit_out.stdout).trim().to_string();

                // 5. git update-ref with sanitized safe_session
                let ref_name = format!("refs/abacus/checkpoints/{}/{}", safe_session, dir_key);
                let update_ref_out = Command::new("git")
                    .args(["update-ref", &ref_name, &commit_sha])
                    .current_dir(&self.root)
                    .output()?;
                if !update_ref_out.status.success() {
                    anyhow::bail!("git update-ref failed: {}", String::from_utf8_lossy(&update_ref_out.stderr));
                }

                Ok(commit_sha)
            })();

            if let Ok(sha) = git_attempt {
                git_commit_sha = Some(sha);
                is_fallback = false;
            }
        }

        // In fallback mode, prepare checkpoint directory without scanning entire workspace
        if is_fallback {
            let backup_dir = self.root.join(".abacus").join("checkpoints").join(&safe_session).join(&dir_key);
            fs::create_dir_all(&backup_dir)?;
        }

        let checkpoint = Checkpoint {
            id,
            session_id: session_id.to_string(),
            turn_index,
            step_index,
            tool_name: tool_name.to_string(),
            timestamp,
            prompt_summary,
            commit_sha: git_commit_sha,
            touched_files: Vec::new(),
            created_files: Vec::new(),
            is_fallback,
        };

        self.checkpoints.entry(session_id.to_string()).or_default().push(checkpoint.clone());

        Ok(checkpoint)
    }

    /// Records a file touch on a specific `checkpoint_id`.
    pub fn record_checkpoint_touch(
        &mut self,
        session_id: &str,
        checkpoint_id: &str,
        relative_path: &str,
        was_new: bool,
    ) {
        let norm_path = normalize_rel_path(relative_path);
        let safe_session = safe_session_name(session_id);

        if let Some(list) = self.checkpoints.get_mut(session_id)
            && let Some(chk) = list.iter_mut().find(|c| c.id == checkpoint_id)
        {
            let dir_key = checkpoint_dir_key(chk.turn_index, chk.step_index);
            if was_new {
                if !chk.created_files.contains(&norm_path) {
                    chk.created_files.push(norm_path);
                }
            } else if !chk.created_files.contains(&norm_path) {
                if !chk.touched_files.contains(&norm_path) {
                    chk.touched_files.push(norm_path.clone());
                }
                if chk.is_fallback {
                    let src = self.root.join(&norm_path);
                    let dest = self
                        .root
                        .join(".abacus")
                        .join("checkpoints")
                        .join(&safe_session)
                        .join(&dir_key)
                        .join(&norm_path);
                    if !dest.exists() && src.exists() {
                        if let Some(parent) = dest.parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                        let _ = fs::copy(&src, &dest);
                    }
                }
            }
        }
    }

    /// Records a file touch by an agent tool for `turn_index`.
    /// `was_new` indicates whether the file was newly created in this turn.
    pub fn record_tool_touch(&mut self, session_id: &str, turn_index: usize, relative_path: &str, was_new: bool) {
        let target_id = self
            .checkpoints
            .get(session_id)
            .and_then(|list| list.iter().rev().find(|c| c.turn_index == turn_index))
            .map(|c| c.id.clone());
        if let Some(cid) = target_id {
            self.record_checkpoint_touch(session_id, &cid, relative_path, was_new);
        }
    }

    /// Reads the content of `norm_path` as it existed at `chk` (before the edit).
    fn read_snapshot_file(&self, chk: &Checkpoint, norm_path: &str) -> Option<String> {
        if chk.created_files.iter().any(|p| p == norm_path) {
            return None;
        }
        let safe_session = safe_session_name(&chk.session_id);
        let dir_key = checkpoint_dir_key(chk.turn_index, chk.step_index);
        if !chk.is_fallback {
            let sha = chk.commit_sha.as_ref()?;
            let spec = format!("{}:{}", sha, norm_path);
            let out = Command::new("git").args(["show", &spec]).current_dir(&self.root).output().ok()?;
            if out.status.success() { Some(String::from_utf8_lossy(&out.stdout).to_string()) } else { None }
        } else {
            let backup_path =
                self.root.join(".abacus").join("checkpoints").join(&safe_session).join(&dir_key).join(norm_path);
            fs::read_to_string(&backup_path).ok()
        }
    }

    /// Lists per-file revision histories across the session, sorted by most recently modified first.
    pub fn list_file_histories(&self, session_id: &str) -> Vec<FileHistory> {
        let Some(list) = self.checkpoints.get(session_id) else {
            return Vec::new();
        };

        let mut file_order: Vec<String> = Vec::new();
        let mut revisions_map: HashMap<String, Vec<FileRevision>> = HashMap::new();
        let mut earliest_chk_map: HashMap<String, Checkpoint> = HashMap::new();

        for chk in list {
            let mut all_in_chk: Vec<(String, bool)> = Vec::new();
            for f in &chk.created_files {
                all_in_chk.push((f.clone(), true));
            }
            for f in &chk.touched_files {
                if !chk.created_files.contains(f) {
                    all_in_chk.push((f.clone(), false));
                }
            }

            for (path, was_created) in all_in_chk {
                if let Some(pos) = file_order.iter().position(|p| p == &path) {
                    file_order.remove(pos);
                }
                file_order.push(path.clone());

                earliest_chk_map.entry(path.clone()).or_insert_with(|| chk.clone());

                revisions_map.entry(path).or_default().push(FileRevision {
                    checkpoint_id: chk.id.clone(),
                    turn_index: chk.turn_index,
                    step_index: chk.step_index,
                    timestamp: chk.timestamp,
                    tool_name: if chk.tool_name.is_empty() { "edit".to_string() } else { chk.tool_name.clone() },
                    prompt_summary: chk.prompt_summary.clone(),
                    was_created,
                    commit_sha: chk.commit_sha.clone(),
                });
            }
        }

        // Most recently modified files first
        file_order.reverse();

        let mut result = Vec::with_capacity(file_order.len());
        for path in file_order {
            let revs = revisions_map.remove(&path).unwrap_or_default();
            let earliest_chk = earliest_chk_map.get(&path);
            let was_initially_created = revs.first().map(|r| r.was_created).unwrap_or(false);
            let disk_path = self.root.join(&path);
            let after_content = fs::read_to_string(&disk_path).ok();
            let before_content = earliest_chk.and_then(|c| self.read_snapshot_file(c, &path));

            let status = if after_content.is_none() {
                "deleted".to_string()
            } else if was_initially_created || before_content.is_none() {
                "created".to_string()
            } else {
                "modified".to_string()
            };

            let (added_lines, removed_lines, _) =
                compute_unified_diff(&path, before_content.as_deref(), after_content.as_deref());

            result.push(FileHistory { file_path: path, status, added_lines, removed_lines, revisions: revs });
        }

        result
    }

    /// Computes a unified diff for `file_path` between `checkpoint_id` (before edit) and current workspace file on disk.
    pub fn diff_file(
        &self,
        session_id: &str,
        checkpoint_id: &str,
        file_path: &str,
    ) -> anyhow::Result<FileDiffResponse> {
        let norm_path = normalize_rel_path(file_path);
        let list = self
            .checkpoints
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("No checkpoints found for session '{}'", session_id))?;
        let chk = list
            .iter()
            .find(|c| c.id == checkpoint_id || c.turn_index.to_string() == checkpoint_id)
            .ok_or_else(|| anyhow::anyhow!("Checkpoint '{}' not found in session '{}'", checkpoint_id, session_id))?;

        let before_content = self.read_snapshot_file(chk, &norm_path);
        let disk_path = self.root.join(&norm_path);
        let after_content = fs::read_to_string(&disk_path).ok();

        let status = if after_content.is_none() {
            "deleted".to_string()
        } else if before_content.is_none() {
            "created".to_string()
        } else {
            "modified".to_string()
        };

        let (added_lines, removed_lines, diff_text) =
            compute_unified_diff(&norm_path, before_content.as_deref(), after_content.as_deref());

        Ok(FileDiffResponse {
            checkpoint_id: chk.id.clone(),
            file_path: norm_path,
            status,
            added_lines,
            removed_lines,
            diff_text,
            before_content,
            after_content,
        })
    }

    /// Selectively rolls back ONLY `file_path` to its state captured in `checkpoint_id`,
    /// leaving all other files in the workspace and in that checkpoint untouched.
    pub fn rollback_file(
        &mut self,
        session_id: &str,
        checkpoint_id: &str,
        file_path: &str,
    ) -> anyhow::Result<Vec<String>> {
        let norm_path = normalize_rel_path(file_path);
        let checkpoint = {
            let list = self
                .checkpoints
                .get(session_id)
                .ok_or_else(|| anyhow::anyhow!("No checkpoints found for session '{}'", session_id))?;
            list.iter()
                .find(|c| c.id == checkpoint_id || c.turn_index.to_string() == checkpoint_id)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("Checkpoint '{}' not found in session '{}'", checkpoint_id, session_id)
                })?
        };

        let safe_session = safe_session_name(session_id);
        let dir_key = checkpoint_dir_key(checkpoint.turn_index, checkpoint.step_index);
        let was_created_here = checkpoint.created_files.iter().any(|p| p == &norm_path);

        if was_created_here {
            let full = self.root.join(&norm_path);
            if full.exists() {
                if full.is_dir() {
                    let _ = fs::remove_dir_all(&full);
                } else {
                    let _ = fs::remove_file(&full);
                }
            }
            return Ok(vec![norm_path]);
        }

        if !checkpoint.is_fallback {
            if let Some(ref sha) = checkpoint.commit_sha {
                // Check if file exists in commit tree
                let spec = format!("{}:{}", sha, norm_path);
                let exists_in_commit = Command::new("git")
                    .args(["cat-file", "-e", &spec])
                    .current_dir(&self.root)
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);

                if exists_in_commit {
                    let out = Command::new("git")
                        .args(["restore", "--worktree", &format!("--source={}", sha), "--", &norm_path])
                        .current_dir(&self.root)
                        .output()?;
                    if !out.status.success() {
                        let err = String::from_utf8_lossy(&out.stderr);
                        anyhow::bail!("git restore failed for {}: {}", norm_path, err);
                    }
                } else {
                    // File did not exist at that snapshot commit; remove it
                    let full = self.root.join(&norm_path);
                    if full.exists() {
                        let _ = fs::remove_file(&full);
                    }
                }
            }
        } else {
            let backup_path =
                self.root.join(".abacus").join("checkpoints").join(&safe_session).join(&dir_key).join(&norm_path);
            let target_path = self.root.join(&norm_path);
            if backup_path.exists() {
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&backup_path, &target_path)?;
            }
        }

        Ok(vec![norm_path])
    }

    /// Rolls back the workspace to the state captured in `checkpoint_id`.
    /// Restores touched files and removes newly created files while preserving
    /// user edits outside `touched_files` and leaving `.git/index` untouched.
    pub fn rollback(&mut self, session_id: &str, checkpoint_id: &str) -> anyhow::Result<Vec<String>> {
        let checkpoint = {
            let list = self
                .checkpoints
                .get(session_id)
                .ok_or_else(|| anyhow::anyhow!("No checkpoints found for session '{}'", session_id))?;
            list.iter()
                .find(|c| c.id == checkpoint_id || c.turn_index.to_string() == checkpoint_id)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("Checkpoint '{}' not found in session '{}'", checkpoint_id, session_id)
                })?
        };

        let safe_session = safe_session_name(session_id);
        let dir_key = checkpoint_dir_key(checkpoint.turn_index, checkpoint.step_index);
        let mut restored_files = Vec::new();

        if !checkpoint.is_fallback {
            // Git mode:
            // 1. Delete created files
            for rel_path in &checkpoint.created_files {
                let full = self.root.join(rel_path);
                if full.exists() {
                    if full.is_dir() {
                        let _ = fs::remove_dir_all(&full);
                    } else {
                        let _ = fs::remove_file(&full);
                    }
                }
                restored_files.push(rel_path.clone());
            }

            // 2. Filter out any created files before invoking git restore
            let checkout_paths: Vec<&str> = checkpoint
                .touched_files
                .iter()
                .filter(|p| !checkpoint.created_files.contains(p))
                .map(|s| s.as_str())
                .collect();

            // 3. Restore pre-turn commit using `git restore --worktree --source=<sha>`
            // This updates ONLY the working tree and guarantees zero pollution of the user's active `.git/index`
            if let Some(ref sha) = checkpoint.commit_sha
                && !checkout_paths.is_empty()
            {
                let mut cmd = Command::new("git");
                cmd.args(["restore", "--worktree", &format!("--source={}", sha), "--"]);
                for path in &checkout_paths {
                    cmd.arg(path.replace('\\', "/"));
                }
                cmd.current_dir(&self.root);
                let out = cmd.output()?;
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    anyhow::bail!("git restore failed: {}", err);
                }
                for path in checkout_paths {
                    restored_files.push(path.to_string());
                }
            }
        } else {
            // Fallback mode:
            // 1. Delete created files
            for rel_path in &checkpoint.created_files {
                let full = self.root.join(rel_path);
                if full.exists() {
                    if full.is_dir() {
                        let _ = fs::remove_dir_all(&full);
                    } else {
                        let _ = fs::remove_file(&full);
                    }
                }
                restored_files.push(rel_path.clone());
            }

            // 2. Restore touched files from backup directory (excluding created files)
            let backup_base = self.root.join(".abacus").join("checkpoints").join(&safe_session).join(&dir_key);

            for rel_path in &checkpoint.touched_files {
                if checkpoint.created_files.contains(rel_path) {
                    continue;
                }
                let backup_path = backup_base.join(rel_path);
                let target_path = self.root.join(rel_path);
                if backup_path.exists() {
                    if let Some(parent) = target_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(&backup_path, &target_path)?;
                    restored_files.push(rel_path.clone());
                }
            }
        }

        restored_files.sort();
        restored_files.dedup();
        Ok(restored_files)
    }

    /// List all checkpoints recorded for a given session.
    pub fn list_checkpoints(&self, session_id: &str) -> Vec<Checkpoint> {
        self.checkpoints.get(session_id).cloned().unwrap_or_default()
    }

    /// Save session checkpoint metadata to `.abacus/sessions/<session_id>/checkpoints.json`.
    pub fn save_metadata(&self, session_id: &str) -> anyhow::Result<()> {
        let safe_session = safe_session_name(session_id);
        let dir = self.root.join(".abacus").join("sessions").join(&safe_session);
        fs::create_dir_all(&dir)?;
        let file_path = dir.join("checkpoints.json");
        let list = self.list_checkpoints(session_id);
        let json_str = serde_json::to_string_pretty(&list)?;
        fs::write(&file_path, json_str)?;
        Ok(())
    }

    /// Load session checkpoint metadata from `.abacus/sessions/<session_id>/checkpoints.json`.
    pub fn load_metadata(&mut self, session_id: &str) -> anyhow::Result<()> {
        let safe_session = safe_session_name(session_id);
        let file_path = self.root.join(".abacus").join("sessions").join(&safe_session).join("checkpoints.json");
        if file_path.exists() {
            let content = fs::read_to_string(&file_path)?;
            let list: Vec<Checkpoint> = serde_json::from_str(&content)?;
            self.checkpoints.insert(session_id.to_string(), list);
        }
        Ok(())
    }

    /// Prune old checkpoints exceeding `max_keep` for the session.
    pub fn prune(&mut self, session_id: &str, max_keep: usize) -> anyhow::Result<()> {
        let safe_session = safe_session_name(session_id);
        if let Some(list) = self.checkpoints.get_mut(session_id)
            && list.len() > max_keep
        {
            let split_index = list.len() - max_keep;
            let removed: Vec<Checkpoint> = list.drain(..split_index).collect();
            for chk in removed {
                let dir_key = checkpoint_dir_key(chk.turn_index, chk.step_index);
                if !chk.is_fallback {
                    let ref_name = format!("refs/abacus/checkpoints/{}/{}", safe_session, dir_key);
                    let _ = Command::new("git").args(["update-ref", "-d", &ref_name]).current_dir(&self.root).output();
                } else {
                    let backup_dir = self.root.join(".abacus").join("checkpoints").join(&safe_session).join(&dir_key);
                    let _ = fs::remove_dir_all(&backup_dir);
                }
            }
        }
        let _ = self.save_metadata(session_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::tempdir;

    fn init_test_git_repo(path: &std::path::Path) {
        Command::new("git").arg("init").current_dir(path).output().expect("git init failed");
        Command::new("git")
            .args(["config", "core.autocrlf", "false"])
            .current_dir(path)
            .output()
            .expect("git config core.autocrlf failed");
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(path)
            .output()
            .expect("git config user.name failed");
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(path)
            .output()
            .expect("git config user.email failed");
    }

    #[test]
    fn test_git_shadow_checkpoint_and_selective_rollback() -> anyhow::Result<()> {
        let dir = tempdir()?;
        init_test_git_repo(dir.path());

        fs::write(dir.path().join("a.txt"), "original a\n")?;
        fs::write(dir.path().join("user.txt"), "original user\n")?;

        Command::new("git").args(["add", "."]).current_dir(dir.path()).output()?;
        Command::new("git").args(["commit", "-m", "initial commit"]).current_dir(dir.path()).output()?;

        let mut mgr = CheckpointManager::new(dir.path().to_path_buf());
        let chk = mgr.prepare_turn_checkpoint("sess_1", 1, "update a and create b")?;
        assert!(!chk.is_fallback, "expected git shadow mode");
        assert!(chk.commit_sha.is_some());

        // Agent modifies a.txt
        fs::write(dir.path().join("a.txt"), "modified a by agent\n")?;
        mgr.record_tool_touch("sess_1", 1, "a.txt", false);

        // Agent creates b.txt
        fs::write(dir.path().join("b.txt"), "new b by agent\n")?;
        mgr.record_tool_touch("sess_1", 1, "b.txt", true);

        // Concurrent user edits user.txt (NOT recorded in agent touches)
        fs::write(dir.path().join("user.txt"), "concurrent user edit\n")?;

        // Rollback
        let restored = mgr.rollback("sess_1", &chk.id)?;

        // Verify selective restoration
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt"))?.replace("\r\n", "\n"),
            "original a\n",
            "a.txt should be reverted to pre-turn state"
        );
        assert!(!dir.path().join("b.txt").exists(), "b.txt should be deleted upon rollback");
        assert_eq!(
            fs::read_to_string(dir.path().join("user.txt"))?.replace("\r\n", "\n"),
            "concurrent user edit\n",
            "user.txt should NOT be touched during rollback"
        );
        assert!(restored.contains(&"a.txt".to_string()));
        assert!(restored.contains(&"b.txt".to_string()));
        assert!(!restored.contains(&"user.txt".to_string()));

        // Verify zero pollution of user's active .git/index
        let diff_cached =
            Command::new("git").args(["diff", "--cached", "--name-only"]).current_dir(dir.path()).output()?;
        let cached_out = String::from_utf8_lossy(&diff_cached.stdout);
        assert!(
            cached_out.trim().is_empty(),
            "active .git/index must have zero staged files after rollback, got: {}",
            cached_out
        );

        Ok(())
    }

    #[test]
    fn test_create_and_modify_same_turn_rolls_back_cleanly() -> anyhow::Result<()> {
        let dir = tempdir()?;
        init_test_git_repo(dir.path());

        fs::write(dir.path().join("existing.txt"), "initial\n")?;
        Command::new("git").args(["add", "."]).current_dir(dir.path()).output()?;
        Command::new("git").args(["commit", "-m", "init"]).current_dir(dir.path()).output()?;

        let mut mgr = CheckpointManager::new(dir.path().to_path_buf());
        // Use a session ID with special characters to also test ref sanitization
        let chk = mgr.prepare_turn_checkpoint("sess/special:id?#1", 1, "create then edit")?;

        // 1. Agent creates new_file.txt
        fs::write(dir.path().join("new_file.txt"), "first write\n")?;
        mgr.record_tool_touch("sess/special:id?#1", 1, "new_file.txt", true);

        // 2. Later in same turn, agent modifies new_file.txt
        fs::write(dir.path().join("new_file.txt"), "second modified write\n")?;
        mgr.record_tool_touch("sess/special:id?#1", 1, "new_file.txt", false);

        // Verify that new_file.txt stayed in created_files and was NOT added to touched_files
        let current_chk = mgr.list_checkpoints("sess/special:id?#1");
        assert_eq!(current_chk[0].created_files, vec!["new_file.txt"]);
        assert!(current_chk[0].touched_files.is_empty());

        // 3. Rollback should cleanly delete new_file.txt without crashing git restore
        let restored = mgr.rollback("sess/special:id?#1", &chk.id)?;

        assert!(!dir.path().join("new_file.txt").exists(), "new_file.txt should be deleted on rollback");
        assert!(restored.contains(&"new_file.txt".to_string()));

        // Also test prune with sanitized ref name
        mgr.prune("sess/special:id?#1", 0)?;

        Ok(())
    }

    #[test]
    fn test_non_git_fallback_checkpoint_and_rollback() -> anyhow::Result<()> {
        let dir = tempdir()?;
        // Non-git directory: do NOT call git init
        fs::write(dir.path().join("a.txt"), "original a\n")?;
        fs::write(dir.path().join("user.txt"), "original user\n")?;

        let mut mgr = CheckpointManager::new(dir.path().to_path_buf());
        let chk = mgr.prepare_turn_checkpoint("sess_fallback", 1, "non git turn")?;
        assert!(chk.is_fallback, "expected fallback mode for non-git directory");

        // Agent touches a.txt prior to modification
        mgr.record_tool_touch("sess_fallback", 1, "a.txt", false);
        fs::write(dir.path().join("a.txt"), "modified a in fallback\n")?;

        // Agent creates b.txt
        mgr.record_tool_touch("sess_fallback", 1, "b.txt", true);
        fs::write(dir.path().join("b.txt"), "new b in fallback\n")?;

        // Concurrent user edit to user.txt
        fs::write(dir.path().join("user.txt"), "user edit preserved\n")?;

        // Rollback
        let restored = mgr.rollback("sess_fallback", &chk.id)?;

        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt"))?.replace("\r\n", "\n"),
            "original a\n",
            "a.txt should be restored from backup"
        );
        assert!(!dir.path().join("b.txt").exists(), "b.txt should be deleted upon rollback");
        assert_eq!(
            fs::read_to_string(dir.path().join("user.txt"))?.replace("\r\n", "\n"),
            "user edit preserved\n",
            "user.txt should be untouched"
        );
        assert!(restored.contains(&"a.txt".to_string()));
        assert!(restored.contains(&"b.txt".to_string()));
        assert!(!restored.contains(&"user.txt".to_string()));

        Ok(())
    }

    #[test]
    fn test_metadata_save_and_load() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let mut mgr = CheckpointManager::new(dir.path().to_path_buf());

        let chk = mgr.prepare_turn_checkpoint("sess_meta", 1, "test metadata")?;
        mgr.record_tool_touch("sess_meta", 1, "test.rs", false);
        mgr.record_tool_touch("sess_meta", 1, "new.rs", true);

        mgr.save_metadata("sess_meta")?;

        let meta_file = dir.path().join(".abacus").join("sessions").join("sess_meta").join("checkpoints.json");
        assert!(meta_file.exists(), "checkpoints.json should exist");

        let mut mgr2 = CheckpointManager::new(dir.path().to_path_buf());
        assert!(mgr2.list_checkpoints("sess_meta").is_empty());

        mgr2.load_metadata("sess_meta")?;
        let loaded = mgr2.list_checkpoints("sess_meta");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, chk.id);
        assert_eq!(loaded[0].touched_files, vec!["test.rs"]);
        assert_eq!(loaded[0].created_files, vec!["new.rs"]);

        Ok(())
    }

    #[test]
    fn test_prune_checkpoints() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let mut mgr = CheckpointManager::new(dir.path().to_path_buf());

        mgr.prepare_turn_checkpoint("sess_prune", 1, "turn 1")?;
        mgr.prepare_turn_checkpoint("sess_prune", 2, "turn 2")?;
        mgr.prepare_turn_checkpoint("sess_prune", 3, "turn 3")?;

        assert_eq!(mgr.list_checkpoints("sess_prune").len(), 3);

        mgr.prune("sess_prune", 2)?;
        let remaining = mgr.list_checkpoints("sess_prune");
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].turn_index, 2);
        assert_eq!(remaining[1].turn_index, 3);

        Ok(())
    }

    #[test]
    fn test_per_file_rollback_preserves_other_files_in_same_checkpoint() -> anyhow::Result<()> {
        let dir = tempdir()?;
        init_test_git_repo(dir.path());

        fs::write(dir.path().join("a.txt"), "original a\n")?;
        fs::write(dir.path().join("b.txt"), "original b\n")?;

        Command::new("git").args(["add", "."]).current_dir(dir.path()).output()?;
        Command::new("git").args(["commit", "-m", "initial commit"]).current_dir(dir.path()).output()?;

        let mut mgr = CheckpointManager::new(dir.path().to_path_buf());
        let chk = mgr.prepare_tool_checkpoint("sess_file", 1, 1, "edit_file", "edit a, b, create c")?;

        // Agent modifies a.txt and b.txt, and creates c.txt
        fs::write(dir.path().join("a.txt"), "modified a\n")?;
        mgr.record_checkpoint_touch("sess_file", &chk.id, "a.txt", false);

        fs::write(dir.path().join("b.txt"), "modified b\n")?;
        mgr.record_checkpoint_touch("sess_file", &chk.id, "b.txt", false);

        fs::write(dir.path().join("c.txt"), "new c\n")?;
        mgr.record_checkpoint_touch("sess_file", &chk.id, "c.txt", true);

        // 1. Rollback ONLY a.txt
        let restored_a = mgr.rollback_file("sess_file", &chk.id, "a.txt")?;
        assert_eq!(restored_a, vec!["a.txt"]);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt"))?.replace("\r\n", "\n"),
            "original a\n",
            "a.txt should be restored"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt"))?.replace("\r\n", "\n"),
            "modified b\n",
            "b.txt must remain modified"
        );
        assert!(dir.path().join("c.txt").exists(), "c.txt must still exist after rolling back only a.txt");

        // 2. Rollback ONLY c.txt (newly created file)
        let restored_c = mgr.rollback_file("sess_file", &chk.id, "c.txt")?;
        assert_eq!(restored_c, vec!["c.txt"]);
        assert!(!dir.path().join("c.txt").exists(), "c.txt should be deleted when selectively rolled back");
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt"))?.replace("\r\n", "\n"),
            "modified b\n",
            "b.txt must still remain modified"
        );

        Ok(())
    }

    #[test]
    fn test_file_histories_and_unified_diff() -> anyhow::Result<()> {
        let dir = tempdir()?;
        init_test_git_repo(dir.path());

        fs::write(dir.path().join("a.txt"), "line1\nline2\n")?;
        Command::new("git").args(["add", "."]).current_dir(dir.path()).output()?;
        Command::new("git").args(["commit", "-m", "initial commit"]).current_dir(dir.path()).output()?;

        let mut mgr = CheckpointManager::new(dir.path().to_path_buf());

        // Step 1: edit a.txt
        let chk1 = mgr.prepare_tool_checkpoint("sess_diff", 1, 1, "edit_file", "update a")?;
        mgr.record_checkpoint_touch("sess_diff", &chk1.id, "a.txt", false);
        fs::write(dir.path().join("a.txt"), "line1\nline2_mod\nline3\n")?;

        // Step 2: create new.txt
        let chk2 = mgr.prepare_tool_checkpoint("sess_diff", 1, 2, "write_file", "create new")?;
        mgr.record_checkpoint_touch("sess_diff", &chk2.id, "new.txt", true);
        fs::write(dir.path().join("new.txt"), "hello\nworld\n")?;

        let histories = mgr.list_file_histories("sess_diff");
        assert_eq!(histories.len(), 2);

        let a_hist = histories.iter().find(|h| h.file_path == "a.txt").expect("a.txt history");
        assert_eq!(a_hist.status, "modified");
        assert_eq!(a_hist.revisions.len(), 1);
        assert_eq!(a_hist.revisions[0].tool_name, "edit_file");

        let new_hist = histories.iter().find(|h| h.file_path == "new.txt").expect("new.txt history");
        assert_eq!(new_hist.status, "created");
        assert_eq!(new_hist.added_lines, 2);

        let diff_a = mgr.diff_file("sess_diff", &chk1.id, "a.txt")?;
        assert!(diff_a.diff_text.contains("-line2"));
        assert!(diff_a.diff_text.contains("+line2_mod"));
        assert_eq!(diff_a.added_lines, 2);
        assert_eq!(diff_a.removed_lines, 1);

        let diff_new = mgr.diff_file("sess_diff", &chk2.id, "new.txt")?;
        assert!(diff_new.before_content.is_none());
        assert!(diff_new.diff_text.contains("+hello"));
        assert_eq!(diff_new.added_lines, 2);
        assert_eq!(diff_new.removed_lines, 0);

        Ok(())
    }
}
