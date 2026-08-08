use log::{error, info};

use crate::config::Config;
use crate::lockfile::acquire_lock;
use crate::notify::notify_failure;

pub fn guarded_run(
    agent: &ureq::Agent,
    name: &str,
    f: fn(&Config, &[String]) -> anyhow::Result<()>,
    args: &[String],
) -> anyhow::Result<()> {
    let config = Config::load()?;

    info!("{name} starting");

    let lock_path = config.runtime_dir.join("resticbackup.lock");
    let _lock = acquire_lock(&lock_path).inspect_err(|e| {
        error!("failed to acquire lock: {e}");
        notify_failure(
            agent,
            &config,
            name,
            &format!("failed to acquire lock: {e}"),
        );
    })?;

    f(&config, args)
        .inspect(|()| info!("{name} completed successfully"))
        .inspect_err(|e| {
            error!("{name} failed: {e}");
            notify_failure(agent, &config, name, &e.to_string());
        })
}

pub fn backup_args(config: &Config) -> Vec<String> {
    let source = config.backup_source.to_string_lossy().to_string();
    let cache_excl = config
        .home_dir
        .join("Library")
        .join("Caches")
        .to_string_lossy()
        .to_string();

    vec![
        "backup".into(),
        format!("--tag={}", config.hostname),
        "--exclude-caches".into(),
        format!("--exclude={cache_excl}"),
        source,
    ]
}

pub fn check_args(config: &Config) -> Vec<String> {
    let subset = normalized_subset_percent(&config.check_percent);
    vec!["check".into(), format!("--read-data-subset={subset}")]
}

pub fn forget_args(config: &Config) -> Vec<String> {
    vec![
        "forget".into(),
        "--prune".into(),
        format!("--tag={}", config.hostname),
        format!("--keep-hourly={}", config.keep_hourly),
        format!("--keep-daily={}", config.keep_daily),
        format!("--keep-weekly={}", config.keep_weekly),
        format!("--keep-monthly={}", config.keep_monthly),
        format!("--keep-yearly={}", config.keep_yearly),
    ]
}

pub fn normalized_subset_percent(raw: &str) -> String {
    format!("{}%", raw.trim().trim_end_matches('%'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dummy_config() -> Config {
        Config {
            home_dir: PathBuf::from("/home/alice"),
            username: "alice".to_string(),
            secrets_path: PathBuf::from("/home/alice/.secrets/env"),
            runtime_dir: PathBuf::from("/home/alice/.run"),
            log_dir: PathBuf::from("/home/alice/.logs"),
            cache_dir: PathBuf::from("/home/alice/.cache"),
            backup_source: PathBuf::from("/home/alice"),
            hostname: "myhost".to_string(),
            ntfy_server: "https://ntfy.sh".to_string(),
            ntfy_prefix: "prefix".into(),
            keep_hourly: "24".to_string(),
            keep_daily: "14".to_string(),
            keep_weekly: "4".to_string(),
            keep_monthly: "12".to_string(),
            keep_yearly: "10".to_string(),
            check_percent: "10".to_string(),
        }
    }

    #[test]
    fn normalizes_percent() {
        assert_eq!(normalized_subset_percent("10"), "10%");
        assert_eq!(normalized_subset_percent("10%"), "10%");
        assert_eq!(normalized_subset_percent(" 25% "), "25%");
    }

    #[test]
    fn normalized_percent_empty_string() {
        // Edge: empty input → just "%"
        assert_eq!(normalized_subset_percent(""), "%");
    }

    #[test]
    fn backup_args_structure() {
        let cfg = dummy_config();
        let args = backup_args(&cfg);
        assert_eq!(args[0], "backup");
        assert_eq!(args[1], "/home/alice");
        assert!(args.contains(&"--tag".to_string()));
        assert!(args.contains(&"myhost".to_string()));
        assert!(args.contains(&"--exclude-caches".to_string()));
        assert!(args.contains(&"--exclude".to_string()));
        // The excluded path must be under home/Library/Caches.
        let excl_idx = args.iter().position(|a| a == "--exclude").unwrap();
        assert!(args[excl_idx + 1].contains("Caches"));
    }

    #[test]
    fn backup_args_uses_backup_source_not_home_when_different() {
        let mut cfg = dummy_config();
        cfg.backup_source = PathBuf::from("/data/important");
        let args = backup_args(&cfg);
        assert_eq!(args[1], "/data/important");
    }

    #[test]
    fn check_args_structure() {
        let cfg = dummy_config();
        let args = check_args(&cfg);
        assert_eq!(args[0], "check");
        assert_eq!(args[1], "--read-data-subset");
        assert_eq!(args[2], "10%");
    }

    #[test]
    fn check_args_normalizes_percent() {
        let mut cfg = dummy_config();
        cfg.check_percent = "20%".to_string();
        let args = check_args(&cfg);
        assert_eq!(args[2], "20%");
    }

    #[test]
    fn forget_args_structure() {
        let cfg = dummy_config();
        let args = forget_args(&cfg);
        assert_eq!(args[0], "forget");
        assert!(args.contains(&"--tag".to_string()));
        assert!(args.contains(&"myhost".to_string()));
        assert!(args.contains(&"--keep-hourly".to_string()));
        assert!(args.contains(&"24".to_string()));
        assert!(args.contains(&"--keep-daily".to_string()));
        assert!(args.contains(&"14".to_string()));
        assert!(args.contains(&"--keep-weekly".to_string()));
        assert!(args.contains(&"4".to_string()));
        assert!(args.contains(&"--keep-monthly".to_string()));
        assert!(args.contains(&"12".to_string()));
        assert!(args.contains(&"--keep-yearly".to_string()));
        assert!(args.contains(&"10".to_string()));
    }

    #[test]
    fn forget_args_uses_config_retention_values() {
        let mut cfg = dummy_config();
        cfg.keep_daily = "30".to_string();
        cfg.keep_yearly = "5".to_string();
        let args = forget_args(&cfg);
        assert!(args.contains(&"30".to_string()));
        assert!(args.contains(&"5".to_string()));
    }
}
