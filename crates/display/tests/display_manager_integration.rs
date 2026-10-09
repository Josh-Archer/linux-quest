use linux_quest_display::backend::mock::MockDisplayBackend;
use linux_quest_display::{
    DisplayError, DisplayLayoutMode, VirtualDisplayManager, VirtualMonitorConfig,
};

#[tokio::test]
async fn test_toggle_monitors_1_2_3() {
    let mock = MockDisplayBackend::new();
    let mut manager = VirtualDisplayManager::new(Box::new(mock));

    let base_cfg = VirtualMonitorConfig::preset_1440p(1, 90);

    // 1. Toggle 1 monitor
    let monitors = manager
        .set_monitor_count(1, &base_cfg, DisplayLayoutMode::Horizontal)
        .await
        .expect("Toggle 1 monitor failed");
    assert_eq!(monitors.len(), 1);
    assert_eq!(monitors[0].id, 1);
    assert_eq!(monitors[0].width, 2560);
    assert_eq!(monitors[0].height, 1440);
    assert_eq!(monitors[0].refresh_rate, 90);
    assert_eq!(monitors[0].x, 0);

    // 2. Toggle 2 monitors
    let monitors = manager
        .set_monitor_count(2, &base_cfg, DisplayLayoutMode::Horizontal)
        .await
        .expect("Toggle 2 monitors failed");
    assert_eq!(monitors.len(), 2);
    assert_eq!(monitors[0].id, 1);
    assert_eq!(monitors[1].id, 2);
    assert_eq!(monitors[0].x, 0);
    assert_eq!(monitors[1].x, 2560);

    // 3. Toggle 3 monitors
    let monitors = manager
        .set_monitor_count(3, &base_cfg, DisplayLayoutMode::Horizontal)
        .await
        .expect("Toggle 3 monitors failed");
    assert_eq!(monitors.len(), 3);
    assert_eq!(monitors[0].id, 1);
    assert_eq!(monitors[1].id, 2);
    assert_eq!(monitors[2].id, 3);
    assert_eq!(monitors[0].x, 0);
    assert_eq!(monitors[1].x, 2560);
    assert_eq!(monitors[2].x, 5120);

    // 4. Toggle back down to 1 monitor
    let monitors = manager
        .set_monitor_count(1, &base_cfg, DisplayLayoutMode::Horizontal)
        .await
        .expect("Toggle back to 1 monitor failed");
    assert_eq!(monitors.len(), 1);
    assert_eq!(monitors[0].id, 1);
    assert_eq!(monitors[0].x, 0);
}

#[tokio::test]
async fn test_toggle_boundary_validation() {
    let mock = MockDisplayBackend::new();
    let mut manager = VirtualDisplayManager::new(Box::new(mock));
    let base_cfg = VirtualMonitorConfig::preset_1080p(1, 60);

    // 0 monitors rejected
    assert!(manager
        .set_monitor_count(0, &base_cfg, DisplayLayoutMode::Horizontal)
        .await
        .is_err());

    // >3 monitors rejected
    let err = manager
        .set_monitor_count(4, &base_cfg, DisplayLayoutMode::Horizontal)
        .await
        .expect_err("Toggling 4 monitors should fail");

    match err {
        DisplayError::MaxMonitorsExceeded(3) => (),
        other => panic!("Expected MaxMonitorsExceeded(3), got {:?}", other),
    }
}

#[tokio::test]
async fn test_id_allocation_reuse_after_removal() {
    let mock = MockDisplayBackend::new();
    let mut manager = VirtualDisplayManager::new(Box::new(mock));
    let base_cfg = VirtualMonitorConfig::preset_1080p(1, 60);

    // Spawn 3 monitors: ids [1, 2, 3]
    manager
        .set_monitor_count(3, &base_cfg, DisplayLayoutMode::Horizontal)
        .await
        .unwrap();

    // Remove monitor id 2
    manager.remove_monitor(2).await.unwrap();
    assert_eq!(manager.list_active_monitors().len(), 2);

    // Add 3rd monitor back: should allocate id 2 cleanly and maintain left-to-right spatial layout
    let monitors = manager
        .set_monitor_count(3, &base_cfg, DisplayLayoutMode::Horizontal)
        .await
        .unwrap();
    assert_eq!(monitors.len(), 3);
    assert_eq!(monitors[0].id, 1);
    assert_eq!(monitors[1].id, 2);
    assert_eq!(monitors[2].id, 3);
    assert_eq!(monitors[0].x, 0);
    assert_eq!(monitors[1].x, 1920);
    assert_eq!(monitors[2].x, 3840);
}

