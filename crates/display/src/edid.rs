use crate::error::DisplayError;
use crate::timing::CvtTiming;

/// Generates a valid 128-byte VESA EDID 1.4 binary descriptor for a virtual display.
pub struct EdidGenerator;

impl EdidGenerator {
    /// Generates a standard 128-byte EDID block matching the target resolution, refresh rate, and display ID.
    ///
    /// Validates that dimensions do not exceed 12-bit limits (4095x4095), that the required
    /// pixel clock does not exceed the 16-bit 10 kHz limit (655.35 MHz) of a base EDID 1.4 DTD,
    /// and that the horizontal line frequency falls within the declared range limits [15.0, 250.0] kHz.
    pub fn generate(
        width: u32,
        height: u32,
        refresh_rate: u32,
        name: &str,
        id: u32,
    ) -> Result<Vec<u8>, DisplayError> {
        if width == 0 || height == 0 || refresh_rate == 0 {
            return Err(DisplayError::Edid(
                "Dimensions and refresh rate must be non-zero".into(),
            ));
        }

        if width > 4095 || height > 4095 {
            return Err(DisplayError::Edid(format!(
                "Dimensions {}x{} exceed base EDID 1.4 12-bit limit (4095x4095)",
                width, height
            )));
        }

        let timing = CvtTiming::calculate(width, height, refresh_rate)?;
        timing.validate_edid_limits()?;

        let mut edid = vec![0u8; 128];

        // 1. EDID Header: 00 FF FF FF FF FF FF 00
        edid[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);

        // 2. Manufacturer ID: "LQT" (LinuxQuest)
        let mfg = encode_mfg_id("LQT");
        edid[8] = (mfg >> 8) as u8;
        edid[9] = (mfg & 0xFF) as u8;

        // 3. Product code (little-endian): 0x5100 + id (unique product code per virtual display)
        let product_code = 0x5100u16 | (id as u16 & 0xFF);
        edid[10] = (product_code & 0xFF) as u8;
        edid[11] = (product_code >> 8) as u8;

        // 4. Serial number: monotonic display id (little-endian)
        let id_bytes = id.to_le_bytes();
        edid[12..16].copy_from_slice(&id_bytes);

        // 5. Manufacture date: Week 40 of 2026 (2026 - 1990 = 36)
        edid[16] = 40;
        edid[17] = 36;

        // 6. EDID Version 1, Revision 4
        edid[18] = 0x01;
        edid[19] = 0x04;

        // 7. Basic Display Parameters
        edid[20] = 0xA5; // Digital video interface, 8 bits/color, DisplayPort
        edid[21] = 60; // Horizontal screen size: 60 cm
        edid[22] = 34; // Vertical screen size: 34 cm
        edid[23] = 120; // Display gamma: (2.2 * 100) - 100 = 120
        edid[24] = 0x06; // sRGB primary color space + preferred timing in DTD 1

        // 8. Color Characteristics: Standard sRGB primaries (CIE 1931)
        edid[25] = 0xEE;
        edid[26] = 0x91;
        edid[27] = 0xA3;
        edid[28] = 0x54;
        edid[29] = 0x4C;
        edid[30] = 0x99;
        edid[31] = 0x26;
        edid[32] = 0x0F;
        edid[33] = 0x50;
        edid[34] = 0x54;

        // 9. Established timings (640x480@60, 800x600@60, 1024x768@60)
        edid[35] = 0x21;
        edid[36] = 0x08;
        edid[37] = 0x00;

        // 10. Standard Timings (unused entries filled with 0x01, 0x01)
        edid[38..54].fill(0x01);

        // 11. Descriptor Block 1 (Bytes 54-71): Detailed Timing Descriptor (DTD) for native mode
        fill_detailed_timing(&mut edid[54..72], &timing);

        // 12. Descriptor Block 2 (Bytes 72-89): Display Range Limits (Tag 0xFD)
        fill_range_limits(&mut edid[72..90]);

        // 13. Descriptor Block 3 (Bytes 90-107): Monitor Name Descriptor (Tag 0xFC)
        // Keep unique name retaining display ID suffix within 13 ASCII chars
        let display_name = if name.len() <= 13 {
            name.to_string()
        } else {
            format!("Quest-VR-{id}")
        };
        fill_string_descriptor(&mut edid[90..108], 0xFC, &display_name);

        // 14. Descriptor Block 4 (Bytes 108-125): Serial Number Descriptor (Tag 0xFF)
        let serial_str = format!("LQST-VR-{:02}", id);
        fill_string_descriptor(&mut edid[108..126], 0xFF, &serial_str);

        // 15. Extension Flag
        edid[126] = 0x00;

        // 16. Checksum: byte 127 must make sum(0..128) % 256 == 0
        let sum: u32 = edid[0..127].iter().map(|&b| b as u32).sum();
        edid[127] = ((256 - (sum % 256)) % 256) as u8;

        Ok(edid)
    }
}

