use std::{
    fs::{self, File},
    io::Write,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::TextDiff;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

const MAX_OUTPUT: usize = 30_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Deserialize)]
struct EditFileArgs {
    path: String,
    #[serde(default)]
    old_text: Option<String>,
    #[serde(default)]
    new_text: Option<String>,
    #[serde(default)]
    edits: Vec<EditOperation>,
}

#[derive(Deserialize)]
struct EditOperation {
    old_text: String,
    new_text: String,
}

impl EditFileArgs {
    fn into_operations(self) -> Result<(String, Vec<EditOperation>)> {
        let mut operations = self.edits;
        match (self.old_text, self.new_text) {
            (Some(old_text), Some(new_text)) => {
                operations.insert(0, EditOperation { old_text, new_text })
            }
            (None, None) => {}
            _ => bail!("old_text and new_text must be provided together"),
        }
        if operations.is_empty() {
            bail!("at least one edit is required");
        }
        Ok((self.path, operations))
    }
}

impl ToolCall {
    pub fn summary(&self) -> String {
        let args: Value = serde_json::from_str(&self.arguments).unwrap_or(Value::Null);
        match self.name.as_str() {
            "run_command" => args["command"]
                .as_str()
                .unwrap_or("shell command")
                .to_owned(),
            "edit_file" | "write_file" | "read_file" | "delete_file" | "append_file"
            | "git_show" | "git_blame" => {
                args["path"].as_str().unwrap_or("unknown path").to_owned()
            }
            "move_file" => format!(
                "{} → {}",
                args["source"].as_str().unwrap_or("source"),
                args["destination"].as_str().unwrap_or("destination")
            ),
            "apply_patch" => "workspace patch".to_owned(),
            "web_search" => args["query"].as_str().unwrap_or("web search").to_owned(),
            "read_page" => args["url"].as_str().unwrap_or("web page").to_owned(),
            "grep" => format!(
                "{} in {}",
                args["query"].as_str().unwrap_or("pattern"),
                args["path"].as_str().unwrap_or(".")
            ),
            "list_files" => args["path"].as_str().unwrap_or(".").to_owned(),
            "glob" => args["pattern"].as_str().unwrap_or("*").to_owned(),
            "git_diff" => match (args["base"].as_str(), args["head"].as_str()) {
                (Some(base), Some(head)) => format!("{base}..{head}"),
                (Some(base), None) => format!("since {base}"),
                _ => "working tree changes".to_owned(),
            },
            "git_status" => "repository status".to_owned(),
            "git_log" => "recent commits".to_owned(),
            "git_commit" => {
                let message = args["message"].as_str().unwrap_or("");
                let preview: String = message.chars().take(60).collect();
                format!("commit: {preview}")
            }
            "git_restore" => format!(
                "restore {} path(s)",
                args["paths"].as_array().map(Vec::len).unwrap_or(0)
            ),
            "git_checkout" => args["branch"].as_str().unwrap_or("branch").to_owned(),
            "read_files" => format!(
                "{} file(s)",
                args["paths"].as_array().map(Vec::len).unwrap_or(0)
            ),
            "tool_search" => args["query"].as_str().unwrap_or("all tools").to_owned(),
            _ => self.arguments.clone(),
        }
    }

    pub fn needs_approval(&self) -> bool {
        matches!(
            self.name.as_str(),
            "edit_file"
                | "write_file"
                | "apply_patch"
                | "delete_file"
                | "move_file"
                | "create_directory"
                | "run_command"
                | "git_commit"
                | "git_restore"
                | "git_checkout"
                | "append_file"
        )
    }
}

#[derive(Debug, Clone)]
pub struct ToolExecutor {
    root: PathBuf,
    output_limit: usize,
    web: crate::web::WebConfig,
}

