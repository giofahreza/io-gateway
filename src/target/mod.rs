pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod copilot;
pub mod deepseek;
pub mod gemini;
pub mod glm;
pub mod grok;
pub mod minimax;
pub mod oauth;
pub mod qwen;

pub(crate) fn atomic_write_json(
    path: &std::path::Path,
    value: &serde_json::Value,
) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(value).map_err(|err| err.to_string())?;
    atomic_write(path, &data, true)
}

pub(crate) fn atomic_write(
    path: &std::path::Path,
    data: &[u8],
    private: bool,
) -> Result<(), String> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("credential");
    let tmp = path.with_file_name(format!(".{}.{}.tmp", file_name, uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|err| err.to_string())?;
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|err| err.to_string())?;
        }
        let _ = private;
        file.write_all(data).map_err(|err| err.to_string())?;
        file.sync_all().map_err(|err| err.to_string())?;
        std::fs::rename(&tmp, path).map_err(|err| err.to_string())?;
        // A synced temporary file alone is not enough: a crash after rename
        // can still lose the directory entry on filesystems that journal data
        // and metadata separately.  Sync the containing directory once the
        // replacement is visible so authorization/config writes are durable.
        //
        // The rename has already made this exact value authoritative, though.
        // Do not report it as an uncommitted write if a platform rejects a
        // directory fsync: callers that then retain old in-memory policy would
        // disagree with the file visible to a fresh process. Warn so an
        // operator can correct the filesystem durability problem instead.
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::File::open(parent).and_then(|directory| directory.sync_all())
            {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "atomic write renamed data but could not fsync its parent directory"
                );
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
