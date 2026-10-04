use serde::{Deserialize, Serialize};

/// Detailed latency telemetry tracking a single frame's journey from Linux host capture to Quest VSync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FrameLatencyBreakdown {
    pub frame_id: u64,
    /// Host frame capture time (microsecond epoch)
    pub capture_start_us: u64,
    /// DMA-BUF export / acquisition duration
    pub capture_duration_us: u32,
    /// NVENC / VA-API hardware encode duration
    pub encode_duration_us: u32,
    /// Network packetization & queue delay
    pub host_queue_duration_us: u32,
    /// Transport transit delay (USB or Wi-Fi)
    pub transport_duration_us: u32,
    /// AMediaCodec hardware decode duration on Quest
    pub client_decode_duration_us: u32,
    /// OpenXR composition layer blit duration
    pub client_render_duration_us: u32,
    /// End-to-end glass-to-glass latency in microseconds
    pub total_motion_to_photon_us: u32,
}

impl FrameLatencyBreakdown {
    pub fn new(frame_id: u64, capture_start_us: u64) -> Self {
        Self {
            frame_id,
            capture_start_us,
            ..Default::default()
        }
    }

    pub fn compute_total(&mut self) -> u32 {
        self.total_motion_to_photon_us = self.capture_duration_us
            + self.encode_duration_us
            + self.host_queue_duration_us
            + self.transport_duration_us
            + self.client_decode_duration_us
            + self.client_render_duration_us;
        self.total_motion_to_photon_us
    }

    pub fn total_ms(&self) -> f32 {
        self.total_motion_to_photon_us as f32 / 1000.0
    }
}
