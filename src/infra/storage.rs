//! Storage for uploaded files (attachments, thumbnails, avatars, banners).
//!
//! Keys are relative paths such as `attachments/202610/12_abc.attach`. Stored files are
//! immutable: a changed upload gets a new key, so files can be cached forever and copied between
//! nodes without coordination.
//!
//! Two backends:
//! * `local` (default): files under `RBB_UPLOAD_DIR`. With several web nodes that directory must
//!   be shared (NFS or similar).
//! * `s3`: any S3-compatible object store (`RBB_STORAGE=s3`, `RBB_S3_BUCKET`, optional
//!   `RBB_S3_ENDPOINT`, `RBB_S3_REGION`, `RBB_S3_PREFIX`; credentials from the usual
//!   `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` environment). Nodes then share nothing on disk.

use crate::config::Config;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use object_store::ObjectStoreExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub enum Storage {
    Local(PathBuf),
    Object {
        store: Arc<dyn object_store::ObjectStore>,
        prefix: String,
    },
}

/// A stored file being read.
pub struct Download {
    pub size: u64,
    pub stream: BoxStream<'static, std::io::Result<Bytes>>,
}

/// Reject keys that could escape the storage root.
fn check_key(key: &str) -> anyhow::Result<()> {
    if key.is_empty()
        || key.starts_with('/')
        || key.contains('\\')
        || key
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        anyhow::bail!("invalid storage key {key:?}");
    }
    Ok(())
}

