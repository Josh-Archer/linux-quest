//! Mock and simulation video decoder for cross-platform testing and development.

use std::collections::VecDeque;
use std::time::Instant;

use crate::config::ClientVideoCodec;
use crate::decoder::{DecodedFrame, DecoderStats, HardwareVideoDecoder};
use crate::error::{ClientError, ClientResult};

/// Parsed AV1 OBU types according to AV1 Bitstream & Decoding Process Specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Av1ObuType {
    SequenceHeader,
    TemporalDelimiter,
    FrameHeader,
    TileGroup,
    Metadata,
    Frame,
    RedundantFrameHeader,
    TileList,
    Padding,
    Unknown(u8),
}

impl From<u8> for Av1ObuType {
    fn from(val: u8) -> Self {
        match val {
            1 => Self::SequenceHeader,
            2 => Self::TemporalDelimiter,
            3 => Self::FrameHeader,
            4 => Self::TileGroup,
            5 => Self::Metadata,
            6 => Self::Frame,
            7 => Self::RedundantFrameHeader,
            8 => Self::TileList,
            15 => Self::Padding,
            other => Self::Unknown(other),
        }
    }
}

/// Parsed HEVC / H.265 NAL unit types according to ITU-T H.265.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HevcNalType {
    TrailN,
    TrailR,
    IdrWRadl,
    IdrNLp,
    CraNut,
    VpsNut,
    SpsNut,
    PpsNut,
    PrefixSeiNut,
    SuffixSeiNut,
    Unknown(u8),
}

impl From<u8> for HevcNalType {
    fn from(val: u8) -> Self {
        match val {
            0 => Self::TrailN,
            1 => Self::TrailR,
            19 => Self::IdrWRadl,
            20 => Self::IdrNLp,
            21 => Self::CraNut,
            32 => Self::VpsNut,
            33 => Self::SpsNut,
            34 => Self::PpsNut,
            39 => Self::PrefixSeiNut,
            40 => Self::SuffixSeiNut,
            other => Self::Unknown(other),
        }
    }
}

/// Information parsed from an encoded video bitstream chunk.
#[derive(Debug, Clone)]
pub struct BitstreamInfo {
    /// Codec type.
    pub codec: ClientVideoCodec,
    /// Whether this chunk contains an intra/keyframe sequence header or IDR slice.
    pub is_keyframe: bool,
    /// Number of parsed NALUs or OBUs in this chunk.
    pub unit_count: usize,
}

/// Parses an AV1 bitstream chunk and extracts OBU structures.
pub fn parse_av1_bitstream(data: &[u8]) -> BitstreamInfo {
    let mut offset = 0;
    let mut is_keyframe = false;
    let mut unit_count = 0;

    while offset < data.len() {
        let header = data[offset];
        // AV1 OBU header: obu_type is in bits [6:3]
        let obu_type_val = (header >> 3) & 0x0F;
        let obu_type = Av1ObuType::from(obu_type_val);
        let has_size = (header & 0x02) != 0;

        if matches!(obu_type, Av1ObuType::SequenceHeader) {
            is_keyframe = true;
        }

        unit_count += 1;
        offset += 1;

        if has_size && offset < data.len() {
            // Read leb128 size safely avoiding bit shift overflow
            let mut size: usize = 0;
            let mut shift = 0;
            while offset < data.len() && shift < usize::BITS {
                let byte = data[offset];
                offset += 1;
                if let Some(shifted) = ((byte & 0x7F) as usize).checked_shl(shift) {
                    size |= shifted;
                }
                if (byte & 0x80) == 0 {
                    break;
                }
                shift += 7;
            }
            offset = offset.saturating_add(size);
        } else {
            // Unbounded / single OBU remaining
            break;
        }
    }

    BitstreamInfo {
        codec: ClientVideoCodec::Av1,
        is_keyframe,
        unit_count,
    }
}

