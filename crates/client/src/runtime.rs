//! Quest client application runtime coordinating OpenXR, Vulkan, decoder, and transport.

use std::time::Instant;

use linux_quest_protocol::packet::Packet;

use crate::config::ClientConfig;
use crate::decoder::{create_decoder, DecoderStats, HardwareVideoDecoder};
use crate::error::ClientResult;
use crate::graphics::VulkanContext;
use crate::hud::DebugHud;
use crate::openxr::{FrameLoopEngine, FramePacingMetrics, OpenXrContext};
use crate::receiver::ClientReceiver;

/// High-performance client runtime managing the VR spatial desktop session.
pub struct QuestClientRuntime {
    config: ClientConfig,
    openxr_context: OpenXrContext,
    vulkan_context: VulkanContext,
    decoder: Box<dyn HardwareVideoDecoder>,
    receiver: ClientReceiver,
    frame_loop: FrameLoopEngine,
    hud: DebugHud,
    is_running: bool,
}

impl QuestClientRuntime {
    /// Initializes the Quest client streaming runtime.
    pub fn new(config: ClientConfig) -> ClientResult<Self> {
        config.validate()?;

        let mut vulkan_context = VulkanContext::new_simulated();

        // Try live OpenXR context, falling back to simulated for CI and desktop testing
        let openxr_context = match OpenXrContext::try_new(&config, &mut vulkan_context) {
            Ok(ctx) => ctx,
            Err(e) => {
                tracing::info!(error = ?e, "Using simulated OpenXR context");
                OpenXrContext::new_simulated(&config)
            }
        };

        let decoder = create_decoder(&config, 1920, 1080);
        let receiver = ClientReceiver::new(&config);
        let frame_loop = FrameLoopEngine::new();
        let hud = DebugHud::new(512, 256);

        Ok(Self {
            config,
            openxr_context,
            vulkan_context,
            decoder,
            receiver,
            frame_loop,
            hud,
            is_running: true,
        })
    }

    /// Processes a single frame iteration of the VR display presentation loop at current time.
    pub fn step_frame(&mut self) -> ClientResult<FramePacingMetrics> {
        self.step_frame_at(Instant::now())
    }

    /// Processes a single frame iteration with an explicit timestamp (for testing/simulation).
    pub fn step_frame_at(&mut self, now: Instant) -> ClientResult<FramePacingMetrics> {
        let render_start = Instant::now();

        // 1. Poll OpenXR events
        self.openxr_context.poll_events()?;

        // If running in live mode, ensure the OpenXR session has transitioned to running
        if !self.openxr_context.is_simulated() && !self.openxr_context.is_session_running() {
            return Ok(FramePacingMetrics::default());
        }

        // 2. Pace rendering loop to target headset refresh rate (72Hz, 90Hz, 120Hz)
        let pacing = self.frame_loop.wait_frame_pacing(&self.openxr_context)?;

        // 3. Pop completed video frames whose playout delay has elapsed at the current moment
        let playout_instant = if self.openxr_context.is_simulated() {
            now
        } else {
            Instant::now()
        };

        let ready_frames = self.receiver.pop_ready_frames(playout_instant)?;
        for frame in ready_frames {
            if let Err(e) = self.decoder.queue_input_buffer(
                &frame.bitstream,
                frame.meta.pts_us,
                frame.meta.is_keyframe,
            ) {
                tracing::warn!(
                    error = ?e,
                    frame_id = frame.meta.frame_id,
                    "Failed to queue video frame into decoder"
                );
            }
        }

        // 4. Dequeue and release hardware-decoded video frames direct to OpenXR surface
        while let Some(frame) = self.decoder.dequeue_output_buffer(0)? {
            let _ = self.decoder.release_output_buffer(frame.buffer_index, true);
        }

        // 5. Update debug HUD telemetry
        let decoder_stats = self.decoder.stats();
        self.hud.update(
            &self.config,
            &pacing,
            &decoder_stats,
            self.receiver.packets_received(),
            self.receiver.fec_recovered_count(),
            self.receiver.packets_lost_count(),
        );

        // 6. Submit composition layers to OpenXR compositor for timewarp display
        let display_time = openxr::Time::from_nanos(pacing.predicted_display_time);
        self.openxr_context
            .render_and_present_layers(display_time, &self.config)?;

        // 7. Record render duration
        let render_duration = render_start.elapsed().as_micros() as u64;
        self.frame_loop.record_render_duration(render_duration);

        Ok(pacing)
    }

