use async_trait::async_trait;
use linux_quest_capture::RawFrame;
use linux_quest_protocol::{VideoChunk, VideoChunkMeta, VideoCodec};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EncoderError {
    #[error("Encoder initialization failed: {0}")]
    InitFailed(String),

    #[error("Frame encoding failed: {0}")]
    EncodeFailed(String),

    #[error("Unsupported codec: {0:?}")]
    UnsupportedCodec(VideoCodec),
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub intra_refresh_period: u32,
    pub max_chunk_size: usize,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::Av1,
            width: 1920,
            height: 1080,
            fps: 90,
            bitrate_kbps: 80_000,
            intra_refresh_period: 30,
            max_chunk_size: 1400,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub frame_id: u64,
    pub is_keyframe: bool,
    pub is_intra_refresh: bool,
    pub chunks: Vec<VideoChunk>,
    pub encode_duration_us: u32,
}

#[async_trait]
pub trait VideoEncoder: Send + Sync {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError>;
    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError>;
    fn request_keyframe(&mut self);
    fn invalidate_reference_picture(&mut self, last_good_frame_id: u64);
}

/// Software mock encoder for testing chunking, rate control, and RPI loops.
pub struct MockVideoEncoder {
    config: EncoderConfig,
    frame_counter: u64,
    force_keyframe: bool,
    last_rpi_frame: Option<u64>,
}

impl MockVideoEncoder {
    pub fn new(config: EncoderConfig) -> Self {
        Self {
            config,
            frame_counter: 0,
            force_keyframe: true,
            last_rpi_frame: None,
        }
    }

    pub fn last_rpi_frame(&self) -> Option<u64> {
        self.last_rpi_frame
    }
}

#[async_trait]
impl VideoEncoder for MockVideoEncoder {
    async fn init(&mut self, config: EncoderConfig) -> Result<(), EncoderError> {
        self.config = config;
        self.frame_counter = 0;
        self.force_keyframe = true;
        Ok(())
    }

    async fn encode(&mut self, frame: &RawFrame) -> Result<EncodedFrame, EncoderError> {
        self.frame_counter += 1;
        let is_keyframe = self.force_keyframe || (self.frame_counter % 120 == 1);
        let is_intra_refresh = !is_keyframe
            && self.config.intra_refresh_period > 0
            && self
                .frame_counter
                .is_multiple_of(self.config.intra_refresh_period as u64);
        self.force_keyframe = false;

        // Produce simulated bitstream from raw frame data
        let sample_len = (frame.data.len() / 200).clamp(64, 4096);
        let simulated_payload = frame.data.slice(..sample_len.min(frame.data.len()));

        let chunk_size = self.config.max_chunk_size.max(512);
        let total_chunks = simulated_payload.len().div_ceil(chunk_size) as u16;

        let mut chunks = Vec::with_capacity(total_chunks as usize);
        for i in 0..total_chunks {
            let start = (i as usize) * chunk_size;
            let end = (start + chunk_size).min(simulated_payload.len());
            let chunk_data = simulated_payload.slice(start..end);

            let meta = VideoChunkMeta {
                frame_id: self.frame_counter,
                chunk_index: i,
                total_chunks,
                codec: self.config.codec,
                is_keyframe,
                is_intra_refresh,
                width: self.config.width,
                height: self.config.height,
                fps: self.config.fps,
                pts_us: frame.pts_us,
            };

            chunks.push(VideoChunk::new(meta, chunk_data));
        }

        Ok(EncodedFrame {
            frame_id: self.frame_counter,
            is_keyframe,
            is_intra_refresh,
            chunks,
            encode_duration_us: 1500, // Simulated 1.5ms
        })
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn invalidate_reference_picture(&mut self, last_good_frame_id: u64) {
        self.last_rpi_frame = Some(last_good_frame_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[tokio::test]
    async fn test_mock_encoder_chunking() {
        let config = EncoderConfig {
            codec: VideoCodec::Av1,
            width: 1920,
            height: 1080,
            fps: 90,
            bitrate_kbps: 80_000,
            intra_refresh_period: 30,
            max_chunk_size: 1024,
        };

        let mut encoder = MockVideoEncoder::new(config);
        let raw = RawFrame {
            display_id: 0,
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            format: linux_quest_capture::PixelFormat::Rgba8,
            pts_us: 1000,
            data: Bytes::from(vec![0xAA; 100_000]),
        };

        let encoded = encoder.encode(&raw).await.expect("Encode failed");
        assert!(encoded.is_keyframe);
        assert!(!encoded.chunks.is_empty());
        assert_eq!(encoded.chunks[0].meta.codec, VideoCodec::Av1);
        for chunk in &encoded.chunks {
            assert!(chunk.payload.len() <= 1024);
        }
    }

    #[test]
    fn test_rpi_invalidation() {
        let mut encoder = MockVideoEncoder::new(EncoderConfig::default());
        encoder.invalidate_reference_picture(42);
        assert_eq!(encoder.last_rpi_frame(), Some(42));
    }
}
