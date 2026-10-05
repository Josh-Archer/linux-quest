use bytes::Bytes;
use linux_quest_protocol::{Packet, PacketHeader, PacketType};
use linux_quest_transport::{PacedUdpEndpoint, TransportEndpoint, TransportMode};
use std::sync::atomic::Ordering;
use std::time::Instant;

#[tokio::test]
async fn test_paced_udp_transport_streaming() {
    let (mut ep_sender, mut ep_receiver) =
        PacedUdpEndpoint::create_connected_pair(150).await.unwrap();
    assert_eq!(ep_sender.mode(), TransportMode::UdpPaced);

    let packet_count = 100;
    let payload = Bytes::from(vec![0x77; 1200]); // 1200 bytes fits within 1400 MTU

    let sender_task = tokio::spawn(async move {
        for seq in 1..=packet_count {
            let header = PacketHeader::new(
                PacketType::VideoFrameChunk,
                0,
                seq,
                seq as u64 * 100,
                &payload,
            );
            ep_sender
                .send_packet(Packet::new(header, payload.clone()))
                .await
                .unwrap();
        }
        ep_sender
    });

    let receiver_task = tokio::spawn(async move {
        for seq in 1..=packet_count {
            let packet = ep_receiver.recv_packet().await.unwrap();
            assert_eq!(packet.header.sequence, seq);
            assert_eq!(packet.payload.len(), 1200);
        }
        ep_receiver
    });

    let (sender_res, receiver_res) = tokio::join!(sender_task, receiver_task);
    let ep_sender = sender_res.unwrap();
    let ep_receiver = receiver_res.unwrap();

    assert_eq!(
        ep_sender.stats().packets_sent.load(Ordering::Relaxed),
        packet_count as u64
    );
    assert_eq!(
        ep_receiver.stats().packets_recv.load(Ordering::Relaxed),
        packet_count as u64
    );
}

#[tokio::test]
async fn test_paced_udp_pacing_rate_limiting() {
    let (mut ep_sender, mut ep_receiver) =
        PacedUdpEndpoint::create_connected_pair(50).await.unwrap();

    // 50 Mbps = 6.25 MB/s
    // Send 10 packets of 1282 wire bytes (32-byte header + 1250-byte payload) = 12,820 bytes = 102,560 bits.
    // With 2800-byte burst allowance, net paced bytes = 10,020 bytes, taking ~1.60ms at 50 Mbps (6.25 MB/s).
    let packet_count = 10;
    let payload = Bytes::from(vec![0x33; 1250]);

    let start = Instant::now();

    let sender = tokio::spawn(async move {
        for seq in 1..=packet_count {
            let header = PacketHeader::new(
                PacketType::VideoFrameChunk,
                0,
                seq,
                seq as u64 * 1000,
                &payload,
            );
            ep_sender
                .send_packet(Packet::new(header, payload.clone()))
                .await
                .unwrap();
        }
    });

    let receiver = tokio::spawn(async move {
        for _ in 1..=packet_count {
            let _ = ep_receiver.recv_packet().await.unwrap();
        }
    });

    tokio::try_join!(sender, receiver).unwrap();
    let elapsed = start.elapsed();

    // 10 packets of 1282 bytes wire (32-byte header + 1250-byte payload) = 12,820 bytes.
    // With 2800-byte burst allowance, net paced bytes = 10,020 bytes, taking ~1.60ms at 50 Mbps (6.25 MB/s).
    // Assert tighter bounds: at least 1.5ms and under 50ms.
    assert!(
        elapsed.as_micros() >= 1500,
        "Pacing should take at least 1.5ms, got {:?}",
        elapsed
    );
    assert!(elapsed.as_millis() < 50);
}
