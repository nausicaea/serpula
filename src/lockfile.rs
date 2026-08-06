use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub fn acquire_lock(path: &Path) -> anyhow::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.try_lock()
        .map_err(|e| anyhow::anyhow!("lock error on {}: {e}", path.display()))?;
    Ok(file)
}
