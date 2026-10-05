//! Video decoder factory for instantiating hardware or mock decoders.

use crate::config::ClientConfig;
use crate::decoder::mock::MockHardwareDecoder;
use crate::decoder::HardwareVideoDecoder;

#[cfg(target_os = "android")]
use crate::decoder::mediacodec::AndroidMediaCodecDecoder;

/// Instantiates an appropriate video decoder based on platform and configuration.
pub fn create_decoder(
    config: &ClientConfig,
    width: u32,
    height: u32,
) -> Box<dyn HardwareVideoDecoder> {
    #[cfg(target_os = "android")]
    {
        if !config.force_mock_decoder {
            let mut decoder = AndroidMediaCodecDecoder::new();
            if decoder.init(width, height, config.codec).is_ok() {
                return Box::new(decoder);
            }
            tracing::warn!(
                "Failed to initialize Android AMediaCodec, falling back to mock decoder"
            );
        }
    }

    let mut mock = MockHardwareDecoder::new(width, height, config.codec);
    let _ = mock.init(width, height, config.codec);
    Box::new(mock)
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
        let decoder = create_decoder(&config, 1920, 1080);
        let stats = decoder.stats();
        assert!(stats.decoder_name.contains("mock"));
    }
}
