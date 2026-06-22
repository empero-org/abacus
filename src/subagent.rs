use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, atomic::AtomicBool},
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, process::Command, sync::mpsc};
use uuid::Uuid;

use crate::{
    agent::{AgentEvent, AgentMode, TurnOptions, initial_messages, run_turn},
    compaction::CompactionState,
    goal::GoalState,
    model_info::CompactionBudget,
    provider::Provider,
    services::AgentServices,
    task::TaskList,
};

const MAX_SUBAGENTS: usize = 8;
const MAX_TASK_CHARS: usize = 24_000;
const MAX_PATCH_CHARS: usize = 120_000;
const MAX_UNTRACKED_FILES: usize = 10_000;
const MAX_UNTRACKED_BYTES: u64 = 500_000_000;
const MAX_UNTRACKED_FILE_BYTES: u64 = 100_000_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    tasks: Vec<SubagentTask>,
    #[serde(default)]
    apply: bool,
    #[serde(default = "default_concurrency")]
    max_concurrency: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentTask {
    name: String,
    prompt: String,
}

#[derive(Debug)]
struct SubagentResult {
    name: String,
    response: String,
    patch: String,
    error: Option<String>,
}

#[derive(Clone)]
pub struct SubagentRuntime {
    workspace: PathBuf,
    provider: Provider,
    services: Arc<AgentServices>,
    max_steps: usize,
    tool_output_limit: usize,
    web_search: crate::web::WebConfig,
}

impl SubagentRuntime {
    pub fn new(
        workspace: PathBuf,
        provider: Provider,
        services: Arc<AgentServices>,
        max_steps: usize,
        tool_output_limit: usize,
        web_search: crate::web::WebConfig,
    ) -> Self {
        Self {
            workspace,
            provider,
            services,
            max_steps,
            tool_output_limit,
            web_search,
        }
    }

