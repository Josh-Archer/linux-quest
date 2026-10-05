//! Video decoder factory for instantiating hardware or mock decoders.

use crate::config::ClientConfig;
use crate::decoder::mock::MockHardwareDecoder;
use crate::decoder::HardwareVideoDecoder;
use crate::error::ClientResult;

#[cfg(target_os = "android")]
use crate::decoder::mediacodec::AndroidMediaCodecDecoder;

/// Instantiates an appropriate video decoder based on platform and configuration.
pub fn create_decoder(
    config: &ClientConfig,
    width: u32,
    height: u32,
    surface_window: Option<*mut std::ffi::c_void>,
) -> ClientResult<Box<dyn HardwareVideoDecoder>> {
    #[cfg(target_os = "android")]
    {
        if !config.force_mock_decoder {
            let mut decoder = AndroidMediaCodecDecoder::new();
            if let Some(win) = surface_window {
                unsafe {
                    decoder.set_surface_window(win as *mut ndk_sys::ANativeWindow);
                }
            }
            decoder.init(width, height, config.codec)?;
            return Ok(Box::new(decoder));
        }
    }

    #[cfg(not(target_os = "android"))]
    let _ = surface_window;

    let mut mock = MockHardwareDecoder::new(width, height, config.codec);
    mock.init(width, height, config.codec)?;
    Ok(Box::new(mock))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_decoder_mock() {
        let config = ClientConfig {
            force_mock_decoder: true,
            ..Default::default()
        };
        let decoder = create_decoder(&config, 1920, 1080, None).unwrap();
        let stats = decoder.stats();
        assert!(stats.decoder_name.contains("mock"));
    }
}
