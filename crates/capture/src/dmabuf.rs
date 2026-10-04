use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Representation of a single DMA-BUF memory plane for zero-copy GPU framebuffer transfer.
/// Owns the underlying file descriptor and closes it upon drop.
#[derive(Debug)]
pub struct DmaBufPlane {
    /// File descriptor representing the exported DRM/KMS or EGL GPU memory.
    pub fd: OwnedFd,
    /// Stride (pitch) in bytes for this plane.
    pub stride: u32,
    /// Offset within the buffer in bytes.
    pub offset: u32,
    /// DRM format modifier (e.g. DRM_FORMAT_MOD_LINEAR or vendor-specific tiled modifier).
    pub modifier: u64,
}

impl DmaBufPlane {
    pub fn new(fd: OwnedFd, stride: u32, offset: u32, modifier: u64) -> Self {
        Self {
            fd,
            stride,
            offset,
            modifier,
        }
    }

    /// Creates a DmaBufPlane by taking ownership of an existing raw file descriptor.
    ///
    /// # Safety
    /// The caller must ensure that `raw_fd` is an open and valid file descriptor.
    pub unsafe fn from_raw_fd(raw_fd: RawFd, stride: u32, offset: u32, modifier: u64) -> Self {
        Self::new(OwnedFd::from_raw_fd(raw_fd), stride, offset, modifier)
    }

    pub fn is_valid(&self) -> bool {
        self.fd.as_raw_fd() >= 0
    }

    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            fd: self.fd.try_clone()?,
            stride: self.stride,
            offset: self.offset,
            modifier: self.modifier,
        })
    }

    /// Helper for creating a valid in-memory DMA-BUF plane backed by a real memfd.
    pub fn create_test_memfd(stride: u32, offset: u32, modifier: u64) -> Self {
        let fd = unsafe { libc::memfd_create(c"test_dmabuf_plane".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed");
        Self::new(
            unsafe { OwnedFd::from_raw_fd(fd) },
            stride,
            offset,
            modifier,
        )
    }
}
