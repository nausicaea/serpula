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
    /// Proxy any restic command
    #[command(external_subcommand)]
    Proxy(Vec<String>),
    /// Generate launchd agents for the four subcommands above.
    Install,
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cli = Cli::parse();
    let agent = ureq::Agent::new_with_defaults();
    match cli.command {
        CliCommand::Proxy(args) => guarded_run(&agent, "proxy", cmd_proxy, &args),
        CliCommand::Install => cmd_install(),
    }
}

fn cmd_proxy(config: &Config, args: &[String]) -> anyhow::Result<()> {
    let args = if let Some(subcommand) = args.first() {
        let default_args = match subcommand.as_str() {
            "backup" => backup_args(config),
            "check" => check_args(config),
            "forget" => forget_args(config),
            _ => vec![],
        };
        if !default_args.is_empty() {
            [&default_args, &args[1..]].concat()
        } else {
            args.to_vec()
        }
    } else {
        args.to_vec()
    };
    let env_vars = build_restic_env(config)?;
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
