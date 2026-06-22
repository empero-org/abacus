use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use cron::Schedule;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use tokio::{process::Command, time::sleep};
use uuid::Uuid;

use crate::config::{AbacusPaths, CronCommand, atomic_write};

const STORE_VERSION: u32 = 1;
const MAX_LOG_BYTES: u64 = 1_000_000;
const MAX_CAPTURE_BYTES: usize = 200_000;
const MAX_CONCURRENT_JOBS: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: Uuid,
    pub name: String,
    pub schedule: String,
    pub prompt: String,
    pub workspace: PathBuf,
    pub profile: Option<String>,
    pub always_approve: bool,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub next_run: DateTime<Utc>,
    pub last_started_at: Option<DateTime<Utc>>,
    pub last_completed_at: Option<DateTime<Utc>>,
    pub last_status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobFile {
    version: u32,
    jobs: Vec<CronJob>,
}

impl Default for JobFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            jobs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CronStore {
    directory: PathBuf,
    jobs_file: PathBuf,
    lock_file: PathBuf,
    logs_dir: PathBuf,
}

struct NewCronJob {
    name: String,
    expression: String,
    prompt: String,
    workspace: PathBuf,
    profile: Option<String>,
    always_approve: bool,
    timeout_minutes: u64,
}

impl CronStore {
    pub fn new(paths: &AbacusPaths) -> Self {
        let directory = paths.root.join("cron");
        Self {
            jobs_file: directory.join("jobs.json"),
            lock_file: directory.join("jobs.lock"),
            logs_dir: directory.join("logs"),
            directory,
        }
    }

    fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.logs_dir)?;
        Ok(())
    }

    fn lock(&self) -> Result<FileLock> {
        self.ensure()?;
        FileLock::acquire(&self.lock_file, Duration::from_secs(10))
    }

    fn load_unlocked(&self) -> Result<JobFile> {
        if !self.jobs_file.exists() {
            return Ok(JobFile::default());
        }
        let content = fs::read(&self.jobs_file)?;
        let jobs: JobFile = serde_json::from_slice(&content).context("invalid cron job store")?;
        if jobs.version > STORE_VERSION {
            bail!(
                "cron store version {} is newer than supported",
                jobs.version
            );
        }
        Ok(jobs)
    }

    fn save_unlocked(&self, jobs: &JobFile) -> Result<()> {
        let content = serde_json::to_vec_pretty(jobs)?;
        atomic_write(&self.jobs_file, &content, true)
    }

    pub fn list(&self) -> Result<Vec<CronJob>> {
        let _lock = self.lock()?;
        Ok(self.load_unlocked()?.jobs)
    }

    fn add(&self, request: NewCronJob) -> Result<CronJob> {
        let NewCronJob {
            name,
            expression,
            prompt,
            workspace,
            profile,
            always_approve,
            timeout_minutes,
        } = request;
        if name.trim().is_empty() || name.len() > 100 {
            bail!("job name must contain 1 to 100 characters");
        }
        if prompt.trim().is_empty() || prompt.len() > 100_000 {
            bail!("job prompt must contain 1 to 100000 characters");
        }
        let workspace = workspace
            .canonicalize()
            .with_context(|| format!("invalid workspace: {}", workspace.display()))?;
        let schedule = normalize_schedule(&expression)?;
        let now = Utc::now();
        let next_run = next_after(&schedule, now)?;
        let job = CronJob {
            id: Uuid::new_v4(),
            name,
            schedule,
            prompt,
            workspace,
            profile,
            always_approve,
            timeout_seconds: timeout_minutes.clamp(1, 24 * 60) * 60,
            enabled: true,
            created_at: now,
            next_run,
            last_started_at: None,
            last_completed_at: None,
            last_status: None,
        };
        let _lock = self.lock()?;
        let mut file = self.load_unlocked()?;
        file.jobs.push(job.clone());
        self.save_unlocked(&file)?;
        Ok(job)
    }

    pub fn remove(&self, id: &str) -> Result<CronJob> {
        let _lock = self.lock()?;
        let mut file = self.load_unlocked()?;
        let index = resolve_job(&file.jobs, id)?;
        let job = file.jobs.remove(index);
        self.save_unlocked(&file)?;
        let _ = fs::remove_file(self.log_path(job.id));
        Ok(job)
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<CronJob> {
        let _lock = self.lock()?;
        let mut file = self.load_unlocked()?;
        let index = resolve_job(&file.jobs, id)?;
        file.jobs[index].enabled = enabled;
        if enabled {
            file.jobs[index].next_run = next_after(&file.jobs[index].schedule, Utc::now())?;
        }
        let job = file.jobs[index].clone();
        self.save_unlocked(&file)?;
        Ok(job)
    }

    pub fn get(&self, id: &str) -> Result<CronJob> {
        let jobs = self.list()?;
        Ok(jobs[resolve_job(&jobs, id)?].clone())
    }

    fn claim_due(&self, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
        let _lock = self.lock()?;
        let mut file = self.load_unlocked()?;
        let mut due = Vec::new();
        for job in &mut file.jobs {
            if job.enabled && job.next_run <= now {
                job.last_started_at = Some(now);
                job.last_status = Some("running".into());
                job.next_run = next_after(&job.schedule, now)?;
                due.push(job.clone());
            }
        }
        if !due.is_empty() {
            self.save_unlocked(&file)?;
        }
        Ok(due)
    }

    fn mark_started(&self, id: Uuid, now: DateTime<Utc>) -> Result<()> {
        let _lock = self.lock()?;
        let mut file = self.load_unlocked()?;
        let job = file
            .jobs
            .iter_mut()
            .find(|job| job.id == id)
            .context("job disappeared before execution")?;
        job.last_started_at = Some(now);
        job.last_status = Some("running".into());
        self.save_unlocked(&file)
    }

    fn finish(&self, id: Uuid, status: String, log: &str) -> Result<()> {
        self.append_log(id, log)?;
        let _lock = self.lock()?;
        let mut file = self.load_unlocked()?;
        let job = file
            .jobs
            .iter_mut()
            .find(|job| job.id == id)
            .context("job disappeared while running")?;
        job.last_completed_at = Some(Utc::now());
        job.last_status = Some(status);
        self.save_unlocked(&file)
    }

    fn log_path(&self, id: Uuid) -> PathBuf {
        self.logs_dir.join(format!("{id}.log"))
    }

    fn append_log(&self, id: Uuid, text: &str) -> Result<()> {
        self.ensure()?;
        let path = self.log_path(id);
        if path.metadata().map(|value| value.len()).unwrap_or(0) > MAX_LOG_BYTES {
            let rotated = path.with_extension("log.1");
            let _ = fs::remove_file(&rotated);
            fs::rename(&path, rotated)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(())
    }

    pub fn logs(&self, id: &str, lines: usize) -> Result<String> {
        let job = self.get(id)?;
        let path = self.log_path(job.id);
        if !path.exists() {
            return Ok(String::new());
        }
        let content = fs::read_to_string(path)?;
        let lines = lines.clamp(1, 10_000);
        let values = content.lines().collect::<Vec<_>>();
        Ok(values[values.len().saturating_sub(lines)..].join("\n"))
    }
}

pub async fn handle(
    action: CronCommand,
    paths: &AbacusPaths,
    default_workspace: PathBuf,
) -> Result<()> {
    let store = CronStore::new(paths);
    match action {
        CronCommand::List => {
            let jobs = store.list()?;
            if jobs.is_empty() {
                println!("No scheduled jobs.");
            }
            for job in jobs {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    &job.id.to_string()[..8],
                    if job.enabled { "enabled" } else { "disabled" },
                    job.next_run
                        .with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M:%S %Z"),
                    job.schedule,
                    job.name
                );
            }
        }
        CronCommand::Add {
            name,
            schedule,
            prompt,
            workspace,
            profile,
            always_approve,
            timeout_minutes,
        } => {
            let job = store.add(NewCronJob {
                name,
                expression: schedule,
                prompt,
                workspace: workspace.unwrap_or(default_workspace),
                profile,
                always_approve,
                timeout_minutes,
            })?;
            println!("Added {} ({})", job.name, &job.id.to_string()[..8]);
            println!("Next run: {}", job.next_run.with_timezone(&Local));
        }
        CronCommand::Remove { id } => println!("Removed {}", store.remove(&id)?.name),
        CronCommand::Enable { id } => println!("Enabled {}", store.set_enabled(&id, true)?.name),
        CronCommand::Disable { id } => println!("Disabled {}", store.set_enabled(&id, false)?.name),
        CronCommand::Logs { id, lines } => println!("{}", store.logs(&id, lines)?),
        CronCommand::Run { id } => {
            let job = store.get(&id)?;
            store.mark_started(job.id, Utc::now())?;
            let result = run_job(&job, &store).await?;
            if !result {
                bail!("scheduled job failed");
            }
        }
        CronCommand::Daemon { once, poll_seconds } => {
            run_daemon(&store, once, poll_seconds).await?
        }
        CronCommand::Install => install_service(paths)?,
        CronCommand::Uninstall => uninstall_service(paths)?,
    }
    Ok(())
}