/// Encodes 3 uppercase ASCII characters into a 16-bit VESA manufacturer ID.
fn encode_mfg_id(id: &str) -> u16 {
    let mut chars = id.chars();
    let c1 = chars.next().unwrap_or('A').to_ascii_uppercase() as u16 - 'A' as u16 + 1;
    let c2 = chars.next().unwrap_or('A').to_ascii_uppercase() as u16 - 'A' as u16 + 1;
    let c3 = chars.next().unwrap_or('A').to_ascii_uppercase() as u16 - 'A' as u16 + 1;
    ((c1 & 0x1F) << 10) | ((c2 & 0x1F) << 5) | (c3 & 0x1F)
}

/// Fills an 18-byte Detailed Timing Descriptor (DTD) block using calculated CVT-RB timings.
fn fill_detailed_timing(buf: &mut [u8], timing: &CvtTiming) {
    let pclk_10khz = ((timing.pixel_clock_hz as f64) / 10_000.0).round() as u16;

    buf[0] = (pclk_10khz & 0xFF) as u8;
    buf[1] = (pclk_10khz >> 8) as u8;

    buf[2] = (timing.h_active & 0xFF) as u8;
    buf[3] = (timing.h_blank & 0xFF) as u8;
    buf[4] = (((timing.h_active >> 8) & 0x0F) << 4 | ((timing.h_blank >> 8) & 0x0F)) as u8;

    buf[5] = (timing.v_active & 0xFF) as u8;
    buf[6] = (timing.v_blank & 0xFF) as u8;
    buf[7] = (((timing.v_active >> 8) & 0x0F) << 4 | ((timing.v_blank >> 8) & 0x0F)) as u8;

    buf[8] = (timing.h_front_porch & 0xFF) as u8;
    buf[9] = (timing.h_sync_pulse & 0xFF) as u8;
    buf[10] = (((timing.v_front_porch & 0x0F) << 4) | (timing.v_sync_pulse & 0x0F)) as u8;
    buf[11] = ((((timing.h_front_porch >> 8) & 0x03) << 6)
        | (((timing.h_sync_pulse >> 8) & 0x03) << 4)
        | (((timing.v_front_porch >> 4) & 0x03) << 2)
        | ((timing.v_sync_pulse >> 4) & 0x03)) as u8;

    let h_size_mm = 600u32;
    let v_size_mm = 340u32;
    buf[12] = (h_size_mm & 0xFF) as u8;
    buf[13] = (v_size_mm & 0xFF) as u8;
    buf[14] = (((h_size_mm >> 8) & 0x0F) << 4 | ((v_size_mm >> 8) & 0x0F)) as u8;

    buf[15] = 0;
    buf[16] = 0;

    let h_pol_bit = if timing.h_sync_pol_positive {
        1 << 1
    } else {
        0
    };
    let v_pol_bit = if timing.v_sync_pol_positive {
        1 << 2
    } else {
        0
    };
    buf[17] = 0x18 | h_pol_bit | v_pol_bit;
}

/// Fills Monitor Range Limits descriptor block (Tag 0xFD).
/// Advertises 30-240 Hz vertical, 15-250 kHz horizontal, 660 MHz maximum pixel clock.
fn fill_range_limits(buf: &mut [u8]) {
    buf[0] = 0x00;
    buf[1] = 0x00;
    buf[2] = 0x00;
    buf[3] = 0xFD; // Monitor range limits tag
    buf[4] = 0x00;

    buf[5] = 30; // Min V (Hz)
    buf[6] = 240; // Max V (Hz)
    buf[7] = 15; // Min H (kHz)
    buf[8] = 250; // Max H (kHz)
    buf[9] = 66; // Max pixel clock (66 * 10 MHz = 660 MHz)
    buf[10] = 0x01; // Range limits only (no secondary timing formula)

    buf[11] = 0x0A; // End-of-string linefeed
    buf[12..18].fill(0x20); // Space padding
}

