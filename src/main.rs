// restic-backup: minimal orchestrator for scheduled restic backups on macOS via launchd.
//
// Suggested Cargo.toml dependencies:
//   [dependencies]
//   directories = "5"
//   fs4 = { version = "0.9", features = ["sync"] }
//   clap = { version = "4", features = ["derive"] }
//
// Everything else (logging, timestamps, HTTP notifications, randomness, hostname
// lookup) is done with the standard library plus the `restic`, `curl`, `hostname`
// and `date`/`launchctl` binaries already present on macOS, to keep the
// dependency tree small. `clap` is the one addition over a hand-rolled parser:
// it gives free `--help`/`--version` output and consistent error messages.

use clap::{Parser, Subcommand};
use directories::{BaseDirs, ProjectDirs};
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(
    name = "restic-backup",
    version,
    about = "Scheduled restic backups for macOS + launchd",
    long_about = "\
ENVIRONMENT:
    BACKUP_PATH             Path to back up (default: $HOME)
    RESTIC_BACKUP_HOSTNAME  Override the detected hostname / tag
    RESTIC_KEEP_HOURLY      default 24
    RESTIC_KEEP_DAILY       default 14
    RESTIC_KEEP_WEEKLY      default 4
    RESTIC_KEEP_MONTHLY     default 12
    RESTIC_KEEP_YEARLY      default 10
    RESTIC_CHECK_PERCENT    default 10
    NTFY_SERVER             default https://ntfy.sh
    NTFY_TOPIC              secret; generated and stored if unset

Secrets (RESTIC_REPOSITORY, RESTIC_PASSWORD, AWS_ACCESS_KEY_ID,
AWS_SECRET_ACCESS_KEY, NTFY_TOPIC) live in a 0600 file under the app's local
data directory (see `install` output for the exact path)."
)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Run `restic backup` against $HOME (or $BACKUP_PATH), tagged with the
    /// hostname, excluding caches. Intended to run hourly.
    Backup,
    /// Run `restic check --read-data-subset` over a percentage of the
    /// repository ($RESTIC_CHECK_PERCENT, default 10). Intended weekly.
    Check,
    /// Apply the retention policy (keep-hourly/daily/weekly/monthly/yearly,
    /// overridable via env) to snapshots tagged with the hostname. Intended daily.
    Forget,
    /// Run `restic prune` to reclaim space from forgotten snapshots.
    /// Intended daily, after forget.
    Prune,
    /// Generate and load launchd agents for the four subcommands above.
    Install,
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug)]
struct AppError(String);

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AppError {}

impl From<io::Error> for AppError {
    fn from(e: io::Error) -> Self {
        AppError(e.to_string())
    }
}

impl From<String> for AppError {
    fn from(s: String) -> Self {
        AppError(s)
    }
}

impl From<&str> for AppError {
    fn from(s: &str) -> Self {
        AppError(s.to_string())
    }
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

struct Logger {
    file: Mutex<File>,
    name: String,
}

impl Logger {
    fn new(log_dir: &Path, name: &str) -> Result<Self> {
        fs::create_dir_all(log_dir)?;
        let path = log_dir.join(format!("{name}.log"));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Logger {
            file: Mutex::new(file),
            name: name.to_string(),
        })
    }

    fn log(&self, level: &str, msg: &str) {
        let line = format!("[{}] [{level}] [{}] {msg}\n", timestamp(), self.name);
        eprint!("{line}");
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn info(&self, msg: &str) {
        self.log("INFO", msg);
    }

    fn warn(&self, msg: &str) {
        self.log("WARN", msg);
    }

    fn error(&self, msg: &str) {
        self.log("ERROR", msg);
    }
}

/// Human readable local timestamp. Shells out to `date` instead of pulling in
/// a datetime crate; this runs a handful of times per invocation so the extra
/// process spawn is irrelevant for a job that runs hourly at most.
fn timestamp() -> String {
    Command::new("date")
        .arg("+%Y-%m-%dT%H:%M:%S%z")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("epoch:{secs}")
        })
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

struct Config {
    home_dir: PathBuf,
    username: String,
    secrets_path: PathBuf,
    state_dir: PathBuf,
    log_dir: PathBuf,
    cache_dir: PathBuf,
    backup_source: PathBuf,
    hostname: String,
    ntfy_server: String,
    keep_hourly: String,
    keep_daily: String,
    keep_weekly: String,
    keep_monthly: String,
    keep_yearly: String,
    check_percent: String,
}

