use async_trait::async_trait;
use bytes::Bytes;
use linux_quest_protocol::{Packet, PacketHeader, HEADER_SIZE};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

use crate::{TransportEndpoint, TransportError, TransportMode, TransportStats};

/// Maximum payload length allowed over TCP transport (16MB).
pub const MAX_TCP_PAYLOAD_SIZE: usize = 16 * 1024 * 1024;

/// High-throughput, ultra-low-latency TCP streaming endpoint optimized for USB ADB reverse-tethering.
/// Configured with TCP_NODELAY and tuned socket buffers to sustain 200-250 Mbps with sub-0.5ms jitter.
pub struct TcpEndpoint {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    peer_addr: Option<SocketAddr>,
    stats: Arc<TransportStats>,
}

impl TcpEndpoint {
    pub fn new(stream: TcpStream) -> Result<Self, TransportError> {
        stream.set_nodelay(true)?;

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = stream.as_raw_fd();
            let buf_size: libc::c_int = 4 * 1024 * 1024; // 4MB socket buffers
            unsafe {
                let rcv_res = libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&buf_size) as libc::socklen_t,
                );
                if rcv_res != 0 {
                    tracing::warn!(
                        "Failed to setsockopt SO_RCVBUF: {}",
                        std::io::Error::last_os_error()
                    );
                }

                let snd_res = libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&buf_size) as libc::socklen_t,
                );
                if snd_res != 0 {
                    tracing::warn!(
                        "Failed to setsockopt SO_SNDBUF: {}",
                        std::io::Error::last_os_error()
                    );
                }

                let mut actual_rcv: libc::c_int = 0;
                let mut len = std::mem::size_of_val(&actual_rcv) as libc::socklen_t;
                if libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &mut actual_rcv as *mut _ as *mut libc::c_void,
                    &mut len,
                ) == 0
                    && actual_rcv < buf_size
                {
                    tracing::debug!(
                        "Kernel granted SO_RCVBUF of {} bytes (clamped by net.core.rmem_max)",
                        actual_rcv
                    );
                }
            }
        }

        let peer_addr = stream.peer_addr().ok();
        let (reader, writer) = stream.into_split();

        Ok(Self {
            reader,
            writer,
            peer_addr,
            stats: Arc::new(TransportStats::default()),
        })
    }

    pub async fn connect(addr: &str) -> Result<Self, TransportError> {
        let stream = TcpStream::connect(addr).await?;
        Self::new(stream)
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }

    pub fn stats(&self) -> &TransportStats {
        &self.stats
    }

    /// Creates an interconnected pair of TCP endpoints over loopback for testing.
    pub async fn create_connected_pair() -> Result<(Self, Self), TransportError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let client_fut = TcpStream::connect(addr);
        let server_fut = listener.accept();

        let (client_res, server_res) = tokio::join!(client_fut, server_fut);
        let client_stream = client_res?;
        let (server_stream, _) = server_res?;

        let client_ep = Self::new(client_stream)?;
        let server_ep = Self::new(server_stream)?;

        Ok((client_ep, server_ep))
    }
}

#[async_trait]
impl TransportEndpoint for TcpEndpoint {
    async fn send_packet(&mut self, packet: Packet) -> Result<(), TransportError> {
        let wire_bytes = packet.to_bytes();
        let len = wire_bytes.len() as u64;

        self.writer
            .write_all(&wire_bytes)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset => {
                    TransportError::ConnectionClosed
                }
                _ => TransportError::Io(e),
            })?;

        self.stats.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_sent.fetch_add(len, Ordering::Relaxed);
        Ok(())
    }

    async fn recv_packet(&mut self) -> Result<Packet, TransportError> {
        let mut header_buf = [0u8; HEADER_SIZE];
        self.reader
            .read_exact(&mut header_buf)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
                    TransportError::ConnectionClosed
                }
                _ => TransportError::Io(e),
            })?;

        let mut slice = &header_buf[..];
        let header = PacketHeader::decode(&mut slice)?;

        let payload_len = header.payload_len as usize;
        if payload_len > MAX_TCP_PAYLOAD_SIZE {
            return Err(TransportError::Protocol(
                linux_quest_protocol::ProtocolError::PayloadLengthMismatch {
                    expected: MAX_TCP_PAYLOAD_SIZE,
                    actual: payload_len,
                },
            ));
        }
        let mut payload_buf = vec![0u8; payload_len];

        if payload_len > 0 {
            self.reader
                .read_exact(&mut payload_buf)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
                        TransportError::ConnectionClosed
                    }
                    _ => TransportError::Io(e),
                })?;
        }

        // Validate checksum
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&payload_buf);
        let calculated = hasher.finalize();

        if calculated != header.checksum {
            return Err(TransportError::Protocol(
                linux_quest_protocol::ProtocolError::ChecksumMismatch {
                    expected: header.checksum,
                    calculated,
                },
            ));
        }

        let packet = Packet::new(header, Bytes::from(payload_buf));

        let total_len = (HEADER_SIZE + payload_len) as u64;
        self.stats.packets_recv.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_recv
            .fetch_add(total_len, Ordering::Relaxed);
        Ok(packet)
    }

    fn mode(&self) -> TransportMode {
        TransportMode::UsbAdb
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_protocol::{PacketHeader, PacketType};

    #[tokio::test]
    async fn test_tcp_endpoint_send_recv() {
        let (mut client, mut server) = TcpEndpoint::create_connected_pair().await.unwrap();

        let payload = Bytes::from_static(b"usb-adb-tcp-stream-test");
        let header = PacketHeader::new(PacketType::Ping, 1, 99, 12345, &payload);
        let packet = Packet::new(header, payload.clone());

        client.send_packet(packet).await.unwrap();
        let received = server.recv_packet().await.unwrap();

        assert_eq!(received.header.packet_type, PacketType::Ping);
        assert_eq!(received.payload, payload);
        assert_eq!(client.mode(), TransportMode::UsbAdb);
    }
}
