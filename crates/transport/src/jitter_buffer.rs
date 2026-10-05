use bytes::Bytes;
use linux_quest_protocol::{Packet, PacketHeader, PacketType, FLAG_FEC_PROTECTED};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::fec::{reed_solomon::ReedSolomonFec, xor::XorFec, FecHeader, FecScheme};

#[derive(Debug, Clone, Copy)]
pub struct JitterBufferConfig {
    pub min_delay: Duration,
    pub max_delay: Duration,
    pub jitter_multiplier: f64,
    pub rtt_multiplier: f64,
    pub max_queue_depth: usize,
    pub fec_hold_timeout: Duration,
}

impl Default for JitterBufferConfig {
    fn default() -> Self {
        Self {
            min_delay: Duration::from_micros(1_500), // 1.5ms for low-latency streaming
            max_delay: Duration::from_micros(20_000), // 20ms cap to prevent lag accumulation
            jitter_multiplier: 2.0,                  // 2-sigma jitter guard
            rtt_multiplier: 0.1,                     // 10% RTT contribution
            max_queue_depth: 1024, // Cap queue depth to prevent unbounded memory growth
            fec_hold_timeout: Duration::from_millis(10), // Wait up to 10ms for FEC parity recovery
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
    first_arrival: Instant,
    sources: HashMap<usize, (u32, Packet)>, // index_in_block -> (seq, Packet)
    parities: HashMap<usize, Bytes>,        // parity_index -> raw_bytes
}

impl FecBlockTracker {
    fn is_complete(&self) -> bool {
        if self.k == 0 {
            return false;
        }
        (0..self.k).all(|i| self.sources.contains_key(&i))
    }

    fn prune_invalid_indices(&mut self) {
        if self.k > 0 {
            self.sources.retain(|&idx, _| idx < self.k);
        }
        if self.m > 0 {
            self.parities.retain(|&idx, _| idx < self.m);
        }
    }
}

/// Maximum number of active FEC blocks tracked simultaneously in memory.
pub const MAX_FEC_BLOCKS: usize = 64;

/// Maximum number of source packets (K) allowed per FEC block in GF(2^8).
pub const MAX_FEC_K: usize = 256;

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

        // Handle FEC parity packet first without polluting media RFC 3550 jitter calculation
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
                        first_arrival: now,
                        sources: HashMap::new(),
                        parities: HashMap::new(),
                    });

                tracker.scheme = fec_header.scheme;
                tracker.k = fec_header.source_count as usize;
                tracker.m = fec_header.parity_count as usize;
                tracker.prune_invalid_indices();
                let p_idx = fec_header.parity_index as usize;
                if tracker.m > 0 && p_idx < tracker.m {
                    tracker
                        .parities
                        .entry(p_idx)
                        .or_insert(packet.payload.clone());
                    self.try_fec_recovery(fec_header.block_id, now);
                }
            }
            return;
        }

        // Media packets: RFC 3550 Interarrival Jitter Calculation
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

        let seq = packet.header.sequence;

        // Discard packets older than expected_sequence immediately
        if let Some(expected) = self.expected_sequence {
            if seq < expected {
                return;
            }
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
                    first_arrival: now,
                    sources: HashMap::new(),
                    parities: HashMap::new(),
                });

            let idx = packet.header.fec_index() as usize;
            let valid_idx = if tracker.k > 0 {
                idx < tracker.k
            } else {
                idx < MAX_FEC_K && tracker.sources.len() < MAX_FEC_K
            };
            if valid_idx {
                tracker.sources.insert(idx, (seq, packet.clone()));
                self.try_fec_recovery(block_id, now);
            }
        }

        // Enforce maximum queue depth to prevent unbounded memory growth
        self.evict_stale(self.config.max_queue_depth.saturating_sub(1));

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
                first_arrival: Instant::now(),
                sources: HashMap::new(),
                parities: HashMap::new(),
            });
        tracker.scheme = scheme;
        tracker.k = k;
        tracker.m = m;
        tracker.prune_invalid_indices();
    }

    /// Attempts FEC recovery on a block if enough data + parity packets are available.
    pub fn try_fec_recovery(&mut self, block_id: u64, _now: Instant) -> bool {
        let tracker = match self.fec_blocks.get_mut(&block_id) {
            Some(t) if t.k > 0 => t,
            _ => return false,
        };

        let k = tracker.k;
        let m = tracker.m;

        if tracker.is_complete() {
            self.fec_blocks.remove(&block_id);
            return false;
        }

        let available = tracker.sources.len() + tracker.parities.len();
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
                let parity = tracker.parities.get(&0);
                XorFec::decode(block_id, k, source_payloads, parity)
            }
            FecScheme::ReedSolomon => {
                let parities_vec: Vec<(usize, Bytes)> = tracker
                    .parities
                    .iter()
                    .map(|(&idx, bytes)| (idx, bytes.clone()))
                    .collect();
                ReedSolomonFec::decode(block_id, k, m, source_payloads, &parities_vec)
            }
        };

        if let Ok(all_sources) = recovered {
            let ref_arrival = tracker.first_arrival;
            let ref_entry = tracker.sources.values().next();
            if let Some((ref_seq, ref_pkt)) = ref_entry {
                let base_seq = ref_seq.saturating_sub(ref_pkt.header.fec_index() as u32);
                let packet_type = ref_pkt.header.packet_type;
                let stream_id = ref_pkt.header.stream_id;
                let timestamp_us = ref_pkt.header.timestamp_us;

                for (idx, payload) in all_sources.into_iter().enumerate() {
                    if let std::collections::hash_map::Entry::Vacant(e) = tracker.sources.entry(idx)
                    {
                        let seq = base_seq.saturating_add(idx as u32);
                        let rec_header =
                            PacketHeader::new(packet_type, stream_id, seq, timestamp_us, &payload)
                                .with_flags(FLAG_FEC_PROTECTED)
                                .with_fec_index(idx as u16);

                        let recovered_packet = Packet::new(rec_header, payload);
                        self.queue.insert(
                            recovered_packet.header.sequence,
                            QueuedPacket {
                                packet: recovered_packet.clone(),
                                arrival_instant: ref_arrival,
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

    /// Checks whether a missing sequence number belongs to an active, incomplete FEC block
    /// that can still be reconstructed (i.e. remaining missing packets <= M).
    pub fn is_sequence_fec_recoverable(&self, missing_seq: u32) -> bool {
        for tracker in self.fec_blocks.values() {
            if tracker.sources.is_empty() {
                continue;
            }
            let (&ref_idx, (ref_seq, _)) = tracker.sources.iter().next().unwrap();
            let base_seq = ref_seq.saturating_sub(ref_idx as u32);
            let k = tracker.k;
            let m = tracker.m;
            if k > 0 {
                if missing_seq >= base_seq && missing_seq < base_seq + (k as u32) {
                    let missing_idx = (missing_seq - base_seq) as usize;
                    if !tracker.sources.contains_key(&missing_idx) {
                        let missing_count = k.saturating_sub(tracker.sources.len());
                        if missing_count > 0 && (m == 0 || missing_count <= m) {
                            return true;
                        }
                    }
                }
            } else if missing_seq >= base_seq && missing_seq < base_seq + (MAX_FEC_K as u32) {
                return true;
            }
        }
        false
    }

    /// Pops the next packet ready for playout if its target delay has elapsed.
    pub fn pop_ready_packet(&mut self, now: Instant) -> Option<Packet> {
        let target_delay = self.current_target_delay();

        while let Some((&first_seq, item)) = self.queue.iter().next() {
            if let Some(expected) = self.expected_sequence {
                if first_seq < expected {
                    // Packet arrived after playout cursor has already advanced past it; discard
                    self.queue.remove(&first_seq);
                    continue;
                }

                if first_seq > expected {
                    // Hole detected between expected and first_seq.
                    // If the missing sequence is part of an incomplete recoverable FEC block,
                    // hold playout up to fec_hold_timeout for parity recovery.
                    if self.is_sequence_fec_recoverable(expected) {
                        let age = now.duration_since(item.arrival_instant);
                        if age < target_delay + self.config.fec_hold_timeout {
                            return None;
                        }
                    }
                }
            }

            let age = now.duration_since(item.arrival_instant);
            if age >= target_delay {
                let item = self.queue.remove(&first_seq)?;
                if let Some(expected) = self.expected_sequence {
                    if first_seq > expected {
                        self.stats.packets_lost += (first_seq - expected) as u64;
                    }
                }
                self.expected_sequence = Some(first_seq + 1);
                self.stats.packets_delivered += 1;
                return Some(item.packet);
            } else {
                return None;
            }
        }

        None
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

    #[test]
    fn test_jitter_buffer_duplicate_parity_recovery() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        let block_id = 1000;
        let sources = vec![
            Bytes::from_static(b"data-0"),
            Bytes::from_static(b"data-1"),
            Bytes::from_static(b"data-2"),
            Bytes::from_static(b"data-3"),
        ];

        let parities = ReedSolomonFec::encode(block_id, &sources, 2).unwrap();
        jb.register_fec_block(block_id, 4, 2);

        // Sources 0 and 1 arrive (sources 2 and 3 dropped)
        for (i, source) in sources.iter().enumerate().take(2) {
            let p = Packet::new(
                PacketHeader::new(
                    PacketType::VideoFrameChunk,
                    0,
                    100 + i as u32,
                    block_id,
                    source,
                )
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(i as u16),
                source.clone(),
            );
            jb.ingest_packet(p, now);
        }

        // Parity 0 arrives twice (network duplication)
        let parity0_dup1 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 200, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );
        let parity0_dup2 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 201, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );
        jb.ingest_packet(parity0_dup1, now);
        jb.ingest_packet(parity0_dup2, now);

        // Parity 1 arrives
        let parity1 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 202, block_id, &parities[1])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[1].clone(),
        );
        jb.ingest_packet(parity1, now);

        // All 4 source packets must be recovered despite duplicate parity
        assert_eq!(jb.stats().packets_reconstructed_fec, 2);
        assert_eq!(jb.queue_len(), 4);
    }

    #[test]
    fn test_jitter_buffer_clean_reconstructed_flags() {
        use linux_quest_protocol::{FLAG_KEYFRAME, FLAG_LAST_CHUNK};

        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        let block_id = 2000;
        let s0 = Bytes::from_static(b"chunk0");
        let s1 = Bytes::from_static(b"chunk1");
        let s2 = Bytes::from_static(b"chunk2");

        let sources = vec![s0.clone(), s1.clone(), s2.clone()];
        let parities = ReedSolomonFec::encode(block_id, &sources, 1).unwrap();
        jb.register_fec_block(block_id, 3, 1);

        // Chunk 0 has KEYFRAME
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, &s0)
                .with_flags(FLAG_FEC_PROTECTED | FLAG_KEYFRAME)
                .with_fec_index(0),
            s0,
        );
        // Chunk 2 has LAST_CHUNK
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 12, block_id, &s2)
                .with_flags(FLAG_FEC_PROTECTED | FLAG_LAST_CHUNK)
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

        let playout = now + Duration::from_millis(50);
        let pkt0 = jb.pop_ready_packet(playout).unwrap();
        let pkt1 = jb.pop_ready_packet(playout).unwrap();
        let pkt2 = jb.pop_ready_packet(playout).unwrap();

        assert_eq!(pkt0.header.sequence, 10);
        assert_eq!(pkt1.header.sequence, 11);
        assert_eq!(pkt2.header.sequence, 12);

        // Recovered chunk 1 MUST NOT copy KEYFRAME or LAST_CHUNK flags
        assert_eq!(pkt1.header.flags, FLAG_FEC_PROTECTED);
    }

    #[test]
    fn test_jitter_buffer_late_packet_discard() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        // Ingest sequence 5
        let p5 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 5, 5000, b"p5"),
            Bytes::from_static(b"p5"),
        );
        jb.ingest_packet(p5, now);

        let playout = now + Duration::from_millis(50);
        let popped = jb.pop_ready_packet(playout).unwrap();
        assert_eq!(popped.header.sequence, 5);

        // Now late packet sequence 4 arrives
        let p4 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 4, 4000, b"p4"),
            Bytes::from_static(b"p4"),
        );
        jb.ingest_packet(p4, now);

        // Popping must discard late sequence 4 and return None
        assert!(jb.pop_ready_packet(playout).is_none());
        assert_eq!(jb.stats().packets_delivered, 1);
    }

    #[test]
    fn test_jitter_buffer_holds_for_fec_recovery() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            fec_hold_timeout: Duration::from_millis(20),
            ..Default::default()
        });
        let now = Instant::now();

        let block_id = 5000;
        let s0 = Bytes::from_static(b"packet-10");
        let s1 = Bytes::from_static(b"packet-11");
        let s2 = Bytes::from_static(b"packet-12");

        let sources = vec![s0.clone(), s1.clone(), s2.clone()];
        let parities = ReedSolomonFec::encode(block_id, &sources, 1).unwrap();
        jb.register_fec_block(block_id, 3, 1);

        // Sequence 10 and 12 arrive (sequence 11 lost)
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

        jb.ingest_packet(p0, now);
        jb.ingest_packet(p2, now);

        // Playout time after min_delay (e.g. 6ms)
        let playout_1 = now + Duration::from_millis(6);
        let popped_0 = jb
            .pop_ready_packet(playout_1)
            .expect("Sequence 10 should pop");
        assert_eq!(popped_0.header.sequence, 10);

        // Playout holds for sequence 11 instead of prematurely popping 12 and discarding 11!
        let hold_check = jb.pop_ready_packet(playout_1);
        assert!(
            hold_check.is_none(),
            "Playout should hold for missing sequence 11 in recoverable block"
        );

        // Now parity arrives within hold window
        let parity_pkt = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 20, block_id, &parities[0]),
            parities[0].clone(),
        );
        jb.ingest_packet(parity_pkt, now + Duration::from_millis(8));

        assert_eq!(jb.stats().packets_reconstructed_fec, 1);

        let playout_2 = now + Duration::from_millis(8);
        let popped_1 = jb
            .pop_ready_packet(playout_2)
            .expect("Sequence 11 should pop after recovery");
        let popped_2 = jb
            .pop_ready_packet(playout_2)
            .expect("Sequence 12 should pop next");

        assert_eq!(popped_1.header.sequence, 11);
        assert_eq!(popped_2.header.sequence, 12);
        assert_eq!(jb.stats().packets_lost, 0);
        assert_eq!(jb.stats().packets_delivered, 3);
    }

    #[test]
    fn test_jitter_buffer_max_queue_depth_eviction() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 5,
            ..Default::default()
        });
        let now = Instant::now();

        for seq in 1..=10 {
            let p = Packet::new(
                PacketHeader::new(PacketType::VideoFrameChunk, 0, seq, 1000, b"data"),
                Bytes::from_static(b"data"),
            );
            jb.ingest_packet(p, now);
        }

        assert!(jb.queue_len() <= 5);
        assert_eq!(jb.stats().packets_lost, 5);
    }

    #[test]
    fn test_jitter_buffer_fec_index_cap_when_k_unknown() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        // Packet with out-of-range fec_index (>= MAX_FEC_K = 256)
        let p_invalid = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 9999, b"data")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(300),
            Bytes::from_static(b"data"),
        );
        jb.ingest_packet(p_invalid, now);

        // Should not have inserted into block sources
        if let Some(tracker) = jb.fec_blocks.get(&9999) {
            assert!(!tracker.sources.contains_key(&300));
        }
    }
}
