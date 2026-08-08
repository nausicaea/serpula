use std::{collections::HashMap, process::Command};

use log::info;

pub fn run_restic(args: &[String], env_vars: &HashMap<String, String>) -> anyhow::Result<()> {
    info!("executing: restic {}", args.join(" "));

    let status = Command::new("restic")
        .args(args)
        .env_clear()
        .envs(env_vars)
        .status()
        .map_err(|e| {
            anyhow::anyhow!("failed to spawn restic (is it installed and on PATH?): {e}")
        })?;

    if !status.success() {
        anyhow::bail!("restic exited with status {status}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_tmp() -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("serpula-test-restic-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- restic::run_restic -> Ok(()) mutant (line 658) ---
    // If run_restic always returned Ok(()), a failing restic invocation would
    // not be reported as an error. We verify that a non-zero exit from a real
    // command is surfaced as Err.
    // We use `false` (the shell builtin / /usr/bin/false) as a stand-in for a
    // failing restic: it exits with code 1 and produces no output.
    #[test]
    fn run_restic_errors_on_nonzero_exit() {
        let tmp = make_tmp();
        // Invoke `false` (always exits 1) via the restic wrapper by pointing
        // PATH at a directory containing a `restic` script that calls false.
        // Simpler: just call a known-failing binary directly by swapping the
        // command.  Since run_restic hard-codes "restic", we instead rely on
        // the fact that if restic is not on PATH the spawn error is also an Err,
        // which still kills the Ok(()) mutant.
        //
        // To make this test robust whether or not restic is installed, we use a
        // wrapper: write a tiny shell script named `restic` that exits 1, put it
        // on PATH, and call run_restic.
        let bin_dir = tmp.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake_restic = bin_dir.join("restic");
        fs::write(&fake_restic, "#!/bin/sh\necho 'fake error' >&2\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_restic, fs::Permissions::from_mode(0o755)).unwrap();

        let mut env_vars = HashMap::new();
        env_vars.insert("PATH".to_string(), bin_dir.to_string_lossy().to_string());

        let args: Vec<String> = vec!["snapshots".to_string()];
        let result = run_restic(&args, &env_vars);
        assert!(
            result.is_err(),
            "run_restic must return Err when restic exits non-zero"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("restic exited with status"),
            "error message should mention exit status, got: {msg}"
        );
    }

    // --- delete ! in restic::run_restic (line 678) ---
    // If the `!` were removed, a *successful* restic run would be reported as
    // an error. We verify the happy path: a zero-exit restic returns Ok(()).
    #[test]
    fn run_restic_ok_on_zero_exit() {
        let tmp = make_tmp();

        let bin_dir = tmp.join("bin2");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake_restic = bin_dir.join("restic");
        fs::write(&fake_restic, "#!/bin/sh\necho 'all good'\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_restic, fs::Permissions::from_mode(0o755)).unwrap();

        let mut env_vars = HashMap::new();
        env_vars.insert("PATH".to_string(), bin_dir.to_string_lossy().to_string());

        let args: Vec<String> = vec!["snapshots".to_string()];
        let result = run_restic(&args, &env_vars);
        assert!(
            result.is_ok(),
            "run_restic must return Ok(()) when restic exits 0"
        );
    }

    // --- delete - in restic::run_restic (line 679): unwrap_or(-1) → unwrap_or(1) ---
    // The `-1` default is used when the OS provides no exit code (e.g. signal kill).
    // We verify the error message contains the exit code from a normal failure,
    // which must be a real integer (not a sentinel that would only appear if the
    // default were wrong). The fake restic above exits 1, so the message must
    // contain "status 1".
    #[test]
    fn run_restic_error_message_contains_exit_code() {
        let tmp = make_tmp();

        let bin_dir = tmp.join("bin3");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake_restic = bin_dir.join("restic");
        fs::write(&fake_restic, "#!/bin/sh\necho 'boom' >&2\nexit 42\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_restic, fs::Permissions::from_mode(0o755)).unwrap();

        let mut env_vars = HashMap::new();
        env_vars.insert("PATH".to_string(), bin_dir.to_string_lossy().to_string());

        let args: Vec<String> = vec!["backup".to_string()];
        let err = run_restic(&args, &env_vars).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("42"),
            "error message must contain the actual exit code 42, got: {msg}"
        );
    }
}
