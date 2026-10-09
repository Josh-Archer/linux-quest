use async_trait::async_trait;
use std::collections::HashMap;

use super::{VirtualDisplayBackend, VirtualMonitor};
use crate::config::VirtualMonitorConfig;
use crate::error::DisplayError;
use crate::layout::DisplayLayout;

/// Deterministic mock virtual display backend for headless benchmarking, CI, and test fixtures.
#[derive(Debug, Default)]
pub struct MockDisplayBackend {
    monitors: HashMap<u32, VirtualMonitor>,
    max_monitors: u32,
    initialized: bool,
}

impl MockDisplayBackend {
    pub fn new() -> Self {
        Self {
            monitors: HashMap::new(),
            max_monitors: 3,
            initialized: false,
        }
    }

    pub fn with_max_monitors(mut self, max: u32) -> Self {
        self.max_monitors = max;
        self
    }
}

#[async_trait]
impl VirtualDisplayBackend for MockDisplayBackend {
    fn name(&self) -> &'static str {
        "Mock (Deterministic)"
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn init(&mut self) -> Result<(), DisplayError> {
        self.initialized = true;
        Ok(())
    }

    async fn create_monitor(
        &mut self,
        config: &VirtualMonitorConfig,
    ) -> Result<VirtualMonitor, DisplayError> {
        config.validate()?;

        if self.monitors.len() as u32 >= self.max_monitors {
            return Err(DisplayError::MaxMonitorsExceeded(self.max_monitors));
        }

        let connector = format!("VIRTUAL-{}", config.id);

        let edid_path = if let Some(edid_bytes) = &config.edid {
            let edid_dir = crate::ensure_display_runtime_dir()?;
            let edid_file = edid_dir.join(format!("edid-{connector}.bin"));
            std::fs::write(&edid_file, edid_bytes).map_err(|e| {
                DisplayError::CommandFailed(format!(
                    "Failed to write runtime EDID file {edid_file:?}: {e}"
                ))
            })?;
            Some(edid_file.to_string_lossy().to_string())
        } else {
            None
        };

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
        if let Some(m) = self.monitors.remove(&id) {
            if let Some(path) = m.edid_path {
                let _ = std::fs::remove_file(path);
            }
            Ok(())
        } else {
            Err(DisplayError::OutputNotFound(format!(
                "Monitor id {id} not found"
            )))
        }
    }

    async fn apply_layout(&mut self, layout: &DisplayLayout) -> Result<(), DisplayError> {
        let placements = layout.compute_placements();
        for p in placements {
            if let Some(m) = self.monitors.get_mut(&p.id) {
                m.x = p.x;
                m.y = p.y;
                m.width = p.width;
                m.height = p.height;
                m.refresh_rate = p.refresh_rate;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::DisplayLayoutMode;

    #[tokio::test]
    async fn test_mock_backend_lifecycle() {
        let mut backend = MockDisplayBackend::new();
        backend.init().await.unwrap();

        let cfg1 = VirtualMonitorConfig::preset_1080p(1, 90);
        let m1 = backend.create_monitor(&cfg1).await.unwrap();
        assert_eq!(m1.id, 1);
        assert_eq!(m1.width, 1920);
        assert_eq!(backend.list_monitors().len(), 1);

        let cfg2 = VirtualMonitorConfig::preset_1440p(2, 120);
        let m2 = backend.create_monitor(&cfg2).await.unwrap();
        assert_eq!(m2.id, 2);
        assert_eq!(backend.list_monitors().len(), 2);

        let layout = DisplayLayout::new(DisplayLayoutMode::Horizontal, vec![cfg1, cfg2]);
        backend.apply_layout(&layout).await.unwrap();

        let active = backend.list_monitors();
        assert_eq!(active[0].x, 0);
        assert_eq!(active[1].x, 1920);

        backend.destroy_monitor(1).await.unwrap();
        assert_eq!(backend.list_monitors().len(), 1);
        assert_eq!(backend.list_monitors()[0].id, 2);
    }

    #[tokio::test]
    async fn test_mock_backend_max_monitors_limit() {
        let mut backend = MockDisplayBackend::new().with_max_monitors(2);
        let c1 = VirtualMonitorConfig::preset_1080p(1, 60);
        let c2 = VirtualMonitorConfig::preset_1080p(2, 60);
        let c3 = VirtualMonitorConfig::preset_1080p(3, 60);

        backend.create_monitor(&c1).await.unwrap();
        backend.create_monitor(&c2).await.unwrap();
        let err = backend.create_monitor(&c3).await.unwrap_err();
        match err {
            DisplayError::MaxMonitorsExceeded(2) => (),
            other => panic!("Expected MaxMonitorsExceeded, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_mock_backend_edid_installation_lifecycle() {
        let mut backend = MockDisplayBackend::new();
        backend.init().await.unwrap();

        let fake_edid = vec![0x42u8; 128];
        let cfg = VirtualMonitorConfig::preset_1080p(1, 60).with_edid(fake_edid.clone());

        let monitor = backend.create_monitor(&cfg).await.unwrap();
        assert!(monitor.edid_path.is_some());
        let edid_path_str = monitor.edid_path.unwrap();
        let edid_path = std::path::Path::new(&edid_path_str);

        // Verify EDID file was written to disk with matching content
        assert!(edid_path.exists(), "EDID file must exist on disk");
        let content = std::fs::read(edid_path).unwrap();
        assert_eq!(content, fake_edid);

        // Destroy monitor and verify EDID file is cleaned up
        backend.destroy_monitor(1).await.unwrap();
        assert!(
            !edid_path.exists(),
            "EDID file must be removed after monitor destruction"
        );
    }
}
