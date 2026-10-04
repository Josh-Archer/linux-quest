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

    /// Process one frame: Capture -> Hardware Encode -> Packetize -> Transport Send.
    pub async fn step_stream_frame(&mut self) -> Result<u64, HostError> {
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
                    let frame_id = u64::from_be_bytes(packet.payload[..8].try_into().unwrap());
                    self.encoder.invalidate_reference_picture(frame_id);
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
    use linux_quest_transport::LoopbackEndpoint;

    #[tokio::test]
    async fn test_host_stream_single_frame() {
        let capture = SyntheticCapture::new(0, 1920, 1080, 60);
        let encoder = MockVideoEncoder::new(EncoderConfig::default());
        let (ep_host, mut ep_client) = LoopbackEndpoint::create_pair();
        let input = MockInputInjector::new();

        let mut session = HostStreamSession::new(capture, encoder, ep_host, input, 0);

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
}