    pub fn tool_spec() -> Value {
        json!({
            "type":"function",
            "function":{
                "name":"spawn_subagents",
                "description":"Delegate independent coding tasks to parallel agents in isolated git worktrees. Prefer this when the request splits into two or more genuinely separable units of work — independent files, modules, or fixes that need no shared intermediate state — and run them in one call; it is the efficient way to parallelize. Do NOT use it for a single task, for tightly-coupled or sequential edits, or for pure investigation — do that work directly. Each worker starts from the current workspace state and cannot spawn its own subagents. Returns each worker's result and patch; set apply=true to apply non-conflicting patches to the parent workspace.",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "tasks":{
                            "type":"array",
                            "minItems":1,
                            "maxItems":MAX_SUBAGENTS,
                            "items":{
                                "type":"object",
                                "properties":{
                                    "name":{"type":"string","description":"Short unique worker name"},
                                    "prompt":{"type":"string","description":"Self-contained coding assignment with expected verification"}
                                },
                                "required":["name","prompt"],
                                "additionalProperties":false
                            }
                        },
                        "apply":{"type":"boolean","description":"Apply each clean patch to the parent workspace after workers finish (default false)"},
                        "max_concurrency":{"type":"integer","minimum":1,"maximum":MAX_SUBAGENTS}
                    },
                    "required":["tasks"],
                    "additionalProperties":false
                }
            }
        })
    }

    pub fn approval_details(arguments: &str) -> String {
        match parse_args(arguments) {
            Ok(args) => {
                let names = args
                    .tasks
                    .iter()
                    .map(|task| task.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "Run {} isolated worker(s): {names}\nApply patches to this workspace: {}",
                    args.tasks.len(),
                    args.apply
                )
            }
            Err(error) => format!("Invalid subagent request: {error:#}"),
        }
    }

    pub async fn execute(&self, arguments: &str) -> String {
        match self.execute_inner(arguments).await {
            Ok(output) => output,
            Err(error) => format!("Error: {error:#}"),
        }
    }

    async fn execute_inner(&self, arguments: &str) -> Result<String> {
        let args = parse_args(arguments)?;
        let context = WorktreeContext::capture(&self.workspace).await?;
        let concurrency = args.max_concurrency.clamp(1, MAX_SUBAGENTS);
        let runtime = self.clone();
        let context = Arc::new(context);
        let mut results = stream::iter(args.tasks.into_iter().map(|task| {
            let runtime = runtime.clone();
            let context = context.clone();
            async move { runtime.run_one(context, task).await }
        }))
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
        results.sort_by(|left, right| left.name.cmp(&right.name));

        if args.apply {
            for result in &mut results {
                if result.error.is_none()
                    && !result.patch.trim().is_empty()
                    && let Err(error) = apply_patch(&context.repo_root, &result.patch).await
                {
                    result.error = Some(format!("patch was not applied: {error:#}"));
                }
            }
        }

        Ok(format_results(&results, args.apply))
    }

    async fn run_one(&self, context: Arc<WorktreeContext>, task: SubagentTask) -> SubagentResult {
        let name = task.name.clone();
        match self.run_one_inner(&context, &task).await {
            Ok((response, patch)) => SubagentResult {
                name,
                response,
                patch,
                error: None,
            },
            Err(error) => SubagentResult {
                name,
                response: String::new(),
                patch: String::new(),
                error: Some(format!("{error:#}")),
            },
        }
    }

    async fn run_one_inner(
        &self,
        context: &WorktreeContext,
        task: &SubagentTask,
    ) -> Result<(String, String)> {
        let worker_root = std::env::temp_dir().join("abacus-worktrees").join(format!(
            "{}-{}",
            safe_name(&task.name),
            Uuid::new_v4()
        ));
        if let Some(parent) = worker_root.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let setup = context.create(&worker_root).await;
        if let Err(error) = setup {
            let _ = context.remove(&worker_root).await;
            return Err(error);
        }

        let mut guard = WorktreeGuard::new(context.repo_root.clone(), worker_root.clone());
        let result = self.run_in_worktree(context, &worker_root, task).await;
        let cleanup = context.remove(&worker_root).await;
        if cleanup.is_ok() {
            guard.disarm();
        }
        match (result, cleanup) {
            (Ok(result), Ok(())) => Ok(result),
            (Ok(_), Err(error)) => Err(error.context("worker succeeded but cleanup failed")),
            (Err(error), _) => Err(error),
        }
    }

    async fn run_in_worktree(
        &self,
        context: &WorktreeContext,
        worker_root: &Path,
        task: &SubagentTask,
    ) -> Result<(String, String)> {
        let worker_workspace = worker_root.join(&context.workspace_relative);
        let mut messages = initial_messages(&worker_workspace);
        messages.push(json!({
            "role":"system",
            "content":"You are an isolated subagent. Complete only the delegated task. You may edit and test this worktree. Do not spawn more subagents, commit, push, or modify paths outside the workspace. Finish with a concise summary and exact checks run."
        }));
        messages.push(json!({"role":"user","content":task.prompt}));
        let (events, mut receiver) = mpsc::unbounded_channel();
        let services = Arc::new(self.services.for_workspace(worker_workspace.clone()));
        let turn = run_turn(
            self.provider.clone(),
            messages,
            TurnOptions {
                workspace: worker_workspace,
                max_steps: self.max_steps,
                tool_output_limit: self.tool_output_limit,
                mode: AgentMode::Build,
                allow_mutations: Arc::new(AtomicBool::new(true)),
                services,
                session_id: None,
                goal: GoalState::default(),
                tasks: TaskList::default(),
                compaction: CompactionState::default(),
                compaction_budget: CompactionBudget::default(),
                allow_subagents: false,
                web_search: self.web_search.clone(),
            },
            events,
        );
        let mut turn = turn;
        let mut final_messages = None;
        let mut failure = None;
        loop {
            tokio::select! {
                () = &mut turn => break,
                event = receiver.recv() => {
                    if let Some(event) = event {
                        capture_event(event, &mut final_messages, &mut failure);
                    }
                }
            }
        }
        while let Ok(event) = receiver.try_recv() {
            capture_event(event, &mut final_messages, &mut failure);
        }
        if let Some(error) = failure {
            bail!("subagent stopped: {error}");
        }
        let response = final_assistant_text(&final_messages.unwrap_or_default());
        let patch = context.diff(worker_root).await?;
        Ok((response, patch))
    }
}

