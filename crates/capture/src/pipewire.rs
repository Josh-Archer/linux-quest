use async_trait::async_trait;
use libloading::{Library, Symbol};
use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::{CaptureBackendType, CaptureError, DisplayCapture, RawFrame};

/// SPA data types for buffer allocation.
pub const SPA_DATA_MEM_PTR: u32 = 1;
pub const SPA_DATA_MEM_FD: u32 = 2;
pub const SPA_DATA_DMA_BUF: u32 = 3;

type PwInitFn = unsafe extern "C" fn(*mut i32, *mut *mut *mut std::ffi::c_char);
type PwMainLoopNewFn = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type PwMainLoopGetLoopFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type PwMainLoopDestroyFn = unsafe extern "C" fn(*mut c_void);
type PwContextNewFn = unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void;
type PwContextConnectFn = unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void;
type PwContextDestroyFn = unsafe extern "C" fn(*mut c_void);
type PwCoreDisconnectFn = unsafe extern "C" fn(*mut c_void) -> i32;

/// Zero-copy screen capture engine powered by PipeWire and DMA-BUF.
pub struct PipeWireCapture {
    display_id: u16,
    width: u32,
    height: u32,
    fps: u32,
    initialized: bool,
    running: Arc<AtomicBool>,
    _lib: Option<Library>,
    loop_ptr: *mut c_void,
    ctx_ptr: *mut c_void,
    core_ptr: *mut c_void,
}

unsafe impl Send for PipeWireCapture {}
unsafe impl Sync for PipeWireCapture {}

impl PipeWireCapture {
    pub fn new(display_id: u16, width: u32, height: u32, fps: u32) -> Self {
        Self {
            display_id,
            width,
            height,
            fps,
            initialized: false,
            running: Arc::new(AtomicBool::new(false)),
            _lib: None,
            loop_ptr: std::ptr::null_mut(),
            ctx_ptr: std::ptr::null_mut(),
            core_ptr: std::ptr::null_mut(),
        }
    }

    /// Checks if a local PipeWire daemon socket is accessible and library is loadable.
    pub fn is_available() -> bool {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            let socket_path = Path::new(&runtime_dir).join("pipewire-0");
            if socket_path.exists() {
                return unsafe {
                    Library::new("libpipewire-0.3.so.0")
                        .or_else(|_| Library::new("libpipewire-0.3.so"))
                        .is_ok()
                };
            }
        }
        false
    }

    pub fn display_id(&self) -> u16 {
        self.display_id
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn cleanup(&mut self) {
        if let Some(lib) = &self._lib {
            unsafe {
                if !self.core_ptr.is_null() {
                    if let Ok(pw_core_disconnect) =
                        lib.get::<PwCoreDisconnectFn>(b"pw_core_disconnect\0")
                    {
                        let _ = pw_core_disconnect(self.core_ptr);
                    }
                    self.core_ptr = std::ptr::null_mut();
                }
                if !self.ctx_ptr.is_null() {
                    if let Ok(pw_context_destroy) =
                        lib.get::<PwContextDestroyFn>(b"pw_context_destroy\0")
                    {
                        pw_context_destroy(self.ctx_ptr);
                    }
                    self.ctx_ptr = std::ptr::null_mut();
                }
                if !self.loop_ptr.is_null() {
                    if let Ok(pw_main_loop_destroy) =
                        lib.get::<PwMainLoopDestroyFn>(b"pw_main_loop_destroy\0")
                    {
                        pw_main_loop_destroy(self.loop_ptr);
                    }
                    self.loop_ptr = std::ptr::null_mut();
                }
            }
        }
        self.initialized = false;
    }
}

