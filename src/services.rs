use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

use crate::{
    config::{AbacusPaths, ProjectExtensions, Settings},
    extensions::{PluginRegistry, SkillRegistry},
    mcp::{McpManager, McpServerConfig},
    tools::{ToolCall, tool_specs},
};

#[derive(Clone)]
pub struct AgentServices {
    pub skills: Arc<SkillRegistry>,
    pub plugins: Arc<PluginRegistry>,
    pub mcp: Arc<McpManager>,
    workspace: PathBuf,
    project_trusted: bool,
}

impl AgentServices {
    pub async fn discover(
        workspace: &Path,
        paths: &AbacusPaths,
        settings: &Settings,
    ) -> Result<Self> {
        let project_trusted = settings.trust.contains(workspace);
        let project = if project_trusted {
            ProjectExtensions::load(workspace)?
        } else {
            ProjectExtensions::default()
        };

        let user_plugin_paths = resolve_paths(&settings.plugins.paths, &paths.root);
        let mut disabled = settings.plugins.disabled.clone();
        disabled.extend(project.plugins.disabled);
        let mut project_plugin_paths = user_plugin_paths;
        if project_trusted {
            project_plugin_paths.extend(resolve_paths(&project.plugins.paths, workspace));
        }
        let plugins = PluginRegistry::discover(
            paths,
            workspace,
            &project_plugin_paths,
            &disabled,
            project_trusted,
        );

        let mut skill_paths = resolve_paths(&settings.skills.paths, &paths.root);
        if project_trusted {
            skill_paths.extend(resolve_paths(&project.skills.paths, workspace));
        }
        let skills =
            SkillRegistry::discover(paths, workspace, &plugins.skill_roots(), &skill_paths);

        let mut mcp_configs = settings.mcp.clone();
        mcp_configs.extend(plugins.mcp_configs());
        if project_trusted {
            mcp_configs.extend(project.mcp);
        }
        let mcp = McpManager::connect(&mcp_configs, workspace).await;

        Ok(Self {
            skills: Arc::new(skills),
            plugins: Arc::new(plugins),
            mcp: Arc::new(mcp),
            workspace: workspace.to_owned(),
            project_trusted,
        })
    }

    pub fn empty(workspace: PathBuf) -> Self {
        Self {
            skills: Arc::new(SkillRegistry::default()),
            plugins: Arc::new(PluginRegistry::default()),
            mcp: Arc::new(McpManager::default()),
            workspace,
            project_trusted: false,
        }
    }

    pub fn for_workspace(&self, workspace: PathBuf) -> Self {
        Self {
            skills: self.skills.clone(),
            plugins: self.plugins.clone(),
            mcp: self.mcp.clone(),
            workspace,
            project_trusted: self.project_trusted,
        }
    }

    pub fn project_trusted(&self) -> bool {
        self.project_trusted
    }

    pub fn tool_specs(&self) -> Vec<Value> {
        let mut specs = tool_specs();
        specs.extend(SkillRegistry::tool_specs());
        specs.extend(self.mcp.tool_specs());
        specs
    }