/// Parses a HEVC bitstream chunk with Annex B 3- or 4-byte start codes.
pub fn parse_hevc_bitstream(data: &[u8]) -> BitstreamInfo {
    let mut offset = 0;
    let mut is_keyframe = false;
    let mut unit_count = 0;

    while offset + 3 <= data.len() {
        // Find Annex B start code: 0x000001 or 0x00000001
        if data[offset] == 0 && data[offset + 1] == 0 {
            let start_len = if data[offset + 2] == 1 {
                3
            } else if offset + 4 <= data.len() && data[offset + 2] == 0 && data[offset + 3] == 1 {
                4
            } else {
                offset += 1;
                continue;
            };

            let header_offset = offset + start_len;
            if header_offset < data.len() {
                // HEVC NAL header: nal_unit_type is bits [14:9] across 2 bytes
                let nal_type_val = (data[header_offset] >> 1) & 0x3F;
                let nal_type = HevcNalType::from(nal_type_val);

                if matches!(
                    nal_type,
                    HevcNalType::IdrWRadl
                        | HevcNalType::IdrNLp
                        | HevcNalType::CraNut
                        | HevcNalType::VpsNut
                        | HevcNalType::SpsNut
                ) {
                    is_keyframe = true;
                }

                unit_count += 1;
            }
            offset += start_len;
        } else {
            offset += 1;
        }
    }

    BitstreamInfo {
        codec: ClientVideoCodec::Hevc,
        is_keyframe,
        unit_count: unit_count.max(1),
    }
}

/// A mock hardware decoder for simulation, host testing, and CI environments.
pub struct MockHardwareDecoder {
    width: u32,
    height: u32,
    codec: ClientVideoCodec,
    stats: DecoderStats,
    output_queue: VecDeque<DecodedFrame>,
    simulated_latency_us: u64,
    next_buffer_index: usize,
    last_enqueue_time: Option<Instant>,
}

impl Default for MockHardwareDecoder {
    fn default() -> Self {
        Self::new(1920, 1080, ClientVideoCodec::Av1)
    }
}

impl MockHardwareDecoder {
    /// Creates a new mock decoder instance.
    pub fn new(width: u32, height: u32, codec: ClientVideoCodec) -> Self {
        let mut decoder = Self {
            width,
            height,
            codec,
            stats: DecoderStats::default(),
            output_queue: VecDeque::with_capacity(32),
            simulated_latency_us: 2500, // 2.5ms Qualcomm XR2 Gen 2 decode target
            next_buffer_index: 0,
            last_enqueue_time: None,
        };
        decoder.stats.decoder_name = match codec {
            ClientVideoCodec::Av1 => "mock.c2.qti.av1.decoder.low_latency".to_string(),
            ClientVideoCodec::Hevc => "mock.c2.qti.hevc.decoder.low_latency".to_string(),
        };
        decoder
    }

    /// Sets the simulated hardware decoding latency in microseconds.
    pub fn set_simulated_latency_us(&mut self, latency_us: u64) {
        self.simulated_latency_us = latency_us;
    }
}

impl HardwareVideoDecoder for MockHardwareDecoder {
    fn init(&mut self, width: u32, height: u32, codec: ClientVideoCodec) -> ClientResult<()> {
        if width == 0 || height == 0 {
            return Err(ClientError::Decoder(format!(
                "Invalid resolution: {width}x{height}"
            )));
        }
        self.width = width;
        self.height = height;
        self.codec = codec;
        self.output_queue.clear();
        self.stats = DecoderStats {
            decoder_name: format!("mock.{}", codec.qti_low_latency_name()),
            ..Default::default()
        };
        Ok(())
    }

