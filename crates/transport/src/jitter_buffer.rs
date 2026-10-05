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
    base_sequence: Option<u32>,
    unwrapped_base_seq: Option<u64>,
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

/// Base offset used for continuous 64-bit sequence number unwrapping.
/// Starts at 2^32 so that differences backwards from the initial sequence
/// can never underflow u64 even under severe packet reordering.
const SEQUENCE_EPOCH_OFFSET: u64 = 1u64 << 32;

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
    highest_seen_unwrapped: Option<u64>,
    expected_unwrapped: Option<u64>,
    fec_hold_anchor: Option<(u64, Instant)>,

    queue: BTreeMap<u64, QueuedPacket>,
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
            highest_seen_unwrapped: None,
            expected_unwrapped: None,
            fec_hold_anchor: None,
            queue: BTreeMap::new(),
            fec_blocks: HashMap::new(),
            stats: JitterBufferStats::default(),
        }
    }

    /// Returns the currently expected 32-bit sequence number (cursor), if initialized.
    pub fn expected_sequence(&self) -> Option<u32> {
        self.expected_unwrapped.map(|s| s as u32)
    }

    #[cfg(test)]
    pub(crate) fn set_expected_sequence(&mut self, seq: u32) {
        let unwrapped = self.unwrap_sequence(seq);
        self.expected_unwrapped = Some(unwrapped);
    }

    /// Unwraps 32-bit wire sequence numbers into a monotonically increasing 64-bit sequence space.
    fn unwrap_sequence(&mut self, seq: u32) -> u64 {
        match self.highest_seen_unwrapped {
            None => {
                let unwrapped = SEQUENCE_EPOCH_OFFSET + (seq as u64);
                self.highest_seen_unwrapped = Some(unwrapped);
                unwrapped
            }
            Some(highest) => {
                let highest_wire = highest as u32;
                let diff = seq.wrapping_sub(highest_wire) as i32 as i64;
                let unwrapped = (highest as i64 + diff) as u64;
                if unwrapped > highest {
                    self.highest_seen_unwrapped = Some(unwrapped);
                }
                unwrapped
            }
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

                let src_cnt = fec_header.source_count as usize;
                let par_cnt = fec_header.parity_count as usize;
                let p_idx = fec_header.parity_index as usize;

                // Validate FEC dimensions: k >= 1, m >= 1, k + m <= 256, parity_index < m
                if src_cnt == 0 || par_cnt == 0 || src_cnt + par_cnt > MAX_FEC_K || p_idx >= par_cnt
                {
                    return;
                }

                let tracker = self
                    .fec_blocks
                    .entry(fec_header.block_id)
                    .or_insert_with(|| FecBlockTracker {
                        scheme: fec_header.scheme,
                        k: src_cnt,
                        m: par_cnt,
                        base_sequence: None,
                        unwrapped_base_seq: None,
                        first_arrival: now,
                        sources: HashMap::new(),
                        parities: HashMap::new(),
                    });

                tracker.scheme = fec_header.scheme;
                tracker.k = src_cnt;
                tracker.m = par_cnt;
                tracker.prune_invalid_indices();
                if tracker.parities.len() < tracker.m {
                    tracker
                        .parities
                        .entry(p_idx)
                        .or_insert(packet.payload.clone());
                    self.try_fec_recovery(fec_header.block_id, now);
                }
            }
            return;
        }

        let seq = packet.header.sequence;
        let unwrapped_seq = self.unwrap_sequence(seq);

        // Discard packets older than expected playout cursor immediately.
        // Check this BEFORE updating jitter or last_arrival/sender_timestamp to prevent
        // stale out-of-order packets from polluting RFC 3550 jitter estimates.
        if let Some(expected) = self.expected_unwrapped {
            if unwrapped_seq < expected {
                return;
            }
        }

        // Discard duplicate packets immediately if already present in queue.
        // This prevents retransmissions from overwriting arrival_instant (which would restart
        // playout delay and FEC hold timers) and prevents duplicates from polluting RFC 3550 jitter estimates.
        if self.queue.contains_key(&unwrapped_seq) {
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
                    base_sequence: None,
                    unwrapped_base_seq: None,
                    first_arrival: now,
                    sources: HashMap::new(),
                    parities: HashMap::new(),
                });

            let idx = packet.header.fec_index() as usize;
            let valid_idx = if tracker.k > 0 {
                idx < tracker.k && tracker.sources.len() < tracker.k
            } else {
                idx < MAX_FEC_K && tracker.sources.len() < MAX_FEC_K
            };
            if valid_idx {
                let implied_base = seq.wrapping_sub(idx as u32);
                let unwrapped_implied_base = unwrapped_seq.saturating_sub(idx as u64);
                let consistent = match tracker.base_sequence {
                    None => {
                        tracker.base_sequence = Some(implied_base);
                        tracker.unwrapped_base_seq = Some(unwrapped_implied_base);
                        true
                    }
                    Some(b) => b == implied_base,
                };
                if consistent {
                    tracker.sources.insert(idx, (seq, packet.clone()));
                    self.try_fec_recovery(block_id, now);
                } else {
                    tracing::warn!(
                        "Inconsistent FEC (seq, index) in block {}: seq={}, idx={}, expected_base={:?}",
                        block_id, seq, idx, tracker.base_sequence
                    );
                }
            }
        }

        self.queue.insert(
            unwrapped_seq,
            QueuedPacket {
                packet,
                arrival_instant: now,
            },
        );

        // Enforce maximum queue depth to prevent unbounded memory growth.
        // Eviction runs strictly AFTER insertion so that a newly arrived in-order
        // packet filling a hole is never skipped or pre-emptively evicted by cursor advancement.
        self.evict_stale(self.config.max_queue_depth);
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
        if k == 0 || k + m > MAX_FEC_K {
            return;
        }
        if !self.fec_blocks.contains_key(&block_id) && self.fec_blocks.len() >= MAX_FEC_BLOCKS {
            if let Some(&oldest) = self.fec_blocks.keys().min() {
                self.fec_blocks.remove(&oldest);
            }
        }
        let tracker = self
            .fec_blocks
            .entry(block_id)
            .or_insert_with(|| FecBlockTracker {
                scheme,
                k,
                m,
                base_sequence: None,
                unwrapped_base_seq: None,
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
            let base_seq = match tracker.base_sequence {
                Some(b) => b,
                None => return false,
            };
            let unwrapped_base = match tracker.unwrapped_base_seq {
                Some(b) => b,
                None => return false,
            };
            let ref_entry = tracker.sources.values().next();
            if let Some((_ref_seq, ref_pkt)) = ref_entry {
                let packet_type = ref_pkt.header.packet_type;
                let stream_id = ref_pkt.header.stream_id;
                let timestamp_us = ref_pkt.header.timestamp_us;

                for (idx, payload) in all_sources.into_iter().enumerate() {
                    if let std::collections::hash_map::Entry::Vacant(e) = tracker.sources.entry(idx)
                    {
                        let seq = base_seq.wrapping_add(idx as u32);
                        let unwrapped_seq = unwrapped_base + (idx as u64);
                        if let std::collections::btree_map::Entry::Vacant(q_entry) =
                            self.queue.entry(unwrapped_seq)
                        {
                            let rec_header = PacketHeader::new(
                                packet_type,
                                stream_id,
                                seq,
                                timestamp_us,
                                &payload,
                            )
                            .with_flags(FLAG_FEC_PROTECTED)
                            .with_fec_index(idx as u16);

                            let recovered_packet = Packet::new(rec_header, payload);
                            q_entry.insert(QueuedPacket {
                                packet: recovered_packet.clone(),
                                arrival_instant: ref_arrival,
                            });
                            e.insert((seq, recovered_packet));
                            if let Some(highest) = self.highest_seen_unwrapped {
                                if unwrapped_seq > highest {
                                    self.highest_seen_unwrapped = Some(unwrapped_seq);
                                }
                            }
                            self.stats.packets_reconstructed_fec += 1;
                        }
                    }
                }
                self.fec_blocks.remove(&block_id);
                // Cap queue depth after inserting recovered packets
                self.evict_stale(self.config.max_queue_depth);
                return true;
            }
        }

        false
    }

    /// Checks whether a missing sequence number belongs to an active, incomplete FEC block
    /// that can still be reconstructed (i.e. K > 0, M > 0, remaining missing packets <= M).
    pub fn is_sequence_fec_recoverable(&self, missing_seq: u32) -> bool {
        for tracker in self.fec_blocks.values() {
            let k = tracker.k;
            let m = tracker.m;
            // Playout should only hold if block dimensions are known and parity is configured
            if k == 0 || m == 0 {
                continue;
            }

            let base_seq = match tracker.base_sequence {
                Some(b) => b,
                None => continue,
            };

            let offset = missing_seq.wrapping_sub(base_seq);
            if offset < (k as u32) {
                let missing_idx = offset as usize;
                if !tracker.sources.contains_key(&missing_idx) {
                    let missing_count = k.saturating_sub(tracker.sources.len());
                    if missing_count > 0 && missing_count <= m {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Unwrapped 64-bit sequence check for FEC recoverability.
    fn is_unwrapped_sequence_fec_recoverable(&self, missing_unwrapped: u64) -> bool {
        for tracker in self.fec_blocks.values() {
            let k = tracker.k;
            let m = tracker.m;
            if k == 0 || m == 0 {
                continue;
            }

            if let Some(unwrapped_base) = tracker.unwrapped_base_seq {
                if missing_unwrapped >= unwrapped_base
                    && missing_unwrapped < unwrapped_base + (k as u64)
                {
                    let missing_idx = (missing_unwrapped - unwrapped_base) as usize;
                    if !tracker.sources.contains_key(&missing_idx) {
                        let missing_count = k.saturating_sub(tracker.sources.len());
                        if missing_count > 0 && missing_count <= m {
                            return true;
                        }
                    }
                }
            } else if let Some(base_seq) = tracker.base_sequence {
                let missing_seq = missing_unwrapped as u32;
                let offset = missing_seq.wrapping_sub(base_seq);
                if offset < (k as u32) {
                    let missing_idx = offset as usize;
                    if !tracker.sources.contains_key(&missing_idx) {
                        let missing_count = k.saturating_sub(tracker.sources.len());
                        if missing_count > 0 && missing_count <= m {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Pops the next packet ready for playout if its target delay has elapsed.
    pub fn pop_ready_packet(&mut self, now: Instant) -> Option<Packet> {
        let target_delay = self.current_target_delay();

        while let Some((&first_unwrapped_seq, item)) = self.queue.iter().next() {
            if let Some(expected) = self.expected_unwrapped {
                if first_unwrapped_seq < expected {
                    // Packet arrived after playout cursor has already advanced past it; discard
                    self.queue.remove(&first_unwrapped_seq);
                    continue;
                }

                if first_unwrapped_seq > expected {
                    // Hole detected between expected and first_unwrapped_seq.
                    // If ANY missing sequence in the gap belongs to an incomplete recoverable FEC block
                    // (accounting for parity packets consuming sequence numbers without being queued),
                    // hold playout up to fec_hold_timeout anchored to when the hole was first detected.
                    let gap_has_recoverable = (expected
                        ..first_unwrapped_seq.min(expected + (MAX_FEC_K as u64)))
                        .any(|seq| self.is_unwrapped_sequence_fec_recoverable(seq));

                    if gap_has_recoverable {
                        let hold_start = match self.fec_hold_anchor {
                            Some((held_seq, start)) if held_seq == expected => start,
                            _ => {
                                self.fec_hold_anchor = Some((expected, now));
                                now
                            }
                        };

                        if now.duration_since(hold_start) < self.config.fec_hold_timeout {
                            return None;
                        }
                    }
                    // Hole is not recoverable or FEC hold deadline has expired: clear anchor
                    self.fec_hold_anchor = None;
                }
            }

            let age = now.duration_since(item.arrival_instant);
            if age >= target_delay {
                let item = self.queue.remove(&first_unwrapped_seq)?;
                if let Some(expected) = self.expected_unwrapped {
                    if first_unwrapped_seq > expected {
                        let diff = first_unwrapped_seq - expected;
                        self.stats.packets_lost += diff;
                    }
                }
                self.fec_hold_anchor = None;
                self.expected_unwrapped = Some(first_unwrapped_seq + 1);
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
            if let Some((&first_unwrapped_seq, _)) = self.queue.iter().next() {
                self.queue.remove(&first_unwrapped_seq);
                if let Some(expected) = self.expected_unwrapped {
                    if first_unwrapped_seq >= expected {
                        let diff = first_unwrapped_seq - expected;
                        self.stats.packets_lost += diff + 1;
                        self.expected_unwrapped = Some(first_unwrapped_seq + 1);
                    }
                } else {
                    self.stats.packets_lost += 1;
                    self.expected_unwrapped = Some(first_unwrapped_seq + 1);
                }
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

    #[test]
    fn test_jitter_buffer_m_zero_does_not_hold_playout() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        let block_id = 12345;

        // Block with zero parity (m = 0) can never be reconstructed
        jb.register_fec_block(block_id, 3, 0);

        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, b"chunk0")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            Bytes::from_static(b"chunk0"),
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 12, block_id, b"chunk2")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(2),
            Bytes::from_static(b"chunk2"),
        );
        jb.ingest_packet(p0, now);
        jb.ingest_packet(p2, now);

        // Sequence 11 is missing, but with m == 0 it should not hold playout
        assert!(
            !jb.is_sequence_fec_recoverable(11),
            "Sequence should not be recoverable when m == 0"
        );
    }

    #[test]
    fn test_jitter_buffer_unknown_k_does_not_hold_playout() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        let block_id = 54321;

        // Media packet arrives with FEC_PROTECTED before any parity or register_fec_block (k == 0)
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, block_id, b"chunk0")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            Bytes::from_static(b"chunk0"),
        );
        jb.ingest_packet(p0, now);

        // With k == 0 (dimensions unknown), playout should not hold
        assert!(
            !jb.is_sequence_fec_recoverable(11),
            "Sequence should not be recoverable when k is unknown"
        );
    }

    #[test]
    fn test_jitter_buffer_fec_recovery_respects_max_queue_depth() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 4,
            ..Default::default()
        });
        let now = Instant::now();
        let block_id = 8888;

        let s0 = Bytes::from_static(b"source-0");
        let s1 = Bytes::from_static(b"source-1");
        let s2 = Bytes::from_static(b"source-2");

        let sources = vec![s0.clone(), s1.clone(), s2.clone()];
        let parities = ReedSolomonFec::encode(block_id, &sources, 1).unwrap();
        jb.register_fec_block(block_id, 3, 1);

        // Ingest source 0 and 2 (source 1 missing)
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

        // Ingest 2 more independent packets to fill queue to max depth 4
        let p_extra1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 13, 9001, b"extra1"),
            Bytes::from_static(b"extra1"),
        );
        let p_extra2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 14, 9002, b"extra2"),
            Bytes::from_static(b"extra2"),
        );
        jb.ingest_packet(p_extra1, now);
        jb.ingest_packet(p_extra2, now);
        assert_eq!(jb.queue_len(), 4);

        // Now parity arrives and triggers recovery of source 1 (seq 11)
        let parity_pkt = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 20, block_id, &parities[0]),
            parities[0].clone(),
        );
        jb.ingest_packet(parity_pkt, now);

        // FEC recovery should have occurred
        assert_eq!(jb.stats().packets_reconstructed_fec, 1);
        // Queue length MUST be clamped to max_queue_depth
        assert!(
            jb.queue_len() <= 4,
            "Queue length {} should not exceed max_queue_depth 4",
            jb.queue_len()
        );
    }

    #[test]
    fn test_jitter_buffer_eviction_advances_expected_sequence_without_double_counting() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 3,
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Ingest packets 1 and 2
        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 1000, b"data1"),
            Bytes::from_static(b"data1"),
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 2, 1000, b"data2"),
            Bytes::from_static(b"data2"),
        );
        jb.ingest_packet(p1, now);
        jb.ingest_packet(p2, now);

        // Pop packet 1 -> expected_sequence is initialized to Some(2)
        let popped_1 = jb.pop_ready_packet(now).expect("Packet 1 ready");
        assert_eq!(popped_1.header.sequence, 1);
        assert_eq!(jb.expected_sequence(), Some(2));
        assert_eq!(jb.stats().packets_lost, 0);

        // Now burst of packets arrives: 5, 6, 7, 8 (packets 2, 3, 4 are lost in transit)
        for seq in 5..=8 {
            let p = Packet::new(
                PacketHeader::new(PacketType::VideoFrameChunk, 0, seq, 1000, b"data"),
                Bytes::from_static(b"data"),
            );
            jb.ingest_packet(p, now);
        }

        // With max_queue_depth = 3, packets should be clamped
        assert!(jb.queue_len() <= 3);

        // Now pop the next ready packet from queue (which will be seq 6 or later after eviction)
        let popped_next = jb.pop_ready_packet(now).expect("Next packet ready");
        let popped_seq = popped_next.header.sequence;

        // Total delivered: packet 1 + popped_next
        assert_eq!(jb.stats().packets_delivered, 2);
        // Total packets up to popped_seq was popped_seq.
        // Packets lost + packets delivered should equal popped_seq! (No double counting)
        assert_eq!(
            jb.stats().packets_lost + jb.stats().packets_delivered,
            popped_seq as u64,
            "Gaps must be counted exactly once without double counting between eviction and playout"
        );
    }

    #[test]
    fn test_jitter_buffer_inconsistent_fec_index_rejected() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        let block_id = 424242;

        jb.register_fec_block(block_id, 4, 1);

        // First packet establishes base_sequence: seq 100 with fec_index 0 -> base = 100
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 100, block_id, b"s0")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            Bytes::from_static(b"s0"),
        );
        jb.ingest_packet(p0, now);

        // Inconsistent packet: seq 200 with fec_index 1 -> implied base = 199 != 100
        let p_inconsistent = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 200, block_id, b"s1_bad")
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            Bytes::from_static(b"s1_bad"),
        );
        jb.ingest_packet(p_inconsistent, now);

        let tracker = jb.fec_blocks.get(&block_id).expect("Tracker exists");
        assert_eq!(tracker.base_sequence, Some(100));
        assert!(tracker.sources.contains_key(&0));
        // Index 1 with inconsistent base must be rejected from tracker.sources
        assert!(!tracker.sources.contains_key(&1));
    }

    #[test]
    fn test_jitter_buffer_fec_parity_dimensions_clamped_and_wrap_safe() {
        use crate::fec::FecHeader;

        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();
        let block_id = 999999;

        // Malformed parity packet with massive k=65535, m=65535
        let raw_hdr = FecHeader::new(FecScheme::ReedSolomon, block_id, 65535, 65535, 0, 100);
        let mut buf = bytes::BytesMut::new();
        raw_hdr.encode(&mut buf);
        buf.extend_from_slice(b"fake-parity-payload");
        let payload = buf.freeze();

        let p_malformed = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 10, block_id, &payload),
            payload,
        );
        jb.ingest_packet(p_malformed, now);

        // Tracker should not have been created with illegal dimensions
        assert!(!jb.fec_blocks.contains_key(&block_id));

        // Wrap-safe test near u32::MAX
        let wrap_block_id = 888888;
        jb.register_fec_block(wrap_block_id, 2, 1);
        let p_wrap0 = Packet::new(
            PacketHeader::new(
                PacketType::VideoFrameChunk,
                0,
                u32::MAX - 2,
                wrap_block_id,
                b"w0",
            )
            .with_flags(FLAG_FEC_PROTECTED)
            .with_fec_index(0),
            Bytes::from_static(b"w0"),
        );
        jb.ingest_packet(p_wrap0, now);

        // Sequence u32::MAX - 1 is missing, base_seq is u32::MAX - 2.
        // Must not panic with debug overflow arithmetic!
        assert!(jb.is_sequence_fec_recoverable(u32::MAX - 1));
        assert!(!jb.is_sequence_fec_recoverable(10));
    }

    #[test]
    fn test_jitter_buffer_sequence_wrap_across_u32_max() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Ingest packet u32::MAX
        let p_max = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, u32::MAX, 100, b"max"),
            Bytes::from_static(b"max"),
        );
        jb.ingest_packet(p_max, now);

        // Pop packet u32::MAX -> cursor must wrap to 0 without debug panic
        let popped = jb.pop_ready_packet(now).expect("u32::MAX should pop");
        assert_eq!(popped.header.sequence, u32::MAX);
        assert_eq!(jb.expected_sequence(), Some(0));

        // Ingest packet 0 (post-wrap)
        let p_zero = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 0, 101, b"zero"),
            Bytes::from_static(b"zero"),
        );
        jb.ingest_packet(p_zero, now);

        // Packet 0 must not be discarded as old
        let popped_zero = jb.pop_ready_packet(now).expect("Packet 0 should pop");
        assert_eq!(popped_zero.header.sequence, 0);
        assert_eq!(jb.expected_sequence(), Some(1));
    }

    #[test]
    fn test_jitter_buffer_evict_stale_wraps_cursor_at_u32_max() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 1,
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, u32::MAX, 100, b"max"),
            Bytes::from_static(b"max"),
        );
        jb.ingest_packet(p1, now);
        // Pop to set expected_sequence = Some(u32::MAX)
        let _ = jb.pop_ready_packet(now);
        assert_eq!(jb.expected_sequence(), Some(0));

        // Now test eviction when first_seq is u32::MAX and expected is u32::MAX
        let mut jb2 = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 1,
            ..Default::default()
        });
        jb2.set_expected_sequence(u32::MAX);
        let p_max = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, u32::MAX, 100, b"max"),
            Bytes::from_static(b"max"),
        );
        let p_zero = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 0, 101, b"zero"),
            Bytes::from_static(b"zero"),
        );
        jb2.ingest_packet(p_max, now);
        jb2.ingest_packet(p_zero, now);

        // Eviction should have run, advancing expected_sequence to Some(0) instead of saturating
        assert_eq!(jb2.expected_sequence(), Some(0));
    }

    #[test]
    fn test_jitter_buffer_queue_order_and_fec_across_wrap_boundary() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            fec_hold_timeout: Duration::from_millis(20),
            ..Default::default()
        });
        let now = Instant::now();
        let block_id = 12345678;

        let s0 = Bytes::from_static(b"data-wrap-0");
        let s1 = Bytes::from_static(b"data-wrap-1");
        let s2 = Bytes::from_static(b"data-wrap-2");
        let s3 = Bytes::from_static(b"data-wrap-3");

        let sources = vec![s0.clone(), s1.clone(), s2.clone(), s3.clone()];
        let parities = ReedSolomonFec::encode(block_id, &sources, 1).unwrap();
        jb.register_fec_block(block_id, 4, 1);

        // Source packets straddling u32::MAX:
        // Index 0 -> seq u32::MAX - 1 (4294967294)
        // Index 1 -> seq u32::MAX     (4294967295) -- WE DROP THIS PACKET TO TRIGGER FEC RECOVERY
        // Index 2 -> seq 0            (post-wrap)
        // Index 3 -> seq 1            (post-wrap)
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, u32::MAX - 1, block_id, &s0)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s0,
        );
        let p2 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 0, block_id, &s2)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(2),
            s2,
        );
        let p3 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, block_id, &s3)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(3),
            s3,
        );
        let parity_pkt = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 2, block_id, &parities[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities[0].clone(),
        );

        // Ingest ALL packets (both sides of the wrap: pre-wrap p0, post-wrap p2, p3, and parity)
        // simultaneously BEFORE popping anything!
        jb.ingest_packet(p0, now);
        jb.ingest_packet(p2, now);
        jb.ingest_packet(p3, now);
        jb.ingest_packet(parity_pkt, now);

        // Verification 1: FEC recovery reconstructed the dropped packet (seq u32::MAX)
        assert_eq!(
            jb.stats().packets_reconstructed_fec,
            1,
            "FEC must recover packet across the wrap boundary"
        );
        assert_eq!(jb.queue_len(), 4, "All 4 packets must be in queue");

        // Verification 2: Playout delivers strictly in sequential order across wrap
        let playout = now + Duration::from_millis(30);
        let out0 = jb.pop_ready_packet(playout).expect("Packet 0 should pop");
        let out1 = jb.pop_ready_packet(playout).expect("Packet 1 should pop");
        let out2 = jb.pop_ready_packet(playout).expect("Packet 2 should pop");
        let out3 = jb.pop_ready_packet(playout).expect("Packet 3 should pop");

        assert_eq!(out0.header.sequence, u32::MAX - 1);
        assert_eq!(out1.header.sequence, u32::MAX);
        assert_eq!(out2.header.sequence, 0);
        assert_eq!(out3.header.sequence, 1);
        assert!(jb.pop_ready_packet(playout).is_none());

        // Zero packets lost, exactly 4 delivered!
        assert_eq!(jb.stats().packets_lost, 0);
        assert_eq!(jb.stats().packets_delivered, 4);
    }

    #[test]
    fn test_jitter_buffer_late_packets_do_not_corrupt_jitter_estimate() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());
        let now = Instant::now();

        // Ingest sequence 5 with timestamp 5000us
        let p5 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 5, 5000, b"p5"),
            Bytes::from_static(b"p5"),
        );
        jb.ingest_packet(p5, now);

        // Playout pops sequence 5, advancing expected cursor to 6
        let playout = now + Duration::from_millis(50);
        let popped = jb.pop_ready_packet(playout).unwrap();
        assert_eq!(popped.header.sequence, 5);
        assert_eq!(jb.stats().current_jitter_us, 0.0);
        assert_eq!(jb.last_sender_timestamp_us, Some(5000));

        // Now a stale packet (sequence 4) arrives 100ms later with timestamp 4000us
        let late_arrival = now + Duration::from_millis(150);
        let p4 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 4, 4000, b"p4"),
            Bytes::from_static(b"p4"),
        );
        jb.ingest_packet(p4, late_arrival);

        // Assert: Jitter estimate MUST NOT spike, and last_sender_timestamp MUST NOT be overwritten
        assert_eq!(
            jb.stats().current_jitter_us,
            0.0,
            "Late discarded packet must not corrupt RFC 3550 jitter calculation"
        );
        assert_eq!(
            jb.last_sender_timestamp_us,
            Some(5000),
            "Late discarded packet must not overwrite last_sender_timestamp"
        );
        assert_eq!(jb.queue_len(), 0);
    }

    #[test]
    fn test_jitter_buffer_register_fec_block_caps_max_fec_blocks() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig::default());

        for i in 1..=100 {
            jb.register_fec_block(i, 4, 1);
        }

        assert!(
            jb.fec_blocks.len() <= MAX_FEC_BLOCKS,
            "fec_blocks count {} should not exceed MAX_FEC_BLOCKS {}",
            jb.fec_blocks.len(),
            MAX_FEC_BLOCKS
        );
    }

    #[test]
    fn test_jitter_buffer_duplicate_packet_does_not_restart_playout_or_fec_hold() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            ..Default::default()
        });
        let now = Instant::now();

        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, 1000, b"data1"),
            Bytes::from_static(b"data1"),
        );
        jb.ingest_packet(p1.clone(), now);

        // Arrive again 4ms later as a network duplicate
        let t_dup = now + Duration::from_millis(4);
        jb.ingest_packet(p1, t_dup);

        // At now + 5.1ms, original target_delay (5ms) has elapsed since first arrival (now)
        let playout = now + Duration::from_millis(5) + Duration::from_micros(100);
        let popped = jb
            .pop_ready_packet(playout)
            .expect("Packet 1 must pop based on first arrival, not delayed by duplicate");
        assert_eq!(popped.header.sequence, 1);
        assert_eq!(
            jb.stats().current_jitter_us,
            0.0,
            "Duplicate packet must not corrupt jitter estimate"
        );
    }

    #[test]
    fn test_jitter_buffer_eviction_after_insert_does_not_delete_both_packets() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            max_queue_depth: 3,
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..Default::default()
        });
        let now = Instant::now();

        // Packet 10 is ingested and played -> expected cursor is 11
        let p10 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 10, 1000, b"data10"),
            Bytes::from_static(b"data10"),
        );
        jb.ingest_packet(p10, now);
        let popped_10 = jb.pop_ready_packet(now).unwrap();
        assert_eq!(popped_10.header.sequence, 10);
        assert_eq!(jb.expected_sequence(), Some(11));

        // Packets 12, 13, 14 arrive while packet 11 is in transit (queue full to max depth 3)
        for seq in 12..=14 {
            let p = Packet::new(
                PacketHeader::new(PacketType::VideoFrameChunk, 0, seq, 1000, b"data"),
                Bytes::from_static(b"data"),
            );
            jb.ingest_packet(p, now);
        }
        assert_eq!(jb.queue_len(), 3);

        // Now packet 11 arrives. Eviction must run AFTER insert, so packet 12 is preserved!
        let p11 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 11, 1000, b"data11"),
            Bytes::from_static(b"data11"),
        );
        jb.ingest_packet(p11, now);

        // In the previous buggy code, 12 was pre-evicted and cursor became 13, so both 11 and 12 were deleted.
        // With eviction after insert, packet 12 is preserved and delivered!
        let out12 = jb
            .pop_ready_packet(now)
            .expect("Packet 12 must be ready to play");
        assert_eq!(out12.header.sequence, 12);

        let out13 = jb
            .pop_ready_packet(now)
            .expect("Packet 13 must be ready to play");
        assert_eq!(out13.header.sequence, 13);

        let out14 = jb
            .pop_ready_packet(now)
            .expect("Packet 14 must be ready to play");
        assert_eq!(out14.header.sequence, 14);
    }

    #[test]
    fn test_jitter_buffer_fec_hold_inspects_entire_gap_past_parity_sequence() {
        let mut jb = AdaptiveJitterBuffer::new(JitterBufferConfig {
            min_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
            fec_hold_timeout: Duration::from_millis(20),
            ..Default::default()
        });
        let now = Instant::now();

        // Block 1 (Frame 1):
        // Source 0 (seq 1), Source 1 (seq 2), Parity 0 (seq 3)
        let block1 = 1111;
        let s0 = Bytes::from_static(b"f1-s0");
        let s1 = Bytes::from_static(b"f1-s1");
        let sources1 = vec![s0.clone(), s1.clone()];
        let parities1 = ReedSolomonFec::encode(block1, &sources1, 1).unwrap();
        jb.register_fec_block(block1, 2, 1);

        // Ingest and play Block 1
        let p0 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 1, block1, &s0)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(0),
            s0,
        );
        let p1 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 2, block1, &s1)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            s1,
        );
        let p_parity1 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 3, block1, &parities1[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities1[0].clone(),
        );
        jb.ingest_packet(p0, now);
        jb.ingest_packet(p1, now);
        jb.ingest_packet(p_parity1, now);

        let playout_1 = now + Duration::from_millis(10);
        let popped_0 = jb.pop_ready_packet(playout_1).unwrap();
        let popped_1 = jb.pop_ready_packet(playout_1).unwrap();
        assert_eq!(popped_0.header.sequence, 1);
        assert_eq!(popped_1.header.sequence, 2);
        // Expected cursor is now 3 (the sequence consumed by parity packet 3!)
        assert_eq!(jb.expected_sequence(), Some(3));

        // Block 2 (Frame 2):
        // Source 0 (seq 4 - LOST IN TRANSIT), Source 1 (seq 5), Parity 0 (seq 6)
        let block2 = 2222;
        let s2 = Bytes::from_static(b"f2-s0");
        let s3 = Bytes::from_static(b"f2-s1");
        let sources2 = vec![s2.clone(), s3.clone()];
        let parities2 = ReedSolomonFec::encode(block2, &sources2, 1).unwrap();
        jb.register_fec_block(block2, 2, 1);

        // Only packet 5 arrives from Block 2 (packet 4 is missing)
        let p3 = Packet::new(
            PacketHeader::new(PacketType::VideoFrameChunk, 0, 5, block2, &s3)
                .with_flags(FLAG_FEC_PROTECTED)
                .with_fec_index(1),
            s3,
        );
        jb.ingest_packet(p3, now);

        // Playout check:
        // Expected cursor is 3 (parity). Missing gap is [3, 5).
        // Sequence 4 in this gap is FEC-recoverable!
        // Playout MUST HOLD instead of immediately skipping 3..5 and popping 5!
        let hold_check = jb.pop_ready_packet(playout_1);
        assert!(
            hold_check.is_none(),
            "Playout must hold because sequence 4 in gap [3, 5) is FEC-recoverable!"
        );

        // Now parity for Block 2 arrives within hold window
        let p_parity2 = Packet::new(
            PacketHeader::new(PacketType::FecParity, 0, 6, block2, &parities2[0])
                .with_flags(FLAG_FEC_PROTECTED),
            parities2[0].clone(),
        );
        jb.ingest_packet(p_parity2, now + Duration::from_millis(2));

        // Sequence 4 must have been reconstructed!
        assert_eq!(jb.stats().packets_reconstructed_fec, 1);

        // Now pop ready packets:
        let playout_2 = now + Duration::from_millis(15);
        let popped_s2 = jb
            .pop_ready_packet(playout_2)
            .expect("Reconstructed sequence 4 must pop!");
        let popped_s3 = jb
            .pop_ready_packet(playout_2)
            .expect("Sequence 5 must pop next!");

        assert_eq!(popped_s2.header.sequence, 4);
        assert_eq!(popped_s3.header.sequence, 5);
        assert_eq!(jb.stats().packets_delivered, 4);
    }
}
