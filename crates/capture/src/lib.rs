pub mod dmabuf;
pub mod pipewire;

use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;

pub use dmabuf::DmaBufPlane;
pub use pipewire::PipeWireCapture;

#[derive(Error, Debug)]
pub enum CaptureError {
    #[error("Initialization failed: {0}")]
    InitFailed(String),

    #[error("Capture failed: {0}")]
    CaptureFailed(String),

    #[error("Display not found: {0}")]
    DisplayNotFound(u16),

    #[error("Backend unavailable: {0}")]
    BackendUnavailable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba8,
    Bgra8,
    Nv12,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBackendType {
    PipeWire,
    DmaBuf,
    X11Shm,
    Synthetic,
}

/// A captured raw frame prior to video encoding.
/// If `dma_buf` is `Some`, the frame is held in GPU VRAM (strictly zero-copy).
/// If `dma_buf` is `None`, `data` contains system RAM pixels (software fallback/synthetic).
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub display_id: u16,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub pts_us: u64,
    pub dma_buf: Option<Vec<DmaBufPlane>>,
    pub data: Bytes,
}

impl RawFrame {
    pub fn is_zero_copy(&self) -> bool {
        self.dma_buf.is_some()
    }
}

#[async_trait]
pub trait DisplayCapture: Send + Sync {
    async fn init(&mut self) -> Result<(), CaptureError>;
    async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError>;
    fn backend_type(&self) -> CaptureBackendType;
}

/// Deterministic synthetic screen generator for integration tests and headless benchmarking.
pub struct SyntheticCapture {
    display_id: u16,
    width: u32,
    height: u32,
    fps: u32,
    frame_counter: u64,
}

impl SyntheticCapture {
    pub fn new(display_id: u16, width: u32, height: u32, fps: u32) -> Self {
        Self {
            display_id,
            width,
            height,
            fps,
            frame_counter: 0,
        }
    }
}

#[async_trait]
impl DisplayCapture for SyntheticCapture {
    async fn init(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
        self.frame_counter += 1;
        let stride = self.width * 4;
        let frame_size = (stride * self.height) as usize;

        // Generate lightweight animated gradient
        let mut buffer = vec![0u8; frame_size];
        let offset = (self.frame_counter % 256) as u8;
        for i in (0..frame_size).step_by(4) {
            buffer[i] = offset; // R
            buffer[i + 1] = 128; // G
            buffer[i + 2] = 255 - offset; // B
            buffer[i + 3] = 255; // A
        }

        let interval_us = 1_000_000 / (self.fps as u64).max(1);
        let pts_us = self.frame_counter * interval_us;

        Ok(RawFrame {
            display_id: self.display_id,
            width: self.width,
            height: self.height,
            stride,
            format: PixelFormat::Rgba8,
            pts_us,
            dma_buf: None,
            data: Bytes::from(buffer),
        })
    }

    fn backend_type(&self) -> CaptureBackendType {
        CaptureBackendType::Synthetic
    }
}

/// Dynamically discovers and initializes the best available capture engine.
pub struct AutoCapture {
    inner: Box<dyn DisplayCapture>,
}

impl AutoCapture {
    pub fn new(display_id: u16, width: u32, height: u32, fps: u32) -> Self {
        if PipeWireCapture::is_available() {
            Self {
                inner: Box::new(PipeWireCapture::new(display_id, width, height, fps)),
            }
        } else {
            Self {
                inner: Box::new(SyntheticCapture::new(display_id, width, height, fps)),
            }
        }
    }
}

#[async_trait]
impl DisplayCapture for AutoCapture {
    async fn init(&mut self) -> Result<(), CaptureError> {
        self.inner.init().await
    }

    async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
        self.inner.capture_frame().await
    }

    fn backend_type(&self) -> CaptureBackendType {
        self.inner.backend_type()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_synthetic_capture_frame() {
        let mut capture = SyntheticCapture::new(0, 1920, 1080, 60);
        capture
            .init()
            .await
            .expect("Failed to init synthetic capture");

        let frame1 = capture
            .capture_frame()
            .await
            .expect("Failed to capture frame 1");
        assert_eq!(frame1.width, 1920);
        assert_eq!(frame1.height, 1080);
        assert_eq!(frame1.stride, 1920 * 4);
        assert_eq!(frame1.data.len(), 1920 * 1080 * 4);
        assert!(!frame1.is_zero_copy());

        let frame2 = capture
            .capture_frame()
            .await
            .expect("Failed to capture frame 2");
        assert!(frame2.pts_us > frame1.pts_us);
    }

    #[test]
    fn test_dmabuf_plane_creation() {
        let plane = DmaBufPlane::new(42, 7680, 0, 0);
        assert_eq!(plane.fd, 42);
        assert_eq!(plane.stride, 7680);
        assert_eq!(plane.offset, 0);
        assert_eq!(plane.modifier, 0);
    }

    #[tokio::test]
    async fn test_auto_capture_initialization() {
        let mut auto = AutoCapture::new(0, 1280, 720, 60);
        auto.init().await.expect("Auto capture init failed");
        let frame = auto.capture_frame().await.expect("Capture frame failed");
        assert_eq!(frame.width, 1280);
        assert_eq!(frame.height, 720);
    }
}
