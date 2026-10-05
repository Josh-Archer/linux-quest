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
    /// Matches models/products: Quest, Quest 2, Quest 3, Quest Pro, eureka, hollywood, seacliff, etc.
    pub fn is_quest(&self) -> bool {
        let quest_signatures = [
            "quest",
            "eureka",
            "hollywood",
            "seacliff",
            "monterey",
            "pacific",
            "oculus",
            "meta",
        ];

        let search_fields = [
            self.model.as_deref().unwrap_or(""),
            self.product.as_deref().unwrap_or(""),
            self.device_name.as_deref().unwrap_or(""),
        ];

        for field in &search_fields {
            let lower = field.to_lowercase();
            for sig in &quest_signatures {
                if lower.contains(sig) {
                    return true;
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
}