async fn run_daemon(store: &CronStore, once: bool, poll_seconds: u64) -> Result<()> {
    let _guard = DaemonGuard::acquire(&store.directory.join("daemon.lock"))?;
    loop {
        let jobs = store.claim_due(Utc::now())?;
        stream::iter(jobs)
            .map(|job| async move {
                let _ = run_job(&job, store).await;
            })
            .buffer_unordered(MAX_CONCURRENT_JOBS)
            .collect::<Vec<_>>()
            .await;
        if once {
            return Ok(());
        }
        tokio::select! {
            _ = sleep(Duration::from_secs(poll_seconds.clamp(1, 3600))) => {},
            result = tokio::signal::ctrl_c() => {
                result?;
                return Ok(());
            }
        }
    }
}

async fn run_job(job: &CronJob, store: &CronStore) -> Result<bool> {
    let executable = std::env::current_exe().context("could not locate the Abacus executable")?;
    let started = Utc::now();
    let mut command = Command::new(executable);
    command
        .arg(&job.workspace)
        .args([
            "--prompt",
            &job.prompt,
            "--output-format",
            "json",
            "--no-session",
        ])
        .env("ABACUS_CRON_JOB_ID", job.id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(profile) = &job.profile {
        command.args(["--profile", profile]);
    }
    if job.always_approve {
        command.arg("--always-approve");
    }
    let output = match tokio::time::timeout(
        Duration::from_secs(job.timeout_seconds.clamp(60, 24 * 60 * 60)),
        command.output(),
    )
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            let detail = format!("could not start scheduled Abacus run: {error}");
            store.finish(job.id, "failed".into(), &detail)?;
            return Err(error).context("could not start scheduled Abacus run");
        }
        Err(_) => {
            let detail = format!("job timed out after {} seconds", job.timeout_seconds);
            store.finish(job.id, "timed-out".into(), &detail)?;
            bail!(detail);
        }
    };
    let stdout = bounded_text(&output.stdout);
    let stderr = bounded_text(&output.stderr);
    let status = if output.status.success() {
        "ok"
    } else {
        "failed"
    };
    let log = format!(
        "=== {} | {} | {} ===\n{}{}{}",
        started,
        job.name,
        status,
        stdout,
        if !stdout.is_empty() && !stderr.is_empty() {
            "\n"
        } else {
            ""
        },
        stderr
    );
    store.finish(job.id, status.into(), &log)?;
    if output.status.success() {
        println!("{}: completed", job.name);
    } else {
        eprintln!("{}: failed ({})", job.name, output.status);
    }
    Ok(output.status.success())
}

fn bounded_text(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(MAX_CAPTURE_BYTES);
    let mut start = start;
    while start < bytes.len() && std::str::from_utf8(&bytes[start..]).is_err() {
        start += 1;
    }
    let text = String::from_utf8_lossy(&bytes[start..]);
    if start > 0 {
        format!("… output truncated …\n{text}")
    } else {
        text.into_owned()
    }
}

fn default_timeout_seconds() -> u64 {
    2 * 60 * 60
}

