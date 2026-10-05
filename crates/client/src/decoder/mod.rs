//! Hardware video decoder abstraction and implementations.

pub mod factory;
pub mod mock;

#[cfg(target_os = "android")]
pub mod mediacodec;

pub use factory::create_decoder;

use crate::config::ClientVideoCodec;
use crate::error::ClientResult;

/// A decoded video frame produced by the video decoder.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// Internal buffer index (for AMediaCodec buffer releasing).
    pub buffer_index: usize,

    /// Presentation timestamp in microseconds.
    pub pts_us: u64,

    /// Frame width in pixels.
    pub width: u32,

    /// Frame height in pixels.
    pub height: u32,

    /// Whether this frame is an IDR/Keyframe.
    pub is_keyframe: bool,

    /// Raw or reconstructed frame pixels (populated for testing/simulation).
    pub pixel_data: Option<Vec<u8>>,
}

/// Real-time decoder statistics.
#[derive(Debug, Clone, Default)]
pub struct DecoderStats {
    /// Total frames ingested into decoder.
    pub frames_queued: u64,

    /// Total frames successfully decoded.
    pub frames_decoded: u64,

    /// Total frames released to the display surface.
    pub frames_rendered: u64,

    /// Total frames dropped due to queue congestion or deadline misses.
    pub frames_dropped: u64,

    /// Most recent frame decode latency in microseconds.
    pub last_latency_us: u64,

    /// Running average frame decode latency in microseconds.
    pub avg_latency_us: u64,

    /// Active hardware decoder name (e.g. `c2.qti.av1.decoder.low_latency`).
    pub decoder_name: String,
}

/// Common trait for zero-copy hardware video decoders.
pub trait HardwareVideoDecoder: Send {
    /// Initializes the decoder with target resolution and codec.
    fn init(&mut self, width: u32, height: u32, codec: ClientVideoCodec) -> ClientResult<()>;

    /// Feeds an encoded bitstream packet (NALU or OBU) into the decoder.
    fn queue_input_buffer(
        &mut self,
        data: &[u8],
        pts_us: u64,
        is_keyframe: bool,
    ) -> ClientResult<()>;

    /// Dequeues a decoded output frame with a specified timeout in microseconds.
    fn dequeue_output_buffer(&mut self, timeout_us: i64) -> ClientResult<Option<DecodedFrame>>;

    /// Releases a previously dequeued output buffer back to the decoder or renders it to the surface.
    fn release_output_buffer(&mut self, buffer_index: usize, render: bool) -> ClientResult<()>;

    /// Releases a previously dequeued output buffer to be presented at an exact display timestamp in nanoseconds.
    fn release_output_buffer_at_time(
        &mut self,
        buffer_index: usize,
        _render_timestamp_ns: i64,
    ) -> ClientResult<()> {
        self.release_output_buffer(buffer_index, true)
    }

    /// Flushes all pending input and output buffers (e.g. upon stream reconnect or seek).
    fn flush(&mut self) -> ClientResult<()>;

    /// Returns current telemetry statistics from the decoder.
    fn stats(&self) -> DecoderStats;

    /// Configures an Android native surface window (`*mut ANativeWindow` as `*mut std::ffi::c_void`) for zero-copy rendering.
    ///
    /// # Safety
    /// The window pointer must point to a valid `ANativeWindow` on Android, or null to detach.
    unsafe fn set_surface_window(&mut self, _window: *mut std::ffi::c_void) -> ClientResult<()> {
        Ok(())
    }
}