/// Fills an ASCII string descriptor block (0xFC for Name, 0xFF for Serial).
fn fill_string_descriptor(buf: &mut [u8], tag: u8, text: &str) {
    buf[0] = 0x00;
    buf[1] = 0x00;
    buf[2] = 0x00;
    buf[3] = tag;
    buf[4] = 0x00;

    let ascii_bytes: Vec<u8> = text
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control())
        .take(13)
        .map(|c| c as u8)
        .collect();

    let copy_len = ascii_bytes.len().min(13);
    buf[5..5 + copy_len].copy_from_slice(&ascii_bytes[..copy_len]);

    if copy_len < 13 {
        buf[5 + copy_len] = 0x0A; // Linefeed terminator
        buf[(6 + copy_len)..18].fill(0x20); // Space padding
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edid_generation_1080p() {
        let edid = EdidGenerator::generate(1920, 1080, 60, "Quest-VR-1", 1).unwrap();
        assert_eq!(edid.len(), 128);

        // Header check
        assert_eq!(
            &edid[0..8],
            &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]
        );

        // Version 1, Revision 4
        assert_eq!(edid[18], 1);
        assert_eq!(edid[19], 4);

        // Feature byte
        assert_eq!(edid[24], 0x06);

        // Checksum validation
        let sum: u32 = edid.iter().map(|&b| b as u32).sum();
        assert_eq!(sum % 256, 0, "EDID checksum must sum to 0 modulo 256");

        // Verify with edid-decode --check
        if let Ok(mut child) = std::process::Command::new("edid-decode")
            .arg("--check")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(&edid);
            }
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(0));
            let stdout_str = String::from_utf8_lossy(&output.stdout);
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            assert!(
                !stdout_str.contains("Warnings:") && !stderr_str.contains("Warnings:"),
                "edid-decode produced warnings:\n{}\n{}",
                stdout_str,
                stderr_str
            );
        }
    }

    #[test]
    fn test_edid_generation_presets() {
        for (w, h, hz, id) in [
            (2560, 1440, 60, 1),
            (2560, 1440, 90, 2),
            (2560, 1440, 120, 3),
            (3840, 2160, 60, 1),
            (3440, 1440, 60, 2),
            (3440, 1440, 90, 3),
        ] {
            let edid = EdidGenerator::generate(w, h, hz, &format!("Quest-VR-{id}"), id).unwrap();
            assert_eq!(edid.len(), 128);
            let sum: u32 = edid.iter().map(|&b| b as u32).sum();
            assert_eq!(sum % 256, 0);

            // Ensure serial number in bytes 12..16 matches id
            let serial_bytes = &edid[12..16];
            assert_eq!(u32::from_le_bytes(serial_bytes.try_into().unwrap()), id);
        }
    }

    #[test]
    fn test_edid_exceeding_limits_rejected() {
        // Zero dimensions rejected
        assert!(EdidGenerator::generate(0, 1080, 60, "Invalid", 1).is_err());

        // Dimensions > 4095 rejected
        assert!(EdidGenerator::generate(7680, 4320, 60, "8K", 1).is_err());

        // Pixel clock > 655.35 MHz rejected
        assert!(EdidGenerator::generate(3840, 2160, 90, "4K90", 1).is_err());
        assert!(EdidGenerator::generate(3840, 2160, 120, "4K120", 1).is_err());

        // 1080p@240 has horizontal rate 291.35 kHz > 250 kHz -> rejected
        assert!(EdidGenerator::generate(1920, 1080, 240, "OverH", 1).is_err());

        // 2560x1440@144 produces Vfront = 89 lines which exceeds 6-bit DTD limit (63) -> rejected cleanly
        let res = EdidGenerator::generate(2560, 1440, 144, "1440p144", 1);
        assert!(
            res.is_err(),
            "2560x1440@144 must be rejected due to DTD Vfront > 63"
        );
        let err_str = res.unwrap_err().to_string();
        assert!(err_str.contains("vertical front porch of 89 lines which exceeds"));
    }
}
