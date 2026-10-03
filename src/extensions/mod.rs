pub mod plugins;
pub mod skills;

pub use plugins::{Plugin, PluginRegistry};
pub use skills::{Skill, SkillRegistry};

/// Skills, plugins, and plugin commands are all named the same way.
fn validate_name(what: &str, name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || name.ends_with('-')
        || !name.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        anyhow::bail!("{what} must use 1-64 lowercase letters, digits, or hyphens");
    }
    Ok(())
}
