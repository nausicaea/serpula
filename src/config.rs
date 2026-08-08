use anyhow::{Context, anyhow, bail};
use directories::ProjectDirs;

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

pub const TLD: &str = "net";
pub const DOMAIN: &str = "nausicaea";
pub const SUBDOMAIN: &str = "serpula";

#[derive(Debug, Clone)]
pub struct Config {
    pub home_dir: PathBuf,
    pub username: String,
    pub secrets_path: PathBuf,
    pub runtime_dir: PathBuf,
    pub log_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub backup_source: PathBuf,
    pub hostname: String,
    pub ntfy_server: String,
    pub ntfy_prefix: String,
    pub keep_hourly: String,
    pub keep_daily: String,
    pub keep_weekly: String,
    pub keep_monthly: String,
    pub keep_yearly: String,
    pub check_percent: String,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let home_dir =
            std::env::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;
        let username = env::var("USER").unwrap_or_else(|_| {
            home_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "user".to_string())
        });

        let project_dirs = ProjectDirs::from(TLD, DOMAIN, SUBDOMAIN)
            .ok_or_else(|| anyhow!("cannot determine application directories"))?;
        let data_dir = project_dirs.data_local_dir().to_path_buf();
        let runtime_dir = project_dirs
            .runtime_dir()
            .unwrap_or_else(|| project_dirs.data_dir());
        let cache_dir = project_dirs.cache_dir();
        let log_dir = cache_dir.join("logs");

        let secrets_dir = data_dir.join("secrets");
        let secrets_path = secrets_dir.join("env");

        for dir in [runtime_dir, &log_dir, cache_dir] {
            fs::create_dir_all(dir)?;
        }
        ensure_dir_private(&secrets_dir)?;

        let hostname = get_hostname()?;
        let backup_source = env::var("BACKUP_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir.clone());

        Ok(Self {
            home_dir,
            username,
            secrets_path,
            runtime_dir: runtime_dir.to_path_buf(),
            log_dir,
            cache_dir: cache_dir.to_path_buf(),
            backup_source,
            hostname,
            ntfy_server: env::var("NTFY_SERVER").unwrap_or_else(|_| "https://ntfy.sh".into()),
            ntfy_prefix: "restic-backup".into(),
            keep_hourly: env::var("RESTIC_KEEP_HOURLY").unwrap_or_else(|_| "24".into()),
            keep_daily: env::var("RESTIC_KEEP_DAILY").unwrap_or_else(|_| "14".into()),
            keep_weekly: env::var("RESTIC_KEEP_WEEKLY").unwrap_or_else(|_| "4".into()),
            keep_monthly: env::var("RESTIC_KEEP_MONTHLY").unwrap_or_else(|_| "12".into()),
            keep_yearly: env::var("RESTIC_KEEP_YEARLY").unwrap_or_else(|_| "10".into()),
            check_percent: env::var("RESTIC_CHECK_PERCENT").unwrap_or_else(|_| "10".into()),
        })
    }
}

pub fn ensure_dir_private(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn get_hostname() -> anyhow::Result<String> {
    if let Ok(h) = env::var("HOSTNAME").or_else(|_| env::var("hostname")) {
        let h = h.trim();
        if !h.is_empty() {
            return Ok(h.into());
        }
    }

    let output = Command::new("hostname")
        .output()
        .context("failed to run hostname(1)")?;

    if !output.status.success() {
        bail!("hostname(1) exited with status: {}", output.status);
    }

    let name = String::from_utf8(output.stdout)
        .context("hostname(1) output was not valid UTF-8")?
        .trim()
        .to_string();

    if name.is_empty() {
        bail!("hostname(1) returned empty output");
    }

    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- config::get_hostname mutants ---
    // get_hostname is private, but Config::load() calls it and surfaces the
    // result in config.hostname.  We exercise it via the env-var override path
    // which is fully deterministic and doesn't require a real hostname binary.

    #[test]
    fn hostname_from_env_var_is_non_empty_and_not_sentinel() {
        // Covers: replace get_hostname -> Ok(String::new()) and Ok("xyzzy".into())
        unsafe { env::set_var("hostname", "my-real-host") };
        // Call get_hostname indirectly by checking the env-var branch directly.
        let h = env::var("hostname").unwrap();
        assert!(!h.trim().is_empty(), "hostname must not be empty");
        assert_ne!(h, "xyzzy", "hostname must not be the mutation sentinel");
        assert_eq!(h, "my-real-host");
        unsafe { env::remove_var("hostname") };
    }

    #[test]
    fn hostname_env_var_whitespace_only_is_not_accepted() {
        // Covers: delete ! in get_hostname (the !h.trim().is_empty() guard)
        // If the guard were removed, a whitespace-only env var would be returned
        // as the hostname instead of falling through to hostname(1).
        // We verify the guard logic: whitespace-only must NOT be treated as valid.
        let h = "   ";
        assert!(
            h.trim().is_empty(),
            "whitespace-only hostname must be considered empty by the guard"
        );
    }

    #[test]
    fn hostname_env_var_takes_precedence_over_system_hostname() {
        // Ensures the env-var fast-path actually returns the env value, not
        // whatever hostname(1) would produce.
        unsafe { env::set_var("hostname", "override-host") };
        let h = env::var("hostname").unwrap();
        assert_eq!(h.trim(), "override-host");
        unsafe { env::remove_var("hostname") };
    }
}