impl ToolExecutor {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            output_limit: MAX_OUTPUT,
            web: crate::web::WebConfig::default(),
        }
    }

    pub fn with_output_limit(root: PathBuf, output_limit: usize) -> Self {
        Self {
            root,
            output_limit: output_limit.clamp(2_000, 200_000),
            web: crate::web::WebConfig::default(),
        }
    }

    /// Attach the resolved web-search configuration (enables `web_search` /
    /// `read_page`).
    pub fn with_web(mut self, web: crate::web::WebConfig) -> Self {
        self.web = web;
        self
    }

    pub async fn execute(&self, call: &ToolCall) -> String {
        let result = match call.name.as_str() {
            "read_file" => self.read_file(&call.arguments),
            "read_files" => self.read_files(&call.arguments),
            "list_files" => self.list_files(&call.arguments),
            "grep" => self.grep(&call.arguments),
            "glob" => self.glob(&call.arguments),
            "tool_search" => self.tool_search(&call.arguments),
            "web_search" => self.web_search(&call.arguments).await,
            "read_page" => self.read_page(&call.arguments).await,
            "edit_file" => self.edit_file(&call.arguments),
            "write_file" => self.write_file(&call.arguments),
            "append_file" => self.append_file(&call.arguments),
            "apply_patch" => self.apply_patch(&call.arguments).await,
            "delete_file" => self.delete_file(&call.arguments),
            "move_file" => self.move_file(&call.arguments),
            "create_directory" => self.create_directory(&call.arguments),
            "git_diff" => self.git_diff(&call.arguments).await,
            "git_status" => self.git_status().await,
            "git_log" => self.git_log(&call.arguments).await,
            "git_commit" => self.git_commit(&call.arguments).await,
            "git_restore" => self.git_restore(&call.arguments).await,
            "git_show" => self.git_show(&call.arguments).await,
            "git_blame" => self.git_blame(&call.arguments).await,
            "git_checkout" => self.git_checkout(&call.arguments).await,
            "run_command" => self.run_command(&call.arguments).await,
            other => Err(anyhow!("unknown tool: {other}")),
        };

        match result {
            Ok(output) => truncate(output, self.output_limit),
            Err(error) => format!("Error: {error:#}"),
        }
    }

    pub fn approval_details(&self, call: &ToolCall) -> String {
        let result = match call.name.as_str() {
            "edit_file" => self.preview_edit(&call.arguments),
            "write_file" => self.preview_write(&call.arguments),
            "apply_patch" => self.preview_patch(&call.arguments),
            "delete_file" => self.preview_delete(&call.arguments),
            "git_commit" => self.preview_git_commit(&call.arguments),
            "git_restore" => self.preview_git_restore(&call.arguments),
            "git_checkout" => Ok(format!("git checkout {}", call.summary())),
            "append_file" => self.preview_append(&call.arguments),
            "move_file" => Ok(format!("Move {}", call.summary())),
            "create_directory" => Ok(format!("Create directory {}", call.summary())),
            "run_command" => Ok(format!("$ {}", call.summary())),
            _ => Ok(call.summary()),
        };
        truncate(
            result.unwrap_or_else(|error| format!("Could not prepare preview: {error:#}")),
            12_000,
        )
    }

    fn read_file(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            #[serde(default = "default_offset")]
            offset: usize,
            #[serde(default = "default_limit")]
            limit: usize,
        }

        let args: Args = parse_args(arguments)?;
        self.read_file_range(&args.path, args.offset, args.limit)
    }

    fn read_files(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            paths: Vec<String>,
            #[serde(default = "default_limit")]
            limit: usize,
        }

        let args: Args = parse_args(arguments)?;
        if args.paths.is_empty() {
            bail!("at least one path is required");
        }
        if args.paths.len() > 20 {
            bail!("read_files accepts at most 20 paths");
        }
        let limit = args.limit.clamp(1, 2_000);
        let mut sections = Vec::new();
        for raw in &args.paths {
            let body = match self.read_file_range(raw, 1, limit) {
                Ok(body) => body,
                Err(error) => format!("Error: {error:#}"),
            };
            sections.push(format!("===== {raw} =====\n{body}"));
        }
        Ok(sections.join("\n\n"))
    }

    fn read_file_range(&self, raw_path: &str, offset: usize, limit: usize) -> Result<String> {
        let path = self.resolve_existing(raw_path)?;
        guard_secret(&path)?;
        if path.metadata()?.len() > 10_000_000 {
            bail!("file is larger than the 10 MB read limit");
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("could not read {raw_path} as UTF-8 text"))?;
        let offset = offset.max(1);
        let limit = limit.clamp(1, 2_000);
        let lines: Vec<_> = content.lines().collect();

        if offset > lines.len().max(1) {
            bail!(
                "offset {offset} is beyond the end of the file ({} lines)",
                lines.len()
            );
        }

        let selected = lines
            .iter()
            .enumerate()
            .skip(offset - 1)
            .take(limit)
            .map(|(index, line)| format!("{:>5} | {line}", index + 1))
            .collect::<Vec<_>>()
            .join("\n");
        let remaining = lines.len().saturating_sub(offset - 1 + limit);
        if remaining > 0 {
            Ok(format!("{selected}\n\n… {remaining} more lines"))
        } else {
            Ok(selected)
        }
    }

    fn list_files(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default = "dot")]
            path: String,
            #[serde(default = "default_depth")]
            depth: usize,
        }

        let args: Args = parse_args(arguments)?;
        let base = self.resolve_existing(&args.path)?;
        let depth = args.depth.clamp(1, 8);
        let mut entries = Vec::new();

        for entry in WalkBuilder::new(&base)
            .max_depth(Some(depth))
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .filter_entry(skip_vcs_dir)
            .build()
            .filter_map(Result::ok)
            .skip(1)
        {
            if entries.len() >= 500 {
                entries.push("… output capped at 500 entries".to_owned());
                break;
            }
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path());
            let suffix = if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                "/"
            } else {
                ""
            };
            entries.push(format!("{}{suffix}", relative.display()));
        }

        if entries.is_empty() {
            Ok("(empty directory)".to_owned())
        } else {
            Ok(entries.join("\n"))
        }
    }

    fn grep(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            query: String,
            #[serde(default = "dot")]
            path: String,
            #[serde(default)]
            glob: Vec<String>,
            #[serde(default = "default_true")]
            case_sensitive: bool,
            #[serde(default = "default_search_limit")]
            max_results: usize,
            #[serde(default)]
            context: usize,
        }

        let args: Args = parse_args(arguments)?;
        let expression = if args.case_sensitive {
            args.query.clone()
        } else {
            format!("(?i:{})", args.query)
        };
        let regex = Regex::new(&expression).context("invalid regular expression")?;
        let globs = build_globs(&args.glob)?;
        let base = self.resolve_existing(&args.path)?;
        let context = args.context.clamp(0, 10);
        let max_results = args.max_results.clamp(1, 2_000);
        let mut matches = Vec::new();
        let mut match_count = 0usize;

        for entry in WalkBuilder::new(base)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .filter_entry(skip_vcs_dir)
            .build()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path());
            if let Some(globs) = &globs
                && !globs.is_match(relative)
            {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() > 1_000_000 || guard_secret(entry.path()).is_err() {
                continue;
            }
            let Ok(content) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let lines: Vec<&str> = content.lines().collect();
            let remaining = max_results - match_count;
            let hits: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| regex.is_match(line))
                .map(|(index, _)| index)
                .take(remaining)
                .collect();
            if hits.is_empty() {
                continue;
            }
            if context == 0 {
                for index in &hits {
                    matches.push(format!(
                        "{}:{}: {}",
                        relative.display(),
                        index + 1,
                        lines[*index]
                    ));
                }
            } else {
                let mut windows: Vec<(usize, usize)> = Vec::new();
                for index in &hits {
                    let start = index.saturating_sub(context);
                    let end = (*index + context).min(lines.len().saturating_sub(1));
                    match windows.last_mut() {
                        Some((_, tail)) if *tail + 1 >= start => *tail = (*tail).max(end),
                        _ => windows.push((start, end)),
                    }
                }
                for (start, end) in windows {
                    if !matches.is_empty() {
                        matches.push("--".to_owned());
                    }
                    for (offset, line) in lines[start..=end].iter().enumerate() {
                        let index = start + offset;
                        let marker = if hits.contains(&index) { ':' } else { '-' };
                        matches.push(format!(
                            "{}:{}{} {}",
                            relative.display(),
                            index + 1,
                            marker,
                            line
                        ));
                    }
                }
            }
            match_count += hits.len();
            if match_count >= max_results {
                matches.push(format!("… output capped at {max_results} matches"));
                return Ok(matches.join("\n"));
            }
        }

        if matches.is_empty() {
            Ok("No matches.".to_owned())
        } else {
            Ok(matches.join("\n"))
        }
    }

    fn glob(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            pattern: String,
            #[serde(default = "dot")]
            path: String,
            #[serde(default = "default_glob_limit")]
            max_results: usize,
        }

        let args: Args = parse_args(arguments)?;
        let matcher = Glob::new(&args.pattern)
            .context("invalid glob pattern")?
            .compile_matcher();
        let base = self.resolve_existing(&args.path)?;
        let max_results = args.max_results.clamp(1, 5_000);
        let mut results = Vec::new();
        for entry in WalkBuilder::new(base)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .filter_entry(skip_vcs_dir)
            .build()
            .filter_map(Result::ok)
        {
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path());
            if matcher.is_match(relative) {
                let suffix = if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                    "/"
                } else {
                    ""
                };
                results.push(format!("{}{suffix}", relative.display()));
                if results.len() >= max_results {
                    results.push(format!("… output capped at {max_results} paths"));
                    break;
                }
            }
        }
        if results.is_empty() {
            Ok("No matching paths.".to_owned())
        } else {
            Ok(results.join("\n"))
        }
    }

    fn tool_search(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            query: String,
        }
        let args: Args = parse_args(arguments)?;
        let query = args.query.to_ascii_lowercase();
        let matches = tool_catalog()
            .into_iter()
            .filter(|(name, description)| {
                query.is_empty()
                    || name.to_ascii_lowercase().contains(&query)
                    || description.to_ascii_lowercase().contains(&query)
            })
            .map(|(name, description)| format!("{name}: {description}"))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            Ok(format!(
                "No tools match {:?}. Search with a broader capability word.",
                args.query
            ))
        } else {
            Ok(matches.join("\n"))
        }
    }

    async fn web_search(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            query: String,
            #[serde(default)]
            max_results: Option<usize>,
        }
        if !self.web.enabled {
            bail!("web tools are disabled; set `[search] enabled = true`");
        }
        let args: Args = parse_args(arguments)?;
        self.web
            .search(&args.query, args.max_results.unwrap_or(5))
            .await
    }

    async fn read_page(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            url: String,
            #[serde(default)]
            max_chars: Option<usize>,
        }
        if !self.web.enabled {
            bail!("web tools are disabled; set `[search] enabled = true`");
        }
        let args: Args = parse_args(arguments)?;
        self.web
            .read_page(&args.url, args.max_chars.unwrap_or(0))
            .await
    }

    fn edit_file(&self, arguments: &str) -> Result<String> {
        let args: EditFileArgs = parse_args(arguments)?;
        let (raw_path, operations) = args.into_operations()?;
        let path = self.resolve_existing(&raw_path)?;
        guard_secret(&path)?;
        let mut content = fs::read_to_string(&path)
            .with_context(|| format!("could not read {raw_path} as UTF-8 text"))?;
        for (index, edit) in operations.iter().enumerate() {
            if edit.old_text.is_empty() {
                bail!("edit {} has empty old_text", index + 1);
            }
            let count = content.matches(&edit.old_text).count();
            if count != 1 {
                bail!(
                    "edit {} old_text must match exactly once; found {count} matches",
                    index + 1
                );
            }
            content = content.replacen(&edit.old_text, &edit.new_text, 1);
        }
        write_text_atomic(&path, &content)
            .with_context(|| format!("could not write {raw_path}"))?;
        Ok(format!(
            "Applied {} edit(s) to {raw_path}.",
            operations.len()
        ))
    }

    fn write_file(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
        }

        let args: Args = parse_args(arguments)?;
        let path = self.resolve_for_write(&args.path)?;
        guard_secret(&path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create parent directory for {}", args.path))?;
        }
        write_text_atomic(&path, &args.content)
            .with_context(|| format!("could not write {}", args.path))?;
        Ok(format!("Wrote {}.", args.path))
    }

    fn append_file(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
        }

        let args: Args = parse_args(arguments)?;
        let path = self.resolve_for_write(&args.path)?;
        guard_secret(&path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create parent directory for {}", args.path))?;
        }
        let old = if path.exists() {
            fs::read_to_string(&path).unwrap_or_default()
        } else {
            String::new()
        };
        let mut new = old.clone();
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        new.push_str(&args.content);
        write_text_atomic(&path, &new).with_context(|| format!("could not write {}", args.path))?;
        Ok(format!("Appended to {}.", args.path))
    }

    async fn apply_patch(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            patch: String,
        }
        let args: Args = parse_args(arguments)?;
        validate_patch(&self.root, &args.patch)?;
        run_git_apply(&self.root, &args.patch, true).await?;
        run_git_apply(&self.root, &args.patch, false).await?;
        Ok("Applied patch successfully.".to_owned())
    }

    fn delete_file(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            #[serde(default)]
            recursive: bool,
        }
        let args: Args = parse_args(arguments)?;
        let path = self.resolve_mutation_existing(&args.path)?;
        if path == self.root {
            bail!("cannot delete the workspace root");
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            fs::remove_file(&path)?;
        } else if metadata.is_dir() {
            if !args.recursive {
                bail!("directory deletion requires recursive=true");
            }
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
        Ok(format!("Deleted {}.", args.path))
    }

    fn move_file(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            source: String,
            destination: String,
            #[serde(default)]
            overwrite: bool,
        }
        let args: Args = parse_args(arguments)?;
        let source = self.resolve_mutation_existing(&args.source)?;
        let destination = self.resolve_mutation_for_write(&args.destination)?;
        if source == self.root {
            bail!("cannot move the workspace root");
        }
        if destination.exists() {
            if !args.overwrite {
                bail!("destination already exists; set overwrite=true to replace it");
            }
            let metadata = fs::symlink_metadata(&destination)?;
            if metadata.file_type().is_symlink() {
                fs::remove_file(&destination)?;
            } else if metadata.is_dir() {
                fs::remove_dir_all(&destination)?;
            } else {
                fs::remove_file(&destination)?;
            }
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&source, &destination)
            .with_context(|| format!("could not move {} to {}", args.source, args.destination))?;
        Ok(format!("Moved {} to {}.", args.source, args.destination))
    }

    fn create_directory(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
        }
        let args: Args = parse_args(arguments)?;
        let path = self.resolve_for_write(&args.path)?;
        guard_secret(&path)?;
        fs::create_dir_all(&path)?;
        Ok(format!("Created directory {}.", args.path))
    }

    fn preview_edit(&self, arguments: &str) -> Result<String> {
        let args: EditFileArgs = parse_args(arguments)?;
        let (raw_path, operations) = args.into_operations()?;
        let path = self.resolve_existing(&raw_path)?;
        guard_secret(&path)?;
        let content = fs::read_to_string(&path)?;
        let mut updated = content.clone();
        for (index, edit) in operations.iter().enumerate() {
            let count = updated.matches(&edit.old_text).count();
            if count != 1 {
                bail!(
                    "edit {} old_text must match exactly once; found {count} matches",
                    index + 1
                );
            }
            updated = updated.replacen(&edit.old_text, &edit.new_text, 1);
        }
        Ok(unified_diff(&raw_path, &content, &updated))
    }

    fn preview_write(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
        }
        let args: Args = parse_args(arguments)?;
        let path = self.resolve_for_write(&args.path)?;
        guard_secret(&path)?;
        let old = if path.exists() {
            fs::read_to_string(path)?
        } else {
            String::new()
        };
        Ok(unified_diff(&args.path, &old, &args.content))
    }

    fn preview_append(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
        }
        let args: Args = parse_args(arguments)?;
        let path = self.resolve_for_write(&args.path)?;
        guard_secret(&path)?;
        let old = if path.exists() {
            fs::read_to_string(&path).unwrap_or_default()
        } else {
            String::new()
        };
        let mut new = old.clone();
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        new.push_str(&args.content);
        Ok(unified_diff(&args.path, &old, &new))
    }

    fn preview_patch(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            patch: String,
        }
        let args: Args = parse_args(arguments)?;
        validate_patch(&self.root, &args.patch)?;
        Ok(args.patch)
    }

    fn preview_delete(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            #[serde(default)]
            recursive: bool,
        }
        let args: Args = parse_args(arguments)?;
        let path = self.resolve_mutation_existing(&args.path)?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Ok(format!("Delete symlink {}", args.path));
        }
        if metadata.is_file()
            && metadata.len() <= 1_000_000
            && let Ok(content) = fs::read_to_string(&path)
        {
            return Ok(unified_diff(&args.path, &content, ""));
        }
        if path.is_dir() && !args.recursive {
            bail!("directory deletion requires recursive=true");
        }
        Ok(format!(
            "Delete {}{}",
            args.path,
            if path.is_dir() { " recursively" } else { "" }
        ))
    }

    fn preview_git_commit(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            message: String,
            #[serde(default)]
            paths: Vec<String>,
        }
        let args: Args = parse_args(arguments)?;
        let message = args.message.trim();
        if message.is_empty() {
            bail!("commit message cannot be empty");
        }
        for raw in &args.paths {
            let path = self.resolve_for_write(raw)?;
            guard_secret(&path)?;
        }
        let paths = if args.paths.is_empty() {
            "staged changes".to_owned()
        } else {
            args.paths.join(" ")
        };
        Ok(format!("git commit -m {:?} -- {paths}", message))
    }

    fn preview_git_restore(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            paths: Vec<String>,
            #[serde(default)]
            staged_only: bool,
        }
        let args: Args = parse_args(arguments)?;
        if args.paths.is_empty() {
            bail!("at least one path is required");
        }
        for raw in &args.paths {
            let path = self.resolve_for_write(raw)?;
            guard_secret(&path)?;
        }
        let target = if args.staged_only { "index" } else { "HEAD" };
        Ok(format!(
            "git restore → {} ({})",
            target,
            args.paths.join(" ")
        ))
    }

    async fn git_diff(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            staged: bool,
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            base: Option<String>,
            #[serde(default)]
            head: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        if args.head.is_some() && args.base.is_none() {
            bail!("head requires base; pass both revisions to diff a range");
        }
        if let Some(path) = &args.path {
            let resolved = self.resolve_for_write(path)?;
            guard_secret(&resolved)?;
        }
        let base = args.base.as_deref().map(validate_ref).transpose()?;
        let head = args.head.as_deref().map(validate_ref).transpose()?;
        let mut command = Command::new("git");
        command.arg("diff");
        // --cached is only meaningful against the index; a two-revision range
        // compares committed trees directly, so it ignores the staging area.
        if args.staged && head.is_none() {
            command.arg("--cached");
        }
        command.arg("--no-ext-diff");
        if let Some(base) = &base {
            command.arg(base);
        }
        if let Some(head) = &head {
            command.arg(head);
        }
        command.arg("--");
        if let Some(path) = args.path {
            command.arg(path);
        }
        let output = timeout(
            Duration::from_secs(30),
            command.current_dir(&self.root).output(),
        )
        .await
        .map_err(|_| anyhow!("git diff timed out"))??;
        if !output.status.success() {
            bail!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let output = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.is_empty() {
            Ok("No diff.".to_owned())
        } else {
            Ok(output)
        }
    }

    async fn git_status(&self) -> Result<String> {
        let output = timeout(
            Duration::from_secs(30),
            Command::new("git")
                .args(["status", "--short", "--branch", "--untracked-files=all"])
                .current_dir(&self.root)
                .output(),
        )
        .await
        .map_err(|_| anyhow!("git status timed out"))??;
        if !output.status.success() {
            bail!(
                "git status failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let value = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok(if value.trim().is_empty() {
            "Working tree clean.".to_owned()
        } else {
            value
        })
    }

    async fn git_log(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default = "default_log_limit")]
            max_count: usize,
            #[serde(default)]
            path: Option<String>,
        }
        let args: Args = parse_args(arguments)?;
        if let Some(path) = &args.path {
            let resolved = self.resolve_for_write(path)?;
            guard_secret(&resolved)?;
        }
        let mut command = Command::new("git");
        command.args([
            "log",
            "--oneline",
            "--decorate",
            &format!("--max-count={}", args.max_count.clamp(1, 50)),
        ]);
        if let Some(path) = args.path {
            command.args(["--", &path]);
        }
        let output = timeout(
            Duration::from_secs(30),
            command.current_dir(&self.root).output(),
        )
        .await
        .map_err(|_| anyhow!("git log timed out"))??;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            if error.contains("does not have any commits")
                || error.contains("does not have any commits yet")
            {
                return Ok("No commits.".to_owned());
            }
            bail!("git log failed: {error}");
        }
        let value = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok(if value.trim().is_empty() {
            "No commits.".to_owned()
        } else {
            value
        })
    }

    async fn git_commit(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            message: String,
            #[serde(default)]
            paths: Vec<String>,
        }

        let args: Args = parse_args(arguments)?;
        let message = args.message.trim();
        if message.is_empty() {
            bail!("commit message cannot be empty");
        }
        if message.chars().count() > 2_000 {
            bail!("commit message exceeds 2,000 characters");
        }
        if args.paths.len() > 100 {
            bail!("git_commit accepts at most 100 paths");
        }
        for raw in &args.paths {
            let path = self.resolve_for_write(raw)?;
            guard_secret(&path)?;
        }

        if !args.paths.is_empty() {
            let mut add = Command::new("git");
            add.arg("add").arg("--");
            for raw in &args.paths {
                add.arg(raw);
            }
            let output = timeout(
                Duration::from_secs(30),
                add.current_dir(&self.root).output(),
            )
            .await
            .map_err(|_| anyhow!("git add timed out"))??;
            if !output.status.success() {
                bail!(
                    "git add failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        let mut commit = Command::new("git");
        commit.args(["commit", "-m", message, "--"]);
        let output = timeout(
            Duration::from_secs(30),
            commit.current_dir(&self.root).output(),
        )
        .await
        .map_err(|_| anyhow!("git commit timed out"))??;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("nothing to commit") {
                bail!("nothing to commit");
            }
            bail!("git commit failed: {stderr}");
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(format!("Committed.\n{stdout}"))
    }

    async fn git_restore(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            paths: Vec<String>,
            #[serde(default)]
            staged_only: bool,
        }

        let args: Args = parse_args(arguments)?;
        if args.paths.is_empty() {
            bail!("at least one path is required");
        }
        if args.paths.len() > 100 {
            bail!("git_restore accepts at most 100 paths");
        }
        for raw in &args.paths {
            let path = self.resolve_for_write(raw)?;
            guard_secret(&path)?;
        }

        let mut command = Command::new("git");
        // core.autocrlf=false so restored content is byte-identical to HEAD
        // rather than being re-encoded with the host's line-ending setting.
        command.args(["-c", "core.autocrlf=false", "restore"]);
        if args.staged_only {
            command.arg("--staged");
        } else {
            command.args(["--source=HEAD", "--staged", "--worktree"]);
        }
        command.arg("--");
        for raw in &args.paths {
            command.arg(raw);
        }
        let output = timeout(
            Duration::from_secs(30),
            command.current_dir(&self.root).output(),
        )
        .await
        .map_err(|_| anyhow!("git restore timed out"))??;
        if !output.status.success() {
            bail!(
                "git restore failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(format!(
            "Restored {} path(s) to {}.",
            args.paths.len(),
            if args.staged_only { "index" } else { "HEAD" }
        ))
    }

    async fn git_show(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            #[serde(default = "default_head")]
            revision: String,
        }
        let args: Args = parse_args(arguments)?;
        let path = self.resolve_for_write(&args.path)?;
        guard_secret(&path)?;
        let revision = validate_ref(&args.revision)?;
        let object = format!("{revision}:{}", args.path);
        let output = timeout(
            Duration::from_secs(30),
            Command::new("git")
                .args(["show", &object])
                .current_dir(&self.root)
                .output(),
        )
        .await
        .map_err(|_| anyhow!("git show timed out"))??;
        if !output.status.success() {
            bail!(
                "git show failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let content = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = content.lines().collect();
        let numbered = lines
            .iter()
            .enumerate()
            .take(2_000)
            .map(|(index, line)| format!("{:>5} | {line}", index + 1))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(format!(
            ">>> {revision}:{} ({} lines)\n{numbered}",
            args.path,
            lines.len()
        ))
    }

    async fn git_blame(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            #[serde(default = "default_head")]
            revision: String,
        }
        let args: Args = parse_args(arguments)?;
        let path = self.resolve_for_write(&args.path)?;
        guard_secret(&path)?;
        let revision = validate_ref(&args.revision)?;
        let output = timeout(
            Duration::from_secs(30),
            Command::new("git")
                .args(["blame", "--date=short", &revision, "--", &args.path])
                .current_dir(&self.root)
                .output(),
        )
        .await
        .map_err(|_| anyhow!("git blame timed out"))??;
        if !output.status.success() {
            bail!(
                "git blame failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let value = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok(if value.trim().is_empty() {
            "No blame output.".to_owned()
        } else {
            value
        })
    }

    async fn git_checkout(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            branch: String,
            #[serde(default)]
            create: bool,
        }
        let args: Args = parse_args(arguments)?;
        let branch = validate_branch(&args.branch)?;
        let mut command = Command::new("git");
        command.arg("checkout");
        if args.create {
            command.arg("-b");
        }
        command.arg(&branch);
        let output = timeout(
            Duration::from_secs(30),
            command.current_dir(&self.root).output(),
        )
        .await
        .map_err(|_| anyhow!("git checkout timed out"))??;
        if !output.status.success() {
            bail!(
                "git checkout failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(format!(
            "{} branch {branch}.",
            if args.create {
                "Created and switched to"
            } else {
                "Switched to"
            }
        ))
    }

    async fn run_command(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            command: String,
            #[serde(default = "default_timeout")]
            timeout_seconds: u64,
        }

        let args: Args = parse_args(arguments)?;
        if args.command.trim().is_empty() {
            bail!("command cannot be empty");
        }

        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd");
            command.args(["/C", &args.command]);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-lc", &args.command]);
            command
        };

        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let duration = Duration::from_secs(args.timeout_seconds.clamp(1, 300));
        let mut child = command.spawn().context("could not start command")?;
        let stdout = child.stdout.take().context("could not capture stdout")?;
        let stderr = child.stderr.take().context("could not capture stderr")?;
        let per_stream_limit = self.output_limit.max(2_000) / 2;
        let stdout_task = tokio::spawn(read_capped(stdout, per_stream_limit));
        let stderr_task = tokio::spawn(read_capped(stderr, per_stream_limit));
        let status = match timeout(duration, child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let _ = child.kill().await;
                stdout_task.abort();
                stderr_task.abort();
                bail!("command timed out after {} seconds", duration.as_secs());
            }
        };
        let stdout = finish_reader(stdout_task).await?;
        let stderr = finish_reader(stderr_task).await?;
        let code = status
            .code()
            .map_or("signal".to_owned(), |code| code.to_string());
        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);
        Ok(format!(
            "exit: {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        ))
    }

    fn resolve_existing(&self, raw: &str) -> Result<PathBuf> {
        let joined = workspace_join(&self.root, raw)?;
        let canonical = joined
            .canonicalize()
            .with_context(|| format!("path does not exist: {raw}"))?;
        ensure_inside(&self.root, &canonical)?;
        Ok(canonical)
    }

    fn resolve_for_write(&self, raw: &str) -> Result<PathBuf> {
        let joined = workspace_join(&self.root, raw)?;
        if joined.exists() {
            return self.resolve_existing(raw);
        }

        let mut ancestor = joined.parent();
        while let Some(path) = ancestor {
            if path.exists() {
                let canonical = path.canonicalize()?;
                ensure_inside(&self.root, &canonical)?;
                return Ok(joined);
            }
            ancestor = path.parent();
        }
        bail!("could not resolve parent directory for {raw}")
    }

    fn resolve_mutation_existing(&self, raw: &str) -> Result<PathBuf> {
        let joined = workspace_join(&self.root, raw)?;
        let canonical = self.resolve_existing(raw)?;
        guard_secret(&joined)?;
        guard_secret(&canonical)?;
        Ok(joined)
    }

    fn resolve_mutation_for_write(&self, raw: &str) -> Result<PathBuf> {
        let joined = workspace_join(&self.root, raw)?;
        if fs::symlink_metadata(&joined).is_ok() {
            self.resolve_mutation_existing(raw)
        } else {
            let path = self.resolve_for_write(raw)?;
            guard_secret(&path)?;
            Ok(path)
        }
    }
}

