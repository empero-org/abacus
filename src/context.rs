use std::path::{Component, Path};

use anyhow::Result;
use base64::Engine as _;
use serde_json::{Value, json};

/// File extensions treated as images and attached as vision content parts
/// rather than inlined text.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Cap per image and across all images in one message; a screenshot is
/// typically well under this, and past it providers reject the request
/// anyway.
const MAX_IMAGE_BYTES: u64 = 8_000_000;

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

/// Collect image attachments referenced by the prompt and build the user
/// message content. Two reference forms are recognised:
///
/// - `[image:NAME]` — a clipboard paste saved under `attachments`; the token
///   is what Ctrl+V inserts into the composer.
/// - `@path.png` (and the other image extensions) — a file in the workspace,
///   with the same traversal and size rules as text `@` references.
///
/// With no image references the content is the plain string, so text-only
/// sessions keep their existing wire shape and history format.
pub fn user_content(workspace: &Path, attachments: &Path, prompt: &str) -> Value {
    let mut text = prompt.to_owned();
    let mut images: Vec<String> = Vec::new();
    let mut total = 0_u64;

    // Clipboard tokens: resolve strictly to a file *name* inside the
    // attachments directory, so a crafted token cannot walk anywhere else.
    let mut cursor = 0;
    while let Some(start) = text[cursor..].find("[image:") {
        let start = cursor + start;
        let Some(length) = text[start..].find(']') else {
            break;
        };
        let end = start + length + 1;
        let name = &text[start + "[image:".len()..end - 1];
        let clean = Path::new(name)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let path = attachments.join(clean);
        let bytes = if !clean.is_empty() && clean == name {
            std::fs::read(&path).ok()
        } else {
            None
        };
        match bytes {
            Some(bytes) if total + bytes.len() as u64 <= MAX_IMAGE_BYTES => {
                total += bytes.len() as u64;
                images.push(data_url(&bytes));
                let marker = format!("[image #{}]", images.len());
                text.replace_range(start..end, &marker);
                cursor = start + marker.len();
            }
            _ => cursor = end,
        }
    }

    // Workspace image files referenced with `@`, subject to the same
    // workspace-containment rules as text attachments.
    if let Ok(workspace) = workspace.canonicalize() {
        for token in prompt.split_whitespace() {
            let Some(raw) = token.strip_prefix('@') else {
                continue;
            };
            let raw = raw.trim_matches(|ch: char| matches!(ch, ',' | ';' | ')' | ']' | '}'));
            let extension = Path::new(raw)
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !IMAGE_EXTENSIONS.contains(&extension.as_str()) {
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
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if total + bytes.len() as u64 > MAX_IMAGE_BYTES {
                continue;
            }
            total += bytes.len() as u64;
            images.push(data_url_for(&extension, &bytes));
        }
    }

    if images.is_empty() {
        return Value::String(text);
    }
    let mut parts = vec![json!({"type": "text", "text": text})];
    for url in images {
        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
    }
    Value::Array(parts)
}

fn data_url(png: &[u8]) -> String {
    data_url_for("png", png)
}

fn data_url_for(extension: &str, bytes: &[u8]) -> String {
    let mime = match extension {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    };
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
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

    // A 1x1 transparent PNG, the smallest well-formed image to attach.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn text_only_prompts_keep_plain_string_content() {
        let dir = tempdir().unwrap();
        let content = user_content(dir.path(), dir.path(), "just words");
        assert_eq!(content, Value::String("just words".into()));
    }

    #[test]
    fn clipboard_token_becomes_an_image_part_with_a_numbered_marker() {
        let workspace = tempdir().unwrap();
        let attachments = tempdir().unwrap();
        std::fs::write(attachments.path().join("img-ab12.png"), TINY_PNG).unwrap();
        let content = user_content(
            workspace.path(),
            attachments.path(),
            "what is in [image:img-ab12.png] here?",
        );
        let parts = content.as_array().expect("content array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "what is in [image #1] here?");
        let url = parts[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
    }

    #[test]
    fn image_tokens_cannot_escape_the_attachments_directory() {
        let workspace = tempdir().unwrap();
        let attachments = tempdir().unwrap();
        let secret = workspace.path().join("secret.png");
        std::fs::write(&secret, TINY_PNG).unwrap();
        let content = user_content(
            workspace.path(),
            attachments.path(),
            &format!(
                "[image:../{}] and [image:{}]",
                secret.file_name().unwrap().to_str().unwrap(),
                secret.display()
            ),
        );
        assert!(content.is_string(), "traversal must not attach: {content}");
    }

    #[test]
    fn workspace_image_references_attach_with_the_right_mime() {
        let workspace = tempdir().unwrap();
        std::fs::write(workspace.path().join("shot.png"), TINY_PNG).unwrap();
        let content = user_content(workspace.path(), workspace.path(), "look at @shot.png");
        let parts = content.as_array().expect("content array");
        assert_eq!(parts.len(), 2);
        assert!(
            parts[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png"),
        );
        // The @token stays in the text so the model can see the reference.
        assert_eq!(parts[0]["text"], "look at @shot.png");
    }
}
