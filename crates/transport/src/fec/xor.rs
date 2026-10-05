use bytes::{Bytes, BytesMut};
use std::collections::HashMap;

use super::{FecError, FecHeader, FEC_HEADER_SIZE};

pub struct XorFec;

impl XorFec {
    /// Encodes a list of source packet payloads into 1 XOR parity packet with full FecHeader.
    pub fn encode(block_id: u64, source_packets: &[Bytes]) -> Result<Bytes, FecError> {
        let k = source_packets.len();
        if k == 0 || k > u16::MAX as usize {
            return Err(FecError::InvalidConfiguration { k, m: 1 });
        }

        let max_len = source_packets.iter().map(|p| p.len()).max().unwrap_or(0);
        if max_len + 2 > u16::MAX as usize {
            return Err(FecError::InvalidConfiguration { k, m: 1 });
        }
        let symbol_size = max_len + 2;

        let mut parity_symbol = vec![0u8; symbol_size];

        for packet in source_packets {
            let len = packet.len() as u16;
            let len_bytes = len.to_be_bytes();
            parity_symbol[0] ^= len_bytes[0];
            parity_symbol[1] ^= len_bytes[1];

            for (idx, &byte) in packet.iter().enumerate() {
                parity_symbol[2 + idx] ^= byte;
            }
        }

        let header = FecHeader::new(
            super::FecScheme::Xor,
            block_id,
            k as u16,
            1,
            0,
            symbol_size as u16,
        );
        let mut buf = BytesMut::with_capacity(FEC_HEADER_SIZE + symbol_size);
        header.encode(&mut buf);
        buf.extend_from_slice(&parity_symbol);
        Ok(buf.freeze())
    }

    /// Attempts to reconstruct any missing source packets.
    /// Returns the full list of K source packet payloads in index order if recoverable.
    pub fn decode(
        block_id: u64,
        k: usize,
        mut received_sources: HashMap<usize, Bytes>,
        parity_packet: Option<&Bytes>,
    ) -> Result<Vec<Bytes>, FecError> {
        if k == 0 || k > u16::MAX as usize {
            return Err(FecError::InvalidConfiguration { k, m: 1 });
        }

        // Discard any extraneous indices >= k
        received_sources.retain(|&idx, _| idx < k);

        if received_sources.len() == k {
            let mut result = Vec::with_capacity(k);
            for i in 0..k {
                if let Some(pkt) = received_sources.remove(&i) {
                    result.push(pkt);
                } else {
                    return Err(FecError::InsufficientPackets {
                        needed: k,
                        received: received_sources.len(),
                    });
                }
            }
            return Ok(result);
        }

        let valid_sources = (0..k).filter(|i| received_sources.contains_key(i)).count();
        if valid_sources < k - 1 || parity_packet.is_none() {
            return Err(FecError::InsufficientPackets {
                needed: k,
                received: valid_sources + if parity_packet.is_some() { 1 } else { 0 },
            });
        }

        let parity_bytes = parity_packet.unwrap();
        let mut slice = &parity_bytes[..];
        let header = FecHeader::decode(&mut slice)?;

        if header.block_id != block_id
            || header.source_count as usize != k
            || header.scheme != super::FecScheme::Xor
            || header.parity_count != 1
            || header.parity_index != 0
        {
            return Err(FecError::CorruptHeader);
        }

        let symbol_size = header.symbol_size as usize;
        if symbol_size < 2 || slice.len() < symbol_size {
            return Err(FecError::CorruptHeader);
        }

        // Find the single missing index
        let missing_idx = (0..k).find(|i| !received_sources.contains_key(i)).ok_or(
            FecError::InsufficientPackets {
                needed: k,
                received: valid_sources,
            },
        )?;

        // Reconstruct missing symbol by XORing parity symbol with all present symbols
        let mut reconstructed_symbol = slice[..symbol_size].to_vec();

        for (&idx, packet) in &received_sources {
            if idx >= k || packet.len() + 2 > symbol_size {
                return Err(FecError::CorruptHeader);
            }
            if idx == missing_idx {
                continue;
            }
            let len = packet.len() as u16;
            let len_bytes = len.to_be_bytes();
            reconstructed_symbol[0] ^= len_bytes[0];
            reconstructed_symbol[1] ^= len_bytes[1];

            for (p_idx, &byte) in packet.iter().enumerate() {
                reconstructed_symbol[2 + p_idx] ^= byte;
            }
        }

        let recovered_len =
            u16::from_be_bytes([reconstructed_symbol[0], reconstructed_symbol[1]]) as usize;
        if 2 + recovered_len > symbol_size {
            return Err(FecError::CorruptHeader);
        }

        let recovered_payload = Bytes::copy_from_slice(&reconstructed_symbol[2..2 + recovered_len]);
        received_sources.insert(missing_idx, recovered_payload);

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
mod tests {
    use super::*;

    #[test]
    fn test_xor_fec_no_loss() {
        let sources = vec![
            Bytes::from_static(b"packet-zero"),
            Bytes::from_static(b"packet-one-long-data"),
            Bytes::from_static(b"packet-two"),
        ];

        let parity = XorFec::encode(1, &sources).unwrap();

        let mut received = HashMap::new();
        for (i, p) in sources.iter().enumerate() {
            received.insert(i, p.clone());
        }

        let recovered = XorFec::decode(1, 3, received, Some(&parity)).unwrap();
        assert_eq!(recovered, sources);
    }

    #[test]
    fn test_xor_fec_recover_middle_packet() {
        let sources = vec![
            Bytes::from_static(b"alpha-chunk-1234"),
            Bytes::from_static(b"beta-chunk-something-special"),
            Bytes::from_static(b"gamma-chunk-end"),
        ];

        let parity = XorFec::encode(42, &sources).unwrap();

        // Drop index 1 (beta)
        let mut received = HashMap::new();
        received.insert(0, sources[0].clone());
        received.insert(2, sources[2].clone());

        let recovered = XorFec::decode(42, 3, received, Some(&parity)).unwrap();
        assert_eq!(recovered, sources);
        assert_eq!(
            recovered[1],
            Bytes::from_static(b"beta-chunk-something-special")
        );
    }

    #[test]
    fn test_xor_fec_insufficient_packets() {
        let sources = vec![
            Bytes::from_static(b"one"),
            Bytes::from_static(b"two"),
            Bytes::from_static(b"three"),
        ];

        let parity = XorFec::encode(10, &sources).unwrap();

        // Drop two packets: only index 0 received
        let mut received = HashMap::new();
        received.insert(0, sources[0].clone());

        let result = XorFec::decode(10, 3, received, Some(&parity));
        assert!(matches!(result, Err(FecError::InsufficientPackets { .. })));
    }

    #[test]
    fn test_xor_fec_decode_zero_k() {
        let received = HashMap::new();
        let result = XorFec::decode(1, 0, received, None);
        assert!(matches!(
            result,
            Err(FecError::InvalidConfiguration { k: 0, m: 1 })
        ));
    }

    #[test]
    fn test_xor_fec_oversized_payload() {
        let huge_packet = Bytes::from(vec![0u8; 65534]);
        let result = XorFec::encode(1, &[huge_packet]);
        assert!(matches!(result, Err(FecError::InvalidConfiguration { .. })));
    }
}
