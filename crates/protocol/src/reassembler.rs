use bytes::{Bytes, BytesMut};
use std::collections::HashMap;

use crate::error::ProtocolError;
use crate::video::{VideoChunk, VideoChunkMeta};

/// An assembled video frame ready to be handed to hardware decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledFrame {
    pub meta: VideoChunkMeta,
    pub bitstream: Bytes,
}

struct FrameSlot {
    meta: Option<VideoChunkMeta>,
    total_chunks: u16,
    chunks: Vec<Option<Bytes>>,
    received_count: u16,
    total_bytes: usize,
}

impl FrameSlot {
    fn new(total_chunks: u16) -> Self {
        let mut chunks = Vec::with_capacity(total_chunks as usize);
        chunks.resize_with(total_chunks as usize, || None);
        Self {
            meta: None,
            total_chunks,
            chunks,
            received_count: 0,
            total_bytes: 0,
        }
    }

    fn insert(&mut self, chunk: VideoChunk) -> Result<bool, ProtocolError> {
        let idx = chunk.meta.chunk_index as usize;
        if idx >= self.total_chunks as usize {
            return Err(ProtocolError::CorruptChunkIndex {
                chunk: chunk.meta.chunk_index,
                total: self.total_chunks,
            });
        }

        if self.chunks[idx].is_none() {
            self.total_bytes += chunk.payload.len();
            self.chunks[idx] = Some(chunk.payload);
            self.received_count += 1;
            if self.meta.is_none() {
                self.meta = Some(chunk.meta);
            }
        }

        Ok(self.received_count == self.total_chunks)
    }

    fn assemble(self) -> Option<AssembledFrame> {
        let meta = self.meta?;
        let mut buf = BytesMut::with_capacity(self.total_bytes);
        for chunk in self.chunks {
            let chunk_bytes = chunk?;
            buf.extend_from_slice(&chunk_bytes);
        }
        Some(AssembledFrame {
            meta,
            bitstream: buf.freeze(),
        })
    }
}

/// Jitter buffer and frame reassembler for incoming video chunks.
pub struct FrameReassembler {
    active_frames: HashMap<u64, FrameSlot>,
    highest_frame_id: u64,
    max_active_frames: usize,
}

impl Default for FrameReassembler {
    fn default() -> Self {
        Self::new(16)
    }
}

impl FrameReassembler {
    pub fn new(max_active_frames: usize) -> Self {
        Self {
            active_frames: HashMap::new(),
            highest_frame_id: 0,
            max_active_frames,
        }
    }

    /// Feeds a chunk into the reassembler. If the frame is now complete, returns `Some(AssembledFrame)`.
    pub fn ingest_chunk(
        &mut self,
        chunk: VideoChunk,
    ) -> Result<Option<AssembledFrame>, ProtocolError> {
        let frame_id = chunk.meta.frame_id;
        let total_chunks = chunk.meta.total_chunks;

        if frame_id > self.highest_frame_id {
            self.highest_frame_id = frame_id;
            // Evict old incomplete frames to prevent latency spiral
            if self.active_frames.len() >= self.max_active_frames {
                let threshold = self
                    .highest_frame_id
                    .saturating_sub(self.max_active_frames as u64);
                self.active_frames.retain(|&id, _| id >= threshold);
            }
        }

        let slot = self
            .active_frames
            .entry(frame_id)
            .or_insert_with(|| FrameSlot::new(total_chunks));

        let is_complete = slot.insert(chunk)?;
        if is_complete {
            if let Some(slot) = self.active_frames.remove(&frame_id) {
                return Ok(slot.assemble());
            }
        }

        Ok(None)
    }

    pub fn active_frame_count(&self) -> usize {
        self.active_frames.len()
    }
}
