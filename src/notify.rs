use log::{info, warn};

use crate::config::Config;
use crate::secrets::get_ntfy_topic;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Priority {
    /// Really long vibration bursts, default notification sound with a pop-over notification.
    Max,
    /// Long vibration burst, default notification sound with a pop-over notification.
    High,
    /// Short default vibration and sound. Default notification behavior.
    #[default]
    Default,
    /// No vibration or sound. Notification will not visibly show up until notification drawer is pulled down.
    Low,
    /// No vibration or sound. The notification will be under the fold in "Other notifications".
    Min,
}

impl From<Priority> for usize {
    fn from(val: Priority) -> Self {
        match val {
            Priority::Max => 5,
            Priority::High => 4,
            Priority::Default => 3,
            Priority::Low => 2,
            Priority::Min => 1,
        }
    }
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Priority::Max => f.write_str("max"),
            Priority::High => f.write_str("high"),
            Priority::Default => f.write_str("default"),
            Priority::Low => f.write_str("low"),
            Priority::Min => f.write_str("min"),
        }
    }
}

pub fn notify_failure(
    agent: &ureq::Agent,
    config: &Config,
    priority: Priority,
    subcommand: &str,
    message: &str,
) {
    let topic = match get_ntfy_topic(config) {
        Ok(t) => t,
        Err(e) => {
            warn!("cannot resolve ntfy topic, skipping notification: {e}");
            return;
        }
    };

    let url = format!("{}/{}", config.ntfy_server.trim_end_matches('/'), topic);
    let title = format!("restic-backup: {subcommand} failed on {}", config.hostname);

    let result = agent
        .post(url)
        .header("X-Title", title)
        .header("X-Priority", priority.to_string())
        .send(message);

    match result {
        Ok(response) if response.status().is_success() => info!("failure notification sent"),
        Ok(response) => warn!("ntfy notification failed: {response:?}",),
        Err(e) => warn!("failed to invoke curl for notification: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env, fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_tmp() -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("serpula-test-notify-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_config_with_topic(root: &std::path::Path, topic: &str) -> crate::config::Config {
        // Write a secrets file containing the topic so get_ntfy_topic finds it.
        let secrets_path = root.join("secrets").join("env");
        fs::create_dir_all(secrets_path.parent().unwrap()).unwrap();
        fs::write(&secrets_path, format!("NTFY_TOPIC={topic}\n")).unwrap();
        crate::config::Config {
            home_dir: root.to_path_buf(),
            username: "testuser".to_string(),
            secrets_path,
            runtime_dir: root.join("run"),
            log_dir: root.join("logs"),
            cache_dir: root.join("cache"),
            backup_source: root.join("backup"),
            hostname: "testhost".to_string(),
            ntfy_server: "https://ntfy.example.invalid".to_string(),
            ntfy_prefix: "prefix".into(),
            keep_hourly: "24".to_string(),
            keep_daily: "14".to_string(),
            keep_weekly: "4".to_string(),
            keep_monthly: "12".to_string(),
            keep_yearly: "10".to_string(),
            check_percent: "10".to_string(),
        }
    }

    // --- match guard o.status.success() replaced with false (line 639) ---
    // If the guard were always false, even a successful curl would be logged
    // as a warning. We can't easily make curl succeed in a unit test, but we
    // can verify the function doesn't panic and produces some log output.
    #[test]
    fn notify_failure_does_not_panic_with_invalid_topic_server() {
        let tmp = make_tmp();
        unsafe { env::remove_var("NTFY_TOPIC") };
        let config = make_config_with_topic(&tmp, "some-topic");
        let agent = ureq::Agent::new_with_defaults();
        // Must not panic regardless of curl outcome.
        notify_failure(&agent, &config, "xyz", "any message");
    }
}
