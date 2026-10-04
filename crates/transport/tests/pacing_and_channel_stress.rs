use bytes::Bytes;
use linux_quest_protocol::{Packet, PacketHeader, PacketType};
use linux_quest_transport::{LoopbackEndpoint, PacketPacer, TransportEndpoint};
use std::sync::atomic::Ordering;

#[tokio::test]
async fn test_high_throughput_packet_burst() {
    let (mut sender, mut receiver) = LoopbackEndpoint::create_pair();

    let packet_count = 500;
    let payload = Bytes::from_static(&[0x42; 1024]);

    for seq in 1..=packet_count {
        let header = PacketHeader::new(
            PacketType::VideoFrameChunk,
            0,
            seq,
            seq as u64 * 100,
            &payload,
        );
        sender
            .send_packet(Packet::new(header, payload.clone()))
            .await
            .expect("Send failed");
    }

    for seq in 1..=packet_count {
        let packet = receiver.recv_packet().await.expect("Recv failed");
        assert_eq!(packet.header.sequence, seq);
        assert_eq!(packet.payload.len(), 1024);
    }

    assert_eq!(
        sender.stats().packets_sent.load(Ordering::Relaxed),
        packet_count as u64
    );
    assert_eq!(
        receiver.stats().packets_recv.load(Ordering::Relaxed),
        packet_count as u64
    );
}

#[tokio::test]
async fn test_packet_pacer_pacing_rate() {
    let mut pacer = PacketPacer::new(100); // 100 Mbps
    let start = std::time::Instant::now();

    // Pace 5 packets of 1400 bytes each
    for _ in 0..5 {
        pacer.pace(1400).await;
    }

    let elapsed = start.elapsed();
    // 5 * 1400 bytes = 7000 bytes = 56,000 bits. At 100 Mbps, ~0.56ms.
    assert!(elapsed.as_millis() < 50);
}
