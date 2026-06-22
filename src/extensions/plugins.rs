use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{config::AbacusPaths, mcp::McpServerConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommand {
    pub name: String,
    pub description: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginHook {
    pub event: String,
    pub command: String,
    pub args: Vec<String>,
    pub timeout_seconds: u64,
    pub env: BTreeMap<String, String>,
}

impl Default for PluginHook {
    fn default() -> Self {
        Self {
            event: String::new(),
            command: String::new(),
            args: Vec::new(),
            timeout_seconds: 30,
            env: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    manifest_version: u32,
    name: String,
    version: String,
    description: String,
    #[serde(default = "default_skill_dirs")]
    skills: Vec<PathBuf>,
    #[serde(default)]
    commands: Vec<PluginCommand>,
    #[serde(default)]
    hooks: Vec<PluginHook>,
    #[serde(default)]
    mcp: BTreeMap<String, McpServerConfig>,
}

#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub version: String,
    pub description: String,
    pub root: PathBuf,
    pub skill_dirs: Vec<PathBuf>,
    pub commands: Vec<PluginCommand>,
    pub hooks: Vec<PluginHook>,
    pub mcp: BTreeMap<String, McpServerConfig>,
    pub source: String,
}

#[derive(Debug, Clone, Default)]
pub struct PluginRegistry {
    plugins: BTreeMap<String, Plugin>,
    commands: BTreeMap<String, (String, PluginCommand)>,
    diagnostics: Vec<String>,
}

impl PluginRegistry {
    pub fn discover(
        paths: &AbacusPaths,
        workspace: &Path,
        extra_paths: &[PathBuf],
        disabled: &BTreeSet<String>,
        include_project: bool,
    ) -> Self {
        let mut registry = Self::default();
        registry.scan_root(&paths.root.join("plugins"), "user", disabled);
        for path in extra_paths {
            registry.scan_root(path, "configured", disabled);
        }
        if include_project {
            registry.scan_root(&workspace.join(".abacus/plugins"), "project", disabled);
        }
        registry.rebuild_commands();
        registry
    }

    pub fn list(&self) -> impl Iterator<Item = &Plugin> {
        self.plugins.values()
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn skill_roots(&self) -> Vec<PathBuf> {
        self.plugins
            .values()
            .flat_map(|plugin| plugin.skill_dirs.iter().cloned())
            .collect()
    }

    pub fn command(&self, name: &str) -> Option<&PluginCommand> {
        self.commands.get(name).map(|(_, command)| command)
    }

    pub fn mcp_configs(&self) -> BTreeMap<String, McpServerConfig> {
        let mut output = BTreeMap::new();
        for plugin in self.plugins.values() {
            for (name, config) in &plugin.mcp {
                let mut config = config.clone();
                if config.cwd.is_none() {
                    config.cwd = Some(plugin.root.clone());
                }
                output.insert(format!("{}-{name}", plugin.name), config);
            }
        }
        output
    }

    pub fn hooks(&self, event: &str) -> Vec<(&Plugin, &PluginHook)> {
        self.plugins
            .values()
            .flat_map(|plugin| {
                plugin
                    .hooks
                    .iter()
                    .filter(move |hook| hook.event == event)
                    .map(move |hook| (plugin, hook))
            })
            .collect()
    }

    pub fn install(source: &Path, paths: &AbacusPaths, force: bool) -> Result<Plugin> {
        let source = source
            .canonicalize()
            .context("plugin source does not exist")?;
        let plugin = load_plugin(&source, "install-source")?;
        let plugins_root = paths.root.join("plugins");
        fs::create_dir_all(&plugins_root)?;
        let destination = plugins_root.join(&plugin.name);
        if destination.exists() {
            if !force {
                bail!(
                    "plugin `{}` is already installed; use --force to replace it",
                    plugin.name
                );
            }
            let canonical_root = plugins_root.canonicalize()?;
            let canonical = destination.canonicalize()?;
            if !canonical.starts_with(&canonical_root) {
                bail!("plugin destination escapes the plugin directory");
            }
        }
        let staging_root = plugins_root.join(format!(".install-{}", Uuid::new_v4()));
        let staged = staging_root.join(&plugin.name);
        let result = (|| {
            copy_tree(&source, &staged, 0)?;
            load_plugin(&staged, "staged")?;
            if destination.exists() {
                let backup =
                    plugins_root.join(format!(".backup-{}-{}", plugin.name, Uuid::new_v4()));
                fs::rename(&destination, &backup)?;
                if let Err(error) = fs::rename(&staged, &destination) {
                    let _ = fs::rename(&backup, &destination);
                    return Err(error).context("could not activate replacement plugin");
                }
                fs::remove_dir_all(backup)?;
            } else {
                fs::rename(&staged, &destination)?;
            }
            load_plugin(&destination.canonicalize()?, "user")
        })();
        let _ = fs::remove_dir_all(staging_root);
        result
    }

    pub fn remove(name: &str, paths: &AbacusPaths) -> Result<()> {
        validate_name(name)?;
        let root = paths.root.join("plugins");
        let path = root.join(name);
        if !path.exists() {
            bail!("plugin `{name}` is not installed");
        }
        let canonical_root = root.canonicalize()?;
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(&canonical_root) {
            bail!("plugin path escapes the plugin directory");
        }
        fs::remove_dir_all(canonical)?;
        Ok(())
    }

    fn scan_root(&mut self, root: &Path, source: &str, disabled: &BTreeSet<String>) {
        if !root.is_dir() {
            return;
        }
        let Ok(canonical_root) = root.canonicalize() else {
            return;
        };
        let Ok(entries) = fs::read_dir(&canonical_root) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(path) = entry.path().canonicalize() else {
                continue;
            };
            if !path.starts_with(&canonical_root) || !path.is_dir() {
                continue;
            }
            match load_plugin(&path, source) {
                Ok(plugin) if !disabled.contains(&plugin.name) => {
                    self.plugins.insert(plugin.name.clone(), plugin);
                }
                Ok(_) => {}
                Err(error) => self
                    .diagnostics
                    .push(format!("{}: {error:#}", path.display())),
            }
        }
    }

    fn rebuild_commands(&mut self) {
        self.commands.clear();
        for plugin in self.plugins.values() {
            for command in &plugin.commands {
                if self.commands.contains_key(&command.name) {
                    self.diagnostics.push(format!(
                        "plugin command collision for /{}; {} ignored",
                        command.name, plugin.name
                    ));
                    continue;
                }
                self.commands
                    .insert(command.name.clone(), (plugin.name.clone(), command.clone()));
            }
        }
    }
}

fn load_plugin(root: &Path, source: &str) -> Result<Plugin> {
    let root = root.canonicalize()?;
    let manifest_path = root.join("plugin.toml");
    if !manifest_path.is_file() {
        bail!("missing plugin.toml");
    }
    if manifest_path.metadata()?.len() > 256_000 {
        bail!("plugin.toml exceeds 256 KB");
    }
    let manifest: Manifest =
        toml::from_str(&fs::read_to_string(&manifest_path)?).context("invalid plugin.toml")?;
    if manifest.manifest_version != 1 {
        bail!(
            "unsupported plugin manifest version {}",
            manifest.manifest_version
        );
    }
    validate_name(&manifest.name)?;
    if manifest.version.trim().is_empty() || manifest.description.trim().is_empty() {
        bail!("plugin version and description are required");
    }
    let directory = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if directory != manifest.name {
        bail!(
            "plugin name `{}` must match directory `{directory}`",
            manifest.name
        );
    }
    let skill_dirs = manifest
        .skills
        .iter()
        .map(|path| confined_path(&root, path))
        .collect::<Result<Vec<_>>>()?;
    for command in &manifest.commands {
        validate_name(&command.name)?;
        if command.description.trim().is_empty() || command.prompt.trim().is_empty() {
            bail!(
                "plugin command /{} needs description and prompt",
                command.name
            );
        }
    }
    for hook in &manifest.hooks {
        if !matches!(
            hook.event.as_str(),
            "session_start" | "session_end" | "before_tool" | "after_tool"
        ) {
            bail!("unsupported plugin hook event `{}`", hook.event);
        }
        if hook.command.trim().is_empty() {
            bail!("plugin hook command cannot be empty");
        }
    }
    Ok(Plugin {
        name: manifest.name,
        version: manifest.version,
        description: manifest.description,
        root,
        skill_dirs,
        commands: manifest.commands,
        hooks: manifest.hooks,
        mcp: manifest.mcp,
        source: source.to_owned(),
    })
}

fn confined_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!("plugin paths must stay inside the plugin root");
    }
    let path = root.join(relative);
    if !path.exists() {
        return Ok(path);
    }
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(root) {
        bail!("plugin path escapes its root");
    }
    Ok(canonical)
}

fn copy_tree(source: &Path, destination: &Path, depth: usize) -> Result<()> {
    if depth > 32 {
        bail!("plugin directory nesting exceeds 32 levels");
    }
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if file_type.is_symlink() {
            bail!("plugin installation rejects symbolic links");
        } else if file_type.is_dir() {
            copy_tree(&entry.path(), &target, depth + 1)?;
        } else if file_type.is_file() {
            if entry.metadata()?.len() > 20_000_000 {
                bail!("plugin file {} exceeds 20 MB", entry.path().display());
            }
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || name.ends_with('-')
        || !name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        bail!("name must use 1-64 lowercase letters, digits, or hyphens");
    }
    Ok(())
}

fn default_skill_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from("skills")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn loads_plugin_contributions() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("demo");
        fs::create_dir_all(root.join("skills")).unwrap();
        fs::write(
            root.join("plugin.toml"),
            r#"
manifest_version = 1
name = "demo"
version = "1.0.0"
description = "Demo plugin"

[[commands]]
name = "demo-check"
description = "Run a demo check"
prompt = "Inspect the demo."
"#,
        )
        .unwrap();
        let plugin = load_plugin(&root, "test").unwrap();
        assert_eq!(plugin.name, "demo");
        assert_eq!(plugin.commands[0].name, "demo-check");
    }

    #[test]
    fn installs_and_force_replaces_plugin_from_staging() {
        let dir = tempdir().unwrap();
        let paths = AbacusPaths::under(dir.path().join("home"));
        let source = dir.path().join("demo");
        fs::create_dir_all(source.join("skills")).unwrap();
        fs::write(
            source.join("plugin.toml"),
            "manifest_version = 1\nname = \"demo\"\nversion = \"1.0.0\"\ndescription = \"first\"\n",
        )
        .unwrap();
        assert_eq!(
            PluginRegistry::install(&source, &paths, false)
                .unwrap()
                .description,
            "first"
        );
        assert!(PluginRegistry::install(&source, &paths, false).is_err());
        fs::write(
            source.join("plugin.toml"),
            "manifest_version = 1\nname = \"demo\"\nversion = \"2.0.0\"\ndescription = \"second\"\n",
        )
        .unwrap();
        let replacement = PluginRegistry::install(&source, &paths, true).unwrap();
        assert_eq!(replacement.version, "2.0.0");
        assert_eq!(replacement.description, "second");
    }
}