    pub fn prompt_context(&self) -> String {
        let mut sections = Vec::new();
        let skills = self.skills.prompt_index();
        if !skills.is_empty() {
            sections.push(skills);
        }
        if self.mcp.tools().next().is_some() {
            let tools = self
                .mcp
                .tools()
                .map(|tool| format!("- {}: {}", tool.exposed_name, tool.description))
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!("<mcp_tools>\n{tools}\n</mcp_tools>"));
        }
        if self.plugins.list().next().is_some() {
            let plugins = self
                .plugins
                .list()
                .map(|plugin| {
                    format!(
                        "- {} {}: {}",
                        plugin.name, plugin.version, plugin.description
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!("<plugins>\n{plugins}\n</plugins>"));
        }
        sections.join("\n\n")
    }

    pub fn needs_approval(&self, call: &ToolCall) -> bool {
        self.mcp
            .needs_approval(&call.name)
            .unwrap_or_else(|| call.needs_approval())
    }

    pub fn approval_details(&self, call: &ToolCall) -> Option<String> {
        self.mcp.approval_details(&call.name, &call.arguments)
    }

    pub async fn execute(&self, call: &ToolCall) -> Option<String> {
        if let Some(result) = self.skills.execute(&call.name, &call.arguments) {
            return Some(result);
        }
        self.mcp.execute(&call.name, &call.arguments).await
    }

    pub fn search_catalog(&self, query: &str) -> String {
        let query = query.to_ascii_lowercase();
        let mut output = Vec::new();
        for skill in self.skills.search(&query) {
            output.push(format!("skill/{}: {}", skill.name, skill.description));
        }
        for tool in self.mcp.tools() {
            if query.is_empty()
                || tool.exposed_name.to_ascii_lowercase().contains(&query)
                || tool.description.to_ascii_lowercase().contains(&query)
            {
                output.push(format!("{}: {}", tool.exposed_name, tool.description));
            }
        }
        for plugin in self.plugins.list() {
            if query.is_empty()
                || plugin.name.to_ascii_lowercase().contains(&query)
                || plugin.description.to_ascii_lowercase().contains(&query)
            {
                output.push(format!("plugin/{}: {}", plugin.name, plugin.description));
            }
        }
        output.join("\n")
    }

    pub async fn run_hooks(
        &self,
        event: &str,
        session_id: Option<&str>,
        payload: &Value,
    ) -> Result<Vec<String>> {
        let mut outputs = Vec::new();
        for (plugin, hook) in self.plugins.hooks(event) {
            let command = resolve_hook_command(&plugin.root, &hook.command)?;
            let mut process = Command::new(command);
            process
                .args(&hook.args)
                .current_dir(&self.workspace)
                .env("ABACUS_HOOK_EVENT", event)
                .env("ABACUS_PLUGIN_ROOT", &plugin.root)
                .env("ABACUS_WORKSPACE_ROOT", &self.workspace)
                .env("ABACUS_SESSION_ID", session_id.unwrap_or_default())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            for (key, value) in &hook.env {
                process.env(key, value);
            }
            let mut child = process
                .spawn()
                .with_context(|| format!("could not start {event} hook from {}", plugin.name))?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(&serde_json::to_vec(payload)?).await?;
            }
            let duration = Duration::from_secs(hook.timeout_seconds.clamp(1, 300));
            let output = timeout(duration, child.wait_with_output())
                .await
                .map_err(|_| anyhow::anyhow!("{event} hook from {} timed out", plugin.name))??;
            let stdout: String = String::from_utf8_lossy(&output.stdout)
                .chars()
                .take(16_000)
                .collect();
            let stderr: String = String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(16_000)
                .collect();
            if !output.status.success() {
                bail!(
                    "{event} hook from {} rejected the operation: {}",
                    plugin.name,
                    stderr.trim()
                );
            }
            if !stdout.trim().is_empty() {
                outputs.push(format!("{}: {}", plugin.name, stdout.trim()));
            }
        }
        Ok(outputs)
    }

    pub fn diagnostics(&self) -> Vec<String> {
        let mut diagnostics = self.skills.diagnostics().to_vec();
        diagnostics.extend(self.plugins.diagnostics().iter().cloned());
        diagnostics.extend(self.mcp.diagnostics().iter().cloned());
        diagnostics
    }
}

fn resolve_paths(paths: &[PathBuf], base: &Path) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                base.join(path)
            }
        })
        .collect()
}

fn resolve_hook_command(root: &Path, command: &str) -> Result<PathBuf> {
    let path = Path::new(command);
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    };
    let canonical = path
        .canonicalize()
        .with_context(|| format!("hook command does not exist: {}", path.display()))?;
    if !Path::new(command).is_absolute() && !canonical.starts_with(root) {
        bail!("plugin hook command escapes plugin root");
    }
    Ok(canonical)
}

pub fn merge_mcp_configs(
    base: &BTreeMap<String, McpServerConfig>,
    additions: BTreeMap<String, McpServerConfig>,
) -> BTreeMap<String, McpServerConfig> {
    let mut output = base.clone();
    output.extend(additions);
    output
}

// The plugin-hook test relies on Unix executable permissions, so the whole
// module is Unix-only; gating it here keeps its imports from being flagged as
// unused on Windows.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    async fn executes_declared_plugin_hooks_with_context() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let paths = AbacusPaths::under(directory.path().join("home"));
        let plugin = paths.root.join("plugins/audit");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("plugin.toml"),
            r#"manifest_version = 1
name = "audit"
version = "1.0.0"
description = "hook test"

[[hooks]]
event = "session_start"
command = "hook.sh"
"#,
        )
        .unwrap();
        let hook = plugin.join("hook.sh");
        std::fs::write(&hook, "#!/bin/sh\nread payload\nprintf 'event=%s payload=%s' \"$ABACUS_HOOK_EVENT\" \"$payload\"\n").unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&hook, permissions).unwrap();

        let services = AgentServices::discover(&workspace, &paths, &Settings::default())
            .await
            .unwrap();
        let output = services
            .run_hooks("session_start", Some("session-1"), &json!({"ready":true}))
            .await
            .unwrap();
        assert!(output[0].contains("event=session_start"));
        assert!(output[0].contains("ready"));
    }
}
