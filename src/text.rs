//! Cutting text down to size. Every module that shortens something for a
//! prompt, a status line, or a log does it through here, so "how long is too
//! long" is the only thing a caller decides.

use std::borrow::Cow;

use serde_json::Value;

/// A message content's text. Content is either a string or, for a prompt with
/// images, an array of parts; the text parts are joined and the images left
/// out. Anything that reads what was said goes through here, so an image
/// prompt is never mistaken for an empty one.
pub fn content_text(content: &Value) -> Cow<'_, str> {
    match content {
        Value::String(text) => Cow::Borrowed(text),
        Value::Array(parts) => {
            let texts: Vec<&str> =
                parts.iter().filter(|part| part["type"] == "text").filter_map(|part| part["text"].as_str()).collect();
            match texts.as_slice() {
                [only] => Cow::Borrowed(only),
                _ => Cow::Owned(texts.join("\n\n")),
            }
        }
        _ => Cow::Borrowed(""),
    }
}

/// The image URLs (data URLs, in practice) in a message content.
pub fn content_images(content: &Value) -> Vec<&str> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "image_url")
        .filter_map(|part| part.pointer("/image_url/url").and_then(Value::as_str))
        .collect()
}

/// The longest prefix of `text` that fits in `max` bytes without splitting a
/// character.
pub fn prefix(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `text` held to `max` bytes, with `tail` marking that something was cut.
pub fn clip_bytes(text: &str, max: usize, tail: &str) -> String {
    if text.len() <= max { text.to_owned() } else { format!("{}{tail}", prefix(text, max)) }
}

/// `text` held to `max` characters, with `tail` marking that something was cut.
pub fn clip(text: &str, max: usize, tail: &str) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}{tail}", &text[..end]),
        None => text.to_owned(),
    }
}

/// `text` on one line: line breaks become spaces.
pub fn flat(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

/// `text` on one line with every run of whitespace collapsed to a space.
pub fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The most recent thing the assistant said, or nothing.
pub fn last_reply(messages: &[Value]) -> &str {
    messages
        .iter()
        .rev()
        .filter(|message| message["role"] == "assistant")
        .find_map(|message| message["content"].as_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn content_text_reads_strings_and_the_text_of_image_prompts() {
        assert_eq!(content_text(&json!("plain")), "plain");
        let parts = json!([
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
        ]);
        assert_eq!(content_text(&parts), "what is this?");
        assert_eq!(content_images(&parts), ["data:image/png;base64,QUJD"]);
        assert_eq!(content_text(&Value::Null), "");
        assert!(content_images(&json!("plain")).is_empty());
    }

    #[test]
    fn cuts_never_split_a_character() {
        assert_eq!(prefix("héllo", 2), "h");
        assert_eq!(clip_bytes("héllo", 2, "…"), "h…");
        assert_eq!(clip("héllo", 2, "…"), "hé…");
        assert_eq!(clip("hé", 2, "…"), "hé", "nothing cut, nothing marked");
    }

    #[test]
    fn one_line_forms_differ_only_in_how_they_treat_runs() {
        assert_eq!(flat("a\n\nb"), "a  b");
        assert_eq!(squeeze(" a\n\n b "), "a b");
    }
}
