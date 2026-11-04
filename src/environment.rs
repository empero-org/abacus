//! Where a session runs, as the model is told when one starts and each time
//! it is resumed: the operating system, the working directory, who is signed
//! in, the locale and the time zone. A resumed session may be on another
//! device, in another place, so this is said again rather than assumed.

use std::{path::Path, sync::OnceLock};

/// How the note describing the environment opens.
pub(crate) const TAG: &str = "<environment>";

/// The `<environment>` block for a session working in `workspace`.
pub fn block(workspace: &Path) -> String {
    format!(
        "{TAG}\n  <os>{}</os>\n  <cwd>{}</cwd>\n  <username>{}</username>\n  <locale>{}</locale>\n  \
         <timezone>{}</timezone>\n</environment>",
        os(),
        workspace.display(),
        username(),
        locale(),
        timezone(),
    )
}

/// The operating system, its release where it says, and the architecture.
/// Read once: it does not change while Abacus runs.
fn os() -> &'static str {
    static OS: OnceLock<String> = OnceLock::new();
    OS.get_or_init(|| {
        let release = release().unwrap_or_else(|| std::env::consts::OS.to_owned());
        format!("{release}, {}", std::env::consts::ARCH)
    })
}

#[cfg(target_os = "linux")]
fn release() -> Option<String> {
    let distribution = ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                .map(|name| name.trim().trim_matches('"').to_owned())
        });
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok().map(|kernel| kernel.trim().to_owned());
    match (distribution, kernel) {
        (Some(distribution), Some(kernel)) => Some(format!("{distribution}, Linux {kernel}")),
        (Some(distribution), None) => Some(distribution),
        (None, Some(kernel)) => Some(format!("Linux {kernel}")),
        (None, None) => None,
    }
}

#[cfg(target_os = "macos")]
fn release() -> Option<String> {
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    let version =
        plist.split("<key>ProductVersion</key>").nth(1)?.split("<string>").nth(1)?.split("</string>").next()?;
    Some(format!("macOS {}", version.trim()))
}

#[cfg(windows)]
fn release() -> Option<String> {
    Some("Windows".to_owned())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn release() -> Option<String> {
    None
}

fn username() -> String {
    ["USER", "USERNAME", "LOGNAME"].into_iter().find_map(variable).unwrap_or_else(|| "unknown".to_owned())
}

fn locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(variable)
        .or_else(platform_locale)
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(windows)]
fn platform_locale() -> Option<String> {
    // LOCALE_NAME_MAX_LENGTH, the terminating null included.
    const MAX: usize = 85;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultLocaleName(name: *mut u16, size: i32) -> i32;
    }
    let mut name = [0u16; MAX];
    // SAFETY: the buffer holds MAX characters and the call is told so; it
    // returns how many it wrote, the null included, or 0 on failure.
    let written = unsafe { GetUserDefaultLocaleName(name.as_mut_ptr(), MAX as i32) };
    (written > 1).then(|| String::from_utf16_lossy(&name[..written as usize - 1]))
}

#[cfg(not(windows))]
fn platform_locale() -> Option<String> {
    None
}

/// The zone's name where the system says (`Europe/Amsterdam`), and the
/// offset from UTC now.
fn timezone() -> String {
    let offset = chrono::Local::now().format("UTC%:z").to_string();
    match zone_name() {
        Some(name) => format!("{name} ({offset})"),
        None => offset,
    }
}

fn zone_name() -> Option<String> {
    let after_zoneinfo = |path: &str| path.split_once("zoneinfo/").map(|(_, name)| name.to_owned());
    // `TZ` names a zone, or a file in the zone database (`:/usr/share/zoneinfo/…`).
    let named = |tz: String| {
        let tz = tz.trim_start_matches(':');
        if tz.starts_with('/') { after_zoneinfo(tz) } else { Some(tz.to_owned()) }
    };
    let not_empty = |name: &String| !name.is_empty();
    variable("TZ")
        .and_then(named)
        .filter(not_empty)
        // Linux and macOS link /etc/localtime into the zone database.
        .or_else(|| {
            std::fs::read_link("/etc/localtime").ok().and_then(|target| after_zoneinfo(&target.to_string_lossy()))
        })
        .or_else(|| std::fs::read_to_string("/etc/timezone").ok().map(|name| name.trim().to_owned()))
        .filter(not_empty)
}

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_where_the_session_runs() {
        let workspace = Path::new("/work/project");
        let block = block(workspace);
        assert!(block.starts_with(TAG) && block.ends_with("</environment>"), "{block}");
        for element in ["os", "cwd", "username", "locale", "timezone"] {
            let open = format!("<{element}>");
            let close = format!("</{element}>");
            let start = block.find(&open).unwrap_or_else(|| panic!("no {open} in {block}")) + open.len();
            let end = block.find(&close).unwrap();
            assert!(end > start, "{element} is empty: {block}");
        }
        assert!(block.contains("<cwd>/work/project</cwd>"), "{block}");
        assert!(block.contains(std::env::consts::ARCH), "{block}");
        assert!(block.contains("UTC"), "the offset is always there: {block}");
    }
}