impl Config {
    fn load() -> Result<Self> {
        let base_dirs =
            BaseDirs::new().ok_or_else(|| AppError("cannot determine home directory".into()))?;
        let home_dir = base_dirs.home_dir().to_path_buf();
        let username = env::var("USER").unwrap_or_else(|_| {
            home_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "user".to_string())
        });

        let project_dirs = ProjectDirs::from("dev", "personal", "restic-backup")
            .ok_or_else(|| AppError("cannot determine application directories".into()))?;
        let data_dir = project_dirs.data_local_dir().to_path_buf();
        let state_dir = data_dir.join("state");
        let log_dir = data_dir.join("logs");
        let cache_dir = project_dirs.cache_dir().to_path_buf();
        let secrets_dir = data_dir.join("secrets");
        let secrets_path = secrets_dir.join("env");

        for dir in [&state_dir, &log_dir, &cache_dir] {
            fs::create_dir_all(dir)?;
        }
        ensure_dir_private(&secrets_dir)?;

        let hostname = get_hostname()?;
        let backup_source = env::var("BACKUP_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir.clone());
        let ntfy_server = env::var("NTFY_SERVER").unwrap_or_else(|_| "https://ntfy.sh".to_string());

        let keep_hourly = env::var("RESTIC_KEEP_HOURLY").unwrap_or_else(|_| "24".into());
        let keep_daily = env::var("RESTIC_KEEP_DAILY").unwrap_or_else(|_| "14".into());
        let keep_weekly = env::var("RESTIC_KEEP_WEEKLY").unwrap_or_else(|_| "4".into());
        let keep_monthly = env::var("RESTIC_KEEP_MONTHLY").unwrap_or_else(|_| "12".into());
        let keep_yearly = env::var("RESTIC_KEEP_YEARLY").unwrap_or_else(|_| "10".into());
        let check_percent = env::var("RESTIC_CHECK_PERCENT").unwrap_or_else(|_| "10".into());

        Ok(Config {
            home_dir,
            username,
            secrets_path,
            state_dir,
            log_dir,
            cache_dir,
            backup_source,
            hostname,
            ntfy_server,
            keep_hourly,
            keep_daily,
            keep_weekly,
            keep_monthly,
            keep_yearly,
            check_percent,
        })
    }
}

fn ensure_dir_private(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Short hostname, used both as the restic snapshot tag and in the launchd
/// job labels. Can be overridden explicitly since not everyone trusts the
/// local hostname (e.g. DHCP-assigned names).
fn get_hostname() -> Result<String> {
    if let Ok(h) = env::var("RESTIC_BACKUP_HOSTNAME")
        && !h.trim().is_empty()
    {
        return Ok(h);
    }
    let output = Command::new("hostname")
        .arg("-s")
        .output()
        .map_err(|e| AppError(format!("failed to run hostname(1): {e}")))?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        return Err(AppError("hostname(1) returned empty output".into()));
    }
    Ok(name)
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------
//
// A single `KEY=VALUE` file, mode 0600 in a mode 0700 directory, holds every
// sensitive value: the restic repository password, the S3 credentials, and
// the generated ntfy.sh topic. Nothing sensitive ever goes into the launchd
// plists; the plists only invoke this binary, which reads secrets from disk
// at run time.

fn load_secrets(path: &Path) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    if !path.exists() {
        return Ok(map);
    }
    let content = fs::read_to_string(path)?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    Ok(map)
}

fn save_secrets(path: &Path, map: &HashMap<String, String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir_private(parent)?;
    }
    let mut contents =
        String::from("# restic-backup secrets - keep this file private (mode 0600)\n");
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for k in keys {
        contents.push_str(&format!("{k}={}\n", map[k]));
    }
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())?;
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn ensure_secrets_scaffold(config: &Config) -> Result<()> {
    if config.secrets_path.exists() {
        return Ok(());
    }
    let mut map = HashMap::new();
    for key in [
        "RESTIC_REPOSITORY",
        "RESTIC_PASSWORD",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
    ] {
        map.insert(key.to_string(), String::new());
    }
    save_secrets(&config.secrets_path, &map)
}

fn generate_topic() -> Result<String> {
    let mut buf = [0u8; 24];
    let mut f = File::open("/dev/urandom")?;
    f.read_exact(&mut buf)?;
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("restic-backup-{hex}"))
}

