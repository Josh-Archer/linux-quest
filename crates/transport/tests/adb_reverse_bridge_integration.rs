use bytes::Bytes;
use linux_quest_protocol::{Packet, PacketHeader, PacketType};
use linux_quest_transport::adb::{
    AdbBridge, AdbBridgeConfig, AdbBridgeEvent, AdbDevice, AdbDeviceState, MockAdbRunner,
};
use linux_quest_transport::{TcpEndpoint, TransportEndpoint, TransportMode};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[tokio::test]
async fn test_adb_reverse_bridge_lifecycle() {
    let runner = MockAdbRunner::new();
    let config = AdbBridgeConfig {
        remote_port: 8088,
        local_port: 8088,
        poll_interval: Duration::from_millis(5),
        max_reconnect_attempts: 5,
        require_quest_filter: true,
        target_serial: None,
    };

    let bridge = AdbBridge::new(runner.clone(), config);
    let (handle, mut rx) = bridge.spawn_monitor();

    // 1. Initial state: no devices connected
    tokio::time::sleep(Duration::from_millis(20)).await;

    // 2. Connect Meta Quest 3 headset
    runner.add_device(AdbDevice {
        serial: "1WMHH812345678".to_string(),
        state: AdbDeviceState::Device,
        product: Some("eureka".to_string()),
        model: Some("Quest_3".to_string()),
        device_name: Some("eureka".to_string()),
        transport_id: Some("1".to_string()),
    });

    // Expect Headset connection and reverse established
    let mut reverse_established = false;
    let timeout = Instant::now() + Duration::from_secs(2);
    while Instant::now() < timeout {
        if let Ok(AdbBridgeEvent::ReversePortEstablished { serial, port }) = rx.recv().await {
            assert_eq!(serial, "1WMHH812345678");
            assert_eq!(port, 8088);
            reverse_established = true;
            break;
        }
    }
    assert!(
        reverse_established,
        "Failed to establish ADB reverse port forwarding"
    );

    // Verify reverse rule registered in ADB
    let rules = runner.get_rules("1WMHH812345678");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].remote_port, 8088);
    assert_eq!(rules[0].local_port, 8088);

    // 3. Simulate USB cable bump / accidental disconnect
    runner.remove_device("1WMHH812345678");

    let mut bump_detected = false;
    let timeout = Instant::now() + Duration::from_secs(2);
    while Instant::now() < timeout {
        if let Ok(AdbBridgeEvent::CableBumpDetected { serial }) = rx.recv().await {
            assert_eq!(serial, "1WMHH812345678");
            bump_detected = true;
            break;
        }
    }
    assert!(bump_detected, "Failed to detect cable bump / disconnect");

    // 4. Cable re-plugged: automatic reconnection loop must recover
    runner.add_device(AdbDevice {
        serial: "1WMHH812345678".to_string(),
        state: AdbDeviceState::Device,
        product: Some("eureka".to_string()),
        model: Some("Quest_3".to_string()),
        device_name: Some("eureka".to_string()),
        transport_id: Some("2".to_string()),
    });

    let mut reconnected = false;
    let timeout = Instant::now() + Duration::from_secs(2);
    while Instant::now() < timeout {
        if let Ok(AdbBridgeEvent::ReconnectionSuccessful { serial }) = rx.recv().await {
            assert_eq!(serial, "1WMHH812345678");
            reconnected = true;
            break;
        }
    }
    assert!(
        reconnected,
        "Failed to automatically recover and re-establish reverse tunnel"
    );

    handle.stop();
}

#[tokio::test]
async fn test_tcp_endpoint_high_throughput_and_sub_millisecond_jitter() {
    let (mut client_ep, mut server_ep) = TcpEndpoint::create_connected_pair().await.unwrap();
    assert_eq!(client_ep.mode(), TransportMode::UsbAdb);

    // Stream 1,000 video packets (simulating high-bitrate desktop stream @ 200-250 Mbps)
    // 1,000 packets of 32 KB each = 32 MB = 256 Megabits
    let chunk_size = 32 * 1024;
    let payload = Bytes::from(vec![0x5A; chunk_size]);
    let packet_count = 1000;

    let base_instant = Instant::now();

    let sender_task = tokio::spawn(async move {
        for seq in 1..=packet_count {
            let send_time = Instant::now();
            let timestamp_us = send_time.duration_since(base_instant).as_micros() as u64;
            let header =
                PacketHeader::new(PacketType::VideoFrameChunk, 0, seq, timestamp_us, &payload);
            client_ep
                .send_packet(Packet::new(header, payload.clone()))
                .await
                .unwrap();
        }
        client_ep
    });

    let mut rfc3550_jitter_us = 0.0f64;
    let mut prev_transit: Option<f64> = None;
    let stream_start = Instant::now();

    for seq in 1..=packet_count {
        let packet = server_ep.recv_packet().await.unwrap();
        let arrival_time = Instant::now();
        let arrival_us = arrival_time.duration_since(base_instant).as_micros() as f64;
        let send_us = packet.header.timestamp_us as f64;
        let transit_us = arrival_us - send_us;

        assert_eq!(packet.header.sequence, seq);
        assert_eq!(packet.payload.len(), chunk_size);

        if let Some(prev) = prev_transit {
            // RFC 3550 Interarrival Jitter Calculation:
            // D(i, j) = (R_j - S_j) - (R_i - S_i) = transit_j - transit_i
            // J = J + (|D| - J) / 16.0
            let d = (transit_us - prev).abs();
            rfc3550_jitter_us += (d - rfc3550_jitter_us) / 16.0;
        }
        prev_transit = Some(transit_us);
    }

    let elapsed = stream_start.elapsed();
    let _client_ep = sender_task.await.unwrap();

    let total_bytes = (packet_count as u64) * (32 + chunk_size as u64);
    assert_eq!(
        server_ep.stats().packets_recv.load(Ordering::Relaxed),
        packet_count as u64
    );
    assert_eq!(
        server_ep.stats().bytes_recv.load(Ordering::Relaxed),
        total_bytes
    );

    // Compute measured throughput in Mbps
    let throughput_mbps = (total_bytes as f64 * 8.0) / (elapsed.as_secs_f64() * 1_000_000.0);

    // Verify high throughput sustained (> 200 Mbps)
    assert!(
        throughput_mbps > 200.0,
        "Measured throughput {:.2} Mbps below 200 Mbps target",
        throughput_mbps
    );

    // Verify sub-0.5ms (500us) RFC 3550 transport transit jitter
    assert!(
        rfc3550_jitter_us < 500.0,
        "RFC 3550 transit jitter {:.2}us exceeds 500us sub-millisecond target",
        rfc3550_jitter_us
    );
}
