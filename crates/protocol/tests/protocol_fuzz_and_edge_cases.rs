use bytes::Bytes;
use linux_quest_protocol::{
    FrameReassembler, Packet, PacketHeader, PacketType, ProtocolError, VideoChunk, VideoChunkMeta,
    VideoCodec, HEADER_SIZE, PROTOCOL_MAGIC, PROTOCOL_VERSION,
};

#[test]
fn test_truncated_header_resilience() {
    let payload = b"critical test payload";
    let header = PacketHeader::new(PacketType::Ping, 1, 10, 1000, payload);
    let packet = Packet::new(header, Bytes::from_static(payload));
    let wire = packet.to_bytes();

    // Any truncation of the 32-byte header must return BufferTooShort, never panic
    for cut in 0..HEADER_SIZE {
        let truncated = &wire[..cut];
        let err = Packet::from_bytes(truncated).expect_err("Must error on truncated header");
        match err {
            ProtocolError::BufferTooShort { required, found } => {
                assert_eq!(required, HEADER_SIZE);
                assert_eq!(found, cut);
            }
            other => panic!("Unexpected error on cut {cut}: {other:?}"),
        }
    }
}

#[test]
fn test_invalid_magic_rejection() {
    let mut header_bytes = [0u8; HEADER_SIZE];
    header_bytes[0..4].copy_from_slice(b"NOPE");
    header_bytes[4] = PROTOCOL_VERSION;
    header_bytes[5] = PacketType::Ping as u8;

    let mut slice = &header_bytes[..];
    let err = PacketHeader::decode(&mut slice).expect_err("Invalid magic must fail");
    assert_eq!(
        err,
        ProtocolError::InvalidMagic {
            expected: PROTOCOL_MAGIC,
            found: *b"NOPE"
        }
    );
}

#[test]
fn test_unsupported_version_rejection() {
    let mut header_bytes = [0u8; HEADER_SIZE];
    header_bytes[0..4].copy_from_slice(&PROTOCOL_MAGIC);
    header_bytes[4] = 99; // Future unsupported version
    header_bytes[5] = PacketType::Ping as u8;

    let mut slice = &header_bytes[..];
    let err = PacketHeader::decode(&mut slice).expect_err("Invalid version must fail");
    assert_eq!(err, ProtocolError::UnsupportedVersion(99));
}

#[test]
fn test_checksum_mutation_fuzzing() {
    let payload = Bytes::from_static(b"video-stream-compressed-slice-with-padding-1234567890");
    let header = PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 2000, &payload);
    let original = Packet::new(header, payload).to_bytes();

    // Flip bits across different locations in payload
    for offset in HEADER_SIZE..original.len() {
        let mut corrupted = original.to_vec();
        corrupted[offset] ^= 0x01; // flip single bit

        let err = Packet::from_bytes(&corrupted).expect_err("Corrupted payload must fail checksum");
        match err {
            ProtocolError::ChecksumMismatch { .. } => (),
            other => panic!("Expected ChecksumMismatch at offset {offset}, got {other:?}"),
        }
    }
}

#[test]
fn test_reassembler_eviction_under_packet_loss() {
    let mut reassembler = FrameReassembler::new(3);

    // Send chunk 0 of frame 1 (never send chunk 1, simulating lost frame)
    let meta1 = VideoChunkMeta {
        frame_id: 1,
        chunk_index: 0,
        total_chunks: 2,
        codec: VideoCodec::Av1,
        is_keyframe: true,
        is_intra_refresh: false,
        width: 1920,
        height: 1080,
        fps: 60,
        pts_us: 1000,
    };
    reassembler
        .ingest_chunk(VideoChunk::new(meta1, Bytes::from_static(b"f1-incomplete")))
        .unwrap();

    // Now send frames 2, 3, 4, 5 (complete frames)
    for fid in 2..=5 {
        let meta = VideoChunkMeta {
            frame_id: fid,
            chunk_index: 0,
            total_chunks: 1,
            codec: VideoCodec::Av1,
            is_keyframe: false,
            is_intra_refresh: false,
            width: 1920,
            height: 1080,
            fps: 60,
            pts_us: fid * 1000,
        };
        let frame = reassembler
            .ingest_chunk(VideoChunk::new(meta, Bytes::from_static(b"complete")))
            .unwrap()
            .expect("Should complete single-chunk frame");
        assert_eq!(frame.meta.frame_id, fid);
    }

    // Frame 1 should have been evicted to maintain max_active_frames bounds
    assert!(reassembler.active_frame_count() <= 3);
}

#[test]
fn test_reassembler_corrupt_chunk_index() {
    let mut reassembler = FrameReassembler::new(4);
    let corrupt_meta = VideoChunkMeta {
        frame_id: 10,
        chunk_index: 5, // index 5 when total is 2
        total_chunks: 2,
        codec: VideoCodec::Av1,
        is_keyframe: false,
        is_intra_refresh: false,
        width: 1920,
        height: 1080,
        fps: 60,
        pts_us: 1000,
    };

    let err = reassembler
        .ingest_chunk(VideoChunk::new(corrupt_meta, Bytes::from_static(b"bad")))
        .expect_err("Should reject chunk_index >= total_chunks");

    match err {
        ProtocolError::CorruptChunkIndex { chunk, total } => {
            assert_eq!(chunk, 5);
            assert_eq!(total, 2);
        }
        other => panic!("Expected CorruptChunkIndex, got {other:?}"),
    }
}
