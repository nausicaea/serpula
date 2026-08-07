use std::{env, fs, os::unix::fs::PermissionsExt};

use clap::{Parser, Subcommand};
use serpula::{
    app::{backup_args, check_args, forget_args, guarded_run},
    config::Config,
    launchd::{plist_document, plist_label, schedule_calendar, schedule_interval},
    restic::run_restic,
    secrets::{build_restic_env, ensure_secrets_scaffold, get_ntfy_topic},
};

/// Scheduled restic backups for macOS + launchd.
///
/// Secrets (RESTIC_REPOSITORY, RESTIC_PASSWORD, AWS_ACCESS_KEY_ID,
/// AWS_SECRET_ACCESS_KEY, NTFY_TOPIC) live in a 0600 file under the app's
/// local data directory (see `install` output for the exact path).
#[derive(Debug, Parser)]
#[command(name = "restic-backup", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Run `restic backup` against $HOME (or $BACKUP_PATH), tagged with the
    /// hostname, excluding caches. Intended to run hourly.
    Backup,
    /// Run `restic check --read-data-subset` over a percentage of the
    /// repository ($RESTIC_CHECK_PERCENT, default 10). Intended weekly.
    Check,
    /// Apply the retention policy (keep-hourly/daily/weekly/monthly/yearly,
    /// overridable via env) to snapshots tagged with the hostname. Intended daily.
    Forget,
    /// Generate launchd agents for the four subcommands above.
    Install,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let agent = ureq::Agent::new_with_defaults();
    match cli.command {
        CliCommand::Backup => guarded_run(&agent, "backup", cmd_backup),
        CliCommand::Check => guarded_run(&agent, "check", cmd_check),
        CliCommand::Forget => guarded_run(&agent, "forget", cmd_forget),
        CliCommand::Install => cmd_install(),
    }
}

fn cmd_backup(config: &Config) -> anyhow::Result<()> {
    let env_vars = build_restic_env(config)?;
    let args = backup_args(config);
    run_restic(&args, &env_vars)
}

fn cmd_check(config: &Config) -> anyhow::Result<()> {
    let env_vars = build_restic_env(config)?;
    let args = check_args(config);
    run_restic(&args, &env_vars)
}

fn cmd_forget(config: &Config) -> anyhow::Result<()> {
    let env_vars = build_restic_env(config)?;
    let args = forget_args(config);
    run_restic(&args, &env_vars)
}

fn cmd_install() -> anyhow::Result<()> {
    let config = Config::load()?;
    let exe = env::current_exe()
        .map_err(|e| anyhow::anyhow!("cannot resolve current executable path: {e}"))?;

    ensure_secrets_scaffold(&config)?;
    let _ = get_ntfy_topic(&config)?;

    let agents_dir = config.home_dir.join("Library").join("LaunchAgents");
    fs::create_dir_all(&agents_dir)?;

    let jobs: [(&str, String); 3] = [
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
    ];

    for (name, xml) in &jobs {
        let label = plist_label(&config, name);
        let path = agents_dir.join(format!("{label}.plist"));

        fs::write(&path, xml)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        println!("wrote {}", path.display());
    }

    println!();
    println!("Secrets file: {}", config.secrets_path.display());
    println!("Fill in RESTIC_REPOSITORY, RESTIC_PASSWORD, AWS_ACCESS_KEY_ID and");
    println!("AWS_SECRET_ACCESS_KEY there before the next scheduled run.");
    println!();

    Ok(())
}
