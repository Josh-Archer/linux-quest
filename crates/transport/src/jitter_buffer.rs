use bytes::Bytes;
use linux_quest_protocol::{Packet, PacketType, FLAG_FEC_PROTECTED};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::fec::{reed_solomon::ReedSolomonFec, xor::XorFec, FecHeader, FecScheme};

#[derive(Debug, Clone, Copy)]
pub struct JitterBufferConfig {
    pub min_delay: Duration,
    pub max_delay: Duration,
    pub jitter_multiplier: f64,
    pub rtt_multiplier: f64,
}

impl Default for JitterBufferConfig {
    fn default() -> Self {
        Self {
            min_delay: Duration::from_micros(1_500), // 1.5ms for low-latency streaming
            max_delay: Duration::from_micros(20_000), // 20ms cap to prevent lag accumulation
            jitter_multiplier: 2.0,                  // 2-sigma jitter guard
            rtt_multiplier: 0.1,                     // 10% RTT contribution
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct JitterBufferStats {
    pub current_jitter_us: f64,
    pub smoothed_rtt_us: f64,
    pub current_target_delay_us: u64,
    pub packets_received: u64,
    pub packets_delivered: u64,
    pub packets_lost: u64,
    pub packets_reconstructed_fec: u64,
}

impl JitterBufferStats {
    pub fn loss_rate(&self) -> f64 {
        let total = self.packets_received + self.packets_lost;
        if total == 0 {
            0.0
        } else {
            self.packets_lost as f64 / total as f64
        }
    }
}

struct QueuedPacket {
    packet: Packet,
    arrival_instant: Instant,
}

struct FecBlockTracker {
    scheme: FecScheme,
    k: usize,
    m: usize,
    sources: HashMap<usize, (u32, Packet)>, // index_in_block -> (seq, Packet)
    parities: Vec<(usize, Bytes)>,          // (parity_index, raw_bytes)
}

/// Maximum number of active FEC blocks tracked simultaneously in memory.
pub const MAX_FEC_BLOCKS: usize = 64;

/// Dynamic, adaptive jitter buffer with RFC 3550 jitter calculation and integrated FEC recovery.
pub struct AdaptiveJitterBuffer {
    config: JitterBufferConfig,
    jitter_us: f64,
    smoothed_rtt_us: f64,
    last_arrival_instant: Option<Instant>,
    last_sender_timestamp_us: Option<u64>,
    expected_sequence: Option<u32>,

    queue: BTreeMap<u32, QueuedPacket>,
    fec_blocks: HashMap<u64, FecBlockTracker>,

    stats: JitterBufferStats,
}

impl AdaptiveJitterBuffer {
    pub fn new(config: JitterBufferConfig) -> Self {
        Self {
            config,
            jitter_us: 0.0,
            smoothed_rtt_us: 0.0,
            last_arrival_instant: None,
            last_sender_timestamp_us: None,
            expected_sequence: None,
            queue: BTreeMap::new(),
            fec_blocks: HashMap::new(),
            stats: JitterBufferStats::default(),
        }
    }

    pub fn stats(&self) -> &JitterBufferStats {
        &self.stats
    }

    /// Update smoothed RTT estimate via Exponentially Weighted Moving Average (EWMA, alpha = 0.875).
    pub fn update_rtt(&mut self, rtt_sample: Duration) {
        let sample_us = rtt_sample.as_micros() as f64;
        if self.smoothed_rtt_us == 0.0 {
            self.smoothed_rtt_us = sample_us;
        } else {
            // EWMA with alpha = 0.875 (RFC 6298 / Jacobson's algorithm)
            self.smoothed_rtt_us = 0.875 * self.smoothed_rtt_us + 0.125 * sample_us;
        }
        self.stats.smoothed_rtt_us = self.smoothed_rtt_us;
    }

    /// Computes the dynamically adjusted playout delay target based on current network jitter and RTT.
    pub fn current_target_delay(&self) -> Duration {
        let jitter_contrib = self.jitter_us * self.config.jitter_multiplier;
        let rtt_contrib = self.smoothed_rtt_us * self.config.rtt_multiplier;
        let dynamic_us = (jitter_contrib + rtt_contrib).round() as u64;

        let dynamic_dur = Duration::from_micros(dynamic_us);
        dynamic_dur.clamp(self.config.min_delay, self.config.max_delay)
    }

    /// Ingests an arriving packet into the jitter buffer, updating jitter statistics and FEC blocks.
    pub fn ingest_packet(&mut self, packet: Packet, now: Instant) {
        self.stats.packets_received += 1;

        // RFC 3550 Interarrival Jitter Calculation:
        // D(i, j) = (R_j - S_j) - (R_i - S_i)
        // J = J + (|D| - J) / 16
        if let (Some(last_arr), Some(last_ts)) =
            (self.last_arrival_instant, self.last_sender_timestamp_us)
        {
            let elapsed_arr_us = now.duration_since(last_arr).as_micros() as f64;
            let elapsed_sender_us = (packet.header.timestamp_us as f64) - (last_ts as f64);
            let d = (elapsed_arr_us - elapsed_sender_us).abs();

            self.jitter_us += (d - self.jitter_us) / 16.0;
            self.stats.current_jitter_us = self.jitter_us;
        }

        self.last_arrival_instant = Some(now);
        self.last_sender_timestamp_us = Some(packet.header.timestamp_us);

        let target_delay = self.current_target_delay();
        self.stats.current_target_delay_us = target_delay.as_micros() as u64;

        if self.expected_sequence.is_none() {
            self.expected_sequence = Some(packet.header.sequence);
        }

        let seq = packet.header.sequence;

        // Handle FEC parity packet
        if packet.header.packet_type == PacketType::FecParity {
            let mut slice = &packet.payload[..];
            if let Ok(fec_header) = FecHeader::decode(&mut slice) {
                if !self.fec_blocks.contains_key(&fec_header.block_id)
                    && self.fec_blocks.len() >= MAX_FEC_BLOCKS
                {
                    if let Some(&oldest) = self.fec_blocks.keys().min() {
                        self.fec_blocks.remove(&oldest);
                    }
                }

                let tracker = self
                    .fec_blocks
                    .entry(fec_header.block_id)
                    .or_insert_with(|| FecBlockTracker {
                        scheme: fec_header.scheme,
                        k: fec_header.source_count as usize,
                        m: fec_header.parity_count as usize,
                        sources: HashMap::new(),
                        parities: Vec::new(),
                    });

                tracker.scheme = fec_header.scheme;
                tracker.k = fec_header.source_count as usize;
                tracker.m = fec_header.parity_count as usize;
                let p_idx = fec_header.parity_index as usize;
                if tracker.m > 0 && p_idx < tracker.m {
                    tracker.parities.push((p_idx, packet.payload.clone()));
                    self.try_fec_recovery(fec_header.block_id, now);
                }
            }
            return;
        }

        // Check if packet belongs to an FEC protected block
        if (packet.header.flags & FLAG_FEC_PROTECTED) != 0 {
            let block_id = packet.header.timestamp_us; // Group by frame/block timestamp
            if !self.fec_blocks.contains_key(&block_id) && self.fec_blocks.len() >= MAX_FEC_BLOCKS {
                if let Some(&oldest) = self.fec_blocks.keys().min() {
                    self.fec_blocks.remove(&oldest);
                }
            }

            let tracker = self
                .fec_blocks
                .entry(block_id)
                .or_insert_with(|| FecBlockTracker {
                    scheme: FecScheme::ReedSolomon,
                    k: 0,
                    m: 0,
                    sources: HashMap::new(),
                    parities: Vec::new(),
                });

            let idx = packet.header.fec_index() as usize;
            if tracker.k == 0 || idx < tracker.k {
                tracker.sources.insert(idx, (seq, packet.clone()));
                self.try_fec_recovery(block_id, now);
            }
        }

        self.queue.insert(
            seq,
            QueuedPacket {
                packet,
                arrival_instant: now,
            },
        );
    }

    /// Registers FEC block parameters for a block of source packets with default ReedSolomon scheme.
    pub fn register_fec_block(&mut self, block_id: u64, k: usize, m: usize) {
        self.register_fec_block_with_scheme(block_id, k, m, FecScheme::ReedSolomon);
    }

    /// Registers FEC block parameters with explicit FEC scheme.
    pub fn register_fec_block_with_scheme(
        &mut self,
        block_id: u64,
        k: usize,
        m: usize,
        scheme: FecScheme,
    ) {
        let tracker = self
            .fec_blocks
            .entry(block_id)
            .or_insert_with(|| FecBlockTracker {
                scheme,
                k,
                m,
                sources: HashMap::new(),
                parities: Vec::new(),
            });
        tracker.scheme = scheme;
        tracker.k = k;
        tracker.m = m;
    }

    /// Attempts FEC recovery on a block if enough data + parity packets are available.
    pub fn try_fec_recovery(&mut self, block_id: u64, now: Instant) -> bool {
        let tracker = match self.fec_blocks.get_mut(&block_id) {
            Some(t) if t.k > 0 => t,
            _ => return false,
        };

        let k = tracker.k;
        let m = tracker.m;
        let available = tracker.sources.len() + tracker.parities.len();

        if tracker.sources.len() == k {
            self.fec_blocks.remove(&block_id);
            return false;
        }

        if available < k {
            return false;
        }

        let mut source_payloads: HashMap<usize, Bytes> =
            HashMap::with_capacity(tracker.sources.len());
        for (&idx, (_seq, pkt)) in &tracker.sources {
            source_payloads.insert(idx, pkt.payload.clone());
        }

        let recovered = match tracker.scheme {
            FecScheme::Xor => {
                let parity = tracker.parities.first().map(|p| &p.1);
                XorFec::decode(block_id, k, source_payloads, parity)
            }
            FecScheme::ReedSolomon => {
                ReedSolomonFec::decode(block_id, k, m, source_payloads, &tracker.parities)
            }
        };

        if let Ok(all_sources) = recovered {
            // Find reference header to synthesize packet headers for recovered packets
            let ref_header = tracker
                .sources
                .values()
                .next()
                .map(|(_, p)| p.header.clone());

            if let Some(ref header) = ref_header {
                let base_seq = header.sequence.saturating_sub(header.fec_index() as u32);
                for (idx, payload) in all_sources.into_iter().enumerate() {
                    if let std::collections::hash_map::Entry::Vacant(e) = tracker.sources.entry(idx)
                    {
                        let mut rec_header = header.clone();
                        rec_header.sequence = base_seq.saturating_add(idx as u32);
                        rec_header = rec_header.with_fec_index(idx as u16);
                        rec_header.payload_len = payload.len() as u32;
                        rec_header.checksum = crc32fast::hash(&payload);

                        let recovered_packet = Packet::new(rec_header, payload);
                        self.queue.insert(
                            recovered_packet.header.sequence,
                            QueuedPacket {
                                packet: recovered_packet.clone(),
                                arrival_instant: now,
                            },
                        );
                        e.insert((recovered_packet.header.sequence, recovered_packet));
                        self.stats.packets_reconstructed_fec += 1;
                    }
                }
                self.fec_blocks.remove(&block_id);
                return true;
            }
        }

        false
    }

    /// Pops the next packet ready for playout if its target delay has elapsed.
    pub fn pop_ready_packet(&mut self, now: Instant) -> Option<Packet> {
        let target_delay = self.current_target_delay();

        let (seq, ready) = {
            let (first_seq, item) = self.queue.iter().next()?;
            let age = now.duration_since(item.arrival_instant);
            (*first_seq, age >= target_delay)
        };

        if ready {
            let item = self.queue.remove(&seq)?;

            if let Some(expected) = self.expected_sequence {
                if seq > expected {
                    self.stats.packets_lost += (seq - expected) as u64;
                    self.expected_sequence = Some(seq + 1);
                } else if seq == expected {
                    self.expected_sequence = Some(seq + 1);
                }
            }

            self.stats.packets_delivered += 1;
            Some(item.packet)
        } else {
            None
        }
    }

    /// Evicts ancient packets if queue exceeds maximum threshold to avoid bufferbloat.
    pub fn evict_stale(&mut self, max_depth: usize) {
        while self.queue.len() > max_depth {
            if let Some((&first_seq, _)) = self.queue.iter().next() {
                self.queue.remove(&first_seq);
                self.stats.packets_lost += 1;
            } else {
                break;
            }
        }
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_protocol::{PacketHeader, PacketType};

    #[test]
    fn test_jitter_buffer_in_order_delivery() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        let payload1 = Bytes::from_static(b"frame-chunk-1");
        let header1 = PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 1000, &payload1);
        jb.ingest_packet(Packet::new(header1, payload1), now);

        // Before target delay, packet is not ready
        assert!(jb.pop_ready_packet(now).is_none());

        // After target delay, packet pops
        let playout_time = now + Duration::from_millis(5);
        let popped = jb.pop_ready_packet(playout_time).expect("Should be ready");
        assert_eq!(popped.header.sequence, 1);
    }

    #[test]
    fn test_jitter_buffer_reordering() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 1000, b"p1"),
            Bytes::from_static(b"p1"),
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 2, 2000, b"p2"),
            Bytes::from_static(b"p2"),
        );

        // Arrive out of order: 2 then 1
        jb.ingest_packet(p2, now);
        jb.ingest_packet(p1, now);

        let playout = now + Duration::from_millis(5);
        let out1 = jb.pop_ready_packet(playout).unwrap();
        let out2 = jb.pop_ready_packet(playout).unwrap();

        assert_eq!(out1.header.sequence, 1);
        assert_eq!(out2.header.sequence, 2);
    }

    #[test]
    fn test_jitter_buffer_fec_recovery() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        let block_id = 999;
        let s0 = Bytes::from_static(b"slice-0");
        let s1 = Bytes::from_static(b"slice-1");
        let s2 = Bytes::from_static(b"slice-2");

        let sources = vec![s0.clone(), s1.clone(), s2.clone()];
        let parities = ReedSolomonFec::encode(block_id, &sources, 1).unwrap();

        jb.register_fec_block(block_id, 3, 1);

        // Ingest s0 and s2 (s1 dropped), plus parity 0
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, &s0)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s0,
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 12, block_id, &s2)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(2),
            s2,
        );

        let parity_pkt = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 13, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );

        jb.ingest_packet(p0, now);
        jb.ingest_packet(p2, now);
        jb.ingest_packet(parity_pkt, now);

        assert_eq!(jb.stats().packets_reconstructed_fec, 1);
        assert_eq!(jb.queue_len(), 3); // s0, reconstructed s1, s2
    }
}
