use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

use linux_quest_display::{
    DisplayError, DisplayLayoutMode, VirtualDisplayBackend, VirtualDisplayManager, VirtualMonitor,
    VirtualMonitorConfig,
};

/// Orchestrator for multi-monitor provisioning, virtual display toggling, and multi-stream session management.
pub struct MultiDisplayHost {
    manager: Arc<Mutex<VirtualDisplayManager>>,
}

impl MultiDisplayHost {
    /// Creates a multi-display host with an explicit backend.
    pub fn new(backend: Box<dyn VirtualDisplayBackend>) -> Self {
        Self {
            manager: Arc::new(Mutex::new(VirtualDisplayManager::new(backend))),
        }
    }

    /// Automatically discovers and initializes the best display backend (Mutter, wlroots, X11, or Mock).
    pub async fn create_auto() -> Result<Self, DisplayError> {
        let mgr = VirtualDisplayManager::create_auto().await?;
        Ok(Self {
            manager: Arc::new(Mutex::new(mgr)),
        })
    }

    /// Returns the active backend name.
    pub async fn backend_name(&self) -> &'static str {
        let mgr = self.manager.lock().await;
        mgr.backend_name()
    }

    /// Toggles the number of active virtual monitors (1, 2, or 3).
    /// Dynamically provisions virtual displays and arranges their spatial desktop layout.
    pub async fn toggle_monitors(
        &self,
        count: u32,
        base_config: &VirtualMonitorConfig,
        layout: DisplayLayoutMode,
    ) -> Result<Vec<VirtualMonitor>, DisplayError> {
        info!(
            target_count = count,
            layout = ?layout,
            "Toggling virtual monitor count"
        );
        let mut mgr = self.manager.lock().await;
        mgr.set_monitor_count(count, base_config, layout).await
    }

    /// Adds a single virtual monitor.
    pub async fn add_monitor(
        &self,
        config: VirtualMonitorConfig,
    ) -> Result<VirtualMonitor, DisplayError> {
        let mut mgr = self.manager.lock().await;
        mgr.add_monitor(config).await
    }

    /// Destroys a virtual monitor by identifier.
    pub async fn remove_monitor(&self, id: u32) -> Result<(), DisplayError> {
        let mut mgr = self.manager.lock().await;
        mgr.remove_monitor(id).await
    }

    /// Sets display layout arrangement (Horizontal, Vertical, Grid).
    pub async fn set_layout(&self, layout: DisplayLayoutMode) -> Result<(), DisplayError> {
        let mut mgr = self.manager.lock().await;
        mgr.set_layout_mode(layout).await
    }

    /// Returns all currently active virtual displays recognized by the Linux desktop.
    pub async fn list_monitors(&self) -> Vec<VirtualMonitor> {
        let mgr = self.manager.lock().await;
        mgr.list_active_monitors()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_display::backend::mock::MockDisplayBackend;

    #[tokio::test]
    async fn test_multi_display_host_toggling() {
        let mock = MockDisplayBackend::new();
        let host = MultiDisplayHost::new(Box::new(mock));

        let cfg = VirtualMonitorConfig::preset_1440p(1, 90);

        // Toggle 1
        let m1 = host
            .toggle_monitors(1, &cfg, DisplayLayoutMode::Horizontal)
            .await
            .unwrap();
        assert_eq!(m1.len(), 1);

        // Toggle 3
        let m3 = host
            .toggle_monitors(3, &cfg, DisplayLayoutMode::Horizontal)
            .await
            .unwrap();
        assert_eq!(m3.len(), 3);
        assert_eq!(m3[2].id, 3);
        assert_eq!(m3[2].x, 5120);

        // Toggle back to 2
        let m2 = host
            .toggle_monitors(2, &cfg, DisplayLayoutMode::Vertical)
            .await
            .unwrap();
        assert_eq!(m2.len(), 2);
        assert_eq!(m2[1].y, 1440);
    }
}