#[tokio::test]
async fn test_same_count_mode_update() {
    let mock = MockDisplayBackend::new();
    let mut manager = VirtualDisplayManager::new(Box::new(mock));

    // Spawn 1 monitor at 1080p
    let cfg_1080p = VirtualMonitorConfig::preset_1080p(1, 60);
    manager
        .set_monitor_count(1, &cfg_1080p, DisplayLayoutMode::Horizontal)
        .await
        .unwrap();
    let m = manager.list_active_monitors();
    assert_eq!(m[0].width, 1920);

    // Update to 1440p with same count = 1
    let cfg_1440p = VirtualMonitorConfig::preset_1440p(1, 90);
    manager
        .set_monitor_count(1, &cfg_1440p, DisplayLayoutMode::Horizontal)
        .await
        .unwrap();
    let m_updated = manager.list_active_monitors();
    assert_eq!(m_updated.len(), 1);
    assert_eq!(m_updated[0].width, 2560);
    assert_eq!(m_updated[0].height, 1440);
    assert_eq!(m_updated[0].refresh_rate, 90);
}

#[tokio::test]
async fn test_dynamic_resolution_negotiation() {
    let mock = MockDisplayBackend::new();
    let mut manager = VirtualDisplayManager::new(Box::new(mock));

    // Start with 1080p @ 60Hz
    let m1 = VirtualMonitorConfig::preset_1080p(1, 60);
    manager.add_monitor(m1).await.unwrap();

    let active = manager.list_active_monitors();
    assert_eq!(active[0].width, 1920);
    assert_eq!(active[0].height, 1080);
    assert_eq!(active[0].refresh_rate, 60);

    // Remove and re-add negotiated 4K @ 60Hz
    manager.remove_monitor(1).await.unwrap();
    let m1_4k = VirtualMonitorConfig::preset_4k(1, 60);
    manager.add_monitor(m1_4k).await.unwrap();

    let active = manager.list_active_monitors();
    assert_eq!(active[0].width, 3840);
    assert_eq!(active[0].height, 2160);
    assert_eq!(active[0].refresh_rate, 60);
}

#[tokio::test]
async fn test_layout_switching_vertical_and_grid() {
    let mock = MockDisplayBackend::new();
    let mut manager = VirtualDisplayManager::new(Box::new(mock));
    let base_cfg = VirtualMonitorConfig::preset_1080p(1, 90);

    // Spawn 2 monitors
    manager
        .set_monitor_count(2, &base_cfg, DisplayLayoutMode::Horizontal)
        .await
        .unwrap();

    // Switch to vertical stacked layout
    manager
        .set_layout_mode(DisplayLayoutMode::Vertical)
        .await
        .unwrap();

    let active = manager.list_active_monitors();
    assert_eq!(active[0].x, 0);
    assert_eq!(active[0].y, 0);
    assert_eq!(active[1].x, 0);
    assert_eq!(active[1].y, 1080);

    // Add 3rd monitor and switch to Grid
    manager
        .set_monitor_count(3, &base_cfg, DisplayLayoutMode::Grid)
        .await
        .unwrap();

    let active = manager.list_active_monitors();
    assert_eq!(active[0].x, 0);
    assert_eq!(active[0].y, 0);
    assert_eq!(active[1].x, 1920);
    assert_eq!(active[1].y, 0);
    assert_eq!(active[2].x, 0);
    assert_eq!(active[2].y, 1080);
}

#[tokio::test]
async fn test_raising_count_reconfigures_existing_monitors() {
    let mock = MockDisplayBackend::new();
    let mut manager = VirtualDisplayManager::new(Box::new(mock));

    // 1 monitor at 1080p60
    let cfg_1080p = VirtualMonitorConfig::preset_1080p(1, 60);
    manager
        .set_monitor_count(1, &cfg_1080p, DisplayLayoutMode::Horizontal)
        .await
        .unwrap();
    let m = manager.list_active_monitors();
    assert_eq!(m[0].width, 1920);
    assert_eq!(m[0].refresh_rate, 60);

    // Raise to 2 monitors at 1440p90: both monitors must be 1440p90
    let cfg_1440p = VirtualMonitorConfig::preset_1440p(1, 90);
    let monitors = manager
        .set_monitor_count(2, &cfg_1440p, DisplayLayoutMode::Horizontal)
        .await
        .unwrap();
    assert_eq!(monitors.len(), 2);
    assert_eq!(monitors[0].width, 2560);
    assert_eq!(monitors[0].height, 1440);
    assert_eq!(monitors[0].refresh_rate, 90);
    assert_eq!(monitors[1].width, 2560);
    assert_eq!(monitors[1].height, 1440);
    assert_eq!(monitors[1].refresh_rate, 90);
}

