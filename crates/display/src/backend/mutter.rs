use async_trait::async_trait;
use std::collections::HashMap;
use tracing::info;

use super::{VirtualDisplayBackend, VirtualMonitor};
use crate::config::VirtualMonitorConfig;
use crate::error::DisplayError;
use crate::layout::DisplayLayout;

/// GNOME Mutter virtual monitor backend via D-Bus (`org.gnome.Mutter.ScreenCast` & `org.gnome.Mutter.DisplayConfig`).
pub struct MutterDisplayBackend {
    connection: Option<zbus::Connection>,
    monitors: HashMap<u32, VirtualMonitor>,
    session_paths: HashMap<u32, (String, String)>, // id -> (session_path, stream_path)
    is_available: bool,
}

impl MutterDisplayBackend {
    pub fn new() -> Self {
        let is_avail = Self::check_quick_availability();
        Self {
            connection: None,
            monitors: HashMap::new(),
            session_paths: HashMap::new(),
            is_available: is_avail,
        }
    }

    fn check_quick_availability() -> bool {
        if let Ok(desktop) = std::env::var("XDG_CURRENT_DESKTOP") {
            if desktop.to_lowercase().contains("gnome") {
                return true;
            }
        }
        if let Ok(session) = std::env::var("DESKTOP_SESSION") {
            if session.to_lowercase().contains("gnome") {
                return true;
            }
        }
        false
    }

    /// Verifies that GNOME Mutter ScreenCast is actively registered and owned on the session bus.
    pub async fn probe_screencast_service(conn: &zbus::Connection) -> bool {
        let dbus_proxy = match zbus::Proxy::new(
            conn,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await
        {
            Ok(p) => p,
            Err(_) => return false,
        };

        let has_owner: Result<String, _> = dbus_proxy
            .call("GetNameOwner", &("org.gnome.Mutter.ScreenCast",))
            .await;

        has_owner.is_ok()
    }
}

impl Default for MutterDisplayBackend {
    fn default() -> Self {
        Self::new()
    }
}

// Mutter D-Bus type signatures:
// GetCurrentState: (ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv})
// ApplyMonitorsConfig: (uua(iiduba(ssa{sv}))a{sv})
type MutterMonitorSpec = (String, String, String, String);
type MutterMode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, zbus::zvariant::OwnedValue>,
);
type MutterMonitor = (
    MutterMonitorSpec,
    Vec<MutterMode>,
    HashMap<String, zbus::zvariant::OwnedValue>,
);
type MutterLogicalMonitorState = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, String, String)>,
    HashMap<String, zbus::zvariant::OwnedValue>,
);
type MutterCurrentState = (
    u32,
    Vec<MutterMonitor>,
    Vec<MutterLogicalMonitorState>,
    HashMap<String, zbus::zvariant::OwnedValue>,
);

type MutterLogicalMonitorApply = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(
        String,
        String,
        HashMap<String, zbus::zvariant::Value<'static>>,
    )>,
);