impl Storage {
    pub fn from_config(cfg: &Config) -> anyhow::Result<Storage> {
        match std::env::var("RBB_STORAGE").unwrap_or_default().as_str() {
            "" | "local" => Ok(Storage::Local(PathBuf::from(&cfg.upload_dir))),
            "s3" => {
                let bucket = std::env::var("RBB_S3_BUCKET")
                    .map_err(|_| anyhow::anyhow!("RBB_STORAGE=s3 needs RBB_S3_BUCKET"))?;
                let mut b = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket);
                if let Ok(e) = std::env::var("RBB_S3_ENDPOINT") {
                    b = b.with_allow_http(e.starts_with("http://")).with_endpoint(e);
                }
                if let Ok(r) = std::env::var("RBB_S3_REGION") {
                    b = b.with_region(r);
                }
                let prefix = std::env::var("RBB_S3_PREFIX")
                    .unwrap_or_default()
                    .trim_matches('/')
                    .to_string();
                Ok(Storage::Object {
                    store: Arc::new(b.build()?),
                    prefix,
                })
            }
            other => anyhow::bail!("unknown RBB_STORAGE {other:?} (use local or s3)"),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Storage::Local(_))
    }

    fn object_path(prefix: &str, key: &str) -> object_store::path::Path {
        if prefix.is_empty() {
            object_store::path::Path::from(key)
        } else {
            object_store::path::Path::from(format!("{prefix}/{key}"))
        }
    }

    fn local_path(root: &Path, key: &str) -> anyhow::Result<PathBuf> {
        check_key(key)?;
        Ok(root.join(key))
    }

    /// Store a local file under `key`, consuming it (moved when possible).
    pub async fn put_file(&self, key: &str, file: &Path) -> anyhow::Result<()> {
        check_key(key)?;
        match self {
            Storage::Local(root) => {
                let dest = Self::local_path(root, key)?;
                if let Some(dir) = dest.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                if tokio::fs::rename(file, &dest).await.is_err() {
                    // Different filesystem: copy, then remove the original.
                    tokio::fs::copy(file, &dest).await?;
                    let _ = tokio::fs::remove_file(file).await;
                }
                Ok(())
            }
            Storage::Object { store, prefix } => {
                use tokio::io::AsyncReadExt;
                let upload = store.put_multipart(&Self::object_path(prefix, key)).await?;
                let mut w = object_store::WriteMultipart::new(upload);
                let mut f = tokio::fs::File::open(file).await?;
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    let n = f.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    w.wait_for_capacity(4).await?;
                    w.write(&buf[..n]);
                }
                w.finish().await?;
                let _ = tokio::fs::remove_file(file).await;
                Ok(())
            }
        }
    }

    /// Store small generated content (thumbnails, avatars, banners).
    pub async fn put_bytes(&self, key: &str, data: Bytes) -> anyhow::Result<()> {
        check_key(key)?;
        match self {
            Storage::Local(root) => {
                let dest = Self::local_path(root, key)?;
                if let Some(dir) = dest.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                // Write to a temporary name first so readers never see half a file.
                let tmp = dest.with_extension(format!("tmp{}", crate::util::random_token(6)));
                tokio::fs::write(&tmp, &data).await?;
                tokio::fs::rename(&tmp, &dest).await?;
                Ok(())
            }
            Storage::Object { store, prefix } => {
                store
                    .put(&Self::object_path(prefix, key), data.into())
                    .await?;
                Ok(())
            }
        }
    }

    /// Read a stored file as a stream. `None` if it does not exist.
    pub async fn get(&self, key: &str) -> anyhow::Result<Option<Download>> {
        check_key(key)?;
        match self {
            Storage::Local(root) => {
                let path = Self::local_path(root, key)?;
                let file = match tokio::fs::File::open(&path).await {
                    Ok(f) => f,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e.into()),
                };
                let size = file.metadata().await?.len();
                let stream = futures::stream::unfold(file, |mut f| async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = vec![0u8; 64 * 1024];
                    match f.read(&mut buf).await {
                        Ok(0) => None,
                        Ok(n) => {
                            buf.truncate(n);
                            Some((Ok(Bytes::from(buf)), f))
                        }
                        Err(e) => Some((Err(e), f)),
                    }
                })
                .boxed();
                Ok(Some(Download { size, stream }))
            }
            Storage::Object { store, prefix } => {
                match store.get(&Self::object_path(prefix, key)).await {
                    Ok(r) => {
                        let size = r.meta.size;
                        let stream = r
                            .into_stream()
                            .map(|c| c.map_err(std::io::Error::other))
                            .boxed();
                        Ok(Some(Download { size, stream }))
                    }
                    Err(object_store::Error::NotFound { .. }) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    /// Remove a stored file. Removing one that is already gone succeeds (jobs may repeat).
    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        check_key(key)?;
        match self {
            Storage::Local(root) => {
                match tokio::fs::remove_file(Self::local_path(root, key)?).await {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }
            Storage::Object { store, prefix } => {
                match store.delete(&Self::object_path(prefix, key)).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }
        }
    }
}

/// Remove a stored file (outbox job helper).
pub async fn delete(app: &crate::app::App, key: &str) -> anyhow::Result<()> {
    app.storage.delete(key).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_cannot_escape() {
        for bad in ["", "/etc/passwd", "a/../b", "../x", "a//b", "a\\b", "./a"] {
            assert!(check_key(bad).is_err(), "{bad:?}");
        }
        assert!(check_key("attachments/202610/1_abc.attach").is_ok());
    }

    #[tokio::test]
    async fn local_round_trip() {
        let root =
            std::env::temp_dir().join(format!("rbb-storage-{}", crate::util::random_token(8)));
        let s = Storage::Local(root.clone());
        s.put_bytes("a/b.txt", Bytes::from_static(b"hello"))
            .await
            .unwrap();
        let d = s.get("a/b.txt").await.unwrap().unwrap();
        assert_eq!(d.size, 5);
        let body: Vec<Bytes> = d.stream.map(|c| c.unwrap()).collect().await;
        assert_eq!(body.concat(), b"hello");
        s.delete("a/b.txt").await.unwrap();
        s.delete("a/b.txt").await.unwrap();
        assert!(s.get("a/b.txt").await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
