//! In-memory [`QrFrameSource`] for in-process tests.
//!
//! The US1 RED tests drive `perchstation enroll` as a separate process
//! via `assert_cmd`, so they hand the binary a PNG file with
//! `--qr-source file --qr-file <path>` rather than constructing this
//! source. `FakeQrSource` exists for later in-process tests that need
//! deterministic frames without spawning subprocesses (e.g., unit tests
//! exercising the enrollment-confirm exchange directly).

use async_trait::async_trait;
use image::GrayImage;
use perchstation_core::hw_traits::{QrFrameError, QrFrameSource};

/// Single-shot in-memory QR source. Returns the held image on the first
/// call and an `Unavailable` error on subsequent calls — mirrors the
/// "one decode per enrollment attempt" contract.
pub struct FakeQrSource {
    image: Option<GrayImage>,
}

impl FakeQrSource {
    #[must_use]
    pub fn new(image: GrayImage) -> Self {
        Self { image: Some(image) }
    }
}

#[async_trait]
impl QrFrameSource for FakeQrSource {
    async fn next_frame(&mut self) -> Result<GrayImage, QrFrameError> {
        self.image.take().ok_or_else(|| QrFrameError::Unavailable("fake source exhausted".into()))
    }
}
