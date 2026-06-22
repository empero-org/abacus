use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::AbacusPaths;

const MAX_SKILL_BYTES: u64 = 1_000_000;
const MAX_RESOURCE_BYTES: u64 = 2_000_000;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub root: PathBuf,
    pub source: String,
    instructions: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, Skill>,
    diagnostics: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
}

impl SkillRegistry {
    pub fn discover(
        paths: &AbacusPaths,
        workspace: &Path,
        plugin_roots: &[PathBuf],
        extra_paths: &[PathBuf],
    ) -> Self {
        let mut registry = Self::default();
        for root in plugin_roots {
            registry.scan_root(root, "plugin");
        }
        registry.scan_root(&paths.root.join("skills"), "user");
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            registry.scan_root(&PathBuf::from(home).join(".agents/skills"), "agents");
        }
        for root in extra_paths {
            registry.scan_root(root, "configured");
        }
        registry.scan_root(&workspace.join(".agents/skills"), "project-agents");
        registry.scan_root(&workspace.join(".abacus/skills"), "project");
        registry
    }

    pub fn list(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    pub fn search(&self, query: &str) -> Vec<&Skill> {
        let words = query
            .to_ascii_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut matches = self
            .skills
            .values()
            .filter_map(|skill| {
                let haystack = format!(
                    "{} {}",
                    skill.name.to_ascii_lowercase(),
                    skill.description.to_ascii_lowercase()
                );
                let score = words
                    .iter()
                    .filter(|word| haystack.contains(word.as_str()))
                    .count();
                (words.is_empty() || score > 0).then_some((score, skill))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.name.cmp(&right.1.name))
        });
        matches.into_iter().map(|(_, skill)| skill).collect()
    }

    pub fn prompt_index(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut output = String::from(
            "<available_skills>\nLoad a relevant skill with skill_load before following it. Only metadata is shown here.\n",
        );
        for skill in self.skills.values() {
            output.push_str(&format!(
                "- {}: {} [source={}]\n",
                skill.name, skill.description, skill.source
            ));
        }
        output.push_str("</available_skills>");
        output
    }

    pub fn tool_specs() -> Vec<Value> {
        vec![
            function(
                "skill_search",
                "Search installed Agent Skills by capability. Skills are reusable instruction bundles.",
                json!({
                    "type":"object",
                    "properties":{"query":{"type":"string"}},
                    "required":["query"]
                }),
            ),
            function(
                "skill_load",
                "Load the complete SKILL.md instructions for one installed skill.",
                json!({
                    "type":"object",
                    "properties":{"name":{"type":"string"}},
                    "required":["name"]
                }),
            ),
            function(
                "skill_read",
                "Read a referenced text resource inside an installed skill directory.",
                json!({
                    "type":"object",
                    "properties":{
                        "name":{"type":"string"},
                        "path":{"type":"string","description":"Skill-relative resource path"}
                    },
                    "required":["name","path"]
                }),
            ),
        ]
    }

    pub fn execute(&self, tool: &str, arguments: &str) -> Option<String> {
        let result = match tool {
            "skill_search" => self.execute_search(arguments),
            "skill_load" => self.execute_load(arguments),
            "skill_read" => self.execute_read(arguments),
            _ => return None,
        };
        Some(result.unwrap_or_else(|error| format!("Error: {error:#}")))
    }

    pub fn invocation(&self, name: &str, user_text: &str) -> Result<String> {
        let skill = self
            .skills
            .get(name)
            .with_context(|| format!("skill `{name}` is not installed"))?;
        Ok(format!(
            "The user explicitly invoked the `{}` skill. Follow these instructions for this request:\n\n<skill name=\"{}\" root=\"{}\">\n{}\n</skill>\n\nUser request: {}",
            skill.name,
            skill.name,
            skill.root.display(),
            skill.instructions,
            user_text.trim()
        ))
    }

    fn scan_root(&mut self, root: &Path, source: &str) {
        if !root.is_dir() {
            return;
        }
        let canonical_root = match root.canonicalize() {
            Ok(root) => root,
            Err(error) => {
                self.diagnostics
                    .push(format!("{}: {error}", root.display()));
                return;
            }
        };
        let entries = match fs::read_dir(&canonical_root) {
            Ok(entries) => entries,
            Err(error) => {
                self.diagnostics
                    .push(format!("{}: {error}", canonical_root.display()));
                return;
            }
        };
        for entry in entries.flatten() {
            let candidate = entry.path();
            let Ok(candidate) = candidate.canonicalize() else {
                continue;
            };
            if !candidate.starts_with(&canonical_root) || !candidate.is_dir() {
                continue;
            }
            match load_skill(&candidate, source) {
                Ok(skill) => {
                    self.skills.insert(skill.name.clone(), skill);
                }
                Err(error) => self
                    .diagnostics
                    .push(format!("{}: {error:#}", candidate.display())),
            }
        }
    }

    fn execute_search(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            query: String,
        }
        let args: Args = serde_json::from_str(arguments)?;
        let matches = self.search(&args.query);
        if matches.is_empty() {
            return Ok("No installed skills match that query.".to_owned());
        }
        Ok(matches
            .into_iter()
            .map(|skill| format!("{}: {}", skill.name, skill.description))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn execute_load(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
        }
        let args: Args = serde_json::from_str(arguments)?;
        let skill = self
            .skills
            .get(&args.name)
            .with_context(|| format!("skill `{}` is not installed", args.name))?;
        Ok(format!(
            "<skill name=\"{}\" root=\"{}\">\n{}\n</skill>",
            skill.name,
            skill.root.display(),
            skill.instructions
        ))
    }

    fn execute_read(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
            path: String,
        }
        let args: Args = serde_json::from_str(arguments)?;
        let skill = self
            .skills
            .get(&args.name)
            .with_context(|| format!("skill `{}` is not installed", args.name))?;
        let relative = Path::new(&args.path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::RootDir))
        {
            bail!("skill resource path must stay inside the skill");
        }
        let path = skill.root.join(relative).canonicalize()?;
        if !path.starts_with(&skill.root) || !path.is_file() {
            bail!("skill resource does not exist or escapes its root");
        }
        if path.metadata()?.len() > MAX_RESOURCE_BYTES {
            bail!("skill resource exceeds the 2 MB limit");
        }
        fs::read_to_string(&path).context("skill resource is not UTF-8 text")
    }
}