/// Resolves the ntfy.sh topic: explicit env var, then the secrets file, and
/// finally a freshly generated random topic that gets persisted so future
/// runs (and the user, via the secrets file) see the same value.
fn get_ntfy_topic(config: &Config) -> Result<String> {
    if let Ok(t) = env::var("NTFY_TOPIC")
        && !t.trim().is_empty()
    {
        return Ok(t);
    }
    let mut secrets = load_secrets(&config.secrets_path)?;
    if let Some(t) = secrets.get("NTFY_TOPIC")
        && !t.trim().is_empty()
    {
        return Ok(t.clone());
    }
    let topic = generate_topic()?;
    secrets.insert("NTFY_TOPIC".to_string(), topic.clone());
    save_secrets(&config.secrets_path, &secrets)?;
    Ok(topic)
}

/// Builds the exact environment restic is invoked with. Only known,
/// intentionally allow-listed keys are forwarded; the child process does not
/// inherit this process's full environment (see `run_restic`).
fn build_restic_env(config: &Config) -> Result<HashMap<String, String>> {
    let secrets = load_secrets(&config.secrets_path)?;
    let mut env_vars = HashMap::new();
    for key in [
        "RESTIC_REPOSITORY",
        "RESTIC_PASSWORD",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_DEFAULT_REGION",
        "AWS_SESSION_TOKEN",
    ] {
        if let Some(v) = secrets.get(key)
            && !v.trim().is_empty()
        {
            env_vars.insert(key.to_string(), v.clone());
        }
    }
    for required in ["RESTIC_REPOSITORY", "RESTIC_PASSWORD"] {
        if !env_vars.contains_key(required) {
            return Err(AppError(format!(
                "missing {required} in secrets file {}; edit it and fill in the required values",
                config.secrets_path.display()
            )));
        }
    }
    env_vars.insert(
        "RESTIC_CACHE_DIR".to_string(),
        config.cache_dir.to_string_lossy().to_string(),
    );
    env_vars.insert(
        "PATH".to_string(),
        env::var("PATH")
            .unwrap_or_else(|_| "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin".to_string()),
    );
    Ok(env_vars)
}

// ---------------------------------------------------------------------------
// Single-instance lock
// ---------------------------------------------------------------------------

/// One process-wide lock file shared by all four operational subcommands.
/// restic itself locks the repository per-operation, but that does not stop
/// two of *our* jobs (e.g. an hourly backup that overran, plus the nightly
/// forget) from racing on this host; the flock closes that gap.
fn acquire_lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(e) => Err(AppError(format!("lock error on {}: {e}", path.display()))),
    }
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

/// Best-effort failure notification via ntfy.sh, using curl(1) rather than an
/// HTTP client crate. Never itself returns an error to the caller: a failed
/// notification must not crash a program whose job is to report failures.
fn notify(config: &Config, logger: &Logger, subcommand: &str, message: &str) {
    let topic = match get_ntfy_topic(config) {
        Ok(t) => t,
        Err(e) => {
            logger.warn(&format!(
                "cannot resolve ntfy topic, skipping notification: {e}"
            ));
            return;
        }
    };
    let url = format!("{}/{}", config.ntfy_server.trim_end_matches('/'), topic);
    let title = format!("restic-backup: {subcommand} failed on {}", config.hostname);

    let result = Command::new("curl")
        .args(["-fsS", "--max-time", "10"])
        .args(["-H", &format!("Title: {title}")])
        .args(["-H", "Priority: high"])
        .args(["-H", "Tags: warning"])
        .args(["-d", message])
        .arg(&url)
        .output();

    match result {
        Ok(o) if o.status.success() => logger.info("failure notification sent"),
        Ok(o) => logger.warn(&format!(
            "ntfy notification failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => logger.warn(&format!("failed to invoke curl for notification: {e}")),
    }
}

// ---------------------------------------------------------------------------
// restic invocation
// ---------------------------------------------------------------------------

fn run_restic(args: &[&str], env_vars: &HashMap<String, String>, logger: &Logger) -> Result<()> {
    logger.info(&format!("executing: restic {}", args.join(" ")));
    let output = Command::new("restic")
        .args(args)
        .env_clear()
        .envs(env_vars)
        .output()
        .map_err(|e| {
            AppError(format!(
                "failed to spawn restic (is it installed and on PATH?): {e}"
            ))
        })?;

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        logger.info(&format!("restic: {line}"));
    }
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        logger.warn(&format!("restic: {line}"));
    }

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        let stderr_text = String::from_utf8_lossy(&output.stderr).to_string();
        let tail: Vec<&str> = stderr_text
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(AppError(format!(
            "restic exited with status {code}: {}",
            tail.join(" | ")
        )));
    }
    Ok(())
}

