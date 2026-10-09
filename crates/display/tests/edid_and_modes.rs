use linux_quest_display::backend::x11::X11DisplayBackend;
use linux_quest_display::edid::EdidGenerator;

#[test]
fn test_edid_fuzz_standard_resolutions() {
    let valid_resolutions = [
        (1280, 720, 60, 1),
        (1920, 1080, 60, 1),
        (1920, 1080, 72, 2),
        (1920, 1080, 90, 3),
        (1920, 1080, 120, 1),
        (2560, 1440, 60, 2),
        (2560, 1440, 90, 3),
        (2560, 1440, 120, 1),
        (3440, 1440, 60, 2),
        (3440, 1440, 90, 3),
        (3840, 2160, 60, 1),
    ];

    for (w, h, hz, id) in valid_resolutions {
        let name = format!("VR-{w}x{h}");
        let edid = EdidGenerator::generate(w, h, hz, &name, id).expect("EDID generation failed");

        assert_eq!(edid.len(), 128, "EDID must be exactly 128 bytes");
        assert_eq!(
            &edid[0..8],
            &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00],
            "Invalid EDID magic header"
        );

        // Version 1.4
        assert_eq!(edid[18], 1);
        assert_eq!(edid[19], 4);

        // Feature byte 24: sRGB and preferred timing
        assert_eq!(edid[24], 0x06);

        // Verify serial number matches id
        let serial_val = u32::from_le_bytes(edid[12..16].try_into().unwrap());
        assert_eq!(serial_val, id);

        // Decode DTD width and height from block 1 (bytes 54..72)
        let dtd_h_active = (edid[56] as u32) | (((edid[58] as u32 >> 4) & 0x0F) << 8);
        let dtd_v_active = (edid[59] as u32) | (((edid[61] as u32 >> 4) & 0x0F) << 8);
        assert_eq!(
            dtd_h_active, w,
            "Decoded DTD horizontal active must match target width"
        );
        assert_eq!(
            dtd_v_active, h,
            "Decoded DTD vertical active must match target height"
        );

        let sum: u32 = edid.iter().map(|&b| b as u32).sum();
        assert_eq!(
            sum % 256,
            0,
            "EDID checksum failed for {}x{}@{}Hz (sum % 256 != 0)",
            w,
            h,
            hz
        );

        // Validate with system edid-decode validator --check asserting 0 warnings
        use std::io::Write;
        let mut child = std::process::Command::new("edid-decode")
            .arg("--check")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect(
                "edid-decode binary must be installed and executable for compliance validation",
            );

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&edid);
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "edid-decode rejected {}x{}@{}Hz: {}",
            w,
            h,
            hz,
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let stderr_str = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stdout_str.contains("Warnings:") && !stderr_str.contains("Warnings:"),
            "edid-decode --check produced warnings for {}x{}@{}Hz:\n{}\n{}",
            w,
            h,
            hz,
            stdout_str,
            stderr_str
        );
    }

    // Modes exceeding base EDID 1.4 limits or range descriptor limits must fail cleanly
    let excessive_modes = [
        (3840, 2160, 90, 1),
        (3840, 2160, 120, 1),
        (3440, 1440, 120, 1),
        (7680, 4320, 60, 1),
        (1920, 1080, 240, 1), // 291.35 kHz > 250 kHz horizontal limit
        (2560, 1440, 144, 1), // Vfront = 89 > 63 lines 6-bit DTD limit
    ];
    for (w, h, hz, id) in excessive_modes {
        assert!(
            EdidGenerator::generate(w, h, hz, "Excessive", id).is_err(),
            "Mode {}x{}@{}Hz should be rejected as exceeding base EDID 1.4 limits",
            w,
            h,
            hz
        );
    }
}

#[test]
fn test_x11_cvt_modeline_generator() {
    // 1920x1080@90 (VESA CVT 1.2 RB2: 202.86 MHz, V 1080 1113 1121 1127)
    let (name, params) = X11DisplayBackend::generate_cvt_modeline(1920, 1080, 90).unwrap();
    assert_eq!(name, "1920x1080_90.00");
    assert!(params.contains("202.86"));
    assert!(params.contains("1920"));
    assert!(params.contains("1080 1113 1121 1127"));
    assert!(params.contains("+hsync -vsync"));

    // 2560x1440@120 (VESA CVT-RB v1: 497.25 MHz, V 1440 1443 1448 1525)
    let (name_1440p, params_1440p) =
        X11DisplayBackend::generate_cvt_modeline(2560, 1440, 120).unwrap();
    assert_eq!(name_1440p, "2560x1440_120.00");
    assert!(params_1440p.contains("497.25"));
    assert!(params_1440p.contains("2560"));
    assert!(params_1440p.contains("1440 1443 1448 1525"));
    assert!(params_1440p.contains("+hsync -vsync"));

    // 3440x1440@90 (VESA CVT 1.2 RB2: 476.15 MHz, V 1440 1489 1497 1503)
    let (name_uw90, params_uw90) =
        X11DisplayBackend::generate_cvt_modeline(3440, 1440, 90).unwrap();
    assert_eq!(name_uw90, "3440x1440_90.00");
    assert!(params_uw90.contains("476.15"));
    assert!(params_uw90.contains("1440 1489 1497 1503"));

    // 3440x1440@60 (VESA CVT-RB v1 21:9 aspect ratio: 319.75 MHz, V 1440 1443 1453 1481)
    let (name_uw60, params_uw60) =
        X11DisplayBackend::generate_cvt_modeline(3440, 1440, 60).unwrap();
    assert_eq!(name_uw60, "3440x1440_60.00");
    assert!(params_uw60.contains("319.75"));
    assert!(params_uw60.contains("1440 1443 1453 1481"));
}