#[tokio::test]
async fn test_remove_last_monitor_leaves_empty() {
    let mock = MockDisplayBackend::new();
    let mut manager = VirtualDisplayManager::new(Box::new(mock));

    let cfg = VirtualMonitorConfig::preset_1080p(1, 60);
    manager
        .set_monitor_count(1, &cfg, DisplayLayoutMode::Horizontal)
        .await
        .unwrap();

    // Removing the last monitor leaves zero active monitors
    manager.remove_monitor(1).await.unwrap();
    assert_eq!(manager.list_active_monitors().len(), 0);
}

#[test]
fn test_ensure_display_runtime_dir_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = linux_quest_display::ensure_display_runtime_dir().unwrap();
    assert!(dir.exists(), "Display runtime directory must exist");
    let perms = std::fs::metadata(&dir).unwrap().permissions();
    assert_eq!(
        perms.mode() & 0o777,
        0o700,
        "Display runtime directory must have 0700 permissions"
    );
}

#[tokio::test]
async fn test_manager_transactional_mode_change_restores_old_config() {
    use async_trait::async_trait;
    use linux_quest_display::backend::VirtualDisplayBackend;
    use linux_quest_display::error::DisplayError;
    use linux_quest_display::layout::DisplayLayout;
    use linux_quest_display::VirtualMonitor;

    struct FailOnNewModeBackend {
        inner: MockDisplayBackend,
        fail_width: u32,
    }

    #[async_trait]
    impl VirtualDisplayBackend for FailOnNewModeBackend {
        fn name(&self) -> &'static str {
            "FailOnNewModeBackend"
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn init(&mut self) -> Result<(), DisplayError> {
            self.inner.init().await
        }
        async fn create_monitor(
            &mut self,
            config: &VirtualMonitorConfig,
        ) -> Result<VirtualMonitor, DisplayError> {
            if config.width == self.fail_width {
                return Err(DisplayError::BackendUnavailable(
                    "Simulated failure on requested mode".into(),
                ));
            }
            self.inner.create_monitor(config).await
        }
        async fn destroy_monitor(&mut self, id: u32) -> Result<(), DisplayError> {
            self.inner.destroy_monitor(id).await
        }
        async fn apply_layout(&mut self, layout: &DisplayLayout) -> Result<(), DisplayError> {
            self.inner.apply_layout(layout).await
        }
        fn list_monitors(&self) -> Vec<VirtualMonitor> {
            self.inner.list_monitors()
        }
    }

    let backend = FailOnNewModeBackend {
        inner: MockDisplayBackend::new(),
        fail_width: 2560,
    };
    let mut manager = VirtualDisplayManager::new(Box::new(backend));

    // Initially configure 1080p
    let cfg_1080p = VirtualMonitorConfig::preset_1080p(1, 60);
    manager
        .set_monitor_count(1, &cfg_1080p, DisplayLayoutMode::Horizontal)
        .await
        .unwrap();

    let initial = manager.list_active_monitors();
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].width, 1920);

    // Attempt to switch to 1440p (which will fail in create_monitor)
    let cfg_1440p = VirtualMonitorConfig::preset_1440p(1, 90);
    let res = manager
        .set_monitor_count(1, &cfg_1440p, DisplayLayoutMode::Horizontal)
        .await;

    assert!(
        res.is_err(),
        "Mode change must return error on simulated failure"
    );

    // Verify transactional rollback: old configuration was restored!
    let active = manager.list_active_monitors();
    assert_eq!(
        active.len(),
        1,
        "Monitor must not be dropped on mode change failure"
    );
    assert_eq!(active[0].width, 1920, "Previous width must be restored");
    assert_eq!(
        active[0].refresh_rate, 60,
        "Previous refresh rate must be restored"
    );
}
