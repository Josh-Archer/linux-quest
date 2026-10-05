use bytes::Bytes;
use linux_quest_protocol::{
    FrameReassembler, Packet, PacketHeader, PacketType, VideoChunk, VideoChunkMeta, VideoCodec,
    FLAG_FEC_PROTECTED, FLAG_KEYFRAME, FLAG_LAST_CHUNK,
};
use linux_quest_transport::fec::ReedSolomonFec;
use linux_quest_transport::{AdaptiveJitterBuffer, JitterBufferConfig};
use std::time::{Duration, Instant};

/// Simple deterministic pseudo-random generator for reproducible packet drop simulation.
struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u32(&mut self) -> u32 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.state >> 32) as u32
    }

    /// Returns true if packet should be dropped given loss_probability (e.g., 0.05 for 5% loss).
    fn should_drop(&mut self, loss_rate: f64) -> bool {
        let threshold = (loss_rate * (u32::MAX as f64)) as u32;
        self.next_u32() < threshold
    }
}

#[test]
fn test_wifi_5_percent_loss_tolerance_with_fec_recovery() {
    let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
        min_delay: Duration::from_micros(500),
        max_delay: Duration::from_millis(50),
        jitter_multiplier: 1.5,
        rtt_multiplier: 0.1,
    });
    let mut reassembler = FrameReassembler::new(16);
    let mut rng = SimpleRng::new(0xDEADBEEF);

    let frame_count = 30;
    let chunks_per_frame = 10;
    let parity_per_frame = 2; // 2 parity chunks per 10 data chunks (20% redundancy, protects against >10% loss)
    let loss_rate = 0.05; // 5% simulated packet loss

    let mut total_packets_sent = 0;
    let mut total_packets_dropped = 0;
    let mut sequence = 0u32;
    let base_time = Instant::now();

    for frame_id in 1..=frame_count {
        let block_id = frame_id;
        jb.register_fec_block(block_id, chunks_per_frame, parity_per_frame);

        // Generate chunks for this frame
        let mut source_payloads = Vec::with_capacity(chunks_per_frame);
        let mut source_packets = Vec::with_capacity(chunks_per_frame);

        for chunk_idx in 0..chunks_per_frame {
            let meta = VideoChunkMeta {
                frame_id,
                chunk_index: chunk_idx as u16,
                total_chunks: chunks_per_frame as u16,
                codec: VideoCodec::Av1,
                is_keyframe: frame_id == 1,
                is_intra_refresh: false,
                width: 1920,
                height: 1080,
                fps: 90,
                pts_us: frame_id * 11_111,
            };

            let raw_data = format!("frame-{:04}-chunk-{:02}-payload-data", frame_id, chunk_idx);
            let chunk = VideoChunk::new(meta, Bytes::from(raw_data));
            let serialized = chunk.serialize().unwrap();
            source_payloads.push(serialized.clone());

            sequence += 1;
            let mut flags = FLAG_FEC_PROTECTED;
            if frame_id == 1 {
                flags |= FLAG_KEYFRAME;
            }
            if chunk_idx + 1 == chunks_per_frame {
                flags |= FLAG_LAST_CHUNK;
            }

            let header = PacketHeader::new(
                PacketType::VideoFrameChunk,
                0,
                sequence,
                block_id,
                &serialized,
            )
            .with_flags(flags)
            .with_fec_index(chunk_idx as u16);

            source_packets.push(Packet::new(header, serialized));
        }

        // Generate Reed-Solomon parity packets
        let parities =
            ReedSolomonFec::encode(block_id, &source_payloads, parity_per_frame).unwrap();
        let mut parity_packets = Vec::with_capacity(parity_per_frame);

        for (p_idx, parity_bytes) in parities.into_iter().enumerate() {
            sequence += 1;
            let header =
                PacketHeader::new(PacketType::FecParity, 0, sequence, block_id, &parity_bytes)
                    .with_flags(FLAG_FEC_PROTECTED)
                    .with_fec_index((chunks_per_frame + p_idx) as u16);

            parity_packets.push(Packet::new(header, parity_bytes));
        }

        // Transmit source packets through simulated lossy Wi-Fi channel
        let now = base_time + Duration::from_micros(frame_id * 11_111);
        for packet in source_packets {
            total_packets_sent += 1;
            if rng.should_drop(loss_rate) {
                total_packets_dropped += 1;
            } else {
                jb.ingest_packet(packet, now);
            }
        }

        // Transmit parity packets through simulated lossy Wi-Fi channel
        for packet in parity_packets {
            total_packets_sent += 1;
            if rng.should_drop(loss_rate) {
                total_packets_dropped += 1;
            } else {
                jb.ingest_packet(packet, now);
            }
        }
    }

    assert!(total_packets_sent > 0, "Packets must have been sent");
    assert!(
        total_packets_dropped > 0,
        "Loss simulation must drop packets"
    );

    // Playout and frame reassembly
    let playout_now = base_time + Duration::from_millis(500);
    let mut assembled_frames = Vec::new();

    while let Some(packet) = jb.pop_ready_packet(playout_now) {
        if packet.header.packet_type == PacketType::VideoFrameChunk {
            let chunk =
                VideoChunk::deserialize(&packet.payload).expect("Chunk deserialization failed");
            if let Some(frame) = reassembler.ingest_chunk(chunk).expect("Reassembly failed") {
                assembled_frames.push(frame);
            }
        }
    }

    // Verify acceptance criteria: 100% of frames assembled despite packet loss!
    assert_eq!(
        assembled_frames.len(),
        frame_count as usize,
        "Expected all {} frames assembled, but got {}",
        frame_count,
        assembled_frames.len()
    );

    // Verify jitter buffer reconstructed packets via FEC
    assert!(
        jb.stats().packets_reconstructed_fec > 0,
        "FEC should have reconstructed dropped packets (reconstructed: {})",
        jb.stats().packets_reconstructed_fec
    );
}