fn cmd_backup(logger: &Logger, config: &Config) -> Result<()> {
    let env_vars = build_restic_env(config)?;
    let source = config.backup_source.to_string_lossy().to_string();
    let cache_excl = config
        .home_dir
        .join("Library")
        .join("Caches")
        .to_string_lossy()
        .to_string();
    let args = [
        "backup",
        &source,
        "--tag",
        &config.hostname,
        "--exclude-caches",
        "--exclude",
        &cache_excl,
    ];
    run_restic(&args, &env_vars, logger)
}

fn cmd_check(logger: &Logger, config: &Config) -> Result<()> {
    let env_vars = build_restic_env(config)?;
    let subset = format!("{}%", config.check_percent.trim_end_matches('%'));
    let args = ["check", "--read-data-subset", &subset];
    run_restic(&args, &env_vars, logger)
}

fn cmd_forget(logger: &Logger, config: &Config) -> Result<()> {
    let env_vars = build_restic_env(config)?;
    let args = [
        "forget",
        "--tag",
        &config.hostname,
        "--keep-hourly",
        &config.keep_hourly,
        "--keep-daily",
        &config.keep_daily,
        "--keep-weekly",
        &config.keep_weekly,
        "--keep-monthly",
        &config.keep_monthly,
        "--keep-yearly",
        &config.keep_yearly,
    ];
    run_restic(&args, &env_vars, logger)
}

fn cmd_prune(logger: &Logger, config: &Config) -> Result<()> {
    let env_vars = build_restic_env(config)?;
    run_restic(&["prune"], &env_vars, logger)
}

// ---------------------------------------------------------------------------
// launchd unit generation / installation
// ---------------------------------------------------------------------------

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn plist_label(config: &Config, name: &str) -> String {
    format!("com.restic-backup.{}.{name}", sanitize(&config.username))
}

fn schedule_interval(seconds: u64) -> String {
    format!("    <key>StartInterval</key>\n    <integer>{seconds}</integer>")
}

fn schedule_calendar(weekday: Option<u8>, hour: u8, minute: u8) -> String {
    let mut s = String::from("    <key>StartCalendarInterval</key>\n    <dict>\n");
    if let Some(wd) = weekday {
        s.push_str(&format!(
            "        <key>Weekday</key>\n        <integer>{wd}</integer>\n"
        ));
    }
    s.push_str(&format!(
        "        <key>Hour</key>\n        <integer>{hour}</integer>\n        <key>Minute</key>\n        <integer>{minute}</integer>\n    </dict>"
    ));
    s
}

fn plist_document(config: &Config, exe: &Path, name: &str, schedule_xml: &str) -> String {
    let label = plist_label(config, name);
    let out_log = config.log_dir.join(format!("{name}.launchd.out.log"));
    let err_log = config.log_dir.join(format!("{name}.launchd.err.log"));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>{}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
    </dict>
{}
    <key>RunAtLoad</key>
    <false/>
    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
    <key>ProcessType</key>
    <string>Background</string>
    <key>LowPriorityIO</key>
    <true/>
</dict>
</plist>
"#,
        xml_escape(&label),
        xml_escape(&exe.to_string_lossy()),
        xml_escape(name),
        schedule_xml,
        xml_escape(&out_log.to_string_lossy()),
        xml_escape(&err_log.to_string_lossy()),
    )
}

