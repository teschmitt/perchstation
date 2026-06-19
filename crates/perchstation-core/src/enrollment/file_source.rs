//! File-backed [`QrFrameSource`] for `--qr-source=file`.
//!
//! Loads a single PNG/JPEG from disk, converts it to grayscale, and serves
//! it as the next frame. Used by:
//!
//! - the operator recovery path (`perchstation enroll --qr-source=file
//!   --qr-file enrollment.png`), when the camera is not yet wired up or
//!   the operator photographed the QR with a phone.
//! - integration tests, which render an in-memory QR PNG to a temp file
//!   and drive the binary against it.
//!
//! Production camera capture lives in
//! `crates/perchstation-hw/src/camera_qr.rs` (T029).

use std::io::Cursor;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use image::GrayImage;
use thiserror::Error;

use crate::hw_traits::{QrFrameError, QrFrameSource};

/// One-shot QR source backed by a single PNG/JPEG file. Returns the
/// decoded grayscale image the first time [`QrFrameSource::next_frame`]
/// is called and `Unavailable` thereafter — enrollment only ever needs
/// a single frame.
pub struct FileQrSource {
    path: PathBuf,
    consumed: bool,
}

#[derive(Debug, Error)]
pub enum FileQrError {
    #[error("could not read QR file `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not decode QR file `{path}` as an image: {source}")]
    Image {
        path: PathBuf,
        #[source]
        source: image::ImageError,
    },
}

impl FileQrSource {
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self { path: path.as_ref().to_path_buf(), consumed: false }
    }

    fn load(&self) -> Result<GrayImage, FileQrError> {
        let bytes = std::fs::read(&self.path)
            .map_err(|source| FileQrError::Io { path: self.path.clone(), source })?;
        // The QR file is operator-supplied (a recovery PNG or a phone photo)
        // and only semi-trusted. Decode through a size-limited reader so a
        // small file declaring enormous dimensions is rejected before it can
        // allocate gigabytes and OOM-kill the station (a decompression bomb).
        // `image::Limits` leaves both dimension caps unset by default, so
        // they must be set explicitly; `load_from_memory` installs none.
        let mut reader = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|source| FileQrError::Image {
                path: self.path.clone(),
                source: image::ImageError::IoError(source),
            })?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(crate::enrollment::MAX_QR_IMAGE_DIM);
        limits.max_image_height = Some(crate::enrollment::MAX_QR_IMAGE_DIM);
        reader.limits(limits);
        let img = reader
            .decode()
            .map_err(|source| FileQrError::Image { path: self.path.clone(), source })?;
        Ok(img.into_luma8())
    }
}

#[async_trait]
impl QrFrameSource for FileQrSource {
    async fn next_frame(&mut self) -> Result<GrayImage, QrFrameError> {
        if self.consumed {
            return Err(QrFrameError::Unavailable(format!(
                "FileQrSource for {} exhausted",
                self.path.display()
            )));
        }
        let img = self.load().map_err(|err| match err {
            FileQrError::Io { source, .. } => QrFrameError::Io { source },
            FileQrError::Image { source, .. } => QrFrameError::Decode(source.to_string()),
        })?;
        self.consumed = true;
        Ok(img)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;

    use image::{ImageBuffer, ImageFormat, Luma};
    use qrcode::QrCode;
    use tempfile::TempDir;

    fn write_qr_png(dir: &Path, payload: &str) -> PathBuf {
        let code = QrCode::new(payload.as_bytes()).expect("build QR");
        let img: ImageBuffer<Luma<u8>, Vec<u8>> =
            code.render::<Luma<u8>>().min_dimensions(200, 200).quiet_zone(true).build();
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png).expect("encode PNG");
        let path = dir.join("qr.png");
        std::fs::write(&path, &bytes).expect("write png");
        path
    }

    #[tokio::test]
    async fn yields_frame_once_then_exhausted() {
        let dir = TempDir::new().unwrap();
        let png = write_qr_png(dir.path(), "{\"hi\":1}");
        let mut src = FileQrSource::new(&png);
        let frame = src.next_frame().await.expect("first frame");
        assert!(frame.width() > 0 && frame.height() > 0);
        let err = src.next_frame().await.expect_err("second call exhausts");
        assert!(matches!(err, QrFrameError::Unavailable(_)));
    }

    #[tokio::test]
    async fn surfaces_missing_file_as_io_error() {
        let mut src = FileQrSource::new("/definitely/does/not/exist.png");
        let err = src.next_frame().await.expect_err("missing file");
        assert!(matches!(err, QrFrameError::Io { .. }));
    }

    #[tokio::test]
    async fn surfaces_non_image_as_decode_error() {
        let dir = TempDir::new().unwrap();
        let bogus = dir.path().join("not-an-image.png");
        std::fs::write(&bogus, b"this is not an image").unwrap();
        let mut src = FileQrSource::new(&bogus);
        let err = src.next_frame().await.expect_err("non-image");
        assert!(matches!(err, QrFrameError::Decode(_)));
    }

    #[tokio::test]
    async fn rejects_oversized_image_as_decode_error() {
        let dir = TempDir::new().unwrap();
        // A valid grayscale PNG whose declared width exceeds the decode
        // cap but whose payload is tiny. The size-limited reader must
        // reject it (Decode) rather than allocating, so a few-KB file
        // declaring enormous dimensions can't OOM-kill the station.
        let img: GrayImage =
            ImageBuffer::from_pixel(crate::enrollment::MAX_QR_IMAGE_DIM + 1, 1, Luma([0u8]));
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png).expect("encode oversized png");
        let path = dir.path().join("oversized.png");
        std::fs::write(&path, &bytes).unwrap();

        let mut src = FileQrSource::new(&path);
        let err = src.next_frame().await.expect_err("oversized image");
        assert!(matches!(err, QrFrameError::Decode(_)));
    }
}
