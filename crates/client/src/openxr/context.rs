//! OpenXR session lifecycle, reference space, and event polling context.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ash::vk;
use openxr::{
    ApplicationInfo, Entry, ExtensionSet, FormFactor, Instance, Session, SessionState, Space,
    Swapchain, SwapchainCreateFlags, SwapchainCreateInfo, SwapchainUsageFlags, SystemId, Version,
    ViewConfigurationType, Vulkan,
};

use crate::config::ClientConfig;
use crate::error::{ClientError, ClientResult};
use crate::graphics::VulkanContext;
use crate::openxr::refresh_rate::RefreshRateManager;

/// OpenXR application runtime context managing instance, session, and swapchains.
pub struct OpenXrContext {
    instance: Option<Instance>,
    system: SystemId,
    session: Option<Session<Vulkan>>,
    stage_space: Option<Space>,
    view_space: Option<Space>,
    refresh_rate_manager: Option<RefreshRateManager>,
    session_state: SessionState,
    session_running: Arc<AtomicBool>,
    is_simulated: bool,
}

impl OpenXrContext {
    /// Creates a simulated OpenXR context for testing and headless CI.
    pub fn new_simulated(config: &ClientConfig) -> Self {
        let refresh_mgr =
            RefreshRateManager::new(vec![72.0, 90.0, 120.0], config.target_refresh_rate);

        Self {
            instance: None,
            system: SystemId::NULL,
            session: None,
            stage_space: None,
            view_space: None,
            refresh_rate_manager: Some(refresh_mgr),
            session_state: SessionState::FOCUSED,
            session_running: Arc::new(AtomicBool::new(true)),
            is_simulated: true,
        }
    }

    /// Attempts to initialize a live OpenXR session on Meta Horizon OS or desktop OpenXR.
    pub fn try_new(
        _config: &ClientConfig,
        _vulkan_context: &mut VulkanContext,
    ) -> ClientResult<Self> {
        // Load OpenXR entry
        let entry = unsafe {
            #[cfg(target_os = "android")]
            {
                Entry::load(&()).map_err(|e| {
                    ClientError::OpenXr(format!("Failed to load Android OpenXR loader: {e:?}"))
                })?
            }
            #[cfg(not(target_os = "android"))]
            {
                Entry::load(&()).map_err(|e| {
                    ClientError::OpenXr(format!("Failed to load OpenXR dynamic loader: {e:?}"))
                })?
            }
        };

        // Query available instance extensions
        let available_exts = entry
            .enumerate_extensions()
            .map_err(|e| ClientError::OpenXr(format!("Failed to enumerate extensions: {e:?}")))?;

        let mut required_exts = ExtensionSet::default();
        if available_exts.khr_vulkan_enable2 {
            required_exts.khr_vulkan_enable2 = true;
        }
        if available_exts.khr_composition_layer_cylinder {
            required_exts.khr_composition_layer_cylinder = true;
        }
        if available_exts.fb_display_refresh_rate {
            required_exts.fb_display_refresh_rate = true;
        }

        let app_info = ApplicationInfo {
            application_name: "linux-quest",
            application_version: 1,
            engine_name: "linux-quest-engine",
            engine_version: 1,
            api_version: Version::new(1, 0, 0),
        };

        let instance = entry
            .create_instance(&app_info, &required_exts, &[], &())
            .map_err(|e| ClientError::OpenXr(format!("Failed to create OpenXR instance: {e:?}")))?;

        let system = instance
            .system(FormFactor::HEAD_MOUNTED_DISPLAY)
            .map_err(|e| ClientError::OpenXr(format!("Failed to query HMD system: {e:?}")))?;

        Ok(Self {
            instance: Some(instance),
            system,
            session: None,
            stage_space: None,
            view_space: None,
            refresh_rate_manager: None,
            session_state: SessionState::IDLE,
            session_running: Arc::new(AtomicBool::new(false)),
            is_simulated: false,
        })
    }

    /// Whether this OpenXR context is running in simulated/mock mode.
    pub fn is_simulated(&self) -> bool {
        self.is_simulated
    }

    /// Returns current OpenXR session state.
    pub fn session_state(&self) -> SessionState {
        self.session_state
    }