fn capture_event(
    event: AgentEvent,
    final_messages: &mut Option<Vec<Value>>,
    failure: &mut Option<String>,
) {
    match event {
        AgentEvent::Approval(request) => {
            let _ = request.respond.send(crate::agent::ApprovalDecision::Once);
        }
        AgentEvent::Done { messages } => *final_messages = Some(messages),
        AgentEvent::Failed { error, messages } => {
            *failure = Some(error);
            *final_messages = Some(messages);
        }
        _ => {}
    }
}

struct WorktreeGuard {
    repo_root: PathBuf,
    worker_root: PathBuf,
    active: bool,
}

impl WorktreeGuard {
    fn new(repo_root: PathBuf, worker_root: PathBuf) -> Self {
        Self {
            repo_root,
            worker_root,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        if self.active && self.worker_root.exists() {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&self.repo_root)
                .args(["worktree", "remove", "--force"])
                .arg(&self.worker_root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[derive(Debug)]
struct WorktreeContext {
    repo_root: PathBuf,
    workspace_relative: PathBuf,
    baseline_patch: Vec<u8>,
    untracked: Vec<PathBuf>,
}

impl WorktreeContext {
    async fn capture(workspace: &Path) -> Result<Self> {
        let root = git_output(workspace, &["rev-parse", "--show-toplevel"])
            .await
            .context("subagents require a git workspace")?;
        let repo_root = PathBuf::from(root.trim()).canonicalize()?;
        let workspace = workspace.canonicalize()?;
        let workspace_relative = workspace
            .strip_prefix(&repo_root)
            .context("workspace is outside its git repository")?
            .to_owned();
        let scope = git_scope(&workspace_relative);
        let baseline_patch = git_output_bytes(
            &repo_root,
            &["diff", "--binary", "HEAD", "--", scope.as_str()],
            None,
        )
        .await?;
        let untracked_raw = git_output_bytes(
            &repo_root,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                scope.as_str(),
            ],
            None,
        )
        .await?;
        let untracked = untracked_raw
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                String::from_utf8(path.to_vec())
                    .map(PathBuf::from)
                    .context("subagent worktrees do not support non-UTF-8 untracked paths")
            })
            .collect::<Result<Vec<_>>>()?;
        if untracked.len() > MAX_UNTRACKED_FILES {
            bail!("workspace has more than {MAX_UNTRACKED_FILES} untracked files to seed");
        }
        let mut untracked_bytes = 0_u64;
        for relative in &untracked {
            let metadata = fs::symlink_metadata(repo_root.join(relative))?;
            if metadata.len() > MAX_UNTRACKED_FILE_BYTES {
                bail!("untracked file {} exceeds 100 MB", relative.display());
            }
            untracked_bytes = untracked_bytes.saturating_add(metadata.len());
            if untracked_bytes > MAX_UNTRACKED_BYTES {
                bail!("untracked workspace data exceeds 500 MB");
            }
        }
        Ok(Self {
            repo_root,
            workspace_relative,
            baseline_patch,
            untracked,
        })
    }

    async fn create(&self, worker_root: &Path) -> Result<()> {
        run_git(
            &self.repo_root,
            &[
                "worktree",
                "add",
                "--detach",
                path_text(worker_root)?,
                "HEAD",
            ],
            None,
        )
        .await
        .context("could not create isolated git worktree")?;
        if !self.baseline_patch.is_empty() {
            run_git(
                worker_root,
                &["apply", "--binary", "-"],
                Some(&self.baseline_patch),
            )
            .await
            .context("could not seed worker with current tracked changes")?;
        }
        for relative in &self.untracked {
            let source = self.repo_root.join(relative);
            let destination = worker_root.join(relative);
            copy_entry(&source, &destination)?;
        }
        let scope = git_scope(&self.workspace_relative);
        run_git(worker_root, &["add", "-A", "--", scope.as_str()], None).await?;
        let status = Command::new("git")
            .args(["-C", path_text(worker_root)?, "diff", "--cached", "--quiet"])
            .status()
            .await?;
        if !status.success() {
            let mut command = Command::new("git");
            command
                .args([
                    "-C",
                    path_text(worker_root)?,
                    "commit",
                    "-m",
                    "abacus worker baseline",
                ])
                .env("GIT_AUTHOR_NAME", "Abacus")
                .env("GIT_AUTHOR_EMAIL", "abacus@localhost")
                .env("GIT_COMMITTER_NAME", "Abacus")
                .env("GIT_COMMITTER_EMAIL", "abacus@localhost")
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            let output = command.output().await?;
            if !output.status.success() {
                bail!(
                    "could not snapshot worker baseline: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        Ok(())
    }

    async fn diff(&self, worker_root: &Path) -> Result<String> {
        let scope = git_scope(&self.workspace_relative);
        run_git(worker_root, &["add", "-N", "--", scope.as_str()], None).await?;
        let bytes = git_output_bytes(
            worker_root,
            &["diff", "--binary", "HEAD", "--", scope.as_str()],
            None,
        )
        .await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn remove(&self, worker_root: &Path) -> Result<()> {
        if worker_root.exists() {
            run_git(
                &self.repo_root,
                &["worktree", "remove", "--force", path_text(worker_root)?],
                None,
            )
            .await?;
        }
        Ok(())
    }
}

fn parse_args(arguments: &str) -> Result<SpawnArgs> {
    let args: SpawnArgs = serde_json::from_str(arguments).context("invalid subagent arguments")?;
    if args.tasks.is_empty() || args.tasks.len() > MAX_SUBAGENTS {
        bail!("tasks must contain 1 to {MAX_SUBAGENTS} entries");
    }
    let mut names = std::collections::HashSet::new();
    for task in &args.tasks {
        if task.name.trim().is_empty() || task.name.len() > 64 {
            bail!("worker names must contain 1 to 64 characters");
        }
        if !names.insert(task.name.to_ascii_lowercase()) {
            bail!("worker names must be unique");
        }
        if task.prompt.trim().is_empty() || task.prompt.len() > MAX_TASK_CHARS {
            bail!("each worker prompt must contain 1 to {MAX_TASK_CHARS} characters");
        }
    }
    Ok(args)
}

fn default_concurrency() -> usize {
    4
}

fn safe_name(name: &str) -> String {
    let value: String = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(24)
        .collect();
    if value.is_empty() {
        "worker".into()
    } else {
        value
    }
}

fn git_scope(relative: &Path) -> String {
    if relative.as_os_str().is_empty() {
        ".".into()
    } else {
        relative.to_string_lossy().into_owned()
    }
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("git path is not UTF-8"))
}

async fn git_output(directory: &Path, args: &[&str]) -> Result<String> {
    let output = git_output_bytes(directory, args, None).await?;
    String::from_utf8(output).context("git returned non-UTF-8 text")
}

async fn git_output_bytes(
    directory: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let output = run_git_output(directory, args, stdin).await?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

async fn run_git(directory: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<()> {
    let output = run_git_output(directory, args, stdin).await?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn run_git_output(
    directory: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<std::process::Output> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        // Keep seeded/diffed content byte-exact across platforms (Windows git
        // defaults to core.autocrlf=true, which would rewrite line endings).
        .args(["-c", "core.autocrlf=false"])
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("could not start git")?;
    if let (Some(input), Some(mut child_stdin)) = (stdin, child.stdin.take()) {
        child_stdin.write_all(input).await?;
    }
    Ok(child.wait_with_output().await?)
}

async fn apply_patch(repo_root: &Path, patch: &str) -> Result<()> {
    run_git(
        repo_root,
        &["apply", "--check", "--binary", "-"],
        Some(patch.as_bytes()),
    )
    .await?;
    run_git(
        repo_root,
        &["apply", "--binary", "-"],
        Some(patch.as_bytes()),
    )
    .await
}

fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        bail!("refusing to copy untracked symlink {}", source.display());
    }
    if metadata.is_dir() {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, destination)?;
    }
    Ok(())
}

fn final_assistant_text(messages: &[Value]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant" && message["content"].is_string())
        .and_then(|message| message["content"].as_str())
        .unwrap_or("Subagent completed without a textual summary.")
        .to_owned()
}

fn format_results(results: &[SubagentResult], applied: bool) -> String {
    let mut output = String::new();
    for result in results {
        output.push_str(&format!("## {}\n", result.name));
        if let Some(error) = &result.error {
            output.push_str(&format!("Status: failed\n{error}\n\n"));
            continue;
        }
        output.push_str(if applied {
            "Status: patch applied\n"
        } else {
            "Status: completed\n"
        });
        output.push_str(&result.response);
        output.push('\n');
        if !applied && !result.patch.is_empty() {
            let patch: String = result.patch.chars().take(MAX_PATCH_CHARS).collect();
            output.push_str("```diff\n");
            output.push_str(&patch);
            if patch.len() < result.patch.len() {
                output.push_str("\n… patch truncated");
            }
            output.push_str("\n```\n");
        }
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn validates_worker_bounds_and_names() {
        assert!(parse_args(r#"{"tasks":[]}"#).is_err());
        assert!(
            parse_args(r#"{"tasks":[{"name":"a","prompt":"x"},{"name":"A","prompt":"y"}]}"#)
                .is_err()
        );
        assert!(
            parse_args(r#"{"tasks":[{"name":"test","prompt":"verify it"}],"max_concurrency":2}"#)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn worktree_is_seeded_from_dirty_parent_and_returns_only_worker_changes() {
        if std::process::Command::new("git")
            .arg("--version")
            .status()
            .is_err()
        {
            return;
        }
        let directory = tempdir().unwrap();
        let repo = directory.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init"], None).await.unwrap();
        std::fs::write(repo.join("tracked.txt"), "base\n").unwrap();
        run_git(&repo, &["add", "tracked.txt"], None).await.unwrap();
        let output = Command::new("git")
            .args(["-C", path_text(&repo).unwrap(), "commit", "-m", "base"])
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@localhost")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@localhost")
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        std::fs::write(repo.join("tracked.txt"), "parent state\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "parent new\n").unwrap();

        let context = WorktreeContext::capture(&repo).await.unwrap();
        let worker = directory.path().join("worker");
        context.create(&worker).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(worker.join("tracked.txt")).unwrap(),
            "parent state\n"
        );
        assert_eq!(
            std::fs::read_to_string(worker.join("untracked.txt")).unwrap(),
            "parent new\n"
        );

        std::fs::write(worker.join("tracked.txt"), "worker state\n").unwrap();
        std::fs::write(worker.join("worker.txt"), "created\n").unwrap();
        let patch = context.diff(&worker).await.unwrap();
        assert!(patch.contains("worker state"));
        assert!(patch.contains("worker.txt"));
        apply_patch(&repo, &patch).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "worker state\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("worker.txt")).unwrap(),
            "created\n"
        );
        context.remove(&worker).await.unwrap();

        let cancelled_worker = directory.path().join("cancelled-worker");
        context.create(&cancelled_worker).await.unwrap();
        {
            let _guard = WorktreeGuard::new(context.repo_root.clone(), cancelled_worker.clone());
        }
        assert!(!cancelled_worker.exists());
    }
}
