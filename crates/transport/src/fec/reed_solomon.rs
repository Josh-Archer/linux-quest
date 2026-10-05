use bytes::{Bytes, BytesMut};
use std::collections::HashMap;

use super::{FecError, FecHeader, FEC_HEADER_SIZE};

/// Galois Field GF(2^8) with polynomial 0x11d (x^8 + x^4 + x^3 + x^2 + 1).
struct GaloisField {
    exp: [u8; 512],
    log: [u8; 256],
}

impl GaloisField {
    const fn init() -> Self {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x = 1u16;
        let mut i = 0;
        while i < 255 {
            exp[i] = x as u8;
            exp[i + 255] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if (x & 0x100) != 0 {
                x ^= 0x11d;
            }
            i += 1;
        }
        exp[510] = exp[0];
        exp[511] = exp[1];
        GaloisField { exp, log }
    }

    #[inline(always)]
    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            let log_a = self.log[a as usize] as usize;
            let log_b = self.log[b as usize] as usize;
            self.exp[log_a + log_b]
        }
    }

    #[inline(always)]
    fn inv(&self, a: u8) -> Result<u8, FecError> {
        if a == 0 {
            return Err(FecError::SingularMatrix);
        }
        let log_a = self.log[a as usize] as usize;
        Ok(self.exp[255 - log_a])
    }
}

static GF: GaloisField = GaloisField::init();

/// Systematic Cauchy Reed-Solomon Erasure Codec for ultra-low latency packet transmission.
pub struct ReedSolomonFec;

#[allow(clippy::needless_range_loop)]
impl ReedSolomonFec {
    /// Cauchy matrix entry: 1 / (X_i ^ Y_j).
    /// Y_j = j for j in 0..k
    /// X_i = k + i for i in 0..m
    #[inline(always)]
    fn cauchy_coeff(
        k: usize,
        m: usize,
        parity_idx: usize,
        source_idx: usize,
    ) -> Result<u8, FecError> {
        if source_idx >= k || parity_idx >= m {
            return Err(FecError::CorruptHeader);
        }
        let x = (k + parity_idx) as u8;
        let y = source_idx as u8;
        let diff = x ^ y;
        GF.inv(diff)
    }

    /// Encodes K source packets into M parity packets.
    /// Each returned Bytes is a fully framed FEC packet with FecHeader prepended.
    pub fn encode(
        block_id: u64,
        source_packets: &[Bytes],
        parity_count: usize,
    ) -> Result<Vec<Bytes>, FecError> {
        let k = source_packets.len();
        let m = parity_count;

        if k == 0 || m == 0 || k + m > 256 {
            return Err(FecError::InvalidConfiguration { k, m });
        }

        let max_len = source_packets.iter().map(|p| p.len()).max().unwrap_or(0);
        let symbol_size = max_len + 2;

        // Prepare source symbols: [len: u16][payload...][zero pad]
        let mut source_symbols = Vec::with_capacity(k);
        for packet in source_packets {
            let mut symbol = vec![0u8; symbol_size];
            let len = packet.len() as u16;
            let len_bytes = len.to_be_bytes();
            symbol[0] = len_bytes[0];
            symbol[1] = len_bytes[1];
            symbol[2..2 + packet.len()].copy_from_slice(packet);
            source_symbols.push(symbol);
        }

        let mut parity_packets = Vec::with_capacity(m);

        for p_idx in 0..m {
            let mut parity_symbol = vec![0u8; symbol_size];

            for s_idx in 0..k {
                let coeff = Self::cauchy_coeff(k, m, p_idx, s_idx)?;
                let src = &source_symbols[s_idx];
                for b_idx in 0..symbol_size {
                    parity_symbol[b_idx] ^= GF.mul(coeff, src[b_idx]);
                }
            }

            let header = FecHeader::new(
                super::FecScheme::ReedSolomon,
                block_id,
                k as u16,
                m as u16,
                p_idx as u16,
                symbol_size as u16,
            );

            let mut buf = BytesMut::with_capacity(FEC_HEADER_SIZE + symbol_size);
            header.encode(&mut buf);
            buf.extend_from_slice(&parity_symbol);
            parity_packets.push(buf.freeze());
        }

        Ok(parity_packets)
    }

