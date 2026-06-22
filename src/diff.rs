#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
    Hunk,
    Metadata,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct DiffFile {
    pub old_path: String,
    pub new_path: String,
    pub lines: Vec<DiffLine>,
    pub additions: usize,
    pub deletions: usize,
}

impl DiffFile {
    pub fn display_path(&self) -> &str {
        let path = if self.new_path == "/dev/null" || self.new_path.is_empty() {
            &self.old_path
        } else {
            &self.new_path
        };
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiffDocument {
    pub files: Vec<DiffFile>,
    pub additions: usize,
    pub deletions: usize,
}

impl DiffDocument {
    pub fn parse(input: &str) -> Option<Self> {
        if !input.lines().any(|line| line.starts_with("--- "))
            || !input.lines().any(|line| line.starts_with("+++ "))
        {
            return None;
        }
        let mut document = Self::default();
        let mut file: Option<DiffFile> = None;
        let mut old_line = 0_u32;
        let mut new_line = 0_u32;
        let mut in_hunk = false;

        for raw in input.lines() {
            if let Some(path) = raw.strip_prefix("--- ") {
                if let Some(current) = file.take() {
                    push_file(&mut document, current);
                }
                file = Some(DiffFile {
                    old_path: clean_path(path),
                    ..DiffFile::default()
                });
                in_hunk = false;
                continue;
            }
            if let Some(path) = raw.strip_prefix("+++ ") {
                let current = file.get_or_insert_with(DiffFile::default);
                current.new_path = clean_path(path);
                continue;
            }
            let Some(current) = file.as_mut() else {
                continue;
            };
            if raw.starts_with("@@") {
                if let Some((old, new)) = parse_hunk_header(raw) {
                    old_line = old;
                    new_line = new;
                }
                current.lines.push(DiffLine {
                    kind: DiffLineKind::Hunk,
                    old_line: None,
                    new_line: None,
                    text: raw.to_owned(),
                });
                in_hunk = true;
                continue;
            }
            if !in_hunk {
                if !raw.starts_with("diff --git") && !raw.starts_with("index ") {
                    current.lines.push(DiffLine {
                        kind: DiffLineKind::Metadata,
                        old_line: None,
                        new_line: None,
                        text: raw.to_owned(),
                    });
                }
                continue;
            }
            let (kind, old, new, text) = if let Some(text) = raw.strip_prefix('+') {
                let line = new_line;
                new_line = new_line.saturating_add(1);
                current.additions += 1;
                (DiffLineKind::Addition, None, Some(line), text)
            } else if let Some(text) = raw.strip_prefix('-') {
                let line = old_line;
                old_line = old_line.saturating_add(1);
                current.deletions += 1;
                (DiffLineKind::Deletion, Some(line), None, text)
            } else if let Some(text) = raw.strip_prefix(' ') {
                let old = old_line;
                let new = new_line;
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
                (DiffLineKind::Context, Some(old), Some(new), text)
            } else {
                (DiffLineKind::Metadata, None, None, raw)
            };
            current.lines.push(DiffLine {
                kind,
                old_line: old,
                new_line: new,
                text: text.to_owned(),
            });
        }
        if let Some(current) = file {
            push_file(&mut document, current);
        }
        (!document.files.is_empty()).then_some(document)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

fn clean_path(value: &str) -> String {
    value
        .split_once('\t')
        .map(|(path, _)| path)
        .unwrap_or(value)
        .trim()
        .to_owned()
}

fn push_file(document: &mut DiffDocument, file: DiffFile) {
    document.additions += file.additions;
    document.deletions += file.deletions;
    document.files.push(file);
}

fn parse_hunk_header(value: &str) -> Option<(u32, u32)> {
    let mut parts = value.split_whitespace();
    let _marker = parts.next()?;
    let old = range_start(parts.next()?, '-')?;
    let new = range_start(parts.next()?, '+')?;
    Some((old, new))
}

fn range_start(value: &str, prefix: char) -> Option<u32> {
    value.strip_prefix(prefix)?.split(',').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multifile_unified_diff_with_line_numbers_and_stats() {
        let document = DiffDocument::parse(
            "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -2,2 +2,3 @@\n same\n-old\n+new\n+extra\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n",
        )
        .unwrap();
        assert_eq!(document.file_count(), 2);
        assert_eq!(document.additions, 3);
        assert_eq!(document.deletions, 1);
        assert_eq!(document.files[0].display_path(), "src/a.rs");
        let deletion = document.files[0]
            .lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Deletion)
            .unwrap();
        assert_eq!(deletion.old_line, Some(3));
        let addition = document.files[0]
            .lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Addition)
            .unwrap();
        assert_eq!(addition.new_line, Some(3));
    }

    #[test]
    fn rejects_plain_text_as_a_diff() {
        assert!(DiffDocument::parse("$ cargo test").is_none());
    }
}