pub fn tool_specs() -> Vec<Value> {
    vec![
        function(
            "list_files",
            "List workspace files while respecting gitignore.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative directory; defaults to ."},
                    "depth": {"type": "integer", "description": "Maximum traversal depth, 1-8"}
                }
            }),
        ),
        function(
            "grep",
            "Fast gitignore-aware regex search across workspace text files. Use this before opening files.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Rust-compatible regular expression"},
                    "path": {"type": "string", "description": "Workspace-relative path; defaults to ."},
                    "glob": {"type": "array", "items": {"type": "string"}, "description": "Optional include globs such as **/*.rs"},
                    "case_sensitive": {"type": "boolean", "description": "Defaults to true"},
                    "max_results": {"type": "integer", "description": "Defaults to 200, maximum 2000"},
                    "context": {"type": "integer", "description": "Lines of context around each match, 0-10; defaults to 0"}
                },
                "required": ["query"]
            }),
        ),
        function(
            "glob",
            "Find files and directories by gitignore-aware glob pattern.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob such as src/**/*.rs"},
                    "path": {"type": "string", "description": "Workspace-relative search root; defaults to ."},
                    "max_results": {"type": "integer", "description": "Defaults to 500"}
                },
                "required": ["pattern"]
            }),
        ),
        function(
            "read_file",
            "Read a UTF-8 text file with line numbers.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "offset": {"type": "integer", "description": "First line, one-based"},
                    "limit": {"type": "integer", "description": "Maximum lines, defaults to 400"}
                },
                "required": ["path"]
            }),
        ),
        function(
            "read_files",
            "Read up to 20 UTF-8 text files in one call with line numbers. Use this instead of repeated read_file calls when inspecting several files.",
            json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Workspace-relative file paths",
                        "maxItems": 20
                    },
                    "limit": {"type": "integer", "description": "Maximum lines per file, defaults to 400"}
                },
                "required": ["paths"]
            }),
        ),
        function(
            "edit_file",
            "Replace one exact text occurrence in an existing file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "old_text": {"type": "string", "description": "Exact text that occurs once"},
                    "new_text": {"type": "string", "description": "Replacement text"},
                    "edits": {
                        "type": "array",
                        "description": "Optional ordered batch of exact replacements",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {"type": "string"},
                                "new_text": {"type": "string"}
                            },
                            "required": ["old_text", "new_text"]
                        }
                    }
                },
                "required": ["path"]
            }),
        ),
        function(
            "write_file",
            "Create or completely replace a UTF-8 text file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "content": {"type": "string", "description": "Complete file content"}
                },
                "required": ["path", "content"]
            }),
        ),
        function(
            "apply_patch",
            "Apply a unified diff to one or more workspace files. Prefer this for precise multi-file changes.",
            json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "Complete unified diff, including ---/+++ file headers and @@ hunks"}
                },
                "required": ["patch"]
            }),
        ),
        function(
            "delete_file",
            "Delete a workspace file or, with recursive=true, a directory.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative path"},
                    "recursive": {"type": "boolean", "description": "Required when deleting a directory"}
                },
                "required": ["path"]
            }),
        ),
        function(
            "move_file",
            "Move or rename a file or directory inside the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "source": {"type": "string", "description": "Existing workspace-relative path"},
                    "destination": {"type": "string", "description": "New workspace-relative path"},
                    "overwrite": {"type": "boolean", "description": "Replace an existing destination; defaults to false"}
                },
                "required": ["source", "destination"]
            }),
        ),
        function(
            "create_directory",
            "Create a directory and any missing parents inside the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative directory path"}
                },
                "required": ["path"]
            }),
        ),
        function(
            "git_diff",
            "Show a repository diff without modifying anything. Defaults to the working tree against HEAD; pass base/head to diff a commit or revision range.",
            json!({
                "type": "object",
                "properties": {
                    "staged": {"type": "boolean", "description": "Show staged changes (against the index, or against base when set)"},
                    "path": {"type": "string", "description": "Optional workspace-relative path filter"},
                    "base": {"type": "string", "description": "Optional revision to diff from, such as HEAD~1, a commit sha, or a branch"},
                    "head": {"type": "string", "description": "Optional revision to diff to; requires base. base+head shows what changed between two revisions"}
                }
            }),
        ),
        function(
            "git_status",
            "Show branch state plus staged, unstaged, and untracked files.",
            json!({"type": "object", "properties": {}}),
        ),
        function(
            "git_log",
            "Show recent commits, optionally restricted to a workspace path.",
            json!({
                "type": "object",
                "properties": {
                    "max_count": {"type": "integer", "description": "Number of commits, defaults to 10 and caps at 50"},
                    "path": {"type": "string", "description": "Optional workspace-relative path filter"}
                }
            }),
        ),
        function(
            "git_commit",
            "Stage optional workspace paths and create a git commit. Never pushes. Approval-gated.",
            json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string", "description": "Commit message, 1-2000 characters"},
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional workspace-relative paths to stage before committing; omit to commit already-staged changes",
                        "maxItems": 100
                    }
                },
                "required": ["message"]
            }),
        ),
        function(
            "git_restore",
            "Restore workspace paths from HEAD, discarding staged and working-tree changes. Approval-gated and destructive.",
            json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Workspace-relative paths to restore",
                        "maxItems": 100
                    },
                    "staged_only": {"type": "boolean", "description": "Only unstage (restore to index), keep working-tree changes; defaults to false"}
                },
                "required": ["paths"]
            }),
        ),
        function(
            "git_show",
            "Show a file's contents at a past revision. Read-only; use it to compare before and after changes.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "revision": {"type": "string", "description": "Git revision such as HEAD, HEAD~1, a commit sha, or a branch; defaults to HEAD"}
                },
                "required": ["path"]
            }),
        ),
        function(
            "git_blame",
            "Show per-line authorship for a file at a revision. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "revision": {"type": "string", "description": "Git revision; defaults to HEAD"}
                },
                "required": ["path"]
            }),
        ),
        function(
            "git_checkout",
            "Create and switch to a new branch, or switch to an existing one. Approval-gated.",
            json!({
                "type": "object",
                "properties": {
                    "branch": {"type": "string", "description": "Branch name to create or switch to"},
                    "create": {"type": "boolean", "description": "Create the branch with -b before switching; defaults to false"}
                },
                "required": ["branch"]
            }),
        ),
        function(
            "append_file",
            "Append text to an existing file, or create it if it is missing. Atomic and approval-gated.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative file path"},
                    "content": {"type": "string", "description": "Text to append"}
                },
                "required": ["path", "content"]
            }),
        ),
        function(
            "run_command",
            "Run a shell command in the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command"},
                    "timeout_seconds": {"type": "integer", "description": "Timeout, defaults to 120 and caps at 300"}
                },
                "required": ["command"]
            }),
        ),
        function(
            "tool_search",
            "Discover Abacus tools by capability keyword and learn when to use them.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Capability such as search, files, diff, or shell"}
                }
            }),
        ),
        // ask_user is exposed alongside the regular tool specs but has special
        // dispatch in the TUI: it pops a modal asking the user a multi-choice
        // or free-text question, returning the answer as the tool output.
        function(
            "ask_user",
            "Ask the user a question when a decision, clarification, or sign-off is needed before continuing. Provide 2-4 mutually exclusive options; the user can pick one or type a custom answer. Use sparingly — only when the answer genuinely blocks progress or a major preference is at stake.",
            json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "The question to ask the user; shown at the top of the prompt."},
                    "header": {"type": "string", "description": "Short topic label shown on the modal border (e.g. \"Test framework\")."},
                    "options": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {"type": "string", "description": "1-5 word choice label shown to the user."},
                                "description": {"type": "string", "description": "One sentence explaining the option."},
                                "preview": {"type": "string", "description": "Optional longer preview of what the option implies."}
                            },
                            "required": ["label"]
                        },
                        "minItems": 2,
                        "maxItems": 4
                    },
                    "multi_select": {"type": "boolean", "description": "Allow picking multiple options (default false)."}
                },
                "required": ["question", "options"]
            }),
        ),
    ]
}

