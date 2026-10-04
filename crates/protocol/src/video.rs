use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum VideoCodec {
    Av1 = 1,
    Hevc = 2,
    H264 = 3,
}

impl TryFrom<u8> for VideoCodec {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Av1),
            2 => Ok(Self::Hevc),
            3 => Ok(Self::H264),
            other => Err(ProtocolError::DeserializationError(format!(
                "Unknown codec ID: {other}"
            ))),
        }
    }
}

/// Metadata describing a video frame chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoChunkMeta {
    pub frame_id: u64,
    pub chunk_index: u16,
    pub total_chunks: u16,
    pub codec: VideoCodec,
    pub is_keyframe: bool,
    pub is_intra_refresh: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub pts_us: u64,
}

/// Fully packaged video chunk ready for network transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoChunk {
    pub meta: VideoChunkMeta,
    pub payload: Bytes,
}

impl VideoChunk {
    pub fn new(meta: VideoChunkMeta, payload: Bytes) -> Self {
        Self { meta, payload }
    }

    pub fn serialize(&self) -> Result<Bytes, ProtocolError> {
        let meta_bytes = bincode::serialize(&self.meta)
            .map_err(|e| ProtocolError::SerializationError(e.to_string()))?;

        let mut buf = bytes::BytesMut::with_capacity(4 + meta_bytes.len() + self.payload.len());
        buf.extend_from_slice(&(meta_bytes.len() as u32).to_be_bytes());
        buf.extend_from_slice(&meta_bytes);
        buf.extend_from_slice(&self.payload);
        Ok(buf.freeze())
    }

    pub fn deserialize(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < 4 {
            return Err(ProtocolError::BufferTooShort {
                required: 4,
                found: data.len(),
            });
        }

        let meta_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if data.len() < 4 + meta_len {
            return Err(ProtocolError::BufferTooShort {
                required: 4 + meta_len,
                found: data.len(),
            });
        }

        let meta: VideoChunkMeta = bincode::deserialize(&data[4..4 + meta_len])
            .map_err(|e| ProtocolError::DeserializationError(e.to_string()))?;

        let payload = Bytes::copy_from_slice(&data[4 + meta_len..]);
        Ok(Self { meta, payload })
    }
}
