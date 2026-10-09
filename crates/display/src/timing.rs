//! VESA Coordinated Video Timings (CVT) and Reduced Blanking (CVT-RB) calculations.

use crate::error::DisplayError;

/// Accurate VESA CVT / CVT-RB timing parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct CvtTiming {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub pixel_clock_hz: u64,
    pub pixel_clock_mhz: f64,
    pub h_freq_khz: f64,
    pub h_active: u32,
    pub h_front_porch: u32,
    pub h_sync_pulse: u32,
    pub h_back_porch: u32,
    pub h_blank: u32,
    pub h_total: u32,
    pub v_active: u32,
    pub v_front_porch: u32,
    pub v_sync_pulse: u32,
    pub v_back_porch: u32,
    pub v_blank: u32,
    pub v_total: u32,
    pub h_sync_pol_positive: bool,
    pub v_sync_pol_positive: bool,
}

impl CvtTiming {
    /// Calculates VESA CVT Reduced Blanking timing parameters.
    ///
    /// - For multiples of 60 Hz (e.g. 60, 120 Hz): uses VESA CVT-RB v1 with aspect-ratio-dependent
    ///   vertical sync width (4/5/6/7/10 lines) and 160 px horizontal blanking.
    /// - For non-multiples of 60 Hz (e.g. 72, 90, 144 Hz): uses VESA CVT 1.2 RB2 with 80 px
    ///   horizontal blanking, 6 lines vertical back porch, 8 lines vertical sync, and 1 kHz clock step.
    ///
    /// Both modes use +hsync -vsync polarities.
    pub fn calculate(width: u32, height: u32, refresh_rate: u32) -> Result<Self, DisplayError> {
        if width == 0 || height == 0 || refresh_rate == 0 {
            return Err(DisplayError::InvalidConfiguration(
                "Dimensions and refresh rate must be non-zero".into(),
            ));
        }

        // VESA CVT requires horizontal active pixels to be a multiple of 8 character cells
        let width = (width / 8) * 8;
        if width == 0 {
            return Err(DisplayError::InvalidConfiguration(
                "Horizontal resolution must be at least 8 pixels".into(),
            ));
        }

        let is_rb_v1 = refresh_rate.is_multiple_of(60);

        let (
            h_blank,
            h_sync,
            h_front_porch,
            v_sync,
            v_front_porch,
            v_back_porch,
            v_blank,
            v_total,
            pixel_clock_hz,
            pixel_clock_mhz,
        ) = if is_rb_v1 {
            // CVT-RB v1 (VESA-2003-3)
            // Vertical sync width follows the CVT aspect ratio table
            let v_sync = if 3 * width == 4 * height {
                4u32 // 4:3
            } else if 9 * width == 16 * height {
                5u32 // 16:9
            } else if 10 * width == 16 * height || 5 * width == 8 * height {
                6u32 // 16:10
            } else if 4 * width == 5 * height || 9 * width == 15 * height || 3 * width == 5 * height
            {
                7u32 // 5:4 or 15:9
            } else {
                10u32 // 21:9 ultrawide or custom
            };

            let h_blank = 160u32;
            let h_sync = 32u32;
            let h_front_porch = 48u32;
            let v_front_porch = 3u32;

            let v_bi_min_s = 460.0e-6;
            let v_field_rate = refresh_rate as f64;
            let h_period_est = ((1.0 / v_field_rate) - v_bi_min_s) / (height as f64);
            if h_period_est <= 0.0 {
                return Err(DisplayError::InvalidConfiguration(
                    "Requested mode results in invalid line timing".into(),
                ));
            }

            let v_bi_lines = (v_bi_min_s / h_period_est).floor() as u32 + 1;
            let min_v_blank = v_sync + v_front_porch + 1;
            let v_blank = v_bi_lines.max(min_v_blank);
            let v_back_porch = v_blank - v_sync - v_front_porch;
            let v_total = height + v_blank;
            let h_total = width + h_blank;

            // Pixel clock in 0.25 MHz steps
            let act_pixel_freq = (h_total as f64) * (1.0 / h_period_est);
            let pixel_clock_mhz = (act_pixel_freq / 0.25e6).floor() * 0.25;
            let pixel_clock_hz = (pixel_clock_mhz * 1e6).round() as u64;

            (
                h_blank,
                h_sync,
                h_front_porch,
                v_sync,
                v_front_porch,
                v_back_porch,
                v_blank,
                v_total,
                pixel_clock_hz,
                pixel_clock_mhz,
            )
        } else {
            // CVT-RB v2 (VESA 1.2 RB2)
            let h_blank = 80u32;
            let h_sync = 32u32;
            let h_front_porch = 8u32;
            let v_sync = 8u32;
            let v_back_porch = 6u32;

            let v_bi_min_s = 460.0e-6;
            let v_field_rate = refresh_rate as f64;
            let h_period_est = ((1.0 / v_field_rate) - v_bi_min_s) / (height as f64);
            if h_period_est <= 0.0 {
                return Err(DisplayError::InvalidConfiguration(
                    "Requested mode results in invalid line timing".into(),
                ));
            }

            let v_bi_lines = (v_bi_min_s / h_period_est).floor() as u32 + 1;
            let min_v_blank = v_sync + v_back_porch + 1;
            let v_blank = v_bi_lines.max(min_v_blank);
            let v_front_porch = v_blank - v_sync - v_back_porch;
            let v_total = height + v_blank;
            let h_total = width + h_blank;

            // Exact pixel clock = refresh_rate * h_total * v_total, rounded down to 1 kHz (0.001 MHz) per VESA CVT 1.2
            let exact_pclk = (refresh_rate as u64) * (h_total as u64) * (v_total as u64);
            let pclk_khz = ((exact_pclk as f64) / 1000.0).floor() as u64;
            let pixel_clock_hz = pclk_khz * 1000;
            let pixel_clock_mhz = (pixel_clock_hz as f64) / 1_000_000.0;

            (
                h_blank,
                h_sync,
                h_front_porch,
                v_sync,
                v_front_porch,
                v_back_porch,
                v_blank,
                v_total,
                pixel_clock_hz,
                pixel_clock_mhz,
            )
        };

        let h_back_porch = h_blank.saturating_sub(h_sync + h_front_porch);
        let h_total = width + h_blank;
        let h_freq_khz = (pixel_clock_hz as f64 / h_total as f64) / 1000.0;

        Ok(Self {
            width,
            height,
            refresh_rate,
            pixel_clock_hz,
            pixel_clock_mhz,
            h_freq_khz,
            h_active: width,
            h_front_porch,
            h_sync_pulse: h_sync,
            h_back_porch,
            h_blank,
            h_total,
            v_active: height,
            v_front_porch,
            v_sync_pulse: v_sync,
            v_back_porch,
            v_blank,
            v_total,
            h_sync_pol_positive: true,
            v_sync_pol_positive: false, // Reduced blanking specifies +hsync -vsync
        })
    }