fn load_skill(root: &Path, source: &str) -> Result<Skill> {
    let path = root.join("SKILL.md");
    if !path.is_file() {
        bail!("missing SKILL.md");
    }
    if path.metadata()?.len() > MAX_SKILL_BYTES {
        bail!("SKILL.md exceeds the 1 MB limit");
    }
    let content = fs::read_to_string(&path).context("SKILL.md is not UTF-8")?;
    let (frontmatter, instructions) = split_frontmatter(&content)?;
    let metadata: Frontmatter = serde_yaml::from_str(frontmatter).context("invalid frontmatter")?;
    validate_name(&metadata.name)?;
    let directory = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if directory != metadata.name {
        bail!(
            "skill name `{}` must match directory `{directory}`",
            metadata.name
        );
    }
    if metadata.description.trim().is_empty() || metadata.description.len() > 1_024 {
        bail!("skill description must contain 1-1024 bytes");
    }
    Ok(Skill {
        name: metadata.name,
        description: metadata.description.trim().to_owned(),
        license: metadata.license,
        compatibility: metadata.compatibility,
        root: root.to_owned(),
        source: source.to_owned(),
        instructions: instructions.trim().to_owned(),
    })
}

fn split_frontmatter(content: &str) -> Result<(&str, &str)> {
    let content = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .context("SKILL.md must begin with YAML frontmatter")?;
    let Some((frontmatter, body)) = content
        .split_once("\n---\n")
        .or_else(|| content.split_once("\r\n---\r\n"))
    else {
        bail!("SKILL.md frontmatter is not terminated with ---");
    };
    Ok((frontmatter, body))
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
        bail!("skill name must use 1-64 lowercase letters, digits, or hyphens");
    }
    Ok(())
}

fn function(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type":"function",
        "function":{"name":name,"description":description,"parameters":parameters}
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_and_progressively_loads_skill() {
        let dir = tempdir().unwrap();
        let home = AbacusPaths::under(dir.path().join("home"));
        let root = home.root.join("skills/review-helper");
        fs::create_dir_all(root.join("references")).unwrap();
        fs::write(
            root.join("SKILL.md"),
            "---\nname: review-helper\ndescription: Reviews code carefully.\n---\nAlways inspect the diff.\n",
        )
        .unwrap();
        fs::write(root.join("references/checks.md"), "Check errors.").unwrap();
        let registry = SkillRegistry::discover(&home, dir.path(), &[], &[]);
        assert!(registry.prompt_index().contains("Reviews code carefully"));
        assert!(!registry.prompt_index().contains("Always inspect"));
        let loaded = registry
            .execute("skill_load", r#"{"name":"review-helper"}"#)
            .unwrap();
        assert!(loaded.contains("Always inspect the diff"));
        let resource = registry
            .execute(
                "skill_read",
                r#"{"name":"review-helper","path":"references/checks.md"}"#,
            )
            .unwrap();
        assert_eq!(resource, "Check errors.");
    }
}