fn normalize_schedule(value: &str) -> Result<String> {
    let fields = value.split_whitespace().collect::<Vec<_>>();
    let normalized = match fields.len() {
        5 => format!("0 {value}"),
        6 | 7 => value.to_owned(),
        _ => bail!("cron expressions require 5, 6, or 7 fields"),
    };
    Schedule::from_str(&normalized).context("invalid cron expression")?;
    Ok(normalized)
}

fn next_after(expression: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let schedule = Schedule::from_str(expression)?;
    schedule
        .after(&after.with_timezone(&Local))
        .next()
        .map(|value| value.with_timezone(&Utc))
        .context("cron schedule has no future occurrence")
}

fn resolve_job(jobs: &[CronJob], prefix: &str) -> Result<usize> {
    let matches = jobs
        .iter()
        .enumerate()
        .filter(|(_, job)| job.id.to_string().starts_with(prefix))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [index] => Ok(*index),
        [] => bail!("no cron job matches {prefix}"),
        _ => bail!("cron job prefix {prefix} is ambiguous"),
    }
}

struct FileLock {
    path: PathBuf,
}

impl FileLock {
    fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self {
                        path: path.to_owned(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !lock_owner_alive(path) {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        bail!("timed out waiting for cron store lock");
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct DaemonGuard {
    path: PathBuf,
}

impl DaemonGuard {
    fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        for _ in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self {
                        path: path.to_owned(),
                    });
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && !lock_owner_alive(path) =>
                {
                    let _ = fs::remove_file(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    bail!("the Abacus cron daemon is already running")
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("could not acquire the cron daemon lock")
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_owner_alive(path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = content.trim().parse::<u32>() else {
        return false;
    };
    process_alive(pid)
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    unsafe { kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(target_os = "macos")]
fn install_service(paths: &AbacusPaths) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unavailable")?;
    let directory = home.join("Library/LaunchAgents");
    fs::create_dir_all(&directory)?;
    let path = directory.join("com.abacus.agent.plist");
    let executable = xml_escape(&std::env::current_exe()?.to_string_lossy());
    let abacus_home = xml_escape(&paths.root.to_string_lossy());
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.abacus.agent</string>
<key>ProgramArguments</key><array><string>{executable}</string><string>cron</string><string>daemon</string></array>
<key>EnvironmentVariables</key><dict><key>ABACUS_HOME</key><string>{abacus_home}</string></dict>
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
</dict></plist>
"#
    );
    atomic_write(&path, content.as_bytes(), false)?;
    let domain = format!("gui/{}", unsafe { libc_getuid() });
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &domain, path.to_str().unwrap_or_default()])
        .status();
    let status = std::process::Command::new("launchctl")
        .args([
            "bootstrap",
            &domain,
            path.to_str().context("service path is not UTF-8")?,
        ])
        .status()?;
    if !status.success() {
        bail!("launchctl bootstrap failed");
    }
    println!("Installed and started {}", path.display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_service(_paths: &AbacusPaths) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unavailable")?;
    let path = home.join("Library/LaunchAgents/com.abacus.agent.plist");
    let domain = format!("gui/{}", unsafe { libc_getuid() });
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &domain, path.to_str().unwrap_or_default()])
        .status();
    if path.exists() {
        fs::remove_file(&path)?;
    }
    println!("Removed {}", path.display());
    Ok(())
}

#[cfg(target_os = "macos")]
unsafe fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(target_os = "linux")]
fn install_service(paths: &AbacusPaths) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unavailable")?;
    let directory = home.join(".config/systemd/user");
    fs::create_dir_all(&directory)?;
    let path = directory.join("abacus-agent.service");
    let executable = systemd_escape(&std::env::current_exe()?.to_string_lossy());
    let abacus_home = systemd_escape(&paths.root.to_string_lossy());
    let content = format!(
        "[Unit]\nDescription=Abacus Agent scheduler\n\n[Service]\nExecStart=\"{executable}\" cron daemon\nEnvironment=\"ABACUS_HOME={abacus_home}\"\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n"
    );
    atomic_write(&path, content.as_bytes(), false)?;
    run_service_command("systemctl", &["--user", "daemon-reload"])?;
    run_service_command(
        "systemctl",
        &["--user", "enable", "--now", "abacus-agent.service"],
    )?;
    println!("Installed and started {}", path.display());
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_service(_paths: &AbacusPaths) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unavailable")?;
    let path = home.join(".config/systemd/user/abacus-agent.service");
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "abacus-agent.service"])
        .status();
    if path.exists() {
        fs::remove_file(&path)?;
    }
    run_service_command("systemctl", &["--user", "daemon-reload"])?;
    println!("Removed {}", path.display());
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_service(paths: &AbacusPaths) -> Result<()> {
    let executable = std::env::current_exe()?;
    let command = format!("\\\"{}\\\" cron daemon", executable.display());
    let status = std::process::Command::new("schtasks")
        .args([
            "/Create",
            "/F",
            "/SC",
            "ONLOGON",
            "/TN",
            "Abacus Agent",
            "/TR",
            &command,
        ])
        .env("ABACUS_HOME", &paths.root)
        .status()?;
    if !status.success() {
        bail!("schtasks failed");
    }
    println!("Installed Abacus Agent scheduled task");
    Ok(())
}

