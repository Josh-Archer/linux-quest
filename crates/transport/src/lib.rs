use async_trait::async_trait;
use linux_quest_protocol::Packet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Error, Debug)]
pub enum TransportError {
    #[error("Connection closed")]
    ConnectionClosed,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Protocol error: {0}")]
    Protocol(#[from] linux_quest_protocol::ProtocolError),

    #[error("Packet too large: {size} bytes exceeds MTU {mtu}")]
    PacketTooLarge { size: usize, mtu: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    UsbAdb,
    UdpPaced,
    Loopback,
}

#[derive(Debug, Default)]
pub struct TransportStats {
    pub packets_sent: AtomicU64,
    pub packets_recv: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_recv: AtomicU64,
}

#[async_trait]
pub trait TransportEndpoint: Send + Sync {
    async fn send_packet(&mut self, packet: Packet) -> Result<(), TransportError>;
    async fn recv_packet(&mut self) -> Result<Packet, TransportError>;
    fn mode(&self) -> TransportMode;
}

/// In-memory loopback transport pair for testing pipeline latency and error recovery.
pub struct LoopbackEndpoint {
    sender: mpsc::UnboundedSender<Packet>,
    receiver: mpsc::UnboundedReceiver<Packet>,
    stats: Arc<TransportStats>,
}

impl Default for LoopbackEndpoint {
    fn default() -> Self {
        let (ep1, _) = Self::create_pair();
        ep1
    }
}

impl LoopbackEndpoint {
    pub fn create_pair() -> (Self, Self) {
        let (tx1, rx1) = mpsc::unbounded_channel();
        let (tx2, rx2) = mpsc::unbounded_channel();
        let stats1 = Arc::new(TransportStats::default());
        let stats2 = Arc::new(TransportStats::default());

        let ep1 = Self {
            sender: tx1,
            receiver: rx2,
            stats: stats1,
        };
        let ep2 = Self {
            sender: tx2,
            receiver: rx1,
            stats: stats2,
        };
        (ep1, ep2)
    }

    pub fn stats(&self) -> &TransportStats {
        &self.stats
    }
}

#[async_trait]
impl TransportEndpoint for LoopbackEndpoint {
    async fn send_packet(&mut self, packet: Packet) -> Result<(), TransportError> {
        let len = packet.payload.len() as u64;
        self.sender
            .send(packet)
            .map_err(|_| TransportError::ConnectionClosed)?;

        self.stats.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_sent.fetch_add(len, Ordering::Relaxed);
        Ok(())
    }

    async fn recv_packet(&mut self) -> Result<Packet, TransportError> {
        let packet = self
            .receiver
            .recv()
            .await
            .ok_or(TransportError::ConnectionClosed)?;

        let len = packet.payload.len() as u64;
        self.stats.packets_recv.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_recv.fetch_add(len, Ordering::Relaxed);
        Ok(packet)
    }

    fn mode(&self) -> TransportMode {
        TransportMode::Loopback
    }
}

/// Packet pacer to prevent packet bursts from saturating Wi-Fi buffers.
pub struct PacketPacer {
    rate_bytes_per_sec: u64,
    last_send_time: std::time::Instant,
}

impl PacketPacer {
    pub fn new(target_mbps: u32) -> Self {
        Self {
            rate_bytes_per_sec: (target_mbps as u64 * 1_000_000) / 8,
            last_send_time: std::time::Instant::now(),
        }
    }

    pub async fn pace(&mut self, packet_size_bytes: usize) {
        if self.rate_bytes_per_sec == 0 {
            return;
        }

        let delay_nanos = (packet_size_bytes as u64 * 1_000_000_000) / self.rate_bytes_per_sec;
        let target_time = self.last_send_time + Duration::from_nanos(delay_nanos);
        let now = std::time::Instant::now();

        if target_time > now {
            tokio::time::sleep(target_time - now).await;
            self.last_send_time = target_time;
        } else {
            self.last_send_time = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use linux_quest_protocol::{PacketHeader, PacketType};

    #[tokio::test]
    async fn test_loopback_transport_send_recv() {
        let (mut ep1, mut ep2) = LoopbackEndpoint::create_pair();

        let payload = Bytes::from_static(b"ping-packet");
        let header = PacketHeader::new(PacketType::Ping, 0, 1, 100, &payload);
        let packet = Packet::new(header, payload);

        ep1.send_packet(packet.clone()).await.expect("Send failed");
        let received = ep2.recv_packet().await.expect("Recv failed");

        assert_eq!(received.header.packet_type, PacketType::Ping);
        assert_eq!(received.payload, Bytes::from_static(b"ping-packet"));
        assert_eq!(ep1.stats().packets_sent.load(Ordering::Relaxed), 1);
        assert_eq!(ep2.stats().packets_recv.load(Ordering::Relaxed), 1);
    }
}
