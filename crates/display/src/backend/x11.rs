use async_trait::async_trait;
use std::collections::HashMap;
use tokio::process::Command;
use tracing::info;

use super::{VirtualDisplayBackend, VirtualMonitor};
use crate::config::VirtualMonitorConfig;
use crate::error::DisplayError;
use crate::layout::DisplayLayout;
use crate::timing::CvtTiming;

/// X11 virtual display backend using `xrandr` dynamic modelines.
pub struct X11DisplayBackend {
    monitors: HashMap<u32, VirtualMonitor>,
    assigned_outputs: HashMap<u32, String>,
    created_modes: HashMap<u32, String>,
    mode_ref_counts: HashMap<String, usize>,
    is_available: bool,
}

impl X11DisplayBackend {
    pub fn new() -> Self {
        let is_avail = std::env::var("DISPLAY").is_ok();
        Self {
            monitors: HashMap::new(),
            assigned_outputs: HashMap::new(),
            created_modes: HashMap::new(),
            mode_ref_counts: HashMap::new(),
            is_available: is_avail,
        }
    }

    /// Queries `xrandr` to find virtual display sinks (Xdummy, VKMS, EVDI, VIRTUAL)
    /// ahead of disconnected physical ports.
    async fn find_available_output(&self, exclude: &[String]) -> Result<String, DisplayError> {
        let output = Command::new("xrandr")
            .arg("--current")
            .output()
            .await
            .map_err(|e| DisplayError::CommandFailed(format!("xrandr query failed: {e}")))?;

        if !output.status.success() {
            return Err(DisplayError::X11("xrandr exited with error".into()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();

        // 1. High-priority search: dedicated virtual sinks (Xdummy, VKMS, EVDI, VIRTUAL)
        for line in &lines {
            if let Some(first) = line.split_whitespace().next() {
                let name = first.to_uppercase();
                if (name.starts_with("VIRTUAL")
                    || name.starts_with("DUMMY")
                    || name.starts_with("VKMS")
                    || name.starts_with("EVDI"))
                    && !exclude.contains(&first.to_string())
                {
                    return Ok(first.to_string());
                }
            }
        }

        // 2. Secondary fallback: unused/disconnected physical outputs
        for line in &lines {
            if line.contains("disconnected") {
                if let Some(conn) = line.split_whitespace().next() {
                    let conn_str = conn.to_string();
                    if !exclude.contains(&conn_str) {
                        return Ok(conn_str);
                    }
                }
            }
        }

        Err(DisplayError::X11(
            "No virtual display sink (Xdummy, VKMS, EVDI, VIRTUAL) or disconnected output found in xrandr".into(),
        ))
    }

    /// Computes a standard CVT Reduced Blanking modeline string using CvtTiming.
    pub fn generate_cvt_modeline(
        width: u32,
        height: u32,
        refresh_rate: u32,
    ) -> Result<(String, String), DisplayError> {
        let timing = CvtTiming::calculate(width, height, refresh_rate)?;
        let mode_name = format!("{width}x{height}_{refresh_rate}.00");
        let modeline_params = timing.x11_modeline_params();
        Ok((mode_name, modeline_params))
    }

    async fn execute_xrandr(&self, args: &[&str]) -> Result<(), DisplayError> {
        let output = Command::new("xrandr")
            .args(args)
            .output()
            .await
            .map_err(|e| DisplayError::CommandFailed(format!("Failed to run xrandr: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("already exists") {
                return Err(DisplayError::X11(format!("xrandr error: {stderr}")));
            }
        }
        Ok(())
    }
}

impl Default for X11DisplayBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VirtualDisplayBackend for X11DisplayBackend {
    fn name(&self) -> &'static str {
        "X11 (xrandr)"
    }

    fn is_available(&self) -> bool {
        self.is_available
    }

    async fn init(&mut self) -> Result<(), DisplayError> {
        if !self.is_available {
            return Err(DisplayError::BackendUnavailable(
                "X11 display server not accessible ($DISPLAY is unset)".into(),
            ));
        }
        Ok(())
    }

    async fn create_monitor(
        &mut self,
        config: &VirtualMonitorConfig,
    ) -> Result<VirtualMonitor, DisplayError> {
        config.validate()?;

        let excluded: Vec<String> = self.assigned_outputs.values().cloned().collect();
        let connector = self.find_available_output(&excluded).await?;

        let (mode_name, modeline_args) =
            Self::generate_cvt_modeline(config.width, config.height, config.refresh_rate)?;

        // 1. Create modeline in X11
        let mut newmode_cmd = vec!["--newmode", &mode_name];
        for part in modeline_args.split_whitespace() {
            newmode_cmd.push(part);
        }
        self.execute_xrandr(&newmode_cmd).await?;

        // 2. Add mode to connector
        self.execute_xrandr(&["--addmode", &connector, &mode_name])
            .await?;

        // 3. Activate output
        self.execute_xrandr(&["--output", &connector, "--mode", &mode_name])
            .await?;

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
                    // Roll back X11 configuration on EDID failure
                    let _ = self
                        .execute_xrandr(&["--output", &connector, "--off"])
                        .await;
                    let _ = self
                        .execute_xrandr(&["--delmode", &connector, &mode_name])
                        .await;
                    if !self.mode_ref_counts.contains_key(&mode_name) {
                        let _ = self.execute_xrandr(&["--rmmode", &mode_name]).await;
                    }
                    return Err(err);
                }
            }
        } else {
            None
        };

        // Update reference count for this mode only after EDID file write succeeds
        *self.mode_ref_counts.entry(mode_name.clone()).or_insert(0) += 1;

        info!(
            id = config.id,
            connector = %connector,
            mode = %mode_name,
            edid = ?edid_path,
            "Activated X11 virtual display"
        );

        let monitor = VirtualMonitor {
            id: config.id,
            name: config.name.clone(),
            connector: connector.clone(),
            width: config.width,
            height: config.height,
            refresh_rate: config.refresh_rate,
            x: 0,
            y: 0,
            dpi: config.dpi,
            is_virtual: true,
            edid_path,
        };

        self.assigned_outputs.insert(config.id, connector);
        self.created_modes.insert(config.id, mode_name);
        self.monitors.insert(config.id, monitor.clone());

        Ok(monitor)
    }

