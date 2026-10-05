use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdbDeviceState {
    Device,
    Unauthorized,
    Offline,
    Unknown(String),
}

impl From<&str> for AdbDeviceState {
    fn from(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "device" => Self::Device,
            "unauthorized" => Self::Unauthorized,
            "offline" => Self::Offline,
            other => Self::Unknown(other.to_string()),
        }
    }
}

/// Represents an Android device connected via ADB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdbDevice {
    pub serial: String,
    pub state: AdbDeviceState,
    pub product: Option<String>,
    pub model: Option<String>,
    pub device_name: Option<String>,
    pub transport_id: Option<String>,
}

impl AdbDevice {
    /// Checks whether the device matches a Meta Quest headset.
    /// Matches known Quest codenames ("eureka", "hollywood", "seacliff", "monterey", "pacific")
    /// and tokenized model components ("quest", "oculus", "quest2", "quest3", "questpro").
    /// Does not match substrings embedded in words like "request", "conquest", or "questionnaire".
    pub fn is_quest(&self) -> bool {
        let quest_codenames = ["eureka", "hollywood", "seacliff", "monterey", "pacific"];

        let search_fields = [
            self.model.as_deref().unwrap_or(""),
            self.product.as_deref().unwrap_or(""),
            self.device_name.as_deref().unwrap_or(""),
        ];

        for field in &search_fields {
            let lower = field.to_lowercase();
            if lower.is_empty() {
                continue;
            }
            for codename in &quest_codenames {
                if lower == *codename
                    || lower.starts_with(&format!("{codename}_"))
                    || lower.starts_with(&format!("{codename}-"))
                {
                    return true;
                }
            }
            for token in lower.split(|c: char| !c.is_alphanumeric()) {
                if let Some(suffix) = token.strip_prefix("quest") {
                    if suffix.is_empty()
                        || suffix == "2"
                        || suffix == "3"
                        || suffix == "3s"
                        || suffix == "pro"
                        || suffix.chars().all(|c| c.is_ascii_digit())
                    {
                        return true;
                    }
                } else if let Some(suffix) = token.strip_prefix("oculus") {
                    if suffix.is_empty() || suffix.chars().all(|c| c.is_ascii_digit()) {
                        return true;
                    }
                }
            }
        }

        false
    }
}

/// Reverse port forwarding rule reported by `adb reverse --list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdbReverseRule {
    pub remote_port: u16,
    pub local_port: u16,
}

/// Parses the output of `adb devices -l`.
pub fn parse_devices_output(output: &str) -> Vec<AdbDevice> {
    let mut devices = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }

        let serial = parts[0].to_string();
        let state = AdbDeviceState::from(parts[1]);

        let mut product = None;
        let mut model = None;
        let mut device_name = None;
        let mut transport_id = None;

        for part in &parts[2..] {
            if let Some((k, v)) = part.split_once(':') {
                match k {
                    "product" => product = Some(v.to_string()),
                    "model" => model = Some(v.to_string()),
                    "device" => device_name = Some(v.to_string()),
                    "transport_id" => transport_id = Some(v.to_string()),
                    _ => (),
                }
            }
        }

        devices.push(AdbDevice {
            serial,
            state,
            product,
            model,
            device_name,
            transport_id,
        });
    }

    devices
}