    /// Whether the session is actively running.
    pub fn is_session_running(&self) -> bool {
        self.session_running.load(Ordering::SeqCst)
    }

    /// Returns the active refresh rate manager.
    pub fn refresh_rate_manager(&self) -> Option<&RefreshRateManager> {
        self.refresh_rate_manager.as_ref()
    }

    /// Returns the OpenXR SystemId.
    pub fn system(&self) -> SystemId {
        self.system
    }

    /// Returns a mutable reference to the active refresh rate manager.
    pub fn refresh_rate_manager_mut(&mut self) -> Option<&mut RefreshRateManager> {
        self.refresh_rate_manager.as_mut()
    }

    /// Polls OpenXR runtime events and updates session state machine.
    pub fn poll_events(&mut self) -> ClientResult<()> {
        if self.is_simulated {
            return Ok(());
        }

        let instance = self
            .instance
            .as_ref()
            .ok_or_else(|| ClientError::OpenXr("Instance not loaded".to_string()))?;

        let mut event_storage = openxr::EventDataBuffer::new();
        while let Some(event) = instance
            .poll_event(&mut event_storage)
            .map_err(|e| ClientError::OpenXr(format!("Poll event failed: {e:?}")))?
        {
            match event {
                openxr::Event::SessionStateChanged(state_event) => {
                    let new_state = state_event.state();
                    tracing::info!(old = ?self.session_state, new = ?new_state, "OpenXR SessionState changed");
                    self.session_state = new_state;

                    match new_state {
                        SessionState::READY => {
                            if let Some(session) = &self.session {
                                session
                                    .begin(ViewConfigurationType::PRIMARY_STEREO)
                                    .map_err(|e| {
                                        ClientError::OpenXr(format!("Session begin failed: {e:?}"))
                                    })?;
                                self.session_running.store(true, Ordering::SeqCst);
                            }
                        }
                        SessionState::STOPPING => {
                            if let Some(session) = &self.session {
                                let _ = session.end();
                                self.session_running.store(false, Ordering::SeqCst);
                            }
                        }
                        SessionState::EXITING | SessionState::LOSS_PENDING => {
                            self.session_running.store(false, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
                openxr::Event::DisplayRefreshRateChangedFB(rate_event) => {
                    let new_rate = rate_event.from_display_refresh_rate();
                    if let Some(mgr) = &mut self.refresh_rate_manager {
                        mgr.on_refresh_rate_changed(new_rate);
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Creates an OpenXR swapchain for desktop streaming.
    pub fn create_desktop_swapchain(
        &self,
        width: u32,
        height: u32,
        format: vk::Format,
    ) -> ClientResult<Option<Swapchain<Vulkan>>> {
        if self.is_simulated {
            return Ok(None);
        }

        let session = self
            .session
            .as_ref()
            .ok_or_else(|| ClientError::OpenXr("Session not initialized".to_string()))?;

        let info = SwapchainCreateInfo {
            create_flags: SwapchainCreateFlags::EMPTY,
            usage_flags: SwapchainUsageFlags::COLOR_ATTACHMENT | SwapchainUsageFlags::SAMPLED,
            format: format.as_raw() as u32,
            sample_count: 1,
            width,
            height,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        };

        let swapchain = session
            .create_swapchain(&info)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create swapchain: {e:?}")))?;

        Ok(Some(swapchain))
    }

    /// Returns the stage reference space for 6DoF tracking.
    pub fn stage_space(&self) -> Option<&Space> {
        self.stage_space.as_ref()
    }

    /// Returns the head-locked view reference space.
    pub fn view_space(&self) -> Option<&Space> {
        self.view_space.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simulated_openxr_context_lifecycle() {
        let config = ClientConfig::default();
        let mut ctx = OpenXrContext::new_simulated(&config);
        assert!(ctx.is_simulated());
        assert_eq!(ctx.session_state(), SessionState::FOCUSED);
        assert!(ctx.is_session_running());

        assert!(ctx.poll_events().is_ok());

        let refresh_mgr = ctx.refresh_rate_manager().unwrap();
        assert_eq!(refresh_mgr.current_rate(), 90.0);
    }
}