    async fn destroy_monitor(&mut self, id: u32) -> Result<(), DisplayError> {
        let conn = self
            .assigned_outputs
            .get(&id)
            .ok_or_else(|| DisplayError::OutputNotFound(format!("Monitor {id} not found")))?
            .clone();

        let mode = self.created_modes.get(&id).cloned();

        // Turn off output and propagate any errors FIRST
        self.execute_xrandr(&["--output", &conn, "--off"]).await?;

        if let Some(m) = &mode {
            self.execute_xrandr(&["--delmode", &conn, m]).await?;

            // Only remove modeline from X11 if no other monitor is using it.
            // Execute --rmmode BEFORE mutating/removing mode_ref_counts to guarantee atomicity.
            if let Some(&count) = self.mode_ref_counts.get(m) {
                if count <= 1 {
                    self.execute_xrandr(&["--rmmode", m]).await?;
                    self.mode_ref_counts.remove(m);
                } else {
                    self.mode_ref_counts.insert(m.clone(), count - 1);
                }
            }
        }

        // Only drop internal state after compositor commands succeed
        self.assigned_outputs.remove(&id);
        self.created_modes.remove(&id);

        if let Some(m) = self.monitors.remove(&id) {
            if let Some(path) = m.edid_path {
                let _ = std::fs::remove_file(path);
            }
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

                if let Some(conn) = self.assigned_outputs.get(&p.id) {
                    let pos_str = format!("{}x{}", p.x, p.y);
                    commands.push((conn.clone(), pos_str));
                }
            }
        }

        for (conn, pos) in commands {
            self.execute_xrandr(&["--output", &conn, "--pos", &pos])
                .await?;
        }

        Ok(())
    }

    fn list_monitors(&self) -> Vec<VirtualMonitor> {
        let mut list: Vec<_> = self.monitors.values().cloned().collect();
        list.sort_by_key(|m| m.id);
        list
    }
}
