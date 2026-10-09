use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use tokio::process::Command;
use tracing::info;

use super::{VirtualDisplayBackend, VirtualMonitor};
use crate::config::VirtualMonitorConfig;
use crate::error::DisplayError;
use crate::layout::DisplayLayout;
use crate::timing::CvtTiming;

/// wlroots virtual output backend supporting Sway IPC (`swaymsg`) and Hyprland IPC (`hyprctl`).
pub struct WlrDisplayBackend {
    monitors: HashMap<u32, VirtualMonitor>,
    compositor_type: WlrCompositorType,
    is_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WlrCompositorType {
    Sway,
    Hyprland,
}

impl WlrDisplayBackend {
    pub fn new() -> Self {
        let (is_avail, comp) = Self::detect_environment();
        Self {
            monitors: HashMap::new(),
            compositor_type: comp,
            is_available: is_avail,
        }
    }

    fn detect_environment() -> (bool, WlrCompositorType) {
        if std::env::var("SWAYSOCK").is_ok() {
            return (true, WlrCompositorType::Sway);
        }
        if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
            return (true, WlrCompositorType::Hyprland);
        }
        (false, WlrCompositorType::Sway)
    }

    async fn execute_sway(&self, args: &[&str]) -> Result<String, DisplayError> {
        let output = Command::new("swaymsg")
            .args(args)
            .output()
            .await
            .map_err(|e| DisplayError::CommandFailed(format!("Failed to execute swaymsg: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DisplayError::Wayland(format!("swaymsg failure: {stderr}")));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn execute_hyprctl(&self, args: &[&str]) -> Result<String, DisplayError> {
        let output = Command::new("hyprctl")
            .args(args)
            .output()
            .await
            .map_err(|e| DisplayError::CommandFailed(format!("Failed to execute hyprctl: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DisplayError::Wayland(format!("hyprctl failure: {stderr}")));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Queries Sway active outputs via `swaymsg -t get_outputs -r`.
    async fn get_sway_output_names(&self) -> Result<HashSet<String>, DisplayError> {
        let json_str = self.execute_sway(&["-t", "get_outputs", "-r"]).await?;
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json_str).map_err(|e| {
            DisplayError::Wayland(format!("Failed to parse sway outputs JSON: {e}"))
        })?;

        let mut names = HashSet::new();
        for item in parsed {
            if let Some(n) = item.get("name").and_then(|v| v.as_str()) {
                names.insert(n.to_string());
            }
        }
        Ok(names)
    }

    /// Queries Hyprland active outputs via `hyprctl -j monitors`.
    async fn get_hyprland_output_names(&self) -> Result<HashSet<String>, DisplayError> {
        let json_str = self.execute_hyprctl(&["-j", "monitors"]).await?;
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json_str).map_err(|e| {
            DisplayError::Wayland(format!("Failed to parse hyprctl monitors JSON: {e}"))
        })?;

        let mut names = HashSet::new();
        for item in parsed {
            if let Some(n) = item.get("name").and_then(|v| v.as_str()) {
                names.insert(n.to_string());
            }
        }
        Ok(names)
    }
}

impl Default for WlrDisplayBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VirtualDisplayBackend for WlrDisplayBackend {
    fn name(&self) -> &'static str {
        match self.compositor_type {
            WlrCompositorType::Sway => "wlroots (Sway IPC)",
            WlrCompositorType::Hyprland => "wlroots (Hyprland IPC)",
        }
    }

    fn is_available(&self) -> bool {
        self.is_available
    }

    async fn init(&mut self) -> Result<(), DisplayError> {
        if !self.is_available {
            return Err(DisplayError::BackendUnavailable(
                "No supported wlroots compositor (Sway or Hyprland) found".into(),
            ));
        }
        Ok(())
    }

    async fn create_monitor(
        &mut self,
        config: &VirtualMonitorConfig,
    ) -> Result<VirtualMonitor, DisplayError> {
        config.validate()?;

        let connector = match self.compositor_type {
            WlrCompositorType::Sway => {
                // Strict query before output creation
                let existing = self.get_sway_output_names().await?;

                // Issue `create_output` command
                let _ = self.execute_sway(&["create_output"]).await?;

                // Strict query after output creation
                let after = self.get_sway_output_names().await?;
                let newly_added: Vec<String> = after.difference(&existing).cloned().collect();

                // Exactly one new output must be discovered; never guess or overwrite existing outputs
                if newly_added.len() != 1 {
                    return Err(DisplayError::Wayland(format!(
                        "Expected exactly 1 new Sway output after create_output, found {}: {:?}",
                        newly_added.len(),
                        newly_added
                    )));
                }

                let conn = newly_added.into_iter().next().unwrap();

                // Configure resolution and refresh rate with Sway modeline / custom mode
                let timing =
                    CvtTiming::calculate(config.width, config.height, config.refresh_rate)?;
                let modeline_params = timing.x11_modeline_params();

                // 1. Send explicit CVT modeline FIRST so exact calculated porches are applied; fallback to custom mode
                let mut modeline_cmd = vec!["output", &conn, "modeline"];
                for part in modeline_params.split_whitespace() {
                    modeline_cmd.push(part);
                }
                if let Err(e) = self.execute_sway(&modeline_cmd).await {
                    let mode_str = format!(
                        "{}x{}@{}Hz",
                        config.width, config.height, config.refresh_rate
                    );
                    if let Err(fallback_err) = self
                        .execute_sway(&["output", &conn, "mode", "--custom", &mode_str])
                        .await
                    {
                        // Roll back newly created Sway output on failure
                        let _ = self.execute_sway(&["output", &conn, "unplug"]).await;
                        return Err(DisplayError::Wayland(format!(
                            "Failed to apply Sway mode (modeline error: {e}, fallback error: {fallback_err})"
                        )));
                    }
                }

                conn
            }
            WlrCompositorType::Hyprland => {
                let existing = self.get_hyprland_output_names().await?;

                // Spawn headless output
                self.execute_hyprctl(&["output", "create", "headless"])
                    .await?;

                let after = self.get_hyprland_output_names().await?;
                let newly_added: Vec<String> = after.difference(&existing).cloned().collect();

                if newly_added.len() != 1 {
                    return Err(DisplayError::Wayland(format!(
                        "Expected exactly 1 new Hyprland output after output create, found {}: {:?}",
                        newly_added.len(),
                        newly_added
                    )));
                }

                let conn = newly_added.into_iter().next().unwrap();

                // Apply initial monitor configuration with DPI scale
                let scale_str = if config.dpi > 144 { "2" } else { "1" };
                let monitor_cfg = format!(
                    "{},{}x{}@{},0x0,{}",
                    conn, config.width, config.height, config.refresh_rate, scale_str
                );
                if let Err(e) = self
                    .execute_hyprctl(&["keyword", "monitor", &monitor_cfg])
                    .await
                {
                    // Roll back newly created Hyprland output on failure
                    let _ = self.execute_hyprctl(&["output", "remove", &conn]).await;
                    return Err(e);
                }

                conn
            }
        };

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
                Ok(path) => Some(path),
                Err(err) => {
                    // Roll back newly created compositor output on EDID failure
                    match self.compositor_type {
                        WlrCompositorType::Sway => {
                            let _ = self.execute_sway(&["output", &connector, "unplug"]).await;
                        }
                        WlrCompositorType::Hyprland => {
                            let _ = self
                                .execute_hyprctl(&["output", "remove", &connector])
                                .await;
                        }
                    }
                    return Err(err);
                }
            }
        } else {
            None
        };

        info!(
            id = config.id,
            connector = %connector,
            mode = format!("{}x{}@{}Hz", config.width, config.height, config.refresh_rate),
            edid = ?edid_path,
            "Created wlroots virtual monitor"
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

        self.monitors.insert(config.id, monitor.clone());
        Ok(monitor)
    }

    async fn destroy_monitor(&mut self, id: u32) -> Result<(), DisplayError> {
        let m = self
            .monitors
            .get(&id)
            .ok_or_else(|| DisplayError::OutputNotFound(format!("Monitor {id} not found")))?
            .clone();

        match self.compositor_type {
            WlrCompositorType::Sway => {
                self.execute_sway(&["output", &m.connector, "unplug"])
                    .await?;
            }
            WlrCompositorType::Hyprland => {
                self.execute_hyprctl(&["output", "remove", &m.connector])
                    .await?;
            }
        }

        self.monitors.remove(&id);

        if let Some(path) = m.edid_path {
            let _ = std::fs::remove_file(path);
        }

        Ok(())
    }

    async fn apply_layout(&mut self, layout: &DisplayLayout) -> Result<(), DisplayError> {
        let placements = layout.compute_placements();
        let mut commands = Vec::new();

        for p in placements {
            if let Some(m) = self.monitors.get_mut(&p.id) {
                m.x = p.x;
                m.y = p.y;
                m.width = p.width;
                m.height = p.height;
                m.refresh_rate = p.refresh_rate;
                m.dpi = p.dpi;
                commands.push((
                    m.connector.clone(),
                    p.x,
                    p.y,
                    p.width,
                    p.height,
                    p.refresh_rate,
                    p.dpi,
                ));
            }
        }

        for (conn, x, y, width, height, refresh_rate, dpi) in commands {
            match self.compositor_type {
                WlrCompositorType::Sway => {
                    let pos_str = format!("{x} {y}");
                    self.execute_sway(&["output", &conn, "pos", &pos_str])
                        .await?;
                }
                WlrCompositorType::Hyprland => {
                    let scale_str = if dpi > 144 { "2" } else { "1" };
                    let monitor_cfg =
                        format!("{conn},{width}x{height}@{refresh_rate},{x}x{y},{scale_str}");
                    self.execute_hyprctl(&["keyword", "monitor", &monitor_cfg])
                        .await?;
                }
            }
        }

        Ok(())
    }

    fn list_monitors(&self) -> Vec<VirtualMonitor> {
        let mut list: Vec<_> = self.monitors.values().cloned().collect();
        list.sort_by_key(|m| m.id);
        list
    }
}
