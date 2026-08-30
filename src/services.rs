use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, RwLock},
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

/// Everything `SkillRegistry::discover` needs, kept so the registry can be
/// rebuilt without re-deriving trust and plugin state.
#[derive(Clone, Default)]
struct SkillDiscovery {
    paths: Option<AbacusPaths>,
    workspace: PathBuf,
    plugin_roots: Vec<PathBuf>,
    extra_paths: Vec<PathBuf>,
}

#[derive(Clone)]
pub struct AgentServices {
    /// Behind a lock because the agent can now write a skill and register it
    /// mid-turn. `TurnOptions.services` is a fixed `Arc` for the turn, so the
    /// registry itself has to be the mutable part.
    pub skills: Arc<RwLock<SkillRegistry>>,
    pub plugins: Arc<PluginRegistry>,
    pub mcp: Arc<McpManager>,
    skill_discovery: SkillDiscovery,
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
            skills: Arc::new(RwLock::new(skills)),
            skill_discovery: SkillDiscovery {
                paths: Some(paths.clone()),
                workspace: workspace.to_owned(),
                plugin_roots: plugins.skill_roots(),
                extra_paths: skill_paths,
            },
            plugins: Arc::new(plugins),
            mcp: Arc::new(mcp),
            workspace: workspace.to_owned(),
            project_trusted,
        })
    }

    pub fn empty(workspace: PathBuf) -> Self {
        Self {
            skills: Arc::new(RwLock::new(SkillRegistry::default())),
            skill_discovery: SkillDiscovery::default(),
            plugins: Arc::new(PluginRegistry::default()),
            mcp: Arc::new(McpManager::default()),
            workspace,
            project_trusted: false,
        }
    }

    pub fn for_workspace(&self, workspace: PathBuf) -> Self {
        Self {
            skills: self.skills.clone(),
            skill_discovery: self.skill_discovery.clone(),
            plugins: self.plugins.clone(),
            mcp: self.mcp.clone(),
            workspace,
            project_trusted: self.project_trusted,
        }
    }

    /// Rediscover skills in place, so a skill the agent just wrote is callable
    /// without restarting the session. Returns how many are registered.
    pub fn reload_skills(&self) -> usize {
        let Some(paths) = &self.skill_discovery.paths else {
            return 0;
        };
        let rebuilt = SkillRegistry::discover(
            paths,
            &self.skill_discovery.workspace,
            &self.skill_discovery.plugin_roots,
            &self.skill_discovery.extra_paths,
        );
        let count = rebuilt.list().count();
        *self.skills.write().expect("skill registry lock") = rebuilt;
        count
    }

    /// The roots the agent is allowed to write to: its own two workspace and
    /// user directories. Plugin, `~/.agents`, and configured roots are somebody
    /// else's to manage and stay read-only.
    pub fn authorable_skill_roots(&self) -> Option<(PathBuf, PathBuf)> {
        let paths = self.skill_discovery.paths.as_ref()?;
        Some((
            self.workspace.join(".abacus/skills"),
            paths.root.join("skills"),
        ))
    }

    pub fn project_trusted(&self) -> bool {
        self.project_trusted
    }

    pub fn tool_specs(&self) -> Vec<Value> {
        let mut specs = tool_specs();
        specs.extend(SkillRegistry::tool_specs());
        // Authoring is offered only where there is somewhere to write.
        if self.authorable_skill_roots().is_some() {
            specs.extend(SkillRegistry::author_tool_specs());
        }
        specs.extend(self.mcp.tool_specs());
        specs
    }

    pub fn prompt_context(&self) -> String {
        let mut sections = Vec::new();
        let skills = self
            .skills
            .read()
            .expect("skill registry lock")
            .prompt_index();
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
        if let Some(result) = self.execute_authoring(&call.name, &call.arguments) {
            return Some(result);
        }
        if let Some(result) = self
            .skills
            .read()
            .expect("skill registry lock")
            .execute(&call.name, &call.arguments)
        {
            return Some(result);
        }
        self.mcp.execute(&call.name, &call.arguments).await
    }

    /// `skill_create` / `skill_update` / `skill_reload`.
    ///
    /// These live here rather than on the registry because writing needs the
    /// authorable roots and re-registering needs the discovery inputs, and the
    /// registry knows neither.
    fn execute_authoring(&self, tool: &str, arguments: &str) -> Option<String> {
        let result = match tool {
            "skill_create" => self.create_skill(arguments),
            "skill_update" => self.update_skill(arguments),
            "skill_reload" => Ok(format!(
                "Reloaded skills — {} now registered.",
                self.reload_skills()
            )),
            _ => return None,
        };
        Some(result.unwrap_or_else(|error| format!("Error: {error:#}")))
    }

    fn create_skill(&self, arguments: &str) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct Args {
            name: String,
            description: String,
            instructions: String,
            #[serde(default)]
            scope: Option<String>,
        }
        let args: Args =
            serde_json::from_str(arguments).context("invalid skill_create arguments")?;
        let (project_root, user_root) = self
            .authorable_skill_roots()
            .context("this session has no writable skill directory")?;
        let root = match args.scope.as_deref() {
            Some("user") => user_root,
            None | Some("project") => project_root,
            Some(other) => bail!("unknown scope `{other}` — use project or user"),
        };
        // Refuse to shadow a name that already resolves elsewhere: two skills
        // with one name means whichever root wins discovery silently decides.
        if let Some(existing) = self
            .skills
            .read()
            .expect("skill registry lock")
            .root_of(&args.name)
            .map(Path::to_path_buf)
            && !existing.starts_with(&root)
        {
            bail!(
                "a skill named `{}` already exists at {} — pick another name, or use skill_update if it is yours",
                args.name,
                existing.display()
            );
        }
        let path = crate::extensions::skills::write_skill(
            &root,
            &args.name,
            &args.description,
            &args.instructions,
        )?;
        let count = self.reload_skills();
        Ok(format!(
            "Created skill `{}` at {} and registered it ({count} skills active). It is loadable now with skill_load.",
            args.name,
            path.display()
        ))
    }

    fn update_skill(&self, arguments: &str) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct Args {
            name: String,
            #[serde(default)]
            description: Option<String>,
            instructions: String,
        }
        let args: Args =
            serde_json::from_str(arguments).context("invalid skill_update arguments")?;
        let (project_root, user_root) = self
            .authorable_skill_roots()
            .context("this session has no writable skill directory")?;

        let (existing_root, existing_description) = {
            let registry = self.skills.read().expect("skill registry lock");
            let skill = registry
                .get(&args.name)
                .with_context(|| format!("skill `{}` is not installed", args.name))?;
            (skill.root.clone(), skill.description.clone())
        };
        // A plugin's skill, a `~/.agents` skill, or a configured root belongs to
        // whoever installed it. Editing it from here would be an edit the owner
        // never sees and the next reinstall would silently undo.
        let root = if existing_root.starts_with(&project_root) {
            project_root
        } else if existing_root.starts_with(&user_root) {
            user_root
        } else {
            bail!(
                "skill `{}` lives at {} and is not one Abacus manages — only skills under .abacus/skills or ~/.abacus/skills can be edited",
                args.name,
                existing_root.display()
            );
        };

        let description = args.description.unwrap_or(existing_description);
        let path = crate::extensions::skills::write_skill(
            &root,
            &args.name,
            &description,
            &args.instructions,
        )?;
        self.reload_skills();
        Ok(format!(
            "Updated skill `{}` at {}.",
            args.name,
            path.display()
        ))
    }

    pub fn search_catalog(&self, query: &str) -> String {
        let query = query.to_ascii_lowercase();
        let mut output = Vec::new();
        for skill in self
            .skills
            .read()
            .expect("skill registry lock")
            .search(&query)
        {
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
        let mut diagnostics = self
            .skills
            .read()
            .expect("skill registry lock")
            .diagnostics()
            .to_vec();
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

    async fn services(directory: &Path) -> (AgentServices, PathBuf) {
        let workspace = directory.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let paths = AbacusPaths::under(directory.join("home"));
        std::fs::create_dir_all(&paths.root).unwrap();
        let services = AgentServices::discover(&workspace, &paths, &Settings::default())
            .await
            .unwrap();
        (services, workspace)
    }

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "1".to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_string(),
        }
    }

    /// The loop Phase 2 exists to close: write a skill, register it, invoke it,
    /// all inside one session.
    #[tokio::test]
    async fn a_created_skill_is_invocable_without_a_restart() {
        let directory = tempdir().unwrap();
        let (services, workspace) = services(directory.path()).await;

        let created = services
            .execute(&call(
                "skill_create",
                json!({
                    "name": "release-audit",
                    "description": "Audit a release branch before tagging",
                    "instructions": "1. Check the changelog.\n2. Run the tests."
                }),
            ))
            .await
            .expect("skill_create is dispatched");
        assert!(
            created.contains("Created skill `release-audit`"),
            "{created}"
        );
        assert!(
            workspace
                .join(".abacus/skills/release-audit/SKILL.md")
                .is_file(),
            "project scope writes into the workspace"
        );

        // Registered in the same session — no reload call, no restart.
        let loaded = services
            .execute(&call("skill_load", json!({"name": "release-audit"})))
            .await
            .unwrap();
        assert!(loaded.contains("Check the changelog"), "{loaded}");
        assert!(services.prompt_context().contains("release-audit"));
        assert!(
            services
                .execute(&call("skill_search", json!({"query": "release"})))
                .await
                .unwrap()
                .contains("release-audit")
        );
    }

    #[tokio::test]
    async fn user_scope_writes_outside_the_workspace() {
        let directory = tempdir().unwrap();
        let (services, workspace) = services(directory.path()).await;
        services
            .execute(&call(
                "skill_create",
                json!({
                    "name": "commit-style",
                    "description": "How this user likes commit messages",
                    "instructions": "Imperative mood, no trailer.",
                    "scope": "user"
                }),
            ))
            .await
            .unwrap();
        assert!(
            directory
                .path()
                .join("home/skills/commit-style/SKILL.md")
                .is_file()
        );
        assert!(!workspace.join(".abacus/skills/commit-style").exists());
    }

    #[tokio::test]
    async fn update_rewrites_an_owned_skill_and_refuses_a_foreign_one() {
        let directory = tempdir().unwrap();
        let (services, workspace) = services(directory.path()).await;
        services
            .execute(&call(
                "skill_create",
                json!({"name":"mine","description":"d","instructions":"v1 body"}),
            ))
            .await
            .unwrap();
        let updated = services
            .execute(&call(
                "skill_update",
                json!({"name":"mine","instructions":"v2 body"}),
            ))
            .await
            .unwrap();
        assert!(updated.contains("Updated skill `mine`"), "{updated}");
        let loaded = services
            .execute(&call("skill_load", json!({"name":"mine"})))
            .await
            .unwrap();
        assert!(
            loaded.contains("v2 body") && !loaded.contains("v1 body"),
            "{loaded}"
        );
        // The description survives an update that omits it.
        assert!(services.prompt_context().contains("mine: d"));

        // A skill from a root Abacus does not own is read-only: editing it
        // would be invisible to its owner and undone by the next reinstall.
        let foreign = workspace.join(".agents/skills/borrowed");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(
            foreign.join("SKILL.md"),
            "---\nname: borrowed\ndescription: not ours\n---\n\nbody\n",
        )
        .unwrap();
        services.reload_skills();
        let refused = services
            .execute(&call(
                "skill_update",
                json!({"name":"borrowed","instructions":"hijacked"}),
            ))
            .await
            .unwrap();
        assert!(refused.starts_with("Error:"), "{refused}");
        assert!(refused.contains("not one Abacus manages"), "{refused}");
        assert!(
            std::fs::read_to_string(foreign.join("SKILL.md"))
                .unwrap()
                .contains("body"),
            "the foreign skill is untouched"
        );
    }

    #[tokio::test]
    async fn create_refuses_to_shadow_a_name_owned_elsewhere() {
        let directory = tempdir().unwrap();
        let (services, workspace) = services(directory.path()).await;
        let foreign = workspace.join(".agents/skills/taken");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(
            foreign.join("SKILL.md"),
            "---\nname: taken\ndescription: theirs\n---\n\nbody\n",
        )
        .unwrap();
        services.reload_skills();

        // Two skills of one name means whichever root wins discovery silently
        // decides which the model gets.
        let refused = services
            .execute(&call(
                "skill_create",
                json!({"name":"taken","description":"mine","instructions":"body"}),
            ))
            .await
            .unwrap();
        assert!(refused.starts_with("Error:"), "{refused}");
        assert!(refused.contains("already exists"), "{refused}");
    }

    #[tokio::test]
    async fn a_malformed_skill_is_rejected_before_it_reaches_disk() {
        let directory = tempdir().unwrap();
        let (services, _) = services(directory.path()).await;
        for arguments in [
            json!({"name":"Bad Name","description":"d","instructions":"b"}),
            json!({"name":"ok","description":"","instructions":"b"}),
            json!({"name":"ok","description":"d","instructions":"   "}),
            json!({"name":"ok","description":"line\nbreak","instructions":"b"}),
        ] {
            let reply = services
                .execute(&call("skill_create", arguments.clone()))
                .await
                .unwrap();
            assert!(reply.starts_with("Error:"), "{arguments} accepted: {reply}");
        }
    }

    #[tokio::test]
    async fn a_description_with_yaml_punctuation_round_trips() {
        let directory = tempdir().unwrap();
        let (services, _) = services(directory.path()).await;
        // An unquoted `key: value` description would reparse as a YAML map and
        // the skill would fail to load back.
        services
            .execute(&call(
                "skill_create",
                json!({
                    "name":"tricky",
                    "description":"note: use this when tests fail",
                    "instructions":"body"
                }),
            ))
            .await
            .unwrap();
        assert!(
            services
                .prompt_context()
                .contains("note: use this when tests fail")
        );
    }

    #[tokio::test]
    async fn authoring_tools_are_offered_only_with_somewhere_to_write() {
        let directory = tempdir().unwrap();
        let (services, _) = services(directory.path()).await;
        let names: Vec<String> = services
            .tool_specs()
            .iter()
            .filter_map(|spec| spec["function"]["name"].as_str().map(str::to_owned))
            .collect();
        assert!(names.iter().any(|name| name == "skill_create"));
        assert!(names.iter().any(|name| name == "skill_reload"));

        // A bare services object has no discovery inputs and so no root.
        let empty = AgentServices::empty(directory.path().to_owned());
        assert!(empty.authorable_skill_roots().is_none());
        let names: Vec<String> = empty
            .tool_specs()
            .iter()
            .filter_map(|spec| spec["function"]["name"].as_str().map(str::to_owned))
            .collect();
        assert!(!names.iter().any(|name| name == "skill_create"));
    }

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
