use std::time::{Duration, Instant};

use bytes::Bytes;
use linux_quest_client::config::{ClientConfig, ClientVideoCodec};
use linux_quest_client::decoder::mock::MockHardwareDecoder;
use linux_quest_client::decoder::HardwareVideoDecoder;
use linux_quest_client::receiver::ClientReceiver;
use linux_quest_protocol::packet::{Packet, PacketHeader, PacketType};
use linux_quest_protocol::video::{VideoChunk, VideoChunkMeta, VideoCodec};

#[test]
fn test_mock_decoder_av1_zero_copy_release() {
    let mut decoder = MockHardwareDecoder::new(1920, 1080, ClientVideoCodec::Av1);
    assert!(decoder
        .stats()
        .decoder_name
        .contains("c2.qti.av1.decoder.low_latency"));

    let av1_keyframe = vec![
        0x0A, 0x0B, 0x00, 0x00, 0x00, 0x1E, 0x0F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x32, 0x04,
        0x00, 0x00, 0x00, 0x01,
    ];

    // Queue keyframe
    decoder
        .queue_input_buffer(&av1_keyframe, 100_000, true)
        .expect("Failed to queue input");

    // Dequeue output frame
    let frame = decoder
        .dequeue_output_buffer(0)
        .expect("Failed to dequeue")
        .expect("Frame should be available");

    assert_eq!(frame.width, 1920);
    assert_eq!(frame.height, 1080);
    assert_eq!(frame.pts_us, 100_000);
    assert!(frame.is_keyframe);

    // Release with render=true (simulates zero-copy direct presentation)
    decoder
        .release_output_buffer(frame.buffer_index, true)
        .expect("Failed to release frame");

    let stats = decoder.stats();
    assert_eq!(stats.frames_decoded, 1);
    assert_eq!(stats.frames_rendered, 1);
    assert_eq!(stats.frames_dropped, 0);

    // Queue non-keyframe and release with render=false (simulates frame drop)
    let av1_inter = vec![0x32, 0x04, 0x00, 0x00, 0x00, 0x01];
    decoder
        .queue_input_buffer(&av1_inter, 111_111, false)
        .unwrap();
    let frame2 = decoder.dequeue_output_buffer(0).unwrap().unwrap();
    assert!(!frame2.is_keyframe);
    assert_eq!(frame2.pts_us, 111_111);

    decoder
        .release_output_buffer(frame2.buffer_index, false)
        .unwrap();

    let stats2 = decoder.stats();
    assert_eq!(stats2.frames_decoded, 2);
    assert_eq!(stats2.frames_rendered, 1);
    assert_eq!(stats2.frames_dropped, 1);
}

#[test]
fn test_mock_decoder_hevc_annex_b_parsing() {
    let mut decoder = MockHardwareDecoder::new(2560, 1440, ClientVideoCodec::Hevc);

    let hevc_keyframe = vec![
        0x00, 0x00, 0x00, 0x01, 0x40, 0x01, // VPS (type 32)
        0x00, 0x00, 0x00, 0x01, 0x42, 0x01, // SPS (type 33)
        0x00, 0x00, 0x00, 0x01, 0x44, 0x01, // PPS (type 34)
        0x00, 0x00, 0x00, 0x01, 0x26, 0x01, // IDR_W_RADL (type 19)
    ];

    decoder
        .queue_input_buffer(&hevc_keyframe, 50_000, true)
        .unwrap();
    let frame = decoder.dequeue_output_buffer(0).unwrap().unwrap();
    assert!(frame.is_keyframe);
    assert_eq!(frame.width, 2560);
    assert_eq!(frame.height, 1440);

    decoder
        .release_output_buffer(frame.buffer_index, true)
        .unwrap();
    assert_eq!(decoder.stats().frames_rendered, 1);
}

#[test]
fn test_receiver_multi_chunk_reassembly_into_decoder() {
    let config = ClientConfig::default();
    let mut receiver = ClientReceiver::new(&config);
    let mut decoder = MockHardwareDecoder::new(1920, 1080, ClientVideoCodec::Av1);

    let full_frame = vec![0xAA; 3000];

    // Split into 3 chunks of 1000 bytes
    let arrival = Instant::now();
    for (chunk_idx, chunk_data) in full_frame.chunks(1000).enumerate() {
        let meta = VideoChunkMeta {
            frame_id: 42,
            chunk_index: chunk_idx as u16,
            total_chunks: 3,
            codec: VideoCodec::Av1,
            is_keyframe: true,
            is_intra_refresh: false,
            width: 1920,
            height: 1080,
            fps: 90,
            pts_us: 10_000,
        };
        let chunk = VideoChunk::new(meta, Bytes::copy_from_slice(chunk_data));
        let chunk_bytes = chunk.serialize().unwrap();
        let header = PacketHeader::new(
            PacketType::VideoFrameChunk,
            1,
            (chunk_idx + 1) as u32,
            10_000,
            &chunk_bytes,
        );
        let packet = Packet::new(header, chunk_bytes);
        receiver.ingest_packet(packet, arrival);
    }

    // Pop ready frames after jitter buffer delay
    let playout_time = arrival + Duration::from_millis(10);
    let ready_frames = receiver.pop_ready_frames(playout_time).unwrap();
    assert_eq!(ready_frames.len(), 1);
    let reassembled = &ready_frames[0];
    assert_eq!(reassembled.bitstream.len(), 3000);
    assert_eq!(reassembled.meta.frame_id, 42);

    // Queue into decoder
    decoder
        .queue_input_buffer(
            &reassembled.bitstream,
            reassembled.meta.pts_us,
            reassembled.meta.is_keyframe,
        )
        .unwrap();

    let decoded = decoder.dequeue_output_buffer(0).unwrap().unwrap();
    assert_eq!(decoded.pts_us, 10_000);
    assert!(decoded.is_keyframe);

    decoder
        .release_output_buffer(decoded.buffer_index, true)
        .unwrap();
    assert_eq!(decoder.stats().frames_rendered, 1);
}