fn cmd_install() -> Result<()> {
    let config = Config::load()?;
    let exe = env::current_exe()
        .map_err(|e| AppError(format!("cannot resolve current executable path: {e}")))?;

    ensure_secrets_scaffold(&config)?;
    let _topic = get_ntfy_topic(&config)?; // materialize NTFY_TOPIC now if not already set

    let agents_dir = config.home_dir.join("Library").join("LaunchAgents");
    fs::create_dir_all(&agents_dir)?;

    // backup: hourly
    // check: weekly, Sunday 03:30 (reads back only a data subset, so it's cheap)
    // forget: daily, 02:00
    // prune: daily, 02:45 -- after forget; see the note printed below
    let jobs: [(&str, String); 4] = [
        (
            "backup",
            plist_document(&config, &exe, "backup", &schedule_interval(3600)),
        ),
        (
            "check",
            plist_document(&config, &exe, "check", &schedule_calendar(Some(0), 3, 30)),
        ),
        (
            "forget",
            plist_document(&config, &exe, "forget", &schedule_calendar(None, 2, 0)),
        ),
        (
            "prune",
            plist_document(&config, &exe, "prune", &schedule_calendar(None, 2, 45)),
        ),
    ];

    for (name, xml) in &jobs {
        let label = plist_label(&config, name);
        let path = agents_dir.join(format!("{label}.plist"));
        fs::write(&path, xml)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        println!("wrote {}", path.display());

        let _ = Command::new("launchctl").arg("unload").arg(&path).output();
        let load = Command::new("launchctl")
            .arg("load")
            .arg("-w")
            .arg(&path)
            .output()
            .map_err(|e| AppError(format!("failed to invoke launchctl: {e}")))?;
        if !load.status.success() {
            return Err(AppError(format!(
                "launchctl load failed for {label}: {}",
                String::from_utf8_lossy(&load.stderr)
            )));
        }
        println!("loaded {label}");
    }

    println!();
    println!("Secrets file: {}", config.secrets_path.display());
    println!("Fill in RESTIC_REPOSITORY, RESTIC_PASSWORD, AWS_ACCESS_KEY_ID and");
    println!("AWS_SECRET_ACCESS_KEY there before the next scheduled run.");
    println!();
    println!(
        "Note: prune is scheduled 45 minutes after forget, not chained to it. If forget \
         ever runs long, consider merging forget+prune into one job (`restic forget --prune`) \
         or having the forget subcommand invoke prune itself on success."
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn print_usage() {
    eprintln!(
        r#"restic-backup - scheduled restic backups for macOS + launchd

USAGE:
    restic-backup <SUBCOMMAND>

SUBCOMMANDS:
    backup    Run `restic backup` against $HOME (or $BACKUP_PATH), tagged with
              the hostname, excluding caches. Intended to run hourly.
    check     Run `restic check --read-data-subset` over a percentage of the
              repository ($RESTIC_CHECK_PERCENT, default 10). Intended weekly.
    forget    Apply the retention policy (keep-hourly/daily/weekly/monthly/
              yearly, overridable via env) to snapshots tagged with the
              hostname. Intended daily.
    prune     Run `restic prune` to reclaim space from forgotten snapshots.
              Intended daily, after forget.
    install   Generate and load launchd agents for the four subcommands above.

ENVIRONMENT:
    BACKUP_PATH             Path to back up (default: $HOME)
    RESTIC_BACKUP_HOSTNAME  Override the detected hostname / tag
    RESTIC_KEEP_HOURLY      default 24
    RESTIC_KEEP_DAILY       default 14
    RESTIC_KEEP_WEEKLY      default 4
    RESTIC_KEEP_MONTHLY     default 12
    RESTIC_KEEP_YEARLY      default 10
    RESTIC_CHECK_PERCENT    default 10
    NTFY_SERVER             default https://ntfy.sh
    NTFY_TOPIC              secret; generated and stored if unset

Secrets (RESTIC_REPOSITORY, RESTIC_PASSWORD, AWS_ACCESS_KEY_ID,
AWS_SECRET_ACCESS_KEY, NTFY_TOPIC) live in a 0600 file under the app's local
data directory (see `install` output for the exact path)."#
    );
}

fn guarded_run(name: &str, f: fn(&Logger, &Config) -> Result<()>) -> i32 {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e}");
            return 1;
        }
    };
    let logger = match Logger::new(&config.log_dir, name) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot open log file: {e}");
            return 1;
        }
    };

    logger.info(&format!("=== {name} starting ==="));

    let lock_path = config.state_dir.join("resticbackup.lock");
    let _lock = match acquire_lock(&lock_path) {
        Ok(f) => f,
        Err(e) => {
            logger.error(&format!("failed to acquire lock: {e}"));
            notify(
                &config,
                &logger,
                name,
                &format!("failed to acquire lock: {e}"),
            );
            return 1;
        }
    };

    match f(&logger, &config) {
        Ok(()) => {
            logger.info(&format!("=== {name} completed successfully ==="));
            0
        }
        Err(e) => {
            logger.error(&format!("=== {name} failed: {e} ==="));
            notify(&config, &logger, name, &e.to_string());
            1
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("backup") => guarded_run("backup", cmd_backup),
        Some("check") => guarded_run("check", cmd_check),
        Some("forget") => guarded_run("forget", cmd_forget),
        Some("prune") => guarded_run("prune", cmd_prune),
        Some("install") => match cmd_install() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("install failed: {e}");
                1
            }
        },
        Some("help") | Some("-h") | Some("--help") => {
            print_usage();
            0
        }
        Some(other) => {
            eprintln!("unknown subcommand '{other}'\n");
            print_usage();
            2
        }
        None => {
            print_usage();
            2
        }
    };
    std::process::exit(code);
}