    /// Ingests an incoming network or USB packet into the jitter buffer.
    pub fn ingest_packet(&mut self, packet: Packet, now: Instant) {
        self.receiver.ingest_packet(packet, now);
    }

    /// Requests a dynamic display refresh rate change.
    pub fn set_target_refresh_rate(&mut self, target_rate: f32) {
        self.config.target_refresh_rate = target_rate;
        if let Some(mgr) = self.openxr_context.refresh_rate_manager_mut() {
            let best = mgr.find_closest_supported_rate(target_rate);
            mgr.on_refresh_rate_changed(best);
        }
    }

    /// Returns the current HUD telemetry text.
    pub fn hud_text(&self) -> String {
        self.hud.formatted_text()
    }

    /// Returns the decoder statistics.
    pub fn decoder_stats(&self) -> DecoderStats {
        self.decoder.stats()
    }

    /// Whether the client runtime is actively running.
    pub fn is_running(&self) -> bool {
        self.is_running && self.openxr_context.is_session_running()
    }

    /// Returns a reference to the Vulkan context.
    pub fn vulkan_context(&self) -> &VulkanContext {
        &self.vulkan_context
    }

    /// Signals the runtime to gracefully shut down.
    pub fn stop(&mut self) {
        self.is_running = false;
        let _ = self.decoder.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use linux_quest_protocol::video::{VideoChunk, VideoChunkMeta, VideoCodec};
    use std::time::Duration;

    #[test]
    fn test_client_runtime_lifecycle_and_packet_flow() {
        let config = ClientConfig::default();
        let mut runtime = QuestClientRuntime::new(config).unwrap();
        assert!(runtime.is_running());

        // Step a frame
        let pacing = runtime.step_frame().unwrap();
        assert!(pacing.predicted_display_time > 0);

        // Feed a complete video frame packet
        let meta = VideoChunkMeta {
            frame_id: 1,
            chunk_index: 0,
            total_chunks: 1,
            codec: VideoCodec::Av1,
            is_keyframe: true,
            is_intra_refresh: false,
            width: 1920,
            height: 1080,
            fps: 90,
            pts_us: 50_000,
        };
        let chunk = VideoChunk::new(meta, Bytes::from_static(&[0x0A, 0x02, 0x11, 0x22]));
        let chunk_bytes = chunk.serialize().unwrap();
        let header = linux_quest_protocol::packet::PacketHeader::new(
            linux_quest_protocol::packet::PacketType::VideoFrameChunk,
            1,
            1,
            50_000,
            &chunk_bytes,
        );
        let packet = Packet::new(header, chunk_bytes);

        let arrival = Instant::now();
        runtime.ingest_packet(packet, arrival);

        // After jitter playout delay (+10ms), step frame to ingest into decoder and render
        let playout_time = arrival + Duration::from_millis(10);
        let _ = runtime.step_frame_at(playout_time).unwrap();

        let stats = runtime.decoder_stats();
        assert_eq!(stats.frames_queued, 1);
        assert_eq!(stats.frames_rendered, 1);

        let hud_text = runtime.hud_text();
        assert!(hud_text.contains("FPS:"));

        runtime.stop();
        assert!(!runtime.is_running());
    }

    #[test]
    fn test_client_runtime_refresh_rate_switch() {
        let config = ClientConfig::default();
        let mut runtime = QuestClientRuntime::new(config).unwrap();

        runtime.set_target_refresh_rate(120.0);
        assert_eq!(
            runtime
                .openxr_context
                .refresh_rate_manager()
                .unwrap()
                .current_rate(),
            120.0
        );
    }
}
