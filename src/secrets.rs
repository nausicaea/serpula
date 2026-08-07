use std::{
    collections::HashMap,
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use crate::{
    config::{Config, ensure_dir_private},
    launchd::sanitize_as_domain_label,
};

const NTFY_TOPIC: &str = "NTFY_TOPIC";

pub fn load_secrets(path: &Path) -> anyhow::Result<HashMap<String, String>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = fs::read_to_string(path)?;
    Ok(parse_secrets_content(&content))
}

pub fn save_secrets(path: &Path, map: &HashMap<String, String>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir_private(parent)?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(serialize_secrets(map).as_bytes())?;
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub fn ensure_secrets_scaffold(config: &Config) -> anyhow::Result<()> {
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

pub fn get_ntfy_topic(config: &Config) -> anyhow::Result<String> {
    if let Ok(t) = env::var(NTFY_TOPIC) {
        let t_trimmed = t.trim();
        if !t_trimmed.is_empty() {
            return Ok(t_trimmed.to_string());
        }
    }

    let mut sec = load_secrets(&config.secrets_path)?;
    if let Some(t) = sec.get(NTFY_TOPIC)
        && !t.trim().is_empty()
    {
        return Ok(t.clone());
    }

    let topic = generate_topic(Some(&config.ntfy_prefix))?;
    sec.insert(NTFY_TOPIC.to_string(), topic.clone());
    save_secrets(&config.secrets_path, &sec)?;
    Ok(topic)
}

pub fn build_restic_env(config: &Config) -> anyhow::Result<HashMap<String, String>> {
    let sec = load_secrets(&config.secrets_path)?;
    let mut env_vars = HashMap::new();

    for key in [
        "RESTIC_REPOSITORY",
        "RESTIC_PASSWORD",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_DEFAULT_REGION",
        "AWS_SESSION_TOKEN",
    ] {
        if let Some(v) = sec.get(key)
            && !v.trim().is_empty()
        {
            env_vars.insert(key.to_string(), v.clone());
        }
    }

    for required in ["RESTIC_REPOSITORY", "RESTIC_PASSWORD"] {
        if !env_vars.contains_key(required) {
            anyhow::bail!(
                "missing {required} in secrets file {}; edit it and fill in the required values",
                config.secrets_path.display()
            );
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

pub fn parse_secrets_content(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

pub fn serialize_secrets(map: &HashMap<String, String>) -> String {
    let mut out = String::from("# restic-backup secrets - keep this file private (mode 0600)\n");
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for k in keys {
        out.push_str(&format!("{k}={}\n", map[k]));
    }
    out
}

fn generate_topic<S: AsRef<str>>(prefix: Option<&S>) -> anyhow::Result<String> {
    let mut buf = [0u8; 24];
    let mut f = File::open("/dev/urandom")?;
    f.read_exact(&mut buf)?;
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(prefix) = prefix {
        let prefix = sanitize_as_domain_label(prefix.as_ref());
        Ok(format!("{prefix}-{hex}"))
    } else {
        Ok(hex.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Create a unique temp directory for a test and return its path.
    /// The caller is responsible for cleanup (or just let the OS reap it).
    fn make_tmp() -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("serpula-test-secrets-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a minimal Config that points into a temp directory so tests
    /// never touch the real home / XDG dirs.
    fn make_config(root: &Path) -> Config {
        Config {
            home_dir: root.to_path_buf(),
            username: "testuser".to_string(),
            secrets_path: root.join("secrets").join("env"),
            runtime_dir: root.join("run"),
            log_dir: root.join("logs"),
            cache_dir: root.join("cache"),
            backup_source: root.join("backup"),
            hostname: "testhost".to_string(),
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
    fn parse_ignores_comments_and_whitespace() {
        let input = r#"
            # comment
            KEY = value
            EMPTY=
            A=B=C
        "#;
        let m = parse_secrets_content(input);
        assert_eq!(m.get("KEY"), Some(&"value".to_string()));
        assert_eq!(m.get("EMPTY"), Some(&"".to_string()));
        assert_eq!(m.get("A"), Some(&"B=C".to_string()));
    }

    #[test]
    fn parse_line_without_equals_is_ignored() {
        let m = parse_secrets_content("NOEQUALSSIGN\n");
        assert!(m.is_empty());
    }

    #[test]
    fn serialize_sorts_keys() {
        let mut m = HashMap::new();
        m.insert("B".to_string(), "2".to_string());
        m.insert("A".to_string(), "1".to_string());

        let s = serialize_secrets(&m);
        let idx_a = s.find("A=1").unwrap();
        let idx_b = s.find("B=2").unwrap();
        assert!(idx_a < idx_b);
    }

    #[test]
    fn serialize_includes_header_comment() {
        let m = HashMap::new();
        let s = serialize_secrets(&m);
        assert!(s.starts_with("# restic-backup secrets"));
    }

    #[test]
    fn load_secrets_returns_empty_for_missing_file() {
        let tmp = make_tmp();
        let path = tmp.join("nonexistent");
        let m = load_secrets(&path).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn save_and_load_secrets_roundtrip() {
        let tmp = make_tmp();
        let path = tmp.join("secrets").join("env");
        let mut m = HashMap::new();
        m.insert("FOO".to_string(), "bar".to_string());
        save_secrets(&path, &m).unwrap();
        let loaded = load_secrets(&path).unwrap();
        assert_eq!(loaded.get("FOO"), Some(&"bar".to_string()));
    }

    #[test]
    fn ensure_secrets_scaffold_creates_file_when_missing() {
        let tmp = make_tmp();
        let config = make_config(&tmp);
        ensure_secrets_scaffold(&config).unwrap();
        assert!(config.secrets_path.exists());
        let content = fs::read_to_string(&config.secrets_path).unwrap();
        assert!(content.contains("RESTIC_REPOSITORY"));
        assert!(content.contains("RESTIC_PASSWORD"));
    }

    #[test]
    fn ensure_secrets_scaffold_is_noop_when_file_exists() {
        let tmp = make_tmp();
        let config = make_config(&tmp);
        // Pre-create the file with custom content.
        fs::create_dir_all(config.secrets_path.parent().unwrap()).unwrap();
        fs::write(&config.secrets_path, "CUSTOM=value\n").unwrap();
        ensure_secrets_scaffold(&config).unwrap();
        // File must not have been overwritten.
        let content = fs::read_to_string(&config.secrets_path).unwrap();
        assert_eq!(content, "CUSTOM=value\n");
    }

    #[test]
    fn get_ntfy_topic_reads_from_secrets_file() {
        let tmp = make_tmp();
        let config = make_config(&tmp);
        fs::create_dir_all(config.secrets_path.parent().unwrap()).unwrap();
        fs::write(&config.secrets_path, "NTFY_TOPIC=my-test-topic\n").unwrap();
        // Make sure the env var is not set so we fall through to the file.
        unsafe { env::remove_var("NTFY_TOPIC") };
        let topic = get_ntfy_topic(&config).unwrap();
        assert_eq!(topic, "my-test-topic");
    }

    #[test]
    fn get_ntfy_topic_generates_and_persists_when_absent() {
        let tmp = make_tmp();
        let config = make_config(&tmp);
        fs::create_dir_all(config.secrets_path.parent().unwrap()).unwrap();
        // Write a secrets file with no NTFY_TOPIC entry.
        fs::write(&config.secrets_path, "").unwrap();
        unsafe { env::remove_var("NTFY_TOPIC") };
        let topic = get_ntfy_topic(&config).unwrap();
        assert!(topic.starts_with("prefix-"));
        // A second call must return the same persisted topic.
        let topic2 = get_ntfy_topic(&config).unwrap();
        assert_eq!(topic, topic2);
    }

    #[test]
    fn build_restic_env_errors_when_required_keys_missing() {
        let tmp = make_tmp();
        let config = make_config(&tmp);
        // Secrets file exists but is empty → required keys absent.
        fs::create_dir_all(config.secrets_path.parent().unwrap()).unwrap();
        fs::write(&config.secrets_path, "").unwrap();
        let err = build_restic_env(&config).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("missing RESTIC_REPOSITORY") || msg.contains("missing RESTIC_PASSWORD")
        );
    }

    #[test]
    fn build_restic_env_happy_path() {
        let tmp = make_tmp();
        let config = make_config(&tmp);
        fs::create_dir_all(config.secrets_path.parent().unwrap()).unwrap();
        fs::write(
            &config.secrets_path,
            "RESTIC_REPOSITORY=s3:bucket\nRESTIC_PASSWORD=secret\nAWS_ACCESS_KEY_ID=AKID\n",
        )
        .unwrap();
        let env_vars = build_restic_env(&config).unwrap();
        assert_eq!(
            env_vars.get("RESTIC_REPOSITORY").map(String::as_str),
            Some("s3:bucket")
        );
        assert_eq!(
            env_vars.get("RESTIC_PASSWORD").map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            env_vars.get("AWS_ACCESS_KEY_ID").map(String::as_str),
            Some("AKID")
        );
        // RESTIC_CACHE_DIR must always be injected.
        assert!(env_vars.contains_key("RESTIC_CACHE_DIR"));
        assert!(env_vars.contains_key("PATH"));
    }

    #[test]
    fn build_restic_env_skips_blank_optional_keys() {
        let tmp = make_tmp();
        let config = make_config(&tmp);
        fs::create_dir_all(config.secrets_path.parent().unwrap()).unwrap();
        fs::write(
            &config.secrets_path,
            "RESTIC_REPOSITORY=s3:bucket\nRESTIC_PASSWORD=secret\nAWS_ACCESS_KEY_ID=\n",
        )
        .unwrap();
        let env_vars = build_restic_env(&config).unwrap();
        // Blank optional key must not appear.
        assert!(!env_vars.contains_key("AWS_ACCESS_KEY_ID"));
    }

    #[test]
    fn build_restic_env_injects_cache_dir_from_config() {
        let tmp = make_tmp();
        let config = make_config(&tmp);
        fs::create_dir_all(config.secrets_path.parent().unwrap()).unwrap();
        fs::write(
            &config.secrets_path,
            "RESTIC_REPOSITORY=s3:bucket\nRESTIC_PASSWORD=secret\n",
        )
        .unwrap();
        let env_vars = build_restic_env(&config).unwrap();
        assert_eq!(
            env_vars.get("RESTIC_CACHE_DIR").map(String::as_str),
            Some(config.cache_dir.to_str().unwrap())
        );
    }

    // --- secrets::get_ntfy_topic: delete ! mutant (line 272) ---
    // If the `!t.trim().is_empty()` guard were removed, a blank NTFY_TOPIC
    // in the secrets file would be returned instead of generating a new one.
    #[test]
    fn get_ntfy_topic_ignores_blank_topic_in_secrets_file() {
        let tmp = make_tmp();
        let config = make_config(&tmp);
        fs::create_dir_all(config.secrets_path.parent().unwrap()).unwrap();
        // NTFY_TOPIC present but blank → must generate a new topic.
        fs::write(&config.secrets_path, "NTFY_TOPIC=\n").unwrap();
        unsafe { env::remove_var("NTFY_TOPIC") };
        let topic = get_ntfy_topic(&config).unwrap();
        assert!(
            topic.starts_with("prefix-"),
            "blank NTFY_TOPIC in file must trigger generation, got: {topic}"
        );
        assert!(!topic.trim().is_empty());
    }

    #[test]
    fn get_ntfy_topic_ignores_whitespace_only_topic_in_secrets_file() {
        let tmp = make_tmp();
        let config = make_config(&tmp);
        fs::create_dir_all(config.secrets_path.parent().unwrap()).unwrap();
        fs::write(&config.secrets_path, "NTFY_TOPIC=   \n").unwrap();
        unsafe { env::remove_var("NTFY_TOPIC") };
        let topic = get_ntfy_topic(&config).unwrap();
        assert!(
            topic.starts_with("prefix-"),
            "whitespace-only NTFY_TOPIC must trigger generation, got: {topic}"
        );
    }

    // --- secrets::parse_secrets_content: replace || with && mutant (line 335) ---
    // With `&&`, a line that is empty but does NOT start with '#' would not be
    // skipped, and a comment line that is not empty would not be skipped either.
    #[test]
    fn parse_skips_comment_lines_regardless_of_emptiness() {
        // A non-empty comment line must be skipped.
        let m = parse_secrets_content("# this is a comment\nKEY=val\n");
        assert!(
            !m.contains_key("# this is a comment"),
            "comment line must be skipped"
        );
        assert_eq!(m.get("KEY"), Some(&"val".to_string()));
    }

    #[test]
    fn parse_skips_empty_lines_regardless_of_comment_marker() {
        // An empty line (no '#') must also be skipped.
        let m = parse_secrets_content("\n\nKEY=val\n\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("KEY"), Some(&"val".to_string()));
    }

    #[test]
    fn parse_comment_only_content_yields_empty_map() {
        // All lines are comments → map must be empty.
        let m = parse_secrets_content("# line1\n# line2\n");
        assert!(m.is_empty(), "only comments should produce an empty map");
    }
}
