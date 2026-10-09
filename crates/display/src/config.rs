use serde::{Deserialize, Serialize};

use crate::error::DisplayError;
use crate::timing::CvtTiming;

/// Configuration defining the geometry and timing of a virtual display output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualMonitorConfig {
    /// Monotonic identifier for this virtual display (e.g. 1, 2, 3).
    pub id: u32,

    /// Human-readable connector or output name (e.g. "Quest-1", "VIRTUAL-1", "HEADLESS-1").
    pub name: String,

    /// Horizontal resolution in pixels.
    pub width: u32,

    /// Vertical resolution in pixels.
    pub height: u32,

    /// Target refresh rate in Hertz (e.g. 60, 72, 90, 120).
    pub refresh_rate: u32,

    /// Dots per inch for UI scaling.
    pub dpi: u32,

    /// Optional raw VESA EDID 1.4 binary payload.
    pub edid: Option<Vec<u8>>,

    /// Whether this display is currently active.
    pub enabled: bool,
}

impl VirtualMonitorConfig {
    /// Creates a new virtual monitor configuration with horizontal width aligned to 8 character cells.
    pub fn new(
        id: u32,
        name: impl Into<String>,
        width: u32,
        height: u32,
        refresh_rate: u32,
    ) -> Self {
        let aligned_width = (width / 8) * 8;
        Self {
            id,
            name: name.into(),
            width: aligned_width,
            height,
            refresh_rate,
            dpi: 96,
            edid: None,
            enabled: true,
        }
    }

    /// Sets the DPI for the monitor configuration.
    pub fn with_dpi(mut self, dpi: u32) -> Self {
        self.dpi = dpi;
        self
    }

    /// Attaches an EDID byte vector.
    pub fn with_edid(mut self, edid: Vec<u8>) -> Self {
        self.edid = Some(edid);
        self
    }

    /// Validates display resolution and timing boundaries against EDID 1.4 DTD limits.
    pub fn validate(&self) -> Result<(), DisplayError> {
        if self.width < 640 || self.width > 4095 {
            return Err(DisplayError::InvalidConfiguration(format!(
                "Width {} is out of supported range [640, 4095] for standard EDID 1.4 DTD",
                self.width
            )));
        }
        if !self.width.is_multiple_of(8) {
            return Err(DisplayError::InvalidConfiguration(format!(
                "Width {} must be a multiple of 8 character cells for VESA CVT / EDID 1.4 DTD",
                self.width
            )));
        }
        if self.height < 480 || self.height > 4095 {
            return Err(DisplayError::InvalidConfiguration(format!(
                "Height {} is out of supported range [480, 4095] for standard EDID 1.4 DTD",
                self.height
            )));
        }
        if !(30..=240).contains(&self.refresh_rate) {
            return Err(DisplayError::InvalidConfiguration(format!(
                "Refresh rate {} Hz is out of supported range [30, 240]",
                self.refresh_rate
            )));
        }

        let timing = CvtTiming::calculate(self.width, self.height, self.refresh_rate)?;
        timing.validate_edid_limits()?;

        Ok(())
    }

    /// 1080p preset (1920x1080) at specified refresh rate.
    pub fn preset_1080p(id: u32, refresh_rate: u32) -> Self {
        Self::new(id, format!("Quest-1080p-{id}"), 1920, 1080, refresh_rate)
    }

    /// 1440p preset (2560x1440) at specified refresh rate (ideal for Quest 3 1:1 clarity).
    pub fn preset_1440p(id: u32, refresh_rate: u32) -> Self {
        Self::new(id, format!("Quest-1440p-{id}"), 2560, 1440, refresh_rate)
    }

    /// 4K UHD preset (3840x2160) at specified refresh rate (max 60Hz within 655.35 MHz limit).
    pub fn preset_4k(id: u32, refresh_rate: u32) -> Self {
        Self::new(id, format!("Quest-4K-{id}"), 3840, 2160, refresh_rate)
    }

    /// Ultrawide preset (3440x1440) at specified refresh rate (max 90Hz within 655.35 MHz limit).
    pub fn preset_ultrawide(id: u32, refresh_rate: u32) -> Self {
        Self::new(id, format!("Quest-UW-{id}"), 3440, 1440, refresh_rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_width_multiple_of_8_alignment() {
        // 1366x768 should be aligned to 1360 (1366 / 8 * 8 = 1360)
        let cfg = VirtualMonitorConfig::new(1, "Test-1366", 1366, 768, 60);
        assert_eq!(cfg.width, 1360);
        assert!(cfg.validate().is_ok());

        // Unaligned struct literal directly constructed must be rejected by validate
        let mut unaligned = cfg;
        unaligned.width = 1366;
        let err = unaligned.validate().unwrap_err();
        assert!(err.to_string().contains("multiple of 8 character cells"));
    }

    #[test]
    fn test_config_validate_rejects_1440p144_vfront_porch() {
        let cfg = VirtualMonitorConfig::new(1, "Test-1440p144", 2560, 1440, 144);
        let err = cfg.validate().unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains(
                "vertical front porch of 89 lines which exceeds base EDID 1.4 DTD 6-bit limit"
            ),
            "Expected DTD Vfront > 63 error, got: {err_str}"
        );
    }
}
