//! Receiving uploads without holding them in memory, and processing images within limits.
//!
//! A multipart file field is streamed chunk by chunk into a temporary file under
//! `<upload dir>/tmp`, hashed as it arrives, and cut off as soon as it exceeds the size allowed
//! for it — a 2 GB upload costs at most the limit in disk and a few kilobytes of memory. The
//! temporary file is removed when the [`Spooled`] value is dropped, unless it was handed to
//! storage.
//!
//! Image decoding is CPU- and memory-heavy, so it runs on blocking threads, at most
//! [`image_slots`] at a time, and refuses images whose dimensions, pixel count or decoder
//! allocations exceed fixed limits before decoding them.

use axum::extract::multipart::Field;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// Largest image width or height accepted.
pub const MAX_DIMENSION: u32 = 10_000;
/// Largest number of pixels accepted (40 megapixels).
pub const MAX_PIXELS: u64 = 40_000_000;
/// Largest allocation the image decoder may make.
pub const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;

/// An uploaded file on local disk, removed on drop unless taken.
pub struct Spooled {
    path: Option<PathBuf>,
    pub file_name: String,
    pub size: u64,
    /// Hex SHA-256 of the content.
    pub sha256: String,
}

impl Spooled {
    pub fn path(&self) -> &Path {
        self.path.as_deref().expect("spooled file already taken")
    }

    /// Hand the file over (e.g. to storage); it is no longer removed on drop.
    pub fn take(mut self) -> PathBuf {
        self.path.take().expect("spooled file already taken")
    }
}

impl Drop for Spooled {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[derive(Debug)]
pub enum SpoolError {
    TooLarge { limit: u64 },
    Empty,
    Read(String),
    Io(std::io::Error),
}

impl std::fmt::Display for SpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpoolError::TooLarge { limit } => write!(
                f,
                "The file is too large (at most {}).",
                crate::util::format_bytes(*limit as i64)
            ),
            SpoolError::Empty => write!(f, "The uploaded file is empty."),
            SpoolError::Read(e) => write!(f, "The upload was interrupted ({e})."),
            SpoolError::Io(e) => write!(f, "The upload could not be stored ({e})."),
        }
    }
}

impl From<SpoolError> for crate::error::AppError {
    fn from(e: SpoolError) -> Self {
        match e {
            SpoolError::Io(io) => crate::error::AppError::Other(io.into()),
            other => crate::error::AppError::User(other.to_string()),
        }
    }
}

/// Stream a multipart file field to a temporary file, enforcing `limit` bytes as data arrives.
pub async fn spool(
    upload_dir: &str,
    mut field: Field<'_>,
    limit: u64,
) -> Result<Spooled, SpoolError> {
    let file_name = field
        .file_name()
        .unwrap_or("file")
        .replace(['/', '\\', '\0'], "_")
        .chars()
        .take(120)
        .collect::<String>();
    let dir = Path::new(upload_dir).join("tmp");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(SpoolError::Io)?;
    let path = dir.join(format!("{}.part", crate::util::random_token(24)));
    let mut spooled = Spooled {
        path: Some(path.clone()),
        file_name,
        size: 0,
        sha256: String::new(),
    };
    let mut out = tokio::fs::File::create(&path)
        .await
        .map_err(SpoolError::Io)?;
    let mut hash = Sha256::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| SpoolError::Read(e.body_text()))?
    {
        spooled.size += chunk.len() as u64;
        if spooled.size > limit {
            // `spooled` is dropped here, removing the partial file.
            return Err(SpoolError::TooLarge { limit });
        }
        hash.update(&chunk);
        out.write_all(&chunk).await.map_err(SpoolError::Io)?;
    }
    out.flush().await.map_err(SpoolError::Io)?;
    drop(out);
    if spooled.size == 0 {
        return Err(SpoolError::Empty);
    }
    spooled.sha256 = hex::encode(hash.finalize());
    Ok(spooled)
}

/// Concurrent image decodes allowed (half the cores, at least one).
pub fn image_slots() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(1)
        .max(1)
}

static IMAGES: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(image_slots()));

/// Decode the image in `path` within the limits and run `f` on it, on a blocking thread and
/// holding one of the image slots. Errors are user-facing messages.
pub async fn with_image<T: Send + 'static>(
    path: PathBuf,
    f: impl FnOnce(image::DynamicImage) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let _permit = IMAGES
        .acquire()
        .await
        .map_err(|_| "Image processing is unavailable.".to_string())?;
    let t0 = std::time::Instant::now();
    let r = tokio::task::spawn_blocking(move || {
        let img = decode_file(&path).map_err(|_| {
            "The image appears to be corrupt, too large, or is not a supported format (PNG, JPEG, GIF, WebP)."
                .to_string()
        })?;
        f(img)
    })
    .await
    .map_err(|_| "Image processing failed.".to_string())?;
    crate::infra::metrics::observe("rbb_image_seconds", &[], t0.elapsed().as_secs_f64());
    r
}

fn limited_reader<R: std::io::BufRead + std::io::Seek>(
    reader: R,
) -> image::ImageResult<image::ImageReader<R>> {
    let mut r = image::ImageReader::new(reader).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    r.limits(limits);
    Ok(r)
}

/// Decode an image file, refusing it before decoding if it is too big.
pub fn decode_file(path: &Path) -> image::ImageResult<image::DynamicImage> {
    let open =
        || -> image::ImageResult<_> { Ok(std::io::BufReader::new(std::fs::File::open(path)?)) };
    let (w, h) = limited_reader(open()?)?.into_dimensions()?;
    if w as u64 * h as u64 > MAX_PIXELS {
        return Err(image::ImageError::Limits(
            image::error::LimitError::from_kind(image::error::LimitErrorKind::DimensionError),
        ));
    }
    limited_reader(open()?)?.decode()
}

/// Encode an image as PNG.
pub fn png(img: &image::DynamicImage) -> Result<Vec<u8>, String> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// Remove temporary upload files left behind by a crash (older than an hour).
pub async fn sweep_tmp(upload_dir: &str) -> usize {
    let dir = Path::new(upload_dir).join("tmp");
    let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
        return 0;
    };
    let mut n = 0;
    while let Ok(Some(e)) = rd.next_entry().await {
        let old = e
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 3600);
        if old && tokio::fs::remove_file(e.path()).await.is_ok() {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_images_are_refused_before_decoding() {
        let img = image::RgbImage::new(1, 1);
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let mut bytes = bytes.into_inner();
        // Claim 9000 x 9000 (81 megapixels: under the side limit, over the pixel limit).
        bytes[16..20].copy_from_slice(&9000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&9000u32.to_be_bytes());
        let p = std::env::temp_dir().join(format!("rbb-bomb-{}.png", crate::util::random_token(6)));
        std::fs::write(&p, &bytes).unwrap();
        assert!(decode_file(&p).is_err());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn small_images_decode() {
        let img = image::RgbImage::new(4, 3);
        let p = std::env::temp_dir().join(format!("rbb-ok-{}.png", crate::util::random_token(6)));
        img.save(&p).unwrap();
        let d = decode_file(&p).unwrap();
        assert_eq!((d.width(), d.height()), (4, 3));
        let _ = std::fs::remove_file(p);
    }
}
