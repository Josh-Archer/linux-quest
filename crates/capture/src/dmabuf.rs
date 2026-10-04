use std::os::unix::io::RawFd;

/// Representation of a single DMA-BUF memory plane for zero-copy GPU framebuffer transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmaBufPlane {
    /// File descriptor representing the exported DRM/KMS or EGL GPU memory.
    pub fd: RawFd,
    /// Stride (pitch) in bytes for this plane.
    pub stride: u32,
    /// Offset within the buffer in bytes.
    pub offset: u32,
    /// DRM format modifier (e.g. DRM_FORMAT_MOD_LINEAR or vendor-specific tiled modifier).
    pub modifier: u64,
}

impl DmaBufPlane {
    pub fn new(fd: RawFd, stride: u32, offset: u32, modifier: u64) -> Self {
        Self {
            fd,
            stride,
            offset,
            modifier,
        }
    }
}