    /// Decodes and reconstructs any missing source packets.
    /// `received_sources`: map of source_index (0..k-1) -> source payload.
    /// `received_parities`: slice of (parity_index, raw parity packet with FecHeader).
    /// Returns all K source packets in sequential order if recoverable.
    pub fn decode(
        block_id: u64,
        k: usize,
        m: usize,
        mut received_sources: HashMap<usize, Bytes>,
        received_parities: &[(usize, Bytes)],
    ) -> Result<Vec<Bytes>, FecError> {
        if k == 0 || m == 0 || k + m > 256 {
            return Err(FecError::InvalidConfiguration { k, m });
        }

        // Fast path: all K source packets already received
        if received_sources.len() == k {
            let mut result = Vec::with_capacity(k);
            for i in 0..k {
                result.push(
                    received_sources
                        .remove(&i)
                        .ok_or(FecError::InsufficientPackets {
                            needed: k,
                            received: k - 1,
                        })?,
                );
            }
            return Ok(result);
        }

        // Find missing source indices
        let missing_sources: Vec<usize> = (0..k)
            .filter(|i| !received_sources.contains_key(i))
            .collect();

        let num_missing = missing_sources.len();

        if received_parities.len() < num_missing {
            return Err(FecError::InsufficientPackets {
                needed: k,
                received: received_sources.len() + received_parities.len(),
            });
        }

        // Parse parity headers and symbols
        let mut parsed_parities: Vec<(usize, Vec<u8>)> = Vec::with_capacity(num_missing);
        let mut symbol_size = 0;

        for (p_idx, raw_pkt) in received_parities.iter().take(num_missing) {
            if *p_idx >= m {
                return Err(FecError::CorruptHeader);
            }
            let mut slice = &raw_pkt[..];
            let header = FecHeader::decode(&mut slice)?;

            if header.block_id != block_id
                || header.source_count as usize != k
                || header.parity_count as usize != m
                || header.parity_index as usize != *p_idx
                || header.scheme != super::FecScheme::ReedSolomon
            {
                return Err(FecError::CorruptHeader);
            }

            let p_symbol_size = header.symbol_size as usize;
            if p_symbol_size < 2 || slice.len() < p_symbol_size {
                return Err(FecError::CorruptHeader);
            }

            if symbol_size == 0 {
                symbol_size = p_symbol_size;
            } else if symbol_size != p_symbol_size {
                return Err(FecError::CorruptHeader);
            }

            parsed_parities.push((*p_idx, slice[..symbol_size].to_vec()));
        }

        // Prepare known source symbols
        let mut known_symbols: HashMap<usize, Vec<u8>> =
            HashMap::with_capacity(received_sources.len());
        for (&idx, packet) in &received_sources {
            if idx >= k || packet.len() + 2 > symbol_size {
                return Err(FecError::CorruptHeader);
            }
            let mut sym = vec![0u8; symbol_size];
            let len = packet.len() as u16;
            let len_bytes = len.to_be_bytes();
            sym[0] = len_bytes[0];
            sym[1] = len_bytes[1];
            sym[2..2 + packet.len()].copy_from_slice(packet);
            known_symbols.insert(idx, sym);
        }

        // Construct linear system A * X = R
        // where A is num_missing x num_missing
        // R is num_missing x symbol_size
        let mut a: Vec<Vec<u8>> = vec![vec![0u8; num_missing]; num_missing];
        let mut r: Vec<Vec<u8>> = vec![vec![0u8; symbol_size]; num_missing];

        for row in 0..num_missing {
            let (p_idx, ref p_symbol) = parsed_parities[row];

            // R[row] = P[p_idx] ^ sum_{s in known} (C[p_idx, s] * S[s])
            r[row].copy_from_slice(p_symbol);

            for (&s_idx, s_symbol) in &known_symbols {
                let coeff = Self::cauchy_coeff(k, m, p_idx, s_idx)?;
                for b in 0..symbol_size {
                    r[row][b] ^= GF.mul(coeff, s_symbol[b]);
                }
            }

            // Fill row of matrix A: A[row][col] = C[p_idx, missing_sources[col]]
            for col in 0..num_missing {
                let s_missing_idx = missing_sources[col];
                a[row][col] = Self::cauchy_coeff(k, m, p_idx, s_missing_idx)?;
            }
        }

        // Gauss-Jordan elimination to solve for missing symbols
        for i in 0..num_missing {
            // Pivot selection
            let mut pivot_row = i;
            while pivot_row < num_missing && a[pivot_row][i] == 0 {
                pivot_row += 1;
            }

            if pivot_row == num_missing {
                return Err(FecError::SingularMatrix);
            }

            if pivot_row != i {
                a.swap(i, pivot_row);
                r.swap(i, pivot_row);
            }

            let pivot_inv = GF.inv(a[i][i])?;
            for col in 0..num_missing {
                a[i][col] = GF.mul(a[i][col], pivot_inv);
            }
            for b in 0..symbol_size {
                r[i][b] = GF.mul(r[i][b], pivot_inv);
            }

            // Eliminate all other rows
            for other in 0..num_missing {
                if other != i {
                    let factor = a[other][i];
                    if factor != 0 {
                        for col in 0..num_missing {
                            a[other][col] ^= GF.mul(factor, a[i][col]);
                        }
                        for b in 0..symbol_size {
                            let scaled = GF.mul(factor, r[i][b]);
                            r[other][b] ^= scaled;
                        }
                    }
                }
            }
        }

        // Recovered symbols are in r[col]
        for col in 0..num_missing {
            let missing_idx = missing_sources[col];
            let sym = &r[col];
            let len = u16::from_be_bytes([sym[0], sym[1]]) as usize;
            if 2 + len > symbol_size {
                return Err(FecError::CorruptHeader);
            }
            let payload = Bytes::copy_from_slice(&sym[2..2 + len]);
            received_sources.insert(missing_idx, payload);
        }

        let mut result = Vec::with_capacity(k);
        for i in 0..k {
            result.push(
                received_sources
                    .remove(&i)
                    .ok_or(FecError::InsufficientPackets {
                        needed: k,
                        received: k - 1,
                    })?,
            );
        }

        Ok(result)
    }
}

