//! Storage for uploaded files (attachments, thumbnails, avatars, banners).
//!
//! Paths are relative keys such as `attachments/202610/abc.png`. Stored files are immutable:
//! a changed upload gets a new key.

use crate::app::App;

fn local_path(app: &App, key: &str) -> anyhow::Result<std::path::PathBuf> {
    // Keys come from the database, but never let one escape the upload directory.
    if key.split('/').any(|c| c == ".." || c.is_empty()) || key.starts_with('/') {
        anyhow::bail!("invalid storage key {key:?}");
    }
    Ok(std::path::Path::new(&app.cfg.upload_dir).join(key))
}

/// Remove a stored file. Removing one that is already gone succeeds (jobs may repeat).
pub async fn delete(app: &App, key: &str) -> anyhow::Result<()> {
    match tokio::fs::remove_file(local_path(app, key)?).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