/// Parses the output of `adb reverse --list`.
pub fn parse_reverse_list_output(output: &str) -> Vec<AdbReverseRule> {
    let mut rules = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        // Typically: "(usb:1-1) tcp:8088 tcp:8088" or "tcp:8088 tcp:8088"
        let tcp_parts: Vec<&str> = parts
            .into_iter()
            .filter(|p| p.starts_with("tcp:"))
            .collect();
        if tcp_parts.len() >= 2 {
            let remote_str = tcp_parts[0].trim_start_matches("tcp:");
            let local_str = tcp_parts[1].trim_start_matches("tcp:");

            if let (Ok(remote_port), Ok(local_port)) =
                (remote_str.parse::<u16>(), local_str.parse::<u16>())
            {
                rules.push(AdbReverseRule {
                    remote_port,
                    local_port,
                });
            }
        }
    }

    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_adb_devices_output() {
        let sample_output = r#"
List of devices attached
1WMHH812345678         device usb:1-1 product:eureka model:Quest_3 device:eureka transport_id:1
FA1234567890           unauthorized usb:1-2 transport_id:2
192.168.1.42:5555      device product:hollywood model:Quest_2 device:hollywood transport_id:3
"#;

        let devices = parse_devices_output(sample_output);
        assert_eq!(devices.len(), 3);

        assert_eq!(devices[0].serial, "1WMHH812345678");
        assert_eq!(devices[0].state, AdbDeviceState::Device);
        assert_eq!(devices[0].model.as_deref(), Some("Quest_3"));
        assert!(devices[0].is_quest());

        assert_eq!(devices[1].serial, "FA1234567890");
        assert_eq!(devices[1].state, AdbDeviceState::Unauthorized);
        assert!(!devices[1].is_quest());

        assert_eq!(devices[2].serial, "192.168.1.42:5555");
        assert_eq!(devices[2].state, AdbDeviceState::Device);
        assert_eq!(devices[2].model.as_deref(), Some("Quest_2"));
        assert!(devices[2].is_quest());
    }

    #[test]
    fn test_parse_reverse_list_output() {
        let sample = r#"
(usb:1-1) tcp:8088 tcp:8088
(usb:1-1) tcp:9000 tcp:9000
"#;
        let rules = parse_reverse_list_output(sample);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].remote_port, 8088);
        assert_eq!(rules[0].local_port, 8088);
        assert_eq!(rules[1].remote_port, 9000);
        assert_eq!(rules[1].local_port, 9000);
    }

    #[test]
    fn test_is_quest_does_not_match_bare_meta() {
        let unrelated = AdbDevice {
            serial: "RANDOM123".to_string(),
            state: AdbDeviceState::Device,
            product: Some("metadata_box".to_string()),
            model: Some("meta_smart_device".to_string()),
            device_name: Some("meta_watch".to_string()),
            transport_id: Some("10".to_string()),
        };
        assert!(!unrelated.is_quest());

        let quest = AdbDevice {
            serial: "QUEST123".to_string(),
            state: AdbDeviceState::Device,
            product: Some("eureka".to_string()),
            model: Some("Quest_3".to_string()),
            device_name: Some("eureka".to_string()),
            transport_id: Some("11".to_string()),
        };
        assert!(quest.is_quest());
    }

    #[test]
    fn test_is_quest_does_not_match_embedded_quest_words() {
        let test_cases = [
            "request_device",
            "conquest_phone",
            "questionnaire",
            "unrelated_question",
        ];
        for model in &test_cases {
            let dev = AdbDevice {
                serial: "DEV123".to_string(),
                state: AdbDeviceState::Device,
                product: Some(model.to_string()),
                model: Some(model.to_string()),
                device_name: Some(model.to_string()),
                transport_id: Some("1".to_string()),
            };
            assert!(!dev.is_quest(), "Expected {} to NOT match quest", model);
        }

        let valid_cases = [
            "Quest_3",
            "Quest 2",
            "QuestPro",
            "Quest-3S",
            "Meta Quest 3",
            "Oculus Quest",
            "eureka",
            "hollywood",
            "seacliff",
        ];
        for model in &valid_cases {
            let dev = AdbDevice {
                serial: "DEV123".to_string(),
                state: AdbDeviceState::Device,
                product: Some(model.to_string()),
                model: Some(model.to_string()),
                device_name: None,
                transport_id: Some("1".to_string()),
            };
            assert!(dev.is_quest(), "Expected {} to match quest", model);
        }
    }
}
