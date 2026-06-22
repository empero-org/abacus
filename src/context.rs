use std::path::{Component, Path};

use anyhow::Result;

pub fn expand_file_references(workspace: &Path, prompt: &str) -> Result<String> {
    let workspace = workspace.canonicalize()?;
    let mut attachments = Vec::new();
    let mut total = 0_u64;
    for token in prompt.split_whitespace() {
        let Some(raw) = token.strip_prefix('@') else {
            continue;
        };
        let raw = raw.trim_matches(|ch: char| matches!(ch, ',' | ';' | ')' | ']' | '}'));
        if raw.is_empty() || raw.contains("://") {
            continue;
        }
        let relative = Path::new(raw);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            continue;
        }
        let Ok(path) = workspace.join(relative).canonicalize() else {
            continue;
        };
        if !path.starts_with(&workspace) || !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if (name == ".env" || name.starts_with(".env.")) && name != ".env.example" {
            continue;
        }
        let size = path.metadata()?.len();
        if size > 200_000 || total + size > 500_000 {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        total += size;
        attachments.push((raw.to_owned(), content));
        if attachments.len() >= 8 {
            break;
        }
    }
    if attachments.is_empty() {
        return Ok(prompt.to_owned());
    }
    let mut expanded = prompt.to_owned();
    for (path, content) in attachments {
        expanded.push_str(&format!(
            "\n\n<attached_file path=\"{path}\">\n{content}\n</attached_file>"
        ));
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn attaches_workspace_file_but_not_dotenv() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("code.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join(".env"), "TOKEN=secret").unwrap();
        let expanded = expand_file_references(dir.path(), "Read @code.rs and @.env").unwrap();
        assert!(expanded.contains("fn main"));
        assert!(!expanded.contains("TOKEN=secret"));
    }
}
