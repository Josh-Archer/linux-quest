use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;

pub mod reed_solomon;
pub mod xor;

pub use reed_solomon::ReedSolomonFec;
pub use xor::XorFec;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum FecError {
    #[error("Not enough packets received for FEC recovery: needed {needed}, got {received}")]
    InsufficientPackets { needed: usize, received: usize },

    #[error("Invalid FEC block configuration: K={k}, M={m}")]
    InvalidConfiguration { k: usize, m: usize },

    #[error("Corrupt FEC parity packet header")]
    CorruptHeader,

    #[error("Matrix inversion failed during Reed-Solomon decoding")]
    SingularMatrix,
}

pub const FEC_HEADER_SIZE: usize = 20;
pub const FEC_MAGIC_RS: [u8; 4] = *b"QFRS";
pub const FEC_MAGIC_XOR: [u8; 4] = *b"QFXR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FecScheme {
    ReedSolomon,
    Xor,
}

/// Header identifying an FEC parity chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecHeader {
    pub magic: [u8; 4],
    pub scheme: FecScheme,
    pub block_id: u64,
    pub source_count: u16, // K
    pub parity_count: u16, // M
    pub parity_index: u16, // 0..M-1
    pub symbol_size: u16,  // uniform symbol size including 2-byte length prefix
}

impl FecHeader {
    pub fn new(
        scheme: FecScheme,
        block_id: u64,
        source_count: u16,
        parity_count: u16,
        parity_index: u16,
        symbol_size: u16,
    ) -> Self {
        let magic = match scheme {
            FecScheme::ReedSolomon => FEC_MAGIC_RS,
            FecScheme::Xor => FEC_MAGIC_XOR,
        };
        Self {
            magic,
            scheme,
            block_id,
            source_count,
            parity_count,
            parity_index,
            symbol_size,
        }
    }

    pub fn encode(&self, dst: &mut BytesMut) {
        dst.put_slice(&self.magic);
        dst.put_u64(self.block_id);
        dst.put_u16(self.source_count);
        dst.put_u16(self.parity_count);
        dst.put_u16(self.parity_index);
        dst.put_u16(self.symbol_size);
    }

    pub fn decode(src: &mut &[u8]) -> Result<Self, FecError> {
        if src.len() < FEC_HEADER_SIZE {
            return Err(FecError::CorruptHeader);
        }

        let mut magic = [0u8; 4];
        magic.copy_from_slice(&src[0..4]);
        let scheme = if magic == FEC_MAGIC_RS {
            FecScheme::ReedSolomon
        } else if magic == FEC_MAGIC_XOR {
            FecScheme::Xor
        } else {
            return Err(FecError::CorruptHeader);
        };

        let block_id = u64::from_be_bytes(src[4..12].try_into().unwrap());
        let source_count = u16::from_be_bytes(src[12..14].try_into().unwrap());
        let parity_count = u16::from_be_bytes(src[14..16].try_into().unwrap());
        let parity_index = u16::from_be_bytes(src[16..18].try_into().unwrap());
        let symbol_size = u16::from_be_bytes(src[18..20].try_into().unwrap());

        src.advance(FEC_HEADER_SIZE);

        Ok(Self {
            magic,
            scheme,
            block_id,
            source_count,
            parity_count,
            parity_index,
            symbol_size,
        })
    }
}
