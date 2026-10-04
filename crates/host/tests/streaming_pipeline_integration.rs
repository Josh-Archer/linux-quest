use bytes::Bytes;
use linux_quest_capture::SyntheticCapture;
use linux_quest_encoder::{EncoderConfig, MockVideoEncoder};
use linux_quest_host::HostStreamSession;
use linux_quest_input::MockInputInjector;
use linux_quest_protocol::{
    ElementState, FrameReassembler, InputEvent, Packet, PacketHeader, PacketType, VideoChunk,
    VideoCodec,
};
use linux_quest_transport::{LoopbackEndpoint, TransportEndpoint};

#[tokio::test]
async fn test_full_pipeline_multi_frame_streaming_and_reassembly() {
    let capture = SyntheticCapture::new(0, 1920, 1080, 60);
    let encoder = MockVideoEncoder::new(EncoderConfig {
        codec: VideoCodec::Av1,
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_kbps: 50_000,
        intra_refresh_period: 5,
        max_chunk_size: 1024,
    });

    let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
    let input = MockInputInjector::new();

    let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
    let mut reassembler = FrameReassembler::new(8);

    // Stream 10 consecutive frames
    for expected_frame_id in 1..=10 {
        let frame_id = session
            .step_stream_frame()
            .await
            .expect("Frame step failed");
        assert_eq!(frame_id, expected_frame_id);
    }

    // Client receives all packets and reassembles them
    let mut completed_frames = 0;
    while let Ok(packet) = ep_client.recv_packet().await {
        assert_eq!(packet.header.packet_type, PacketType::VideoFrameChunk);

        let chunk = VideoChunk::deserialize(&packet.payload).expect("Chunk deserialization failed");
        if let Some(frame) = reassembler.ingest_chunk(chunk).expect("Reassembly failed") {
            completed_frames += 1;
            assert_eq!(frame.meta.frame_id, completed_frames);
            assert_eq!(frame.meta.codec, VideoCodec::Av1);
            if completed_frames == 1 {
                assert!(frame.meta.is_keyframe);
            }
        }

        if completed_frames == 10 {
            break;
        }
    }

    assert_eq!(completed_frames, 10);
}

#[tokio::test]
async fn test_bidirectional_control_and_input_flow() {
    let capture = SyntheticCapture::new(0, 1920, 1080, 60);
    let encoder = MockVideoEncoder::new(EncoderConfig::default());
    let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
    let input = MockInputInjector::new();

    let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);

    // 1. Client sends Ping, Host responds with Pong
    let ping_payload = Bytes::from_static(b"latency-probe-timestamp");
    let ping_packet = Packet::new(
        PacketHeader::new(PacketType::Ping, 0, 1, 555_000, &ping_payload),
        ping_payload.clone(),
    );

    session
        .handle_incoming_packet(ping_packet)
        .await
        .expect("Handle ping failed");

    let pong_response = ep_client.recv_packet().await.expect("Recv pong failed");
    assert_eq!(pong_response.header.packet_type, PacketType::Pong);
    assert_eq!(pong_response.payload, ping_payload);

    // 2. Client sends Input Events (Key press and Mouse Move)
    let key_event = InputEvent::KeyboardKey {
        keycode: 30, // KEY_A
        state: ElementState::Pressed,
    };
    let key_payload = Bytes::from(bincode::serialize(&key_event).unwrap());
    let key_packet = Packet::new(
        PacketHeader::new(PacketType::InputEvent, 0, 2, 600_000, &key_payload),
        key_payload,
    );

    session
        .handle_incoming_packet(key_packet)
        .await
        .expect("Handle key event failed");

    let mouse_event = InputEvent::MouseMoveRelative { dx: 15, dy: -20 };
    let mouse_payload = Bytes::from(bincode::serialize(&mouse_event).unwrap());
    let mouse_packet = Packet::new(
        PacketHeader::new(PacketType::InputEvent, 0, 3, 601_000, &mouse_payload),
        mouse_payload,
    );

    session
        .handle_incoming_packet(mouse_packet)
        .await
        .expect("Handle mouse event failed");

    // 3. Client sends Reference Picture Invalidation (RPI) feedback
    let rpi_frame_id = 7u64;
    let rpi_payload = Bytes::copy_from_slice(&rpi_frame_id.to_be_bytes());
    let rpi_packet = Packet::new(
        PacketHeader::new(
            PacketType::ReferencePictureInvalidation,
            0,
            4,
            602_000,
            &rpi_payload,
        ),
        rpi_payload,
    );

    session
        .handle_incoming_packet(rpi_packet)
        .await
        .expect("Handle RPI failed");
}

#[tokio::test]
async fn test_auto_hardware_pipeline_stream() {
    let capture = linux_quest_capture::AutoCapture::new(0, 1920, 1080, 90);
    let encoder_config = linux_quest_encoder::EncoderConfig {
        codec: VideoCodec::Av1,
        width: 1920,
        height: 1080,
        fps: 90,
        bitrate_kbps: 100_000,
        intra_refresh_period: 30,
        max_chunk_size: 1400,
    };
    let encoder = linux_quest_encoder::AutoVideoEncoder::new(encoder_config.clone());
    let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
    let input = MockInputInjector::new();

    let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
    session
        .init(encoder_config)
        .await
        .expect("Session init failed");

    let frame_id = session
        .step_stream_frame()
        .await
        .expect("Hardware stream frame failed");
    assert_eq!(frame_id, 1);

    let packet = ep_client.recv_packet().await.expect("Client recv failed");
    assert_eq!(packet.header.packet_type, PacketType::VideoFrameChunk);
}
