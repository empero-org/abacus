//! Modified Enter in Konsole. Konsole sends a carriage return for Ctrl+Enter,
//! the same as for Enter, and ESC O M for Shift+Enter, which the terminal
//! input reader drops: there the composer could not start a new line on
//! either. Konsole lets a program change its tab's profile from inside
//! (OSC 50), key table included, so while the TUI runs it switches its tab to
//! a table of its own (`konsole.keytab`), which spells them as the kitty
//! keyboard protocol does, and back as it exits.
//!
//! It does so only where it knows what to switch back to: a tab whose profile
//! uses Konsole's default table, and not inside tmux or screen, which would
//! not pass the request on.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};

/// The table, as Konsole reads it from its data folder.
const TABLE: &str = include_str!("konsole.keytab");
/// Its name there: the file is `abacus.keytab`.
const NAME: &str = "abacus";
/// Konsole's default table, the one a tab goes back to.
const DEFAULT: &str = "default";
/// The profile Konsole uses when none was made.
const BUILT_IN: &str = "Built-in";

/// Whether this process switched its tab, so it switches back once.
static SWITCHED: AtomicBool = AtomicBool::new(false);

/// Switch the tab Abacus runs in to its key table, where that is safe; says
/// what it did, for the key log.
pub(super) fn switch_keys(out: &mut impl Write) -> String {
    let variable = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    if !in_konsole(variable) {
        return "not in Konsole, or behind tmux or screen".to_owned();
    }
    let asked = tab_profile(variable);
    let profile = asked.clone().or_else(|| default_profile(variable)).unwrap_or_else(|| BUILT_IN.to_owned());
    let source = if asked.is_some() { "over D-Bus" } else { "from konsolerc" };
    let dirs = profile_dirs(variable);
    let bindings = profile_key_bindings(&profile, &dirs);
    if bindings.as_deref() != Some(DEFAULT) {
        return format!("left alone: profile {profile:?} ({source}) uses key table {bindings:?}");
    }
    let Some(data) = dirs.first() else { return "left alone: no data folder".to_owned() };
    if let Err(error) = install(data) {
        return format!("left alone: could not write {}: {error:#}", data.join(format!("{NAME}.keytab")).display());
    }
    if let Err(error) = write!(out, "{}", select(NAME)).and_then(|()| out.flush()) {
        return format!("could not ask for the switch: {error}");
    }
    SWITCHED.store(true, Ordering::SeqCst);
    format!("switched the tab to {NAME} (profile {profile:?}, {source})")
}

/// Give the tab its default table back, if this process switched it.
pub(super) fn switch_back(out: &mut impl Write) {
    if SWITCHED.swap(false, Ordering::SeqCst) {
        let _ = write!(out, "{}", select(DEFAULT));
        let _ = out.flush();
    }
}

/// Konsole's request to change the tab's profile; here, its key table.
fn select(table: &str) -> String {
    format!("\x1b]50;KeyBindings={table}\x07")
}

/// Whether Abacus talks to Konsole directly: tmux and screen sit between and
/// would not pass the request on.
fn in_konsole(variable: impl Fn(&str) -> Option<String>) -> bool {
    variable("KONSOLE_VERSION").is_some()
        && variable("TMUX").is_none()
        && variable("STY").is_none()
        && !variable("TERM").is_some_and(|term| term.starts_with("screen") || term.starts_with("tmux"))
}

/// The profile of the tab, as Konsole says over D-Bus.
fn tab_profile(variable: impl Fn(&str) -> Option<String>) -> Option<String> {
    let service = variable("KONSOLE_DBUS_SERVICE")?;
    let session = variable("KONSOLE_DBUS_SESSION")?;
    let output = Command::new("gdbus")
        .args(["call", "--session", "--timeout", "1", "--dest", &service, "--object-path", &session])
        .args(["--method", "org.kde.konsole.Session.profile"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    reply_text(&String::from_utf8_lossy(&output.stdout))
}

/// The string in a one-string D-Bus reply, as gdbus prints it: `('Built-in',)`.
fn reply_text(reply: &str) -> Option<String> {
    let text = reply.trim().strip_prefix("('")?.strip_suffix("',)")?;
    (!text.contains('\'')).then(|| text.to_owned())
}

/// The profile Konsole opens tabs with, by its file's name; None for its built-in one.
fn default_profile(variable: impl Fn(&str) -> Option<String>) -> Option<String> {
    let config = variable("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| variable("HOME").map(|home| Path::new(&home).join(".config")))?;
    let text = std::fs::read_to_string(config.join("konsolerc")).ok()?;
    let file = ini_value(&text, "Desktop Entry", "DefaultProfile")?;
    Some(file.strip_suffix(".profile").unwrap_or(&file).to_owned())
}

/// Where Konsole keeps profiles and key tables: the user's folder first.
fn profile_dirs(variable: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let user = variable("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| variable("HOME").map(|home| Path::new(&home).join(".local/share")));
    let system = variable("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    user.into_iter()
        .chain(system.split(':').filter(|dir| !dir.is_empty()).map(PathBuf::from))
        .map(|dir| dir.join("konsole"))
        .collect()
}

/// The key table `profile` uses, found by its name or its file's; None when
/// no profile by that name is found.
fn profile_key_bindings(profile: &str, dirs: &[PathBuf]) -> Option<String> {
    if profile == BUILT_IN {
        return Some(DEFAULT.to_owned());
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.extension().is_none_or(|extension| extension != "profile") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let file = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
            if ini_value(&text, "General", "Name").as_deref() == Some(profile) || file == profile {
                return Some(ini_value(&text, "Keyboard", "KeyBindings").unwrap_or_else(|| DEFAULT.to_owned()));
            }
        }
    }
    None
}

/// `key` in `[section]` of an INI file, as Konsole's configuration files are written.
fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut current = "";
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
            current = name;
        } else if current == section
            && let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            return Some(value.trim().to_owned()).filter(|value| !value.is_empty());
        }
    }
    None
}

