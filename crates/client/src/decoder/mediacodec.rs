//! Android AMediaCodec zero-copy hardware video decoder implementation.

use std::collections::VecDeque;
use std::ffi::CString;
use std::time::Instant;

use crate::config::ClientVideoCodec;
use crate::decoder::{DecodedFrame, DecoderStats, HardwareVideoDecoder};
use crate::error::{ClientError, ClientResult};

/// Android hardware video decoder backed by NDK AMediaCodec.
pub struct AndroidMediaCodecDecoder {
    codec_ptr: *mut ndk_sys::AMediaCodec,
    width: u32,
    height: u32,
    video_codec: ClientVideoCodec,
    stats: DecoderStats,
    last_pts_us: u64,
    enqueue_timestamps: VecDeque<(u64, Instant)>,
    surface_window_ptr: *mut ndk_sys::ANativeWindow,
}

// AMediaCodec functions in Android NDK can be safely called across threads.
unsafe impl Send for AndroidMediaCodecDecoder {}

impl AndroidMediaCodecDecoder {
    /// Creates an uninitialized AMediaCodec decoder.
    pub fn new() -> Self {
        Self {
            codec_ptr: std::ptr::null_mut(),
            width: 0,
            height: 0,
            video_codec: ClientVideoCodec::Av1,
            stats: DecoderStats::default(),
            last_pts_us: 0,
            enqueue_timestamps: VecDeque::with_capacity(32),
            surface_window_ptr: std::ptr::null_mut(),
        }
    }

    /// Sets the target native window surface for zero-copy direct rendering.
    ///
    /// # Safety
    /// `window` must point to a valid `ANativeWindow` that remains valid during playback.
    pub unsafe fn set_surface_window(&mut self, window: *mut ndk_sys::ANativeWindow) {
        self.surface_window_ptr = window;
    }
}

impl Default for AndroidMediaCodecDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl HardwareVideoDecoder for AndroidMediaCodecDecoder {
    fn init(&mut self, width: u32, height: u32, codec: ClientVideoCodec) -> ClientResult<()> {
        if width == 0 || height == 0 {
            return Err(ClientError::Decoder(format!(
                "Invalid decode resolution: {width}x{height}"
            )));
        }

        self.width = width;
        self.height = height;
        self.video_codec = codec;

        // Clean up previous codec if exists
        if !self.codec_ptr.is_null() {
            unsafe {
                ndk_sys::AMediaCodec_stop(self.codec_ptr);
                ndk_sys::AMediaCodec_delete(self.codec_ptr);
            }
            self.codec_ptr = std::ptr::null_mut();
        }

        let low_latency_name = codec.qti_low_latency_name();
        let mime_type = codec.mime_type();

        let c_low_latency = CString::new(low_latency_name)
            .map_err(|e| ClientError::Decoder(format!("CString error: {e}")))?;
        let c_mime = CString::new(mime_type)
            .map_err(|e| ClientError::Decoder(format!("CString error: {e}")))?;

        // Try Snapdragon XR2 Gen 2 low-latency decoder first, then generic MIME decoder
        let (codec_ptr, chosen_name) = unsafe {
            let by_name = ndk_sys::AMediaCodec_createCodecByName(c_low_latency.as_ptr());
            if !by_name.is_null() {
                (by_name, low_latency_name.to_string())
            } else {
                let by_type = ndk_sys::AMediaCodec_createDecoderByType(c_mime.as_ptr());
                if !by_type.is_null() {
                    (by_type, format!("generic.{mime_type}"))
                } else {
                    return Err(ClientError::Decoder(format!(
                        "Failed to create AMediaCodec for MIME {mime_type}"
                    )));
                }
            }
        };

        // Configure AMediaFormat
        unsafe {
            let format = ndk_sys::AMediaFormat_new();
            if format.is_null() {
                ndk_sys::AMediaCodec_delete(codec_ptr);
                return Err(ClientError::Decoder(
                    "AMediaFormat_new returned NULL".to_string(),
                ));
            }

            let c_mime_key = CString::new("mime").unwrap();
            let c_width_key = CString::new("width").unwrap();
            let c_height_key = CString::new("height").unwrap();
            let c_low_latency_key = CString::new("low-latency").unwrap();
            let c_priority_key = CString::new("priority").unwrap();
            let c_operating_rate_key = CString::new("operating-rate").unwrap();

            ndk_sys::AMediaFormat_setString(format, c_mime_key.as_ptr(), c_mime.as_ptr());
            ndk_sys::AMediaFormat_setInt32(format, c_width_key.as_ptr(), width as i32);
            ndk_sys::AMediaFormat_setInt32(format, c_height_key.as_ptr(), height as i32);
            ndk_sys::AMediaFormat_setInt32(format, c_low_latency_key.as_ptr(), 1);
            ndk_sys::AMediaFormat_setInt32(format, c_priority_key.as_ptr(), 0); // Real-time priority
            ndk_sys::AMediaFormat_setFloat(format, c_operating_rate_key.as_ptr(), 120.0);

            let status = ndk_sys::AMediaCodec_configure(
                codec_ptr,
                format,
                self.surface_window_ptr,
                std::ptr::null_mut(),
                0,
            );
            ndk_sys::AMediaFormat_delete(format);

            if status.0 != 0 {
                ndk_sys::AMediaCodec_delete(codec_ptr);
                return Err(ClientError::Decoder(format!(
                    "AMediaCodec_configure failed with status {status:?}"
                )));
            }

            let start_status = ndk_sys::AMediaCodec_start(codec_ptr);
            if start_status.0 != 0 {
                ndk_sys::AMediaCodec_delete(codec_ptr);
                return Err(ClientError::Decoder(format!(
                    "AMediaCodec_start failed with status {start_status:?}"
                )));
            }
        }

        self.codec_ptr = codec_ptr;
        self.stats = DecoderStats {
            decoder_name: chosen_name,
            ..Default::default()
        };

        Ok(())
    }

