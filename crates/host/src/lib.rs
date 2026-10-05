use linux_quest_capture::DisplayCapture;
use linux_quest_encoder::VideoEncoder;
use linux_quest_input::InputInjector;
use linux_quest_protocol::{
    Packet, PacketHeader, PacketType, ProtocolError, FLAG_INTRA_REFRESH, FLAG_KEYFRAME,
    FLAG_LAST_CHUNK,
};
use linux_quest_transport::TransportEndpoint;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum HostError {
    #[error("Capture failure: {0}")]
    Capture(#[from] linux_quest_capture::CaptureError),

    #[error("Encoder failure: {0}")]
    Encoder(#[from] linux_quest_encoder::EncoderError),

    #[error("Transport failure: {0}")]
    Transport(#[from] linux_quest_transport::TransportError),

    #[error("Protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("Input error: {0}")]
    Input(#[from] linux_quest_input::InputError),

    #[error("Session is not running")]
    NotRunning,
}

/// Host session managing the streaming lifecycle for a single display.
pub struct HostStreamSession<C, E, T, I>
where
    C: DisplayCapture,
    E: VideoEncoder,
    T: TransportEndpoint,
    I: InputInjector,
{
    capture: C,
    encoder: E,
    transport: T,
    input: I,
    stream_id: u16,
    sequence: u32,
    running: Arc<AtomicBool>,
}

impl<C, E, T, I> HostStreamSession<C, E, T, I>
where
    C: DisplayCapture,
    E: VideoEncoder,
    T: TransportEndpoint,
    I: InputInjector,
{
    pub fn new(capture: C, encoder: E, transport: T, input: I, stream_id: u16) -> Self {
        Self {
            capture,
            encoder,
            transport,
            input,
            stream_id,
            sequence: 0,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn running_flag(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    pub fn encoder(&self) -> &E {
        &self.encoder
    }

    pub fn encoder_mut(&mut self) -> &mut E {
        &mut self.encoder
    }

    /// Initialize capture and encoder backends.
    pub async fn init(
        &mut self,
        config: linux_quest_encoder::EncoderConfig,
    ) -> Result<(), HostError> {
        self.capture.init().await?;
        if let Err(e) = self.encoder.init(config).await {
            self.running.store(false, Ordering::SeqCst);
            return Err(HostError::Encoder(e));
        }
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Process one frame: Capture -> Hardware Encode -> Packetize -> Transport Send.
    pub async fn step_stream_frame(&mut self) -> Result<u64, HostError> {
        if !self.running.load(Ordering::Relaxed) {
            return Err(HostError::NotRunning);
        }
        let raw_frame = self.capture.capture_frame().await?;
        let encoded = self.encoder.encode(&raw_frame).await?;

        let total_chunks = encoded.chunks.len();
        for (idx, chunk) in encoded.chunks.into_iter().enumerate() {
            let serialized_payload = chunk.serialize()?;
            self.sequence += 1;

            let mut flags = 0;
            if encoded.is_keyframe {
                flags |= FLAG_KEYFRAME;
            }
            if encoded.is_intra_refresh {
                flags |= FLAG_INTRA_REFRESH;
            }
            if idx + 1 == total_chunks {
                flags |= FLAG_LAST_CHUNK;
            }

            let header = PacketHeader::new(
                PacketType::VideoFrameChunk,
                self.stream_id,
                self.sequence,
                raw_frame.pts_us,
                &serialized_payload,
            )
            .with_flags(flags);

            let packet = Packet::new(header, serialized_payload);
            self.transport.send_packet(packet).await?;
        }

        Ok(encoded.frame_id)
    }

    /// Process an incoming packet from the Quest client.
    pub async fn handle_incoming_packet(&mut self, packet: Packet) -> Result<(), HostError> {
        match packet.header.packet_type {
            PacketType::Ping => {
                let pong_header = PacketHeader::new(
                    PacketType::Pong,
                    self.stream_id,
                    self.sequence + 1,
                    packet.header.timestamp_us,
                    &packet.payload,
                );
                self.transport
                    .send_packet(Packet::new(pong_header, packet.payload))
                    .await?;
            }
            PacketType::ReferencePictureInvalidation => {
                if packet.payload.len() >= 8 {
                    let pts_us = u64::from_be_bytes(packet.payload[..8].try_into().unwrap());
                    self.encoder.invalidate_reference_picture(pts_us);
                }
            }
            PacketType::InputEvent => {
                if let Ok(event) = bincode::deserialize(&packet.payload) {
                    self.input.inject_event(event)?;
                }
            }
            PacketType::Disconnect => {
                self.running.store(false, Ordering::SeqCst);
            }
            _ => (),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_capture::SyntheticCapture;
    use linux_quest_encoder::{EncoderConfig, MockVideoEncoder};
    use linux_quest_input::MockInputInjector;
    use linux_quest_protocol::{ElementState, InputEvent, MouseButton};
    use linux_quest_transport::{LoopbackEndpoint, PacedUdpEndpoint, TcpEndpoint};

    #[tokio::test]
    async fn test_host_stream_single_frame() {
        let capture = SyntheticCapture::new(0, 1920, 1080, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
        let input = MockInputInjector::new();

        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
        session
            .init(EncoderConfig::default())
            .await
            .expect("HostStreamSession init failed");

        let frame_id = session
            .step_stream_frame()
            .await
            .expect("Step stream frame failed");
        assert_eq!(frame_id, 1);

        // Client receives packets
        let packet = ep_client.recv_packet().await.expect("Recv failed");
        assert_eq!(packet.header.packet_type, PacketType::VideoFrameChunk);
    }

    #[tokio::test]
    async fn test_host_stream_not_running_guard() {
        let capture = SyntheticCapture::new(0, 1920, 1080, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, _) = LoopbackEndpoint::create_pair();
        let input = MockInputInjector::new();

        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
        // Before calling init(), step_stream_frame should return NotRunning error
        let err = session
            .step_stream_frame()
            .await
            .expect_err("Should fail when not running");
        match err {
            HostError::NotRunning => (),
            other => panic!("Expected HostError::NotRunning, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_host_handle_input_event() {
        let capture = SyntheticCapture::new(0, 1920, 1080, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, _) = LoopbackEndpoint::create_pair();
        let input = MockInputInjector::new();

        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);

        let input_event = InputEvent::MouseButton {
            button: MouseButton::Right,
            state: ElementState::Pressed,
        };
        let payload = bytes::Bytes::from(bincode::serialize(&input_event).unwrap());
        let header = PacketHeader::new(PacketType::InputEvent, 0, 1, 1000, &payload);
        let packet = Packet::new(header, payload);

        session
            .handle_incoming_packet(packet)
            .await
            .expect("Handle input failed");
        assert_eq!(session.input.received_events.len(), 1);
        assert_eq!(session.input.received_events[0], input_event);
    }

    #[tokio::test]
    async fn test_host_stream_over_tcp_endpoint() {
        let capture = SyntheticCapture::new(0, 1280, 720, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, mut ep_client) = TcpEndpoint::create_connected_pair().await.unwrap();
        let input = MockInputInjector::new();

        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
        session
            .init(EncoderConfig::default())
            .await
            .expect("HostStreamSession init failed");

        let frame_id = session
            .step_stream_frame()
            .await
            .expect("Step stream frame failed");
        assert_eq!(frame_id, 1);

        let packet = ep_client.recv_packet().await.expect("Recv failed");
        assert_eq!(packet.header.packet_type, PacketType::VideoFrameChunk);
    }

    #[tokio::test]
    async fn test_host_stream_over_paced_udp_endpoint() {
        let capture = SyntheticCapture::new(0, 1280, 720, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, mut ep_client) = PacedUdpEndpoint::create_connected_pair(100).await.unwrap();
        let input = MockInputInjector::new();

        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);
        session
            .init(EncoderConfig::default())
            .await
            .expect("HostStreamSession init failed");

        let frame_id = session
            .step_stream_frame()
            .await
            .expect("Step stream frame failed");
        assert_eq!(frame_id, 1);

        let packet = ep_client.recv_packet().await.expect("Recv failed");
        assert_eq!(packet.header.packet_type, PacketType::VideoFrameChunk);
    }
}