#[async_trait]
impl VirtualDisplayBackend for MutterDisplayBackend {
    fn name(&self) -> &'static str {
        "GNOME Mutter (D-Bus)"
    }

    fn is_available(&self) -> bool {
        self.is_available
    }

    async fn init(&mut self) -> Result<(), DisplayError> {
        let conn = zbus::Connection::session()
            .await
            .map_err(|e| DisplayError::Dbus(format!("Failed to connect to session bus: {e}")))?;

        if !Self::probe_screencast_service(&conn).await {
            return Err(DisplayError::BackendUnavailable(
                "GNOME Mutter ScreenCast service is not owned on D-Bus session bus".into(),
            ));
        }

        self.connection = Some(conn);
        self.is_available = true;
        Ok(())
    }

    async fn create_monitor(
        &mut self,
        config: &VirtualMonitorConfig,
    ) -> Result<VirtualMonitor, DisplayError> {
        config.validate()?;

        let conn = self.connection.as_ref().ok_or_else(|| {
            DisplayError::BackendUnavailable(
                "Mutter backend D-Bus connection not initialized".into(),
            )
        })?;

        // Query existing connectors prior to creation to detect the newly added connector
        let display_config_proxy = zbus::Proxy::new(
            conn,
            "org.gnome.Mutter.DisplayConfig",
            "/org/gnome/Mutter/DisplayConfig",
            "org.gnome.Mutter.DisplayConfig",
        )
        .await
        .ok();

        let connectors_before: std::collections::HashSet<String> =
            if let Some(ref dcp) = display_config_proxy {
                let res: Result<MutterCurrentState, _> = dcp.call("GetCurrentState", &()).await;
                if let Ok(state) = res {
                    state.1.into_iter().map(|(spec, _, _)| spec.0).collect()
                } else {
                    std::collections::HashSet::new()
                }
            } else {
                std::collections::HashSet::new()
            };

        // 1. Connect to org.gnome.Mutter.ScreenCast
        let screencast_proxy = zbus::Proxy::new(
            conn,
            "org.gnome.Mutter.ScreenCast",
            "/org/gnome/Mutter/ScreenCast",
            "org.gnome.Mutter.ScreenCast",
        )
        .await
        .map_err(|e| DisplayError::Dbus(format!("Failed to bind ScreenCast interface: {e}")))?;

        // 2. Call CreateSession
        let empty_props: HashMap<String, zbus::zvariant::Value> = HashMap::new();
        let session_path: zbus::zvariant::OwnedObjectPath = screencast_proxy
            .call("CreateSession", &(empty_props,))
            .await
            .map_err(|e| DisplayError::Dbus(format!("ScreenCast.CreateSession failed: {e}")))?;

        let session_path_str = session_path.as_str().to_string();

        // 3. Connect to the new session
        let session_proxy = zbus::Proxy::new(
            conn,
            "org.gnome.Mutter.ScreenCast",
            session_path_str.as_str(),
            "org.gnome.Mutter.ScreenCast.Session",
        )
        .await
        .map_err(|e| DisplayError::Dbus(format!("Failed to bind Session proxy: {e}")))?;

        // 4. Call RecordVirtual with is-platform: true, modes dict (size, refresh-rate, is-preferred)
        let mut record_props = HashMap::new();
        record_props.insert("cursor-mode".to_string(), zbus::zvariant::Value::from(1u32));
        record_props.insert("is-platform".to_string(), zbus::zvariant::Value::from(true));
        record_props.insert(
            "width".to_string(),
            zbus::zvariant::Value::from(config.width as i32),
        );
        record_props.insert(
            "height".to_string(),
            zbus::zvariant::Value::from(config.height as i32),
        );
        record_props.insert(
            "refresh-rate".to_string(),
            zbus::zvariant::Value::from(config.refresh_rate as f64),
        );

        let mut mode_props: HashMap<String, zbus::zvariant::Value> = HashMap::new();
        mode_props.insert(
            "size".to_string(),
            zbus::zvariant::Value::from((config.width, config.height)),
        );
        mode_props.insert(
            "refresh-rate".to_string(),
            zbus::zvariant::Value::from(config.refresh_rate as f64),
        );
        mode_props.insert(
            "is-preferred".to_string(),
            zbus::zvariant::Value::from(true),
        );
        record_props.insert(
            "modes".to_string(),
            zbus::zvariant::Value::from(vec![mode_props]),
        );

        let stream_path: zbus::zvariant::OwnedObjectPath = session_proxy
            .call("RecordVirtual", &(record_props,))
            .await
            .map_err(|e| DisplayError::Dbus(format!("ScreenCast.RecordVirtual failed: {e}")))?;

        let stream_path_str = stream_path.as_str().to_string();

        // 5. Start the screencast session so the virtual monitor output remains active
        let _: () = session_proxy
            .call("Start", &())
            .await
            .map_err(|e| DisplayError::Dbus(format!("ScreenCast.Session.Start failed: {e}")))?;

        let stream_id = stream_path_str.rsplit('/').next().unwrap_or("0");
        let mut connector = format!("Mutter-Virtual-{stream_id}");

        // Detect newly registered connector name from GetCurrentState
        if let Some(ref dcp) = display_config_proxy {
            let res: Result<MutterCurrentState, _> = dcp.call("GetCurrentState", &()).await;
            if let Ok(state_after) = res {
                if let Some((spec, _, _)) = state_after
                    .1
                    .into_iter()
                    .find(|(spec, _, _)| !connectors_before.contains(&spec.0))
                {
                    connector = spec.0;
                }
            }
        }

        // Install EDID binary payload to runtime directory
        let edid_path = if let Some(edid_bytes) = &config.edid {
            let res: Result<String, DisplayError> = (|| {
                let edid_dir = crate::ensure_display_runtime_dir()?;
                let edid_file = edid_dir.join(format!("edid-{connector}.bin"));
                std::fs::write(&edid_file, edid_bytes).map_err(|e| {
                    DisplayError::CommandFailed(format!(
                        "Failed to write runtime EDID file {edid_file:?}: {e}"
                    ))
                })?;
                Ok(edid_file.to_string_lossy().to_string())
            })();

            match res {
                Ok(p) => Some(p),
                Err(e) => {
                    // Roll back session on EDID write failure
                    let _: Result<(), _> = session_proxy.call("Stop", &()).await;
                    return Err(e);
                }
            }
        } else {
            None
        };

        info!(
            id = config.id,
            connector = %connector,
            session = %session_path_str,
            stream = %stream_path_str,
            edid = ?edid_path,
            "Spawned persistent Mutter platform virtual display"
        );

        let monitor = VirtualMonitor {
            id: config.id,
            name: config.name.clone(),
            connector,
            width: config.width,
            height: config.height,
            refresh_rate: config.refresh_rate,
            x: 0,
            y: 0,
            dpi: config.dpi,
            is_virtual: true,
            edid_path,
        };

        self.session_paths
            .insert(config.id, (session_path_str, stream_path_str));
        self.monitors.insert(config.id, monitor.clone());

        Ok(monitor)
    }

    async fn destroy_monitor(&mut self, id: u32) -> Result<(), DisplayError> {
        let (session_path_str, _) = self
            .session_paths
            .get(&id)
            .ok_or_else(|| DisplayError::OutputNotFound(format!("Monitor {id} not found")))?
            .clone();

        if let Some(conn) = self.connection.as_ref() {
            let session_proxy = zbus::Proxy::new(
                conn,
                "org.gnome.Mutter.ScreenCast",
                session_path_str.as_str(),
                "org.gnome.Mutter.ScreenCast.Session",
            )
            .await
            .map_err(|e| DisplayError::Dbus(format!("Failed to bind Session proxy: {e}")))?;

            let _: () = session_proxy
                .call("Stop", &())
                .await
                .map_err(|e| DisplayError::Dbus(format!("ScreenCast.Session.Stop failed: {e}")))?;
        }

        // Only remove state after Stop succeeds
        self.session_paths.remove(&id);
        if let Some(m) = self.monitors.remove(&id) {
            if let Some(path) = m.edid_path {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }

    async fn apply_layout(&mut self, layout: &DisplayLayout) -> Result<(), DisplayError> {
        let placements = layout.compute_placements();
        for p in &placements {
            if let Some(m) = self.monitors.get_mut(&p.id) {
                m.x = p.x;
                m.y = p.y;
                m.width = p.width;
                m.height = p.height;
                m.refresh_rate = p.refresh_rate;
                m.dpi = p.dpi;
            }
        }

        // Apply monitor geometry via Mutter DisplayConfig
        if let Some(conn) = self.connection.as_ref() {
            let display_config_proxy = zbus::Proxy::new(
                conn,
                "org.gnome.Mutter.DisplayConfig",
                "/org/gnome/Mutter/DisplayConfig",
                "org.gnome.Mutter.DisplayConfig",
            )
            .await
            .map_err(|e| DisplayError::Dbus(format!("Failed to bind DisplayConfig proxy: {e}")))?;

            let state: MutterCurrentState = display_config_proxy
                .call("GetCurrentState", &())
                .await
                .map_err(|e| DisplayError::Dbus(format!("GetCurrentState failed: {e}")))?;

            let serial = state.0;
            let mutter_monitors = state.1;
            let current_logical_monitors = state.2;

            let virtual_connectors: std::collections::HashSet<String> = self
                .monitors
                .values()
                .map(|m| m.connector.clone())
                .collect();

            let mut logical_monitors: Vec<MutterLogicalMonitorApply> = Vec::new();
            let mut physical_has_primary = false;

            // Preserve existing physical displays so laptop / external screens are not disabled
            for (lm_x, lm_y, lm_scale, lm_transform, lm_primary, lm_monitors, _) in
                current_logical_monitors
            {
                let physical_monitors: Vec<(String, String, String, String)> = lm_monitors
                    .into_iter()
                    .filter(|(conn_name, _, _, _)| !virtual_connectors.contains(conn_name))
                    .collect();

                if !physical_monitors.is_empty() {
                    let mut converted_monitors = Vec::new();
                    for (conn_name, _, _, _) in physical_monitors {
                        let mut chosen_mode = String::new();
                        if let Some((_, modes, _)) = mutter_monitors
                            .iter()
                            .find(|(spec, _, _)| spec.0 == conn_name)
                        {
                            // 1. Look for mode with "is-current" == true
                            for (mode_id, _, _, _, _, _, props) in modes {
                                if let Some(val) = props.get("is-current") {
                                    if let zbus::zvariant::Value::Bool(true) = &**val {
                                        chosen_mode = mode_id.clone();
                                        break;
                                    }
                                }
                            }
                            // 2. Look for mode with "is-preferred" == true
                            if chosen_mode.is_empty() {
                                for (mode_id, _, _, _, _, _, props) in modes {
                                    if let Some(val) = props.get("is-preferred") {
                                        if let zbus::zvariant::Value::Bool(true) = &**val {
                                            chosen_mode = mode_id.clone();
                                            break;
                                        }
                                    }
                                }
                            }
                            // 3. Fallback to first available mode
                            if chosen_mode.is_empty() {
                                if let Some(first) = modes.first() {
                                    chosen_mode = first.0.clone();
                                }
                            }
                        }

                        if !chosen_mode.is_empty() {
                            converted_monitors.push((
                                conn_name,
                                chosen_mode,
                                HashMap::<String, zbus::zvariant::Value>::new(),
                            ));
                        }
                    }

                    if !converted_monitors.is_empty() {
                        if lm_primary {
                            physical_has_primary = true;
                        }
                        logical_monitors.push((
                            lm_x,
                            lm_y,
                            lm_scale,
                            lm_transform,
                            lm_primary,
                            converted_monitors,
                        ));
                    }
                }
            }

            // Method 1 = TEMPORARY (verify and apply without persisting to config)
            let method = 1u32;

            // Ensure at least one monitor is marked primary
            let primary_id = placements.iter().map(|p| p.id).min().unwrap_or(1);

            for p in &placements {
                if let Some(m) = self.monitors.get(&p.id) {
                    let mut chosen_mode_id = format!("{}x{}@{}", p.width, p.height, p.refresh_rate);
                    let mut chosen_conn = m.connector.clone();

                    for (spec, modes, _) in &mutter_monitors {
                        let (conn_name, _, _, _) = spec;
                        if conn_name == &m.connector || conn_name.contains("Virtual") {
                            chosen_conn = conn_name.clone();
                            // Match by width, height, and refresh rate
                            if let Some(matched_mode) = modes.iter().find(|mode| {
                                mode.1 == p.width as i32
                                    && mode.2 == p.height as i32
                                    && (mode.3 - p.refresh_rate as f64).abs() < 1.0
                            }) {
                                chosen_mode_id = matched_mode.0.clone();
                                break;
                            } else if let Some(matched_mode) = modes
                                .iter()
                                .find(|mode| mode.1 == p.width as i32 && mode.2 == p.height as i32)
                            {
                                chosen_mode_id = matched_mode.0.clone();
                                break;
                            } else if let Some(first_mode) = modes.first() {
                                chosen_mode_id = first_mode.0.clone();
                                break;
                            }
                        }
                    }

                    let mon_props: HashMap<String, zbus::zvariant::Value> = HashMap::new();
                    let monitor_spec = vec![(chosen_conn, chosen_mode_id, mon_props)];
                    let scale = if p.dpi > 144 { 2.0f64 } else { 1.0f64 };
                    let is_primary = !physical_has_primary && (p.id == primary_id);
                    logical_monitors.push((p.x, p.y, scale, 0u32, is_primary, monitor_spec));
                }
            }

            // If neither physical nor virtual was marked primary, ensure at least first monitor is primary
            if !logical_monitors.is_empty() && !logical_monitors.iter().any(|lm| lm.4) {
                logical_monitors[0].4 = true;
            }

            let properties: HashMap<String, zbus::zvariant::Value> = HashMap::new();
            let _: () = display_config_proxy
                .call(
                    "ApplyMonitorsConfig",
                    &(serial, method, logical_monitors, properties),
                )
                .await
                .map_err(|e| DisplayError::Dbus(format!("ApplyMonitorsConfig failed: {e}")))?;
        }

        Ok(())
    }

    fn list_monitors(&self) -> Vec<VirtualMonitor> {
        let mut list: Vec<_> = self.monitors.values().cloned().collect();
        list.sort_by_key(|m| m.id);
        list
    }
}