#[cfg(target_os = "windows")]
fn uninstall_service(_paths: &AbacusPaths) -> Result<()> {
    let status = std::process::Command::new("schtasks")
        .args(["/Delete", "/F", "/TN", "Abacus Agent"])
        .status()?;
    if !status.success() {
        bail!("schtasks failed");
    }
    println!("Removed Abacus Agent scheduled task");
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn install_service(_paths: &AbacusPaths) -> Result<()> {
    bail!("service installation is unsupported on this platform")
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn uninstall_service(_paths: &AbacusPaths) -> Result<()> {
    bail!("service installation is unsupported on this platform")
}

#[cfg(target_os = "linux")]
fn run_service_command(program: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(program).args(args).status()?;
    if !status.success() {
        bail!("{program} {} failed", args.join(" "));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(target_os = "linux")]
fn systemd_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn accepts_standard_five_field_cron_and_persists_jobs() {
        let directory = tempdir().unwrap();
        let paths = AbacusPaths::under(directory.path().join("home"));
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let store = CronStore::new(&paths);
        let job = store
            .add(NewCronJob {
                name: "daily".into(),
                expression: "0 9 * * *".into(),
                prompt: "Run checks".into(),
                workspace,
                profile: None,
                always_approve: false,
                timeout_minutes: 120,
            })
            .unwrap();
        assert_eq!(job.schedule, "0 0 9 * * *");
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(
            !store
                .set_enabled(&job.id.to_string(), false)
                .unwrap()
                .enabled
        );
        assert_eq!(
            store.remove(&job.id.to_string()[..8]).unwrap().name,
            "daily"
        );
    }

    #[test]
    fn rejects_bad_or_ambiguous_schedules() {
        assert!(normalize_schedule("every minute").is_err());
        assert!(normalize_schedule("* * * * *").is_ok());
    }

    #[test]
    fn recovers_stale_locks_and_claims_due_jobs_once() {
        let directory = tempdir().unwrap();
        let paths = AbacusPaths::under(directory.path().join("home"));
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let store = CronStore::new(&paths);
        store.ensure().unwrap();
        fs::write(&store.lock_file, "2147483647\n").unwrap();
        drop(store.lock().unwrap());

        let job = store
            .add(NewCronJob {
                name: "due".into(),
                expression: "* * * * *".into(),
                prompt: "report".into(),
                workspace,
                profile: None,
                always_approve: false,
                timeout_minutes: 1,
            })
            .unwrap();
        {
            let _lock = store.lock().unwrap();
            let mut file = store.load_unlocked().unwrap();
            file.jobs[0].next_run = Utc::now() - chrono::Duration::seconds(1);
            store.save_unlocked(&file).unwrap();
        }
        let claimed = store.claim_due(Utc::now()).unwrap();
        assert_eq!(claimed[0].id, job.id);
        assert!(store.claim_due(Utc::now()).unwrap().is_empty());
    }
}