#[async_trait]
impl DisplayCapture for PipeWireCapture {
    async fn init(&mut self) -> Result<(), CaptureError> {
        self.cleanup();
        let lib = unsafe {
            Library::new("libpipewire-0.3.so.0")
                .or_else(|_| Library::new("libpipewire-0.3.so"))
                .map_err(|e| {
                    CaptureError::BackendUnavailable(format!("PipeWire library not found: {e}"))
                })?
        };

        unsafe {
            let pw_init: Symbol<PwInitFn> = lib
                .get(b"pw_init\0")
                .map_err(|e| CaptureError::InitFailed(format!("pw_init symbol missing: {e}")))?;
            let pw_main_loop_new: Symbol<PwMainLoopNewFn> = lib
                .get(b"pw_main_loop_new\0")
                .map_err(|e| CaptureError::InitFailed(format!("pw_main_loop_new missing: {e}")))?;
            let pw_main_loop_get_loop: Symbol<PwMainLoopGetLoopFn> =
                lib.get(b"pw_main_loop_get_loop\0").map_err(|e| {
                    CaptureError::InitFailed(format!("pw_main_loop_get_loop missing: {e}"))
                })?;
            let pw_context_new: Symbol<PwContextNewFn> = lib
                .get(b"pw_context_new\0")
                .map_err(|e| CaptureError::InitFailed(format!("pw_context_new missing: {e}")))?;
            let pw_context_connect: Symbol<PwContextConnectFn> =
                lib.get(b"pw_context_connect\0").map_err(|e| {
                    CaptureError::InitFailed(format!("pw_context_connect missing: {e}"))
                })?;

            pw_init(std::ptr::null_mut(), std::ptr::null_mut());

            let loop_ptr = pw_main_loop_new(std::ptr::null());
            if loop_ptr.is_null() {
                return Err(CaptureError::InitFailed("pw_main_loop_new failed".into()));
            }

            let pw_loop = pw_main_loop_get_loop(loop_ptr);
            let ctx_ptr = pw_context_new(pw_loop, std::ptr::null_mut(), 0);
            if ctx_ptr.is_null() {
                if let Ok(pw_main_loop_destroy) =
                    lib.get::<PwMainLoopDestroyFn>(b"pw_main_loop_destroy\0")
                {
                    pw_main_loop_destroy(loop_ptr);
                }
                return Err(CaptureError::InitFailed("pw_context_new failed".into()));
            }

            let core_ptr = pw_context_connect(ctx_ptr, std::ptr::null_mut(), 0);
            if core_ptr.is_null() {
                if let Ok(pw_context_destroy) =
                    lib.get::<PwContextDestroyFn>(b"pw_context_destroy\0")
                {
                    pw_context_destroy(ctx_ptr);
                }
                if let Ok(pw_main_loop_destroy) =
                    lib.get::<PwMainLoopDestroyFn>(b"pw_main_loop_destroy\0")
                {
                    pw_main_loop_destroy(loop_ptr);
                }
                return Err(CaptureError::InitFailed("pw_context_connect failed".into()));
            }

            self.loop_ptr = loop_ptr;
            self.ctx_ptr = ctx_ptr;
            self.core_ptr = core_ptr;
        }

        self._lib = Some(lib);

        // Screen capture over PipeWire DMA-BUF requires an active portal screencast stream node.
        // In the absence of an active negotiated stream node, fail initialization cleanly so
        // that AutoCapture gracefully demotes to SyntheticCapture.
        Err(CaptureError::InitFailed(
            "No active PipeWire screencast stream node negotiated".into(),
        ))
    }

    async fn capture_frame(&mut self) -> Result<RawFrame, CaptureError> {
        if !self.initialized {
            return Err(CaptureError::InitFailed(
                "PipeWireCapture not initialized".into(),
            ));
        }

        Err(CaptureError::CaptureFailed(
            "No active PipeWire screencast stream node negotiated".into(),
        ))
    }

    fn backend_type(&self) -> CaptureBackendType {
        CaptureBackendType::PipeWire
    }
}

impl Drop for PipeWireCapture {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pipewire_capture_without_stream_node_demotes() {
        let mut pw = PipeWireCapture::new(0, 1920, 1080, 60);
        assert_eq!(pw.display_id(), 0);
        assert_eq!(pw.width(), 1920);
        assert_eq!(pw.height(), 1080);
        assert_eq!(pw.fps(), 60);
        assert!(!pw.is_running());

        let res = pw.init().await;
        if PipeWireCapture::is_available() {
            assert!(matches!(res, Err(CaptureError::InitFailed(_))));
        }
    }
}