    /// Formats an X11 / xrandr CVT modeline string.
    pub fn x11_modeline_params(&self) -> String {
        let h_sync_start = self.h_active + self.h_front_porch;
        let h_sync_end = h_sync_start + self.h_sync_pulse;
        let v_sync_start = self.v_active + self.v_front_porch;
        let v_sync_end = v_sync_start + self.v_sync_pulse;

        let h_pol = if self.h_sync_pol_positive {
            "+hsync"
        } else {
            "-hsync"
        };
        let v_pol = if self.v_sync_pol_positive {
            "+vsync"
        } else {
            "-vsync"
        };

        format!(
            "{:.2} {} {} {} {} {} {} {} {} {} {}",
            self.pixel_clock_mhz,
            self.h_active,
            h_sync_start,
            h_sync_end,
            self.h_total,
            self.v_active,
            v_sync_start,
            v_sync_end,
            self.v_total,
            h_pol,
            v_pol
        )
    }

    /// Validates that timing parameters fit within VESA EDID 1.4 DTD limits (655.35 MHz pixel clock,
    /// 15-250 kHz horizontal frequency, 6-bit vertical porches <= 63, and 10-bit horizontal porches <= 1023).
    pub fn validate_edid_limits(&self) -> Result<(), DisplayError> {
        if self.pixel_clock_hz > 655_350_000 {
            return Err(DisplayError::Edid(format!(
                "Mode {}x{}@{}Hz requires pixel clock {:.2} MHz, exceeding base EDID 1.4 DTD limit of 655.35 MHz",
                self.width, self.height, self.refresh_rate, self.pixel_clock_mhz
            )));
        }

        if !(15.0..=250.0).contains(&self.h_freq_khz) {
            return Err(DisplayError::Edid(format!(
                "Horizontal frequency {:.3} kHz falls outside declared range limits [15.0, 250.0] kHz",
                self.h_freq_khz
            )));
        }

        if self.v_front_porch > 63 {
            return Err(DisplayError::Edid(format!(
                "Mode {}x{}@{}Hz has vertical front porch of {} lines which exceeds base EDID 1.4 DTD 6-bit limit (63 lines)",
                self.width, self.height, self.refresh_rate, self.v_front_porch
            )));
        }

        if self.v_sync_pulse > 63 {
            return Err(DisplayError::Edid(format!(
                "Mode {}x{}@{}Hz has vertical sync pulse of {} lines which exceeds base EDID 1.4 DTD 6-bit limit (63 lines)",
                self.width, self.height, self.refresh_rate, self.v_sync_pulse
            )));
        }

        if self.h_front_porch > 1023 || self.h_sync_pulse > 1023 {
            return Err(DisplayError::Edid(format!(
                "Mode {}x{}@{}Hz has horizontal porch/sync exceeding base EDID 1.4 DTD 10-bit limit (1023 pixels)",
                self.width, self.height, self.refresh_rate
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cvt_timing_1080p60() {
        let t = CvtTiming::calculate(1920, 1080, 60).unwrap();
        assert_eq!(t.pixel_clock_mhz, 138.50);
        assert_eq!(t.h_blank, 160);
        assert_eq!(t.h_total, 2080);
        assert_eq!(t.v_blank, 31);
        assert_eq!(t.v_total, 1111);
        assert_eq!(t.h_front_porch, 48);
        assert_eq!(t.h_sync_pulse, 32);
        assert_eq!(t.v_front_porch, 3);
        assert_eq!(t.v_sync_pulse, 5); // 16:9 aspect ratio
        assert!(t.h_sync_pol_positive);
        assert!(!t.v_sync_pol_positive);
    }

    #[test]
    fn test_cvt_timing_1080p90_rb2() {
        let t = CvtTiming::calculate(1920, 1080, 90).unwrap();
        assert_eq!(t.pixel_clock_mhz, 202.86);
        assert_eq!(t.h_blank, 80);
        assert_eq!(t.h_total, 2000);
        assert_eq!(t.v_front_porch, 33);
        assert_eq!(t.v_sync_pulse, 8);
        assert_eq!(t.v_back_porch, 6);
        assert_eq!(t.v_total, 1127);
    }

    #[test]
    fn test_cvt_timing_ultrawide_60_and_90() {
        // 3440x1440@60 (21:9 aspect ratio -> vsync 10)
        let t60 = CvtTiming::calculate(3440, 1440, 60).unwrap();
        assert_eq!(t60.pixel_clock_mhz, 319.75);
        assert_eq!(t60.v_sync_pulse, 10);
        assert_eq!(t60.v_total, 1481);

        // 3440x1440@90 (RB2 -> 476.15 MHz)
        let t90 = CvtTiming::calculate(3440, 1440, 90).unwrap();
        assert_eq!(t90.pixel_clock_mhz, 476.15);
        assert_eq!(t90.v_total, 1503);
    }

    #[test]
    fn test_cvt_timing_1200p60_16_10() {
        // 1920x1200@60 (16:10 aspect ratio -> vsync 6)
        let t = CvtTiming::calculate(1920, 1200, 60).unwrap();
        assert_eq!(t.v_sync_pulse, 6);
        assert_eq!(t.v_active + t.v_front_porch, 1203);
        assert_eq!(t.v_active + t.v_front_porch + t.v_sync_pulse, 1209);
    }

    #[test]
    fn test_cvt_timing_1440p144_rb2_vback() {
        let t = CvtTiming::calculate(2560, 1440, 144).unwrap();
        // VESA CVT 1.2 RB2 requires fixed vertical back porch of 6 lines
        assert_eq!(t.v_back_porch, 6);
        assert_eq!(t.v_sync_pulse, 8);
        assert_eq!(t.v_front_porch, 89);
        assert_eq!(t.pixel_clock_mhz, 586.586);
    }
}