/// Put the table where Konsole looks for it, unless it is there already.
fn install(dir: &Path) -> anyhow::Result<()> {
    let path = dir.join(format!("{NAME}.keytab"));
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == TABLE) {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    crate::config::atomic_write(&path, TABLE.as_bytes(), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// What Konsole sends for Return with the keys and modes given, read from
    /// the table the way Konsole matches an entry: every modifier or mode the
    /// entry names must be as it says, and the rest may be anything.
    fn return_sends(held: &[&str]) -> Vec<String> {
        TABLE
            .lines()
            .filter_map(|line| line.trim().strip_prefix("key Return "))
            .filter_map(|line| line.split_once(':'))
            .filter(|(conditions, _)| {
                let conditions = conditions.replace('+', " +").replace('-', " -");
                conditions.split_whitespace().all(|condition| {
                    let (want, name) = condition.split_at(1);
                    held.contains(&name) == (want == "+")
                })
            })
            .map(|(_, sends)| sends.trim().to_owned())
            .collect()
    }

    #[test]
    fn exactly_one_enter_entry_answers_every_combination() {
        let all = ["Shift", "Ctrl", "Alt", "AppScreen", "NewLine"];
        for mask in 0..(1 << all.len()) {
            let held: Vec<&str> =
                all.iter().enumerate().filter(|(bit, _)| mask & (1 << bit) != 0).map(|(_, name)| *name).collect();
            assert_eq!(return_sends(&held).len(), 1, "{held:?} matches {:?}", return_sends(&held));
        }
    }

    #[test]
    fn a_full_screen_program_can_tell_modified_enter_apart_and_a_prompt_keeps_its_keys() {
        let full_screen: HashMap<&[&str], &str> = HashMap::from([
            (&[][..], r#""\r""#),
            (&["Ctrl"][..], r#""\E[13;5u""#),
            (&["Shift"][..], r#""\E[13;2u""#),
            (&["Shift", "Ctrl"][..], r#""\E[13;6u""#),
            (&["Ctrl", "Alt"][..], r#""\E[13;7u""#),
            // Konsole puts ESC before this one itself.
            (&["Alt"][..], r#""\r""#),
        ]);
        for (held, sends) in full_screen {
            let held: Vec<&str> = held.iter().copied().chain(["AppScreen"]).collect();
            assert_eq!(return_sends(&held), [sends], "{held:?}");
        }
        assert_eq!(return_sends(&["Ctrl"]), [r#""\r""#], "at a prompt, Ctrl+Enter is Enter, as by default");
        assert_eq!(return_sends(&["Shift"]), [r#""\EOM""#]);
    }

    #[test]
    fn talks_to_konsole_only_directly() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned())
        };
        assert!(in_konsole(env(&[("KONSOLE_VERSION", "250402"), ("TERM", "xterm-256color")])));
        assert!(!in_konsole(env(&[("TERM", "xterm-256color")])));
        assert!(!in_konsole(env(&[("KONSOLE_VERSION", "250402"), ("TMUX", "/tmp/tmux-1000/default,1,0")])));
        assert!(!in_konsole(env(&[("KONSOLE_VERSION", "250402"), ("TERM", "screen-256color")])));
    }

    #[test]
    fn reads_the_profile_from_the_d_bus_reply() {
        assert_eq!(reply_text("('Built-in',)\n").as_deref(), Some("Built-in"));
        assert_eq!(reply_text("('Dark work',)").as_deref(), Some("Dark work"));
        assert_eq!(reply_text("Error: no such object").as_deref(), None);
    }

    #[test]
    fn switches_only_a_profile_on_the_default_table() {
        let dir = tempfile::tempdir().unwrap();
        let konsole = dir.path().join("konsole");
        std::fs::create_dir(&konsole).unwrap();
        std::fs::write(konsole.join("Plain.profile"), "[General]\nName=Plain\nParent=FALLBACK/\n").unwrap();
        std::fs::write(konsole.join("Custom.profile"), "[General]\nName=Mine\n\n[Keyboard]\nKeyBindings=vim-ish\n")
            .unwrap();
        let dirs = [konsole];
        assert_eq!(profile_key_bindings(BUILT_IN, &dirs).as_deref(), Some(DEFAULT));
        assert_eq!(profile_key_bindings("Plain", &dirs).as_deref(), Some(DEFAULT));
        assert_eq!(profile_key_bindings("Mine", &dirs).as_deref(), Some("vim-ish"));
        assert_eq!(profile_key_bindings("Custom", &dirs).as_deref(), Some("vim-ish"), "by the file's name too");
        assert_eq!(profile_key_bindings("Gone", &dirs), None, "an unknown profile is left alone");
    }

    #[test]
    fn installs_the_table_once_and_asks_for_it_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let konsole = dir.path().join("konsole");
        install(&konsole).unwrap();
        assert_eq!(std::fs::read_to_string(konsole.join("abacus.keytab")).unwrap(), TABLE);
        install(&konsole).unwrap();
        assert_eq!(select(NAME), "\x1b]50;KeyBindings=abacus\x07");
        assert_eq!(select(DEFAULT), "\x1b]50;KeyBindings=default\x07");
    }
}