    fn queue_input_buffer(
        &mut self,
        data: &[u8],
        pts_us: u64,
        is_keyframe: bool,
    ) -> ClientResult<()> {
        if self.codec_ptr.is_null() {
            return Err(ClientError::Decoder(
                "AMediaCodec not initialized".to_string(),
            ));
        }

        if data.is_empty() {
            return Err(ClientError::Decoder(
                "Cannot queue empty buffer into AMediaCodec".to_string(),
            ));
        }

        // Dequeue an available input buffer with a short 1ms (1000us) timeout
        let input_idx = unsafe { ndk_sys::AMediaCodec_dequeueInputBuffer(self.codec_ptr, 1000) };

        if input_idx < 0 {
            self.stats.frames_dropped += 1;
            return Err(ClientError::Decoder(format!(
                "AMediaCodec input buffer queue unavailable: {input_idx}"
            )));
        }

        let idx = input_idx as usize;
        let mut out_size: usize = 0;
        let buf_ptr =
            unsafe { ndk_sys::AMediaCodec_getInputBuffer(self.codec_ptr, idx, &mut out_size) };

        if buf_ptr.is_null() || out_size < data.len() {
            // Return buffer back to codec so it is not permanently leaked
            unsafe {
                let _ = ndk_sys::AMediaCodec_queueInputBuffer(self.codec_ptr, idx, 0, 0, 0, 0);
            }
            return Err(ClientError::Decoder(format!(
                "AMediaCodec input buffer at index {idx} capacity ({out_size}) too small for packet ({})",
                data.len()
            )));
        }

        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), buf_ptr, data.len());
            let flags = if is_keyframe {
                1 /* BUFFER_FLAG_KEY_FRAME */
            } else {
                0
            };
            let queue_status = ndk_sys::AMediaCodec_queueInputBuffer(
                self.codec_ptr,
                idx,
                0,
                data.len(),
                pts_us,
                flags,
            );
            if queue_status.0 != 0 {
                let _ = ndk_sys::AMediaCodec_flush(self.codec_ptr);
                return Err(ClientError::Decoder(format!(
                    "AMediaCodec_queueInputBuffer failed: {queue_status:?}"
                )));
            }
        }

        self.stats.frames_queued += 1;
        self.last_pts_us = pts_us;
        if self.enqueue_timestamps.len() >= 64 {
            self.enqueue_timestamps.pop_front();
        }
        self.enqueue_timestamps.push_back((pts_us, Instant::now()));
        Ok(())
    }

    fn dequeue_output_buffer(&mut self, timeout_us: i64) -> ClientResult<Option<DecodedFrame>> {
        if self.codec_ptr.is_null() {
            return Err(ClientError::Decoder(
                "AMediaCodec not initialized".to_string(),
            ));
        }

        let mut buffer_info = ndk_sys::AMediaCodecBufferInfo {
            offset: 0,
            size: 0,
            presentationTimeUs: 0,
            flags: 0,
        };

        let output_idx = unsafe {
            ndk_sys::AMediaCodec_dequeueOutputBuffer(
                self.codec_ptr,
                &mut buffer_info,
                timeout_us.max(0),
            )
        };

        if output_idx >= 0 {
            let idx = output_idx as usize;
            let pts_us = buffer_info.presentationTimeUs as u64;
            let is_keyframe = (buffer_info.flags & 1) != 0;

            // Compute pipelined decode latency by matching pts_us
            let latency = if let Some(pos) = self
                .enqueue_timestamps
                .iter()
                .position(|(pts, _)| *pts == pts_us)
            {
                let (_, enqueue_time) = self.enqueue_timestamps.remove(pos).unwrap();
                enqueue_time.elapsed().as_micros() as u64
            } else if let Some((_, enqueue_time)) = self.enqueue_timestamps.pop_front() {
                enqueue_time.elapsed().as_micros() as u64
            } else {
                1500
            };
            self.stats.last_latency_us = latency;
            if self.stats.frames_decoded == 0 {
                self.stats.avg_latency_us = latency;
            } else {
                self.stats.avg_latency_us = (self.stats.avg_latency_us * 7 + latency) / 8;
            }

            self.stats.frames_decoded += 1;

            Ok(Some(DecodedFrame {
                buffer_index: idx,
                pts_us,
                width: self.width,
                height: self.height,
                is_keyframe,
                pixel_data: None, // Zero-copy: rendered straight to surface
            }))
        } else if output_idx == ndk_sys::AMEDIACODEC_INFO_TRY_AGAIN_LATER as isize {
            Ok(None)
        } else if output_idx == ndk_sys::AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED as isize {
            unsafe {
                let format = ndk_sys::AMediaCodec_getOutputFormat(self.codec_ptr);
                if !format.is_null() {
                    let mut w: i32 = 0;
                    let mut h: i32 = 0;
                    let c_w = CString::new("width").unwrap();
                    let c_h = CString::new("height").unwrap();
                    if ndk_sys::AMediaFormat_getInt32(format, c_w.as_ptr(), &mut w) {
                        self.width = w as u32;
                    }
                    if ndk_sys::AMediaFormat_getInt32(format, c_h.as_ptr(), &mut h) {
                        self.height = h as u32;
                    }
                    ndk_sys::AMediaFormat_delete(format);
                    tracing::info!(
                        width = self.width,
                        height = self.height,
                        "AMediaCodec format changed"
                    );
                }
            }
            Ok(None)
        } else {
            Ok(None)
        }
    }

    fn release_output_buffer(&mut self, buffer_index: usize, render: bool) -> ClientResult<()> {
        if self.codec_ptr.is_null() {
            return Err(ClientError::Decoder(
                "AMediaCodec not initialized".to_string(),
            ));
        }

        // Only render to surface if an ANativeWindow was actually attached
        let should_render = render && !self.surface_window_ptr.is_null();
        let status = unsafe {
            ndk_sys::AMediaCodec_releaseOutputBuffer(self.codec_ptr, buffer_index, should_render)
        };

        if status.0 != 0 {
            // Attempt emergency release without rendering to avoid permanently leaking the output buffer slot
            if should_render {
                unsafe {
                    let _ = ndk_sys::AMediaCodec_releaseOutputBuffer(
                        self.codec_ptr,
                        buffer_index,
                        false,
                    );
                }
            }
            return Err(ClientError::Decoder(format!(
                "AMediaCodec_releaseOutputBuffer failed with status {status:?}"
            )));
        }

        if should_render {
            self.stats.frames_rendered += 1;
        } else {
            self.stats.frames_dropped += 1;
        }

        Ok(())
    }

    fn release_output_buffer_at_time(
        &mut self,
        buffer_index: usize,
        render_timestamp_ns: i64,
    ) -> ClientResult<()> {
        if self.codec_ptr.is_null() {
            return Err(ClientError::Decoder(
                "AMediaCodec not initialized".to_string(),
            ));
        }

        // Only render to surface if an ANativeWindow was actually attached
        if self.surface_window_ptr.is_null() {
            return self.release_output_buffer(buffer_index, false);
        }

        let status = unsafe {
            ndk_sys::AMediaCodec_releaseOutputBufferAtTime(
                self.codec_ptr,
                buffer_index,
                render_timestamp_ns,
            )
        };

        if status.0 != 0 {
            // Attempt emergency release without rendering to avoid permanently leaking the output buffer slot
            unsafe {
                let _ =
                    ndk_sys::AMediaCodec_releaseOutputBuffer(self.codec_ptr, buffer_index, false);
            }
            return Err(ClientError::Decoder(format!(
                "AMediaCodec_releaseOutputBufferAtTime failed with status {status:?}"
            )));
        }

        self.stats.frames_rendered += 1;
        Ok(())
    }

    fn flush(&mut self) -> ClientResult<()> {
        self.enqueue_timestamps.clear();
        if !self.codec_ptr.is_null() {
            let status = unsafe { ndk_sys::AMediaCodec_flush(self.codec_ptr) };
            if status.0 != 0 {
                return Err(ClientError::Decoder(format!(
                    "AMediaCodec_flush failed with status {status:?}"
                )));
            }
        }
        Ok(())
    }

    fn stats(&self) -> DecoderStats {
        self.stats.clone()
    }

    unsafe fn set_surface_window(&mut self, window: *mut std::ffi::c_void) -> ClientResult<()> {
        self.surface_window_ptr = window as *mut ndk_sys::ANativeWindow;
        Ok(())
    }
}

impl Drop for AndroidMediaCodecDecoder {
    fn drop(&mut self) {
        if !self.codec_ptr.is_null() {
            unsafe {
                ndk_sys::AMediaCodec_stop(self.codec_ptr);
                ndk_sys::AMediaCodec_delete(self.codec_ptr);
            }
            self.codec_ptr = std::ptr::null_mut();
        }
    }
}
