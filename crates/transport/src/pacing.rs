use std::time::{Duration, Instant};

/// Packet pacer to prevent packet bursts from saturating Wi-Fi router buffers.
/// Uses a leaky-bucket pacing algorithm with sub-millisecond precision.
pub struct PacketPacer {
    rate_bytes_per_sec: u64,
    last_send_time: Instant,
    burst_allowance_bytes: usize,
    accumulated_bytes: usize,
}

impl PacketPacer {
    pub fn new(target_mbps: u32) -> Self {
        let rate_bytes_per_sec = (target_mbps as u64 * 1_000_000) / 8;
        // Default burst allowance: up to ~2 packets (2800 bytes)
        let burst_allowance_bytes = 2800;
        Self {
            rate_bytes_per_sec,
            last_send_time: Instant::now(),
            burst_allowance_bytes,
            accumulated_bytes: 0,
        }
    }

    /// Dynamically update target transmission bitrate.
    pub fn set_target_mbps(&mut self, target_mbps: u32) {
        self.rate_bytes_per_sec = (target_mbps as u64 * 1_000_000) / 8;
    }

    pub fn target_mbps(&self) -> u32 {
        ((self.rate_bytes_per_sec * 8) / 1_000_000) as u32
    }

    /// Wait for transmission slot according to current pacing rate.
    pub async fn pace(&mut self, packet_size_bytes: usize) {
        if self.rate_bytes_per_sec == 0 {
            return;
        }

        let now = Instant::now();
        let mut elapsed = now.duration_since(self.last_send_time);

        // Reset on large gaps (> 1 second) to prevent token arithmetic overflow and ensure freshness
        if elapsed > Duration::from_secs(1) {
            self.accumulated_bytes = 0;
            self.last_send_time = now;
            elapsed = Duration::ZERO;
        }

        let elapsed_nanos = elapsed.as_nanos();

        // Drain accumulated tokens according to elapsed time using u128 arithmetic
        let bytes_budget =
            ((elapsed_nanos * self.rate_bytes_per_sec as u128) / 1_000_000_000) as usize;
        self.accumulated_bytes = self.accumulated_bytes.saturating_sub(bytes_budget);
        self.accumulated_bytes += packet_size_bytes;

        if self.accumulated_bytes > self.burst_allowance_bytes {
            let excess = self.accumulated_bytes - self.burst_allowance_bytes;
            let delay_nanos =
                ((excess as u128 * 1_000_000_000) / self.rate_bytes_per_sec as u128) as u64;
            if delay_nanos > 0 {
                tokio::time::sleep(Duration::from_nanos(delay_nanos)).await;
            }
            self.last_send_time = Instant::now();
            self.accumulated_bytes = self.burst_allowance_bytes;
        } else {
            self.last_send_time = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_packet_pacer_rate_adjustment() {
        let mut pacer = PacketPacer::new(100);
        assert_eq!(pacer.target_mbps(), 100);

        pacer.set_target_mbps(200);
        assert_eq!(pacer.target_mbps(), 200);
    }

    #[tokio::test]
    async fn test_packet_pacer_pacing_delay() {
        let mut pacer = PacketPacer::new(50); // 50 Mbps
        let start = Instant::now();

        // Send 10 packets of 1400 bytes each
        for _ in 0..10 {
            pacer.pace(1400).await;
        }

        let elapsed = start.elapsed();
        // 14000 bytes at 50Mbps = 112,000 bits / 50,000,000 bps = 2.24ms.
        // Assert tighter bounds: at least 1.5ms (>65% of expected) and under 50ms.
        assert!(
            elapsed.as_micros() >= 1500,
            "Pacing should take at least 1.5ms, got {:?}",
            elapsed
        );
        assert!(elapsed.as_millis() < 50);
    }
}