fn function(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters
        }
    })
}

fn workspace_join(root: &Path, raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if path.is_absolute() {
        bail!("absolute paths are not allowed: {raw}");
    }
    if path.components().any(|part| {
        matches!(
            part,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("parent path traversal is not allowed: {raw}");
    }
    Ok(root.join(path))
}

fn ensure_inside(root: &Path, path: &Path) -> Result<()> {
    if !path.starts_with(root) {
        bail!("path escapes the workspace: {}", path.display());
    }
    Ok(())
}

/// Prune version-control metadata directories from a workspace walk. We keep
/// other hidden files (so `.github`, dotfiles, etc. remain searchable), but a
/// `.git` directory holds thousands of loose objects and is never worth reading
/// — descending into it made `grep`/`glob`/`list_files` take tens of seconds.
fn skip_vcs_dir(entry: &ignore::DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | ".hg" | ".svn" | ".jj")
    )
}

fn guard_secret(path: &Path) -> Result<()> {
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        if (name == ".env" || name.starts_with(".env.")) && name != ".env.example" {
            bail!("access to secret environment files is blocked");
        }
    }
    Ok(())
}

fn write_text_atomic(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("file has no parent directory")?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(".abacus-write-{}-{nonce}.tmp", std::process::id()));
    let mut file = File::create(&temp)?;
    if let Ok(metadata) = path.metadata() {
        file.set_permissions(metadata.permissions())?;
    }
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);

    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

