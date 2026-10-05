use async_trait::async_trait;
use linux_quest_protocol::Packet;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::net::UdpSocket;

use crate::pacing::PacketPacer;
use crate::{TransportEndpoint, TransportError, TransportMode, TransportStats};

pub const DEFAULT_UDP_MTU: usize = 1400;

/// UDP network transport endpoint with hardware burst pacing and MTU protection.
pub struct PacedUdpEndpoint {
    socket: Arc<UdpSocket>,
    remote_addr: Option<SocketAddr>,
    pacer: PacketPacer,
    mtu: usize,
    stats: Arc<TransportStats>,
}

impl PacedUdpEndpoint {
    pub fn new(
        socket: UdpSocket,
        remote_addr: Option<SocketAddr>,
        target_mbps: u32,
        mtu: usize,
    ) -> Self {
        Self {
            socket: Arc::new(socket),
            remote_addr,
            pacer: PacketPacer::new(target_mbps),
            mtu,
            stats: Arc::new(TransportStats::default()),
        }
    }

    pub async fn bind(
        bind_addr: &str,
        target_mbps: u32,
        mtu: usize,
    ) -> Result<Self, TransportError> {
        let socket = UdpSocket::bind(bind_addr).await?;
        Ok(Self::new(socket, None, target_mbps, mtu))
    }

    pub fn set_remote_addr(&mut self, remote_addr: SocketAddr) {
        self.remote_addr = Some(remote_addr);
    }

    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.socket.local_addr()?)
    }

    pub fn pacer_mut(&mut self) -> &mut PacketPacer {
        &mut self.pacer
    }

    pub fn stats(&self) -> &TransportStats {
        &self.stats
    }

    /// Creates an interconnected pair of UDP endpoints over localhost for testing.
    pub async fn create_connected_pair(target_mbps: u32) -> Result<(Self, Self), TransportError> {
        let s1 = UdpSocket::bind("127.0.0.1:0").await?;
        let s2 = UdpSocket::bind("127.0.0.1:0").await?;

        let addr1 = s1.local_addr()?;
        let addr2 = s2.local_addr()?;

        let ep1 = Self::new(s1, Some(addr2), target_mbps, DEFAULT_UDP_MTU);
        let ep2 = Self::new(s2, Some(addr1), target_mbps, DEFAULT_UDP_MTU);

        Ok((ep1, ep2))
    }
}

#[async_trait]
impl TransportEndpoint for PacedUdpEndpoint {
    async fn send_packet(&mut self, packet: Packet) -> Result<(), TransportError> {
        let wire_bytes = packet.to_bytes();
        let size = wire_bytes.len();

        if size > self.mtu {
            return Err(TransportError::PacketTooLarge {
                size,
                mtu: self.mtu,
            });
        }

        // Wait for packet pacing slot
        self.pacer.pace(size).await;

        if let Some(target) = self.remote_addr {
            self.socket.send_to(&wire_bytes, target).await?;
        } else {
            self.socket.send(&wire_bytes).await?;
        }

        self.stats.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_sent
            .fetch_add(size as u64, Ordering::Relaxed);
        Ok(())
    }

    async fn recv_packet(&mut self) -> Result<Packet, TransportError> {
        let mut buf = vec![0u8; 65535];
        loop {
            let (len, sender_addr) = self.socket.recv_from(&mut buf).await?;

            if let Some(expected) = self.remote_addr {
                if sender_addr != expected {
                    // Drop datagrams from unexpected sources
                    continue;
                }
            } else {
                self.remote_addr = Some(sender_addr);
            }

            let packet = Packet::from_bytes(&buf[..len])?;

            self.stats.packets_recv.fetch_add(1, Ordering::Relaxed);
            self.stats
                .bytes_recv
                .fetch_add(len as u64, Ordering::Relaxed);
            return Ok(packet);
        }
    }

    fn mode(&self) -> TransportMode {
        TransportMode::UdpPaced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use linux_quest_protocol::{PacketHeader, PacketType};

    #[tokio::test]
    async fn test_udp_endpoint_send_recv() {
        let (mut ep1, mut ep2) = PacedUdpEndpoint::create_connected_pair(100).await.unwrap();

        let payload = Bytes::from_static(b"udp-datagram-test");
        let header = PacketHeader::new(PacketType::Ping, 1, 42, 100, &payload);
        let packet = Packet::new(header, payload.clone());

        ep1.send_packet(packet).await.expect("Send failed");
        let received = ep2.recv_packet().await.expect("Recv failed");

        assert_eq!(received.header.packet_type, PacketType::Ping);
        assert_eq!(received.payload, payload);
        assert_eq!(ep1.stats().packets_sent.load(Ordering::Relaxed), 1);
        assert_eq!(ep2.stats().packets_recv.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_udp_endpoint_mtu_enforcement() {
        let (mut ep1, _ep2) = PacedUdpEndpoint::create_connected_pair(100).await.unwrap();

        // Create packet exceeding 1400 MTU
        let large_payload = Bytes::from(vec![0xAA; 1500]);
        let header = PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 100, &large_payload);
        let packet = Packet::new(header, large_payload);

        let err = ep1
            .send_packet(packet)
            .await
            .expect_err("Should exceed MTU");
        match err {
            TransportError::PacketTooLarge { size, mtu } => {
                assert!(size > 1400);
                assert_eq!(mtu, 1400);
            }
            other => panic!("Expected PacketTooLarge, got {:?}", other),
        }
    }
}