    fn queue_input_buffer(
        &mut self,
        data: &[u8],
        pts_us: u64,
        is_keyframe: bool,
    ) -> ClientResult<()> {
        if data.is_empty() {
            return Err(ClientError::Decoder(
                "Cannot queue empty bitstream packet".to_string(),
            ));
        }

        // Parse bitstream headers to validate packet structure
        let parsed = match self.codec {
            ClientVideoCodec::Av1 => parse_av1_bitstream(data),
            ClientVideoCodec::Hevc => parse_hevc_bitstream(data),
        };

        let effective_keyframe = is_keyframe || parsed.is_keyframe;

        self.stats.frames_queued += 1;
        let buf_idx = self.next_buffer_index;
        self.next_buffer_index = (self.next_buffer_index + 1) % 64;

        // Produce simulated decoded frame
        let frame = DecodedFrame {
            buffer_index: buf_idx,
            pts_us,
            width: self.width,
            height: self.height,
            is_keyframe: effective_keyframe,
            pixel_data: Some(data.to_vec()),
        };

        self.output_queue.push_back(frame);
        self.stats.frames_decoded += 1;
        self.stats.last_latency_us = self.simulated_latency_us;

        // Update running average latency
        if self.stats.frames_decoded == 1 {
            self.stats.avg_latency_us = self.simulated_latency_us;
        } else {
            self.stats.avg_latency_us =
                (self.stats.avg_latency_us * 7 + self.simulated_latency_us) / 8;
        }

        self.last_enqueue_time = Some(Instant::now());
        Ok(())
    }

    fn dequeue_output_buffer(&mut self, _timeout_us: i64) -> ClientResult<Option<DecodedFrame>> {
        Ok(self.output_queue.pop_front())
    }

    fn release_output_buffer(&mut self, _buffer_index: usize, render: bool) -> ClientResult<()> {
        if render {
            self.stats.frames_rendered += 1;
        } else {
            self.stats.frames_dropped += 1;
        }
        Ok(())
    }

    fn flush(&mut self) -> ClientResult<()> {
        self.output_queue.clear();
        Ok(())
    }

    fn stats(&self) -> DecoderStats {
        self.stats.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_av1_obu_sequence_header() {
        // AV1 OBU Sequence Header: type 1, no extension, has size
        // Header byte: (1 << 3) | 0x02 = 0x0A
        let data = [0x0A, 0x04, 0x11, 0x22, 0x33, 0x44];
        let info = parse_av1_bitstream(&data);
        assert_eq!(info.codec, ClientVideoCodec::Av1);
        assert!(info.is_keyframe);
        assert!(info.unit_count >= 1);
    }

    #[test]
    fn test_parse_hevc_idr_frame() {
        // Annex B 4-byte start code + IDR_W_RADL (type 19)
        // NAL header: (19 << 1) = 38 (0x26)
        let data = [0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xAA, 0xBB];
        let info = parse_hevc_bitstream(&data);
        assert_eq!(info.codec, ClientVideoCodec::Hevc);
        assert!(info.is_keyframe);
    }

    #[test]
    fn test_mock_decoder_lifecycle() {
        let mut decoder = MockHardwareDecoder::new(1920, 1080, ClientVideoCodec::Av1);
        assert_eq!(decoder.stats().frames_queued, 0);

        let packet = [0x0A, 0x02, 0x01, 0x02];
        decoder.queue_input_buffer(&packet, 100_000, true).unwrap();

        assert_eq!(decoder.stats().frames_queued, 1);
        assert_eq!(decoder.stats().frames_decoded, 1);

        let frame = decoder.dequeue_output_buffer(1000).unwrap().unwrap();
        assert_eq!(frame.pts_us, 100_000);
        assert!(frame.is_keyframe);
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);

        decoder
            .release_output_buffer(frame.buffer_index, true)
            .unwrap();
        assert_eq!(decoder.stats().frames_rendered, 1);
        assert_eq!(decoder.stats().frames_dropped, 0);
    }

    #[test]
    fn test_mock_decoder_flush() {
        let mut decoder = MockHardwareDecoder::new(1280, 720, ClientVideoCodec::Hevc);
        let packet = [0x00, 0x00, 0x01, 0x02, 0x01, 0x00];
        decoder.queue_input_buffer(&packet, 50_000, false).unwrap();
        assert_eq!(decoder.output_queue.len(), 1);

        decoder.flush().unwrap();
        assert_eq!(decoder.output_queue.len(), 0);
    }
}