#[cfg(test)]
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;

    #[test]
    fn test_galois_field_arithmetic() {
        assert_eq!(GF.mul(0, 42), 0);
        assert_eq!(GF.mul(42, 0), 0);
        assert_eq!(GF.mul(1, 42), 42);
        assert_eq!(GF.mul(42, 1), 42);

        for a in 1..=255u8 {
            let inv = GF.inv(a).expect("inversion should succeed for non-zero");
            assert_eq!(GF.mul(a, inv), 1, "Failed for a={a}");
        }
        assert!(matches!(GF.inv(0), Err(FecError::SingularMatrix)));
    }

    #[test]
    fn test_reed_solomon_single_packet_loss() {
        let sources = vec![
            Bytes::from_static(b"packet-zero"),
            Bytes::from_static(b"packet-one-intermediate-size"),
            Bytes::from_static(b"packet-two"),
            Bytes::from_static(b"packet-three"),
        ];

        let parities = ReedSolomonFec::encode(100, &sources, 2).unwrap();
        assert_eq!(parities.len(), 2);

        // Drop index 1
        let mut received = HashMap::new();
        received.insert(0, sources[0].clone());
        received.insert(2, sources[2].clone());
        received.insert(3, sources[3].clone());

        // Use parity 0
        let received_parities = vec![(0, parities[0].clone())];

        let recovered = ReedSolomonFec::decode(100, 4, 2, received, &received_parities).unwrap();
        assert_eq!(recovered, sources);
    }

    #[test]
    fn test_reed_solomon_multi_packet_loss() {
        let k = 8;
        let m = 4;
        let mut sources = Vec::new();
        for i in 0..k {
            sources.push(Bytes::from(format!(
                "video-chunk-data-stream-slice-{:04}",
                i
            )));
        }

        let parities = ReedSolomonFec::encode(200, &sources, m).unwrap();
        assert_eq!(parities.len(), m);

        // Simulate dropping 3 packets: index 1, 4, 6
        let mut received = HashMap::new();
        for i in 0..k {
            if i != 1 && i != 4 && i != 6 {
                received.insert(i, sources[i].clone());
            }
        }
        assert_eq!(received.len(), 5); // 3 missing

        // Provide 3 parities: parities 0, 2, 3 (parity 1 also dropped in transit!)
        let received_parities = vec![
            (0, parities[0].clone()),
            (2, parities[2].clone()),
            (3, parities[3].clone()),
        ];

        let recovered = ReedSolomonFec::decode(200, k, m, received, &received_parities).unwrap();
        assert_eq!(recovered, sources);
    }

    #[test]
    fn test_reed_solomon_max_loss_recovery() {
        let k = 6;
        let m = 3;
        let sources: Vec<Bytes> = (0..k)
            .map(|i| Bytes::from(vec![i as u8 * 17; 500]))
            .collect();

        let parities = ReedSolomonFec::encode(300, &sources, m).unwrap();

        // Drop all 3 allowed packets (indices 0, 2, 5)
        let mut received = HashMap::new();
        received.insert(1, sources[1].clone());
        received.insert(3, sources[3].clone());
        received.insert(4, sources[4].clone());

        // Provide all 3 parities
        let received_parities = vec![
            (0, parities[0].clone()),
            (1, parities[1].clone()),
            (2, parities[2].clone()),
        ];

        let recovered = ReedSolomonFec::decode(300, k, m, received, &received_parities).unwrap();
        assert_eq!(recovered, sources);
    }
}