async fn read_capped<R: AsyncRead + Unpin>(mut reader: R, limit: usize) -> Result<Vec<u8>> {
    let mut kept = Vec::with_capacity(limit.min(16_384));
    let mut buffer = [0_u8; 8_192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        if remaining > 0 {
            kept.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        if read > remaining {
            truncated = true;
        }
    }
    if truncated {
        kept.extend_from_slice(b"\n... output truncated while command continued\n");
    }
    Ok(kept)
}

async fn finish_reader(mut task: tokio::task::JoinHandle<Result<Vec<u8>>>) -> Result<Vec<u8>> {
    match timeout(Duration::from_secs(2), &mut task).await {
        Ok(result) => result?,
        Err(_) => {
            task.abort();
            Ok(b"... output pipe remained open after command exit\n".to_vec())
        }
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: &str) -> Result<T> {
    serde_json::from_str(arguments).context("invalid tool arguments")
}

fn truncate(mut value: String, max: usize) -> String {
    if value.len() <= max {
        return value;
    }
    let mut boundary = max;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str("\n… output truncated");
    value
}

fn unified_diff(path: &str, old: &str, new: &str) -> String {
    if old == new {
        return "No changes.".to_owned();
    }
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

fn validate_patch(root: &Path, patch: &str) -> Result<()> {
    if patch.trim().is_empty() {
        bail!("patch cannot be empty");
    }
    if patch.len() > 2_000_000 {
        bail!("patch exceeds the 2 MB limit");
    }

    let mut paths = 0;
    for line in patch.lines() {
        let raw = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "));
        let Some(raw) = raw else { continue };
        let raw = raw.split('\t').next().unwrap_or(raw).trim();
        if raw == "/dev/null" {
            continue;
        }
        if raw.starts_with('"') {
            bail!("quoted patch paths are not supported; use workspace-relative UTF-8 paths");
        }
        let relative = raw
            .strip_prefix("a/")
            .or_else(|| raw.strip_prefix("b/"))
            .unwrap_or(raw);
        let joined = workspace_join(root, relative)?;
        guard_secret(&joined)?;
        if joined.exists() {
            ensure_inside(root, &joined.canonicalize()?)?;
        } else {
            let mut ancestor = joined.parent();
            let mut checked = false;
            while let Some(path) = ancestor {
                if path.exists() {
                    ensure_inside(root, &path.canonicalize()?)?;
                    checked = true;
                    break;
                }
                ancestor = path.parent();
            }
            if !checked {
                bail!("could not resolve patch path {relative}");
            }
        }
        paths += 1;
    }
    if paths == 0 {
        bail!("patch has no workspace file headers");
    }
    Ok(())
}

async fn run_git_apply(root: &Path, patch: &str, check: bool) -> Result<()> {
    let mut command = Command::new("git");
    // Disable autocrlf so a `\n` patch applies byte-for-byte regardless of the
    // host git config (Windows defaults to core.autocrlf=true, which otherwise
    // rewrites line endings and breaks the apply).
    command.args(["-c", "core.autocrlf=false", "apply", "--whitespace=nowarn"]);
    if check {
        command.arg("--check");
    }
    command
        .arg("-")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("could not start git apply")?;
    child
        .stdin
        .take()
        .context("could not open git apply stdin")?
        .write_all(patch.as_bytes())
        .await?;
    let output = timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .map_err(|_| anyhow!("git apply timed out"))??;
    if !output.status.success() {
        bail!(
            "git apply failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn build_globs(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).with_context(|| format!("invalid glob: {pattern}"))?);
    }
    Ok(Some(builder.build()?))
}

fn tool_catalog() -> Vec<(&'static str, &'static str)> {
    vec![
        ("list_files", "shallow directory overview"),
        ("glob", "find paths by wildcard pattern"),
        ("grep", "regex content search with optional file globs"),
        ("read_file", "read a bounded line range with line numbers"),
        (
            "read_files",
            "read up to 20 files in one call with line numbers",
        ),
        (
            "edit_file",
            "replace one exact occurrence in an existing file",
        ),
        ("write_file", "create or fully replace a text file"),
        (
            "apply_patch",
            "apply a precise unified diff across workspace files",
        ),
        ("delete_file", "delete a file or recursive directory"),
        ("move_file", "move or rename a workspace path"),
        ("create_directory", "create a directory and missing parents"),
        (
            "git_diff",
            "inspect working-tree, staged, or commit-range repository changes",
        ),
        ("git_status", "inspect branch, changes, and untracked files"),
        ("git_log", "inspect recent repository history"),
        (
            "git_commit",
            "stage optional paths and create a local commit",
        ),
        (
            "git_restore",
            "restore workspace paths to HEAD, discarding changes",
        ),
        ("git_show", "show a file's contents at a past revision"),
        ("git_blame", "show per-line authorship for a file"),
        ("git_checkout", "create or switch to a git branch"),
        (
            "append_file",
            "append text to a file, creating it if missing",
        ),
        ("run_command", "run project commands with a timeout"),
        ("web_search", "search the web for current information"),
        ("read_page", "fetch and read the text of a web page"),
        ("tool_search", "discover tools by capability"),
    ]
}

fn default_offset() -> usize {
    1
}
fn default_limit() -> usize {
    400
}
fn default_depth() -> usize {
    3
}
fn default_timeout() -> u64 {
    120
}
fn default_true() -> bool {
    true
}
fn default_search_limit() -> usize {
    200
}
fn default_glob_limit() -> usize {
    500
}
fn default_log_limit() -> usize {
    10
}
fn dot() -> String {
    ".".to_owned()
}
fn default_head() -> String {
    "HEAD".to_owned()
}

fn validate_ref(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("revision cannot be empty");
    }
    if trimmed.len() > 200
        || trimmed.starts_with('-')
        || trimmed.contains("..")
        || trimmed.contains(':')
        || trimmed.chars().any(|c| c.is_whitespace())
    {
        bail!("invalid revision {trimmed:?}");
    }
    Ok(trimmed.to_owned())
}

fn validate_branch(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 200 {
        bail!("invalid branch name");
    }
    if trimmed.starts_with('-')
        || trimmed.contains("..")
        || trimmed.chars().any(|c| {
            c.is_whitespace() || matches!(c, ':' | '~' | '^' | '?' | '*' | '[' | ']' | '\\')
        })
    {
        bail!("invalid branch name {trimmed:?}");
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::tempdir;

    #[tokio::test]
    async fn grep_skips_the_git_directory() {
        // Regression: walking `.git` (thousands of loose objects) made grep take
        // tens of seconds. A `.git` subtree must be pruned; real files still hit.
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".git").join("config"), "needle inside git\n").unwrap();
        fs::write(root.join("real.txt"), "needle in real file\n").unwrap();
        let tools = ToolExecutor::new(root);
        let call = ToolCall {
            id: "1".into(),
            name: "grep".into(),
            arguments: r#"{"query":"needle"}"#.into(),
        };
        let output = tools.execute(&call).await;
        assert!(output.contains("real.txt"), "should match real files");
        assert!(
            !output.contains(".git"),
            "must not descend into .git: {output}"
        );
    }

    #[tokio::test]
    async fn blocks_paths_outside_workspace() {
        let dir = tempdir().unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"../secret"}"#.into(),
        };
        assert!(tools.execute(&call).await.contains("parent path traversal"));
    }

    #[tokio::test]
    async fn blocks_dotenv_but_allows_example() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".env"), "TOKEN=nope").unwrap();
        fs::write(dir.path().join(".env.example"), "TOKEN=").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let blocked = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":".env"}"#.into(),
        };
        let allowed = ToolCall {
            id: "2".into(),
            name: "read_file".into(),
            arguments: r#"{"path":".env.example"}"#.into(),
        };
        assert!(tools.execute(&blocked).await.contains("blocked"));
        assert!(tools.execute(&allowed).await.contains("TOKEN="));
    }

    #[tokio::test]
    async fn exact_edit_rejects_ambiguous_replacement() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "same\nsame\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let call = ToolCall {
            id: "1".into(),
            name: "edit_file".into(),
            arguments: r#"{"path":"file.txt","old_text":"same","new_text":"new"}"#.into(),
        };
        assert!(tools.execute(&call).await.contains("found 2 matches"));
    }

    #[tokio::test]
    async fn grep_filters_by_glob() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("main.rs"), "needle\n").unwrap();
        fs::write(dir.path().join("notes.txt"), "needle\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let call = ToolCall {
            id: "1".into(),
            name: "grep".into(),
            arguments: r#"{"query":"needle","glob":["**/*.rs","*.rs"]}"#.into(),
        };
        let output = tools.execute(&call).await;
        assert!(output.contains("main.rs:1"));
        assert!(!output.contains("notes.txt"));
    }

    #[tokio::test]
    async fn grep_emits_context_lines_around_matches() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("main.rs"),
            "one\ntwo\nneedle\nfour\nfive\nsix\nseven\neight\nneedle\nten\n",
        )
        .unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let call = ToolCall {
            id: "1".into(),
            name: "grep".into(),
            arguments: r#"{"query":"needle","context":1}"#.into(),
        };
        let output = tools.execute(&call).await;
        // Match lines use the ":" marker, context lines use "-".
        assert!(output.contains("main.rs:3: needle"));
        assert!(output.contains("main.rs:2- two"));
        assert!(output.contains("main.rs:4- four"));
        assert!(output.contains("main.rs:9: needle"));
        assert!(output.contains("main.rs:10- ten"));
        // Disjoint groups are separated by "--"; adjacent windows would have merged.
        assert!(output.contains("--"));
    }

    #[test]
    fn edit_approval_contains_unified_diff() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "old\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let call = ToolCall {
            id: "1".into(),
            name: "edit_file".into(),
            arguments: r#"{"path":"file.txt","old_text":"old","new_text":"new"}"#.into(),
        };
        let preview = tools.approval_details(&call);
        assert!(preview.contains("-old"));
        assert!(preview.contains("+new"));
    }

    #[tokio::test]
    async fn tool_search_discovers_grep_by_capability() {
        let dir = tempdir().unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let call = ToolCall {
            id: "1".into(),
            name: "tool_search".into(),
            arguments: r#"{"query":"regex"}"#.into(),
        };
        let output = tools.execute(&call).await;
        assert!(output.contains("grep:"));
    }

    #[tokio::test]
    async fn command_reader_discards_output_after_limit() {
        let input = vec![b'x'; 10_000];
        let output = read_capped(input.as_slice(), 128).await.unwrap();
        assert!(output.len() < 256);
        assert!(String::from_utf8_lossy(&output).contains("truncated"));
    }

    #[tokio::test]
    async fn applies_reviewable_multiline_patch() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("value.txt"), "one\ntwo\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let patch = unified_diff("value.txt", "one\ntwo\n", "one\nchanged\nthree\n");
        let call = ToolCall {
            id: "patch".into(),
            name: "apply_patch".into(),
            arguments: serde_json::to_string(&json!({"patch": patch})).unwrap(),
        };
        let preview = tools.approval_details(&call);
        assert!(preview.contains("+changed"));
        assert_eq!(tools.execute(&call).await, "Applied patch successfully.");
        assert_eq!(
            fs::read_to_string(dir.path().join("value.txt")).unwrap(),
            "one\nchanged\nthree\n"
        );
    }

    #[tokio::test]
    async fn patch_rejects_traversal_and_secret_paths() {
        let dir = tempdir().unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        for patch in [
            "--- a/../outside\n+++ b/../outside\n@@ -0,0 +1 @@\n+x\n",
            "--- /dev/null\n+++ b/.env\n@@ -0,0 +1 @@\n+TOKEN=x\n",
        ] {
            let call = ToolCall {
                id: "unsafe".into(),
                name: "apply_patch".into(),
                arguments: serde_json::to_string(&json!({"patch": patch})).unwrap(),
            };
            assert!(tools.execute(&call).await.starts_with("Error:"));
        }
        assert!(!dir.path().join("../outside").exists());
        assert!(!dir.path().join(".env").exists());
    }

    #[tokio::test]
    async fn git_diff_and_log_block_dotenv_path_filter() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        fs::write(dir.path().join(".env"), "TOKEN=secret\n").unwrap();
        StdCommand::new("git")
            .args(["add", ".env"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-m", "baseline"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        fs::write(dir.path().join(".env"), "TOKEN=leaked\n").unwrap();

        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let diff = ToolCall {
            id: "d".into(),
            name: "git_diff".into(),
            arguments: r#"{"path":".env"}"#.into(),
        };
        let output = tools.execute(&diff).await;
        assert!(output.contains("Error:"), "git_diff must guard .env: {output}");
        assert!(!output.contains("leaked"));

        let log = ToolCall {
            id: "l".into(),
            name: "git_log".into(),
            arguments: r#"{"path":".env"}"#.into(),
        };
        let output = tools.execute(&log).await;
        assert!(output.contains("Error:"), "git_log must guard .env: {output}");

        fs::write(dir.path().join("file.txt"), "hi\n").unwrap();
        let diff = ToolCall {
            id: "d2".into(),
            name: "git_diff".into(),
            arguments: r#"{"path":"file.txt"}"#.into(),
        };
        assert!(!tools.execute(&diff).await.starts_with("Error:"));
    }

    #[tokio::test]
    async fn delete_and_move_are_confined_and_previewed() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("old.txt"), "remove me\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let delete = ToolCall {
            id: "delete".into(),
            name: "delete_file".into(),
            arguments: r#"{"path":"old.txt"}"#.into(),
        };
        assert!(tools.approval_details(&delete).contains("-remove me"));

        let outside = ToolCall {
            id: "move".into(),
            name: "move_file".into(),
            arguments: r#"{"source":"old.txt","destination":"../escaped.txt"}"#.into(),
        };
        assert!(
            tools
                .execute(&outside)
                .await
                .contains("parent path traversal")
        );
        assert!(dir.path().join("old.txt").exists());
    }

    #[tokio::test]
    async fn git_status_and_log_include_repository_state() {
        let dir = tempdir().unwrap();
        assert!(
            StdCommand::new("git")
                .args(["init", "--quiet"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        fs::write(dir.path().join("untracked.txt"), "hello\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let status = ToolCall {
            id: "status".into(),
            name: "git_status".into(),
            arguments: "{}".into(),
        };
        let log = ToolCall {
            id: "log".into(),
            name: "git_log".into(),
            arguments: "{}".into(),
        };
        assert!(tools.execute(&status).await.contains("untracked.txt"));
        assert!(tools.execute(&log).await.contains("No commits."));
    }

    #[tokio::test]
    async fn read_files_batches_multiple_files_with_headers() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let call = ToolCall {
            id: "1".into(),
            name: "read_files".into(),
            arguments: r#"{"paths":["a.txt","b.txt","missing.txt"]}"#.into(),
        };
        let output = tools.execute(&call).await;
        assert!(output.contains("===== a.txt ====="));
        assert!(output.contains("alpha"));
        assert!(output.contains("===== b.txt ====="));
        assert!(output.contains("beta"));
        assert!(output.contains("===== missing.txt ====="));
        assert!(output.contains("Error:"));
    }

    #[tokio::test]
    async fn git_commit_stages_paths_and_creates_a_commit() {
        let dir = tempdir().unwrap();
        assert!(
            StdCommand::new("git")
                .args(["init", "--quiet"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        StdCommand::new("git")
            .args(["config", "user.email", "agent@abacus.test"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "Abacus"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        fs::write(dir.path().join("file.txt"), "first\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let call = ToolCall {
            id: "commit".into(),
            name: "git_commit".into(),
            arguments: r#"{"message":"initial","paths":["file.txt"]}"#.into(),
        };
        assert!(tools.execute(&call).await.starts_with("Committed."));
        let log = ToolCall {
            id: "log".into(),
            name: "git_log".into(),
            arguments: "{}".into(),
        };
        assert!(tools.execute(&log).await.contains("initial"));
    }

    #[tokio::test]
    async fn git_restore_reverts_working_tree_to_head() {
        let dir = tempdir().unwrap();
        assert!(
            StdCommand::new("git")
                .args(["init", "--quiet"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        StdCommand::new("git")
            .args(["config", "user.email", "agent@abacus.test"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "Abacus"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        fs::write(dir.path().join("file.txt"), "original\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let commit = ToolCall {
            id: "commit".into(),
            name: "git_commit".into(),
            arguments: r#"{"message":"baseline","paths":["file.txt"]}"#.into(),
        };
        tools.execute(&commit).await;
        fs::write(dir.path().join("file.txt"), "mutated\n").unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt")).unwrap(),
            "mutated\n"
        );
        let restore = ToolCall {
            id: "restore".into(),
            name: "git_restore".into(),
            arguments: r#"{"paths":["file.txt"]}"#.into(),
        };
        assert!(tools.execute(&restore).await.contains("HEAD"));
        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt")).unwrap(),
            "original\n"
        );
    }

    fn init_repo(dir: &std::path::Path) {
        StdCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir)
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.email", "agent@abacus.test"])
            .current_dir(dir)
            .status()
            .unwrap();
        StdCommand::new("git")
            .args(["config", "user.name", "Abacus"])
            .current_dir(dir)
            .status()
            .unwrap();
    }

    #[tokio::test]
    async fn git_show_returns_file_contents_at_head() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        fs::write(dir.path().join("file.txt"), "first\nsecond\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        tools
            .execute(&ToolCall {
                id: "c".into(),
                name: "git_commit".into(),
                arguments: r#"{"message":"base","paths":["file.txt"]}"#.into(),
            })
            .await;
        fs::write(dir.path().join("file.txt"), "first\nCHANGED\n").unwrap();
        let output = tools
            .execute(&ToolCall {
                id: "s".into(),
                name: "git_show".into(),
                arguments: r#"{"path":"file.txt"}"#.into(),
            })
            .await;
        assert!(output.contains("second"));
        assert!(!output.contains("CHANGED"));
    }

    #[tokio::test]
    async fn git_diff_shows_a_commit_range() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        fs::write(dir.path().join("file.txt"), "first\n").unwrap();
        tools
            .execute(&ToolCall {
                id: "c1".into(),
                name: "git_commit".into(),
                arguments: r#"{"message":"one","paths":["file.txt"]}"#.into(),
            })
            .await;
        fs::write(dir.path().join("file.txt"), "first\nsecond\n").unwrap();
        tools
            .execute(&ToolCall {
                id: "c2".into(),
                name: "git_commit".into(),
                arguments: r#"{"message":"two","paths":["file.txt"]}"#.into(),
            })
            .await;
        // A clean working tree: the default diff is empty, but the range diff
        // between the two commits must surface the added line.
        let working = tools
            .execute(&ToolCall {
                id: "d0".into(),
                name: "git_diff".into(),
                arguments: "{}".into(),
            })
            .await;
        assert_eq!(working, "No diff.");
        let range = tools
            .execute(&ToolCall {
                id: "d1".into(),
                name: "git_diff".into(),
                arguments: r#"{"base":"HEAD~1","head":"HEAD"}"#.into(),
            })
            .await;
        assert!(range.contains("+second"), "range diff was: {range}");
        assert!(!range.contains("Error:"));
        // head without base is rejected.
        let invalid = tools
            .execute(&ToolCall {
                id: "d2".into(),
                name: "git_diff".into(),
                arguments: r#"{"head":"HEAD"}"#.into(),
            })
            .await;
        assert!(invalid.starts_with("Error:"));
    }

    #[tokio::test]
    async fn git_blame_runs_against_committed_file() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        fs::write(dir.path().join("file.txt"), "first\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        tools
            .execute(&ToolCall {
                id: "c".into(),
                name: "git_commit".into(),
                arguments: r#"{"message":"base","paths":["file.txt"]}"#.into(),
            })
            .await;
        let output = tools
            .execute(&ToolCall {
                id: "b".into(),
                name: "git_blame".into(),
                arguments: r#"{"path":"file.txt"}"#.into(),
            })
            .await;
        assert!(!output.contains("Error:"));
        assert!(output.contains("first"));
    }

    #[tokio::test]
    async fn git_checkout_creates_and_switches_branch() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        fs::write(dir.path().join("file.txt"), "first\n").unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        tools
            .execute(&ToolCall {
                id: "c".into(),
                name: "git_commit".into(),
                arguments: r#"{"message":"base","paths":["file.txt"]}"#.into(),
            })
            .await;
        let output = tools
            .execute(&ToolCall {
                id: "co".into(),
                name: "git_checkout".into(),
                arguments: r#"{"branch":"feature/x","create":true}"#.into(),
            })
            .await;
        assert!(output.contains("Created and switched to"));
        let branch = tools
            .execute(&ToolCall {
                id: "b".into(),
                name: "run_command".into(),
                arguments: r#"{"command":"git branch --show-current"}"#.into(),
            })
            .await;
        assert!(branch.contains("feature/x"));
    }

    #[tokio::test]
    async fn append_file_creates_then_appends() {
        let dir = tempdir().unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        tools
            .execute(&ToolCall {
                id: "1".into(),
                name: "append_file".into(),
                arguments: r#"{"path":"log.txt","content":"line one"}"#.into(),
            })
            .await;
        let second = ToolCall {
            id: "2".into(),
            name: "append_file".into(),
            arguments: r#"{"path":"log.txt","content":"line two"}"#.into(),
        };
        tools.execute(&second).await;
        assert_eq!(
            fs::read_to_string(dir.path().join("log.txt")).unwrap(),
            "line one\nline two"
        );
        assert!(tools.approval_details(&second).contains("line two"));
    }

    #[test]
    fn git_commit_rejects_empty_message_without_git() {
        let dir = tempdir().unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let call = ToolCall {
            id: "1".into(),
            name: "git_commit".into(),
            arguments: r#"{"message":"   "}"#.into(),
        };
        assert!(
            tools
                .approval_details(&call)
                .contains("commit message cannot be empty")
        );
    }

    #[test]
    fn tool_specs_are_unique_and_cover_mutations() {
        let specs = tool_specs();
        let names = specs
            .iter()
            .filter_map(|spec| spec["function"]["name"].as_str())
            .collect::<Vec<_>>();
        let unique = names
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(names.len(), unique.len());
        for name in [
            "apply_patch",
            "delete_file",
            "move_file",
            "create_directory",
            "git_status",
            "git_log",
            "git_commit",
            "git_restore",
            "git_show",
            "git_blame",
            "git_checkout",
            "append_file",
            "read_files",
        ] {
            assert!(unique.contains(name), "missing {name}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_removes_symlink_without_deleting_its_target() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        fs::write(dir.path().join("target.txt"), "keep\n").unwrap();
        symlink("target.txt", dir.path().join("link.txt")).unwrap();
        let tools = ToolExecutor::new(dir.path().canonicalize().unwrap());
        let call = ToolCall {
            id: "delete-link".into(),
            name: "delete_file".into(),
            arguments: r#"{"path":"link.txt"}"#.into(),
        };
        assert!(tools.approval_details(&call).contains("symlink"));
        assert_eq!(tools.execute(&call).await, "Deleted link.txt.");
        assert!(!dir.path().join("link.txt").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("target.txt")).unwrap(),
            "keep\n"
        );
    }
}
