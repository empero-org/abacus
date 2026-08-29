//! Keep the in-flight model output somewhere a crash can still reach.
//!
//! A turn's text lives in the transcript and is only written to the session
//! once the turn ends. When the process dies mid-stream — a panic, or the
//! SIGHUP of a closed SSH session — everything the model had produced went
//! with it, which is the worst moment to lose it: a long answer that crashed
//! at the end is exactly the one worth reading.
//!
//! So the stream is mirrored into a global as it arrives, and the same
//! handlers that put the terminal back write that mirror to a file. Nothing
//! here can fail loudly: a recovery path that panics during a panic would
//! replace a useful message with a confusing one.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Cap on what is mirrored, so a runaway stream cannot grow without bound.
/// Generous: the point is to keep long answers, and this is ~250k characters.
const MAX_CHARS: usize = 250_000;

#[derive(Default)]
struct Partial {
    file: Option<PathBuf>,
    session: Option<String>,
    answer: String,
    thinking: String,
}

impl Partial {
    fn is_empty(&self) -> bool {
        self.answer.trim().is_empty() && self.thinking.trim().is_empty()
    }
}

static PARTIAL: OnceLock<Mutex<Partial>> = OnceLock::new();

fn slot() -> &'static Mutex<Partial> {
    PARTIAL.get_or_init(|| Mutex::new(Partial::default()))
}

/// Point recovery at a file. Until this is called, recording is inert — which
/// is what headless and test runs want.
pub fn arm(file: PathBuf, session: Option<String>) {
    if let Ok(mut partial) = slot().lock() {
        partial.file = Some(file);
        partial.session = session;
    }
}

/// Bind the in-flight turn to its session once the session exists. A fresh
/// screen starts without a session (one is created on the first prompt), so
/// the startup arm cannot know the id yet; `start_turn` calls this instead.
pub fn set_session(session: Option<String>) {
    if let Ok(mut partial) = slot().lock() {
        partial.session = session;
    }
}

/// The session a recovery file belongs to, without consuming it. The startup
/// path uses this to resume the interrupted session rather than opening a
/// fresh empty one and dropping the recovered text into that.
pub fn peek_session(file: &Path) -> Option<String> {
    let content = std::fs::read_to_string(file).ok()?;
    let line = content.lines().find(|line| line.starts_with("Session: "))?;
    let id = line
        .strip_prefix("Session: `")?
        .strip_suffix('`')?
        .trim()
        .to_owned();
    (!id.is_empty()).then_some(id)
}

/// Mirror streamed answer text.
pub fn record_answer(delta: &str) {
    append(delta, false);
}

/// Mirror streamed reasoning.
pub fn record_thinking(delta: &str) {
    append(delta, true);
}

fn append(delta: &str, thinking: bool) {
    let Ok(mut partial) = slot().lock() else {
        return;
    };
    if partial.file.is_none() {
        return;
    }
    let target = if thinking {
        &mut partial.thinking
    } else {
        &mut partial.answer
    };
    if target.len() < MAX_CHARS {
        target.push_str(delta);
    }
}

/// The turn finished on its own terms, so there is nothing left to recover.
pub fn clear() {
    if let Ok(mut partial) = slot().lock() {
        partial.answer.clear();
        partial.thinking.clear();
    }
}

/// Write out whatever the model had produced, returning the path when
/// something was written. Called from the panic hook and the signal handlers,
/// so it swallows every error rather than unwinding again.
pub fn flush() -> Option<PathBuf> {
    let mut partial = slot().lock().ok()?;
    let file = partial.file.clone()?;
    if partial.is_empty() {
        return None;
    }
    let document = render(&partial);
    // A plain write, not the atomic helper: during a panic the simplest path
    // that can succeed is the right one.
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&file, document).ok()?;
    partial.answer.clear();
    partial.thinking.clear();
    Some(file)
}

fn render(partial: &Partial) -> String {
    let mut out = String::from("# Recovered from an interrupted turn\n\n");
    out.push_str(&format!(
        "Written {} because Abacus exited mid-stream. This is how far the model\n\
         got before it stopped.\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    if let Some(session) = &partial.session {
        out.push_str(&format!(
            "\nSession: `{session}` — resume with `abacus --resume`.\n"
        ));
    }
    if !partial.thinking.trim().is_empty() {
        out.push_str("\n## Reasoning\n\n");
        out.push_str(partial.thinking.trim());
        out.push('\n');
    }
    if !partial.answer.trim().is_empty() {
        out.push_str("\n## Answer\n\n");
        out.push_str(partial.answer.trim());
        out.push('\n');
    }
    out
}

/// Read back a recovery file left by an earlier run, if one exists, and remove
/// it — it is surfaced once, on the next start, then it is the user's.
pub fn take(file: &Path) -> Option<String> {
    let content = std::fs::read_to_string(file).ok()?;
    let _ = std::fs::remove_file(file);
    (!content.trim().is_empty()).then_some(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mirror is one process-global, and cargo runs these tests on
    /// parallel threads — so they take a lock and hold it for the duration
    /// rather than racing each other through the same slot.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn reset(file: Option<PathBuf>) {
        if let Ok(mut partial) = slot().lock() {
            *partial = Partial {
                file,
                session: Some("s-1".into()),
                ..Partial::default()
            };
        }
    }

    #[test]
    fn a_stream_cut_short_is_written_out_with_both_halves() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recovered.md");
        reset(Some(file.clone()));

        record_thinking("weighing the two approaches");
        record_answer("Here is the fix: change the ");
        record_answer("wrapping budget.");

        let written = flush().expect("something to recover");
        let text = std::fs::read_to_string(&written).unwrap();
        assert!(text.contains("Here is the fix: change the wrapping budget."));
        assert!(text.contains("weighing the two approaches"));
        assert!(text.contains("s-1"), "names the session to resume");
        // Flushed content is not written twice.
        assert!(flush().is_none(), "nothing left after a flush");
    }

    #[test]
    fn a_turn_that_ended_normally_leaves_nothing_to_recover() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recovered.md");
        reset(Some(file.clone()));

        record_answer("a complete answer");
        clear();

        assert!(flush().is_none(), "a finished turn is not a crash");
        assert!(!file.exists());
    }

    #[test]
    fn recording_before_arming_is_inert() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        reset(None);
        record_answer("this has nowhere to go");
        assert!(flush().is_none());
    }

    #[test]
    fn a_recovery_file_is_surfaced_once_then_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recovered.md");
        std::fs::write(&file, "# Recovered\n\npartial text").unwrap();

        assert!(take(&file).unwrap().contains("partial text"));
        assert!(!file.exists(), "surfaced once, not every launch");
        assert!(take(&file).is_none());
    }
}
