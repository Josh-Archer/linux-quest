//! OpenXR session lifecycle, reference space, and event polling context.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ash::vk;
use ash::vk::Handle;
use openxr::{
    ApplicationInfo, CompositionLayerBase, Entry, EnvironmentBlendMode, ExtensionSet, FormFactor,
    FrameState, FrameStream, FrameWaiter, Instance, Session, SessionState, Space, Swapchain,
    SwapchainCreateFlags, SwapchainCreateInfo, SwapchainUsageFlags, SystemId, Time, Version,
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
    frame_waiter: Option<parking_lot::Mutex<FrameWaiter>>,
    frame_stream: Option<parking_lot::Mutex<FrameStream<Vulkan>>>,
    stage_space: Option<Space>,
    view_space: Option<Space>,
    desktop_swapchain: Option<Swapchain<Vulkan>>,
    hud_swapchain: Option<Swapchain<Vulkan>>,
    refresh_rate_manager: Option<RefreshRateManager>,
    session_state: SessionState,
    session_running: Arc<AtomicBool>,
    supports_cylinder: bool,
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
            frame_waiter: None,
            frame_stream: None,
            stage_space: None,
            view_space: None,
            desktop_swapchain: None,
            hud_swapchain: None,
            refresh_rate_manager: Some(refresh_mgr),
            session_state: SessionState::FOCUSED,
            session_running: Arc::new(AtomicBool::new(true)),
            supports_cylinder: true,
            is_simulated: true,
        }
    }

    /// Attempts to initialize a live OpenXR session on Meta Horizon OS or desktop OpenXR.
    #[allow(clippy::missing_transmute_annotations)]
    pub fn try_new(
        config: &ClientConfig,
        vulkan_context: &mut VulkanContext,
    ) -> ClientResult<Self> {
        #[cfg(target_os = "android")]
        let platform_info = {
            let ctx = ndk_context::android_context();
            unsafe { openxr::AndroidPlatformInfo::new(ctx.vm(), ctx.context()) }
        };
        #[cfg(not(target_os = "android"))]
        let platform_info = ();

        // Load OpenXR entry
        let entry = unsafe {
            Entry::load(&platform_info)
                .map_err(|e| ClientError::OpenXr(format!("Failed to load OpenXR loader: {e:?}")))?
        };

        // Query available instance extensions
        let available_exts = entry
            .enumerate_extensions()
            .map_err(|e| ClientError::OpenXr(format!("Failed to enumerate extensions: {e:?}")))?;

        let mut required_exts = ExtensionSet::default();
        if !available_exts.khr_vulkan_enable2 {
            return Err(ClientError::OpenXr(
                "XR_KHR_vulkan_enable2 is required but not supported by OpenXR runtime".into(),
            ));
        }
        required_exts.khr_vulkan_enable2 = true;

        let supports_cylinder = available_exts.khr_composition_layer_cylinder;
        if supports_cylinder {
            required_exts.khr_composition_layer_cylinder = true;
        }

        let supports_refresh_rate = available_exts.fb_display_refresh_rate;
        if supports_refresh_rate {
            required_exts.fb_display_refresh_rate = true;
        }

        if available_exts.fb_composition_layer_settings {
            required_exts.fb_composition_layer_settings = true;
        }

        #[cfg(target_os = "android")]
        {
            if available_exts.khr_android_create_instance {
                required_exts.khr_android_create_instance = true;
            }
            if available_exts.khr_android_surface_swapchain {
                required_exts.khr_android_surface_swapchain = true;
            }
        }

        let app_info = ApplicationInfo {
            application_name: "linux-quest",
            application_version: 1,
            engine_name: "linux-quest-engine",
            engine_version: 1,
            api_version: Version::new(1, 0, 0),
        };

        let instance = entry
            .create_instance(&app_info, &required_exts, &[], &platform_info)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create OpenXR instance: {e:?}")))?;

        let system = instance
            .system(FormFactor::HEAD_MOUNTED_DISPLAY)
            .map_err(|e| ClientError::OpenXr(format!("Failed to query HMD system: {e:?}")))?;

        let vk_reqs = instance
            .graphics_requirements::<Vulkan>(system)
            .map_err(|e| {
                ClientError::OpenXr(format!("Failed to query graphics requirements: {e:?}"))
            })?;

        tracing::info!(
            min_vulkan_version = ?vk_reqs.min_api_version_supported,
            max_vulkan_version = ?vk_reqs.max_api_version_supported,
            "OpenXR Vulkan requirements verified"
        );

        let vk_entry = unsafe {
            ash::Entry::load().map_err(|e| {
                ClientError::Vulkan(format!("Failed to load Vulkan entry point: {e:?}"))
            })?
        };

        let vk_app_info = vk::ApplicationInfo::default()
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_version(vk::make_api_version(0, 0, 1, 0))
            .api_version(vk::make_api_version(0, 1, 1, 0));

        let vk_instance_create_info =
            vk::InstanceCreateInfo::default().application_info(&vk_app_info);

        let vk_instance_raw = unsafe {
            let res = instance
                .create_vulkan_instance(
                    system,
                    std::mem::transmute(vk_entry.static_fn().get_instance_proc_addr),
                    &vk_instance_create_info as *const _ as *const _,
                )
                .map_err(|e| {
                    ClientError::OpenXr(format!("XR error creating Vulkan instance: {e:?}"))
                })?;
            res.map_err(|e| ClientError::Vulkan(format!("Vulkan error creating instance: {e}")))?
        };

        let vk_physical_device_raw = unsafe {
            instance
                .vulkan_graphics_device(system, vk_instance_raw)
                .map_err(|e| {
                    ClientError::OpenXr(format!("Failed to get Vulkan graphics device: {e:?}"))
                })?
        };
        let vk_physical_device = vk::PhysicalDevice::from_raw(vk_physical_device_raw as _);

        let ash_instance = unsafe {
            ash::Instance::load(
                vk_entry.static_fn(),
                vk::Instance::from_raw(vk_instance_raw as _),
            )
        };

        let queue_family_index = unsafe {
            ash_instance
                .get_physical_device_queue_family_properties(vk_physical_device)
                .into_iter()
                .enumerate()
                .find_map(|(idx, info)| {
                    if info.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                        Some(idx as u32)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| ClientError::Vulkan("No graphics queue family found".into()))?
        };

        let queue_priorities = [1.0f32];
        let queue_create_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities);
        let queue_create_infos = [queue_create_info];

        let vk_device_create_info =
            vk::DeviceCreateInfo::default().queue_create_infos(&queue_create_infos);

        let vk_device_raw = unsafe {
            let res = instance
                .create_vulkan_device(
                    system,
                    std::mem::transmute(vk_entry.static_fn().get_instance_proc_addr),
                    vk_physical_device_raw,
                    &vk_device_create_info as *const _ as *const _,
                )
                .map_err(|e| {
                    ClientError::OpenXr(format!("XR error creating Vulkan device: {e:?}"))
                })?;
            res.map_err(|e| ClientError::Vulkan(format!("Vulkan error creating device: {e}")))?
        };

        // Initialize VulkanContext from OpenXR-created handles
        *vulkan_context = VulkanContext::new_from_openxr(
            vk::Instance::from_raw(vk_instance_raw as _),
            vk_physical_device,
            vk::Device::from_raw(vk_device_raw as _),
            queue_family_index,
            0,
        )?;

        // Create OpenXR Session
        let (session, frame_waiter, frame_stream) = unsafe {
            instance
                .create_session::<Vulkan>(
                    system,
                    &openxr::vulkan::SessionCreateInfo {
                        instance: vk_instance_raw,
                        physical_device: vk_physical_device_raw,
                        device: vk_device_raw,
                        queue_family_index,
                        queue_index: 0,
                    },
                )
                .map_err(|e| {
                    ClientError::OpenXr(format!("Failed to create OpenXR session: {e:?}"))
                })?
        };

        // Reference spaces: Stage (fall back to Local) and View
        let stage_space = session
            .create_reference_space(openxr::ReferenceSpaceType::STAGE, openxr::Posef::IDENTITY)
            .or_else(|_| {
                session.create_reference_space(
                    openxr::ReferenceSpaceType::LOCAL,
                    openxr::Posef::IDENTITY,
                )
            })
            .map_err(|e| ClientError::OpenXr(format!("Failed to create reference space: {e:?}")))?;

        let view_space = session
            .create_reference_space(openxr::ReferenceSpaceType::VIEW, openxr::Posef::IDENTITY)
            .map_err(|e| ClientError::OpenXr(format!("Failed to create view space: {e:?}")))?;

        // Refresh rate manager
        let refresh_rate_manager = if supports_refresh_rate {
            match session.enumerate_display_refresh_rates() {
                Ok(rates) if !rates.is_empty() => {
                    tracing::info!(rates = ?rates, "Enumerated OpenXR display refresh rates");
                    let mgr = RefreshRateManager::new(rates, config.target_refresh_rate);
                    let target = mgr.current_rate();
                    if let Err(e) = session.request_display_refresh_rate(target) {
                        tracing::warn!(error = ?e, rate = target, "Failed to request initial refresh rate");
                    }
                    Some(mgr)
                }
                _ => Some(RefreshRateManager::new(
                    vec![72.0, 90.0, 120.0],
                    config.target_refresh_rate,
                )),
            }
        } else {
            Some(RefreshRateManager::new(
                vec![72.0, 90.0, 120.0],
                config.target_refresh_rate,
            ))
        };

        // Create initial swapchains
        let desktop_info = SwapchainCreateInfo {
            create_flags: SwapchainCreateFlags::EMPTY,
            usage_flags: SwapchainUsageFlags::COLOR_ATTACHMENT | SwapchainUsageFlags::SAMPLED,
            format: vk::Format::R8G8B8A8_UNORM.as_raw() as u32,
            sample_count: 1,
            width: 1920,
            height: 1080,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        };
        let desktop_swapchain = session.create_swapchain(&desktop_info).ok();

        let hud_info = SwapchainCreateInfo {
            create_flags: SwapchainCreateFlags::EMPTY,
            usage_flags: SwapchainUsageFlags::COLOR_ATTACHMENT | SwapchainUsageFlags::SAMPLED,
            format: vk::Format::R8G8B8A8_UNORM.as_raw() as u32,
            sample_count: 1,
            width: 512,
            height: 256,
            face_count: 1,
            array_size: 1,
            mip_count: 1,
        };
        let hud_swapchain = session.create_swapchain(&hud_info).ok();

        Ok(Self {
            instance: Some(instance),
            system,
            session: Some(session),
            frame_waiter: Some(parking_lot::Mutex::new(frame_waiter)),
            frame_stream: Some(parking_lot::Mutex::new(frame_stream)),
            stage_space: Some(stage_space),
            view_space: Some(view_space),
            desktop_swapchain,
            hud_swapchain,
            refresh_rate_manager,
            session_state: SessionState::IDLE,
            session_running: Arc::new(AtomicBool::new(false)),
            supports_cylinder,
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
                    let new_rate = rate_event.to_display_refresh_rate();
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

    /// Returns the active desktop swapchain, if created.
    pub fn desktop_swapchain(&self) -> Option<&Swapchain<Vulkan>> {
        self.desktop_swapchain.as_ref()
    }

    /// Returns the active HUD swapchain, if created.
    pub fn hud_swapchain(&self) -> Option<&Swapchain<Vulkan>> {
        self.hud_swapchain.as_ref()
    }

    /// Whether the OpenXR runtime supports curved cylinder layers.
    pub fn supports_cylinder(&self) -> bool {
        self.supports_cylinder
    }

    /// Waits for the next frame presentation pacing slot from OpenXR.
    pub fn wait_frame(&self) -> ClientResult<FrameState> {
        if self.is_simulated {
            Ok(FrameState {
                predicted_display_time: Time::from_nanos(1),
                predicted_display_period: openxr::Duration::from_nanos(11_111_111),
                should_render: true,
            })
        } else if let Some(waiter) = &self.frame_waiter {
            waiter
                .lock()
                .wait()
                .map_err(|e| ClientError::OpenXr(format!("wait_frame failed: {e:?}")))
        } else {
            Err(ClientError::OpenXr("Frame waiter not initialized".into()))
        }
    }

    /// Signals the start of GPU work for the current frame.
    pub fn begin_frame(&self) -> ClientResult<()> {
        if self.is_simulated {
            Ok(())
        } else if let Some(stream) = &self.frame_stream {
            stream
                .lock()
                .begin()
                .map_err(|e| ClientError::OpenXr(format!("begin_frame failed: {e:?}")))
        } else {
            Err(ClientError::OpenXr("Frame stream not initialized".into()))
        }
    }

    /// Submits composed layers to the OpenXR compositor for timewarp display.
    pub fn end_frame(
        &self,
        display_time: Time,
        layers: &[&CompositionLayerBase<'_, Vulkan>],
    ) -> ClientResult<()> {
        if self.is_simulated {
            Ok(())
        } else if let Some(stream) = &self.frame_stream {
            stream
                .lock()
                .end(display_time, EnvironmentBlendMode::OPAQUE, layers)
                .map_err(|e| ClientError::OpenXr(format!("end_frame failed: {e:?}")))
        } else {
            Err(ClientError::OpenXr("Frame stream not initialized".into()))
        }
    }

    /// Submits an empty layer list when should_render is false.
    pub fn end_frame_empty(&self, display_time: Time) -> ClientResult<()> {
        self.end_frame(display_time, &[])
    }

    /// Composes and presents the desktop and optional HUD layers to OpenXR.
    pub fn render_and_present_layers(
        &mut self,
        display_time: Time,
        config: &ClientConfig,
    ) -> ClientResult<()> {
        if self.is_simulated {
            return Ok(());
        }

        if let (Some(space), Some(desktop_sc)) = (&self.stage_space, &mut self.desktop_swapchain) {
            let _ = desktop_sc.acquire_image();
            let _ = desktop_sc.wait_image(openxr::Duration::INFINITE);
            let _ = desktop_sc.release_image();

            let desktop_cfg = crate::openxr::layers::DesktopLayerConfig {
                mode: config.display_mode,
                quad_size: config.quad_size,
                distance: config.quad_distance,
                cylinder_radius: config.cylinder_radius,
                cylinder_central_angle: config.cylinder_central_angle,
                cylinder_aspect_ratio: config.cylinder_aspect_ratio,
            };

            let stream_mutex = self
                .frame_stream
                .as_ref()
                .ok_or_else(|| ClientError::OpenXr("Frame stream not initialized".into()))?;
            let mut stream = stream_mutex.lock();

            if self.supports_cylinder
                && config.display_mode == crate::config::DisplayMode::CurvedCylinder
            {
                let cyl_layer = crate::openxr::layers::build_cylinder_layer(
                    space,
                    desktop_sc,
                    1920,
                    1080,
                    &desktop_cfg,
                );
                if config.enable_hud {
                    if let (Some(view_space), Some(hud_sc)) =
                        (&self.view_space, &mut self.hud_swapchain)
                    {
                        let _ = hud_sc.acquire_image();
                        let _ = hud_sc.wait_image(openxr::Duration::INFINITE);
                        let _ = hud_sc.release_image();
                        let hud_layer =
                            crate::openxr::layers::build_hud_layer(view_space, hud_sc, 512, 256);
                        let _ = stream.end(
                            display_time,
                            EnvironmentBlendMode::OPAQUE,
                            &[&cyl_layer, &hud_layer],
                        );
                    } else {
                        let _ =
                            stream.end(display_time, EnvironmentBlendMode::OPAQUE, &[&cyl_layer]);
                    }
                } else {
                    let _ = stream.end(display_time, EnvironmentBlendMode::OPAQUE, &[&cyl_layer]);
                }
            } else {
                let quad_layer = crate::openxr::layers::build_quad_layer(
                    space,
                    desktop_sc,
                    1920,
                    1080,
                    &desktop_cfg,
                );
                if config.enable_hud {
                    if let (Some(view_space), Some(hud_sc)) =
                        (&self.view_space, &mut self.hud_swapchain)
                    {
                        let _ = hud_sc.acquire_image();
                        let _ = hud_sc.wait_image(openxr::Duration::INFINITE);
                        let _ = hud_sc.release_image();
                        let hud_layer =
                            crate::openxr::layers::build_hud_layer(view_space, hud_sc, 512, 256);
                        let _ = stream.end(
                            display_time,
                            EnvironmentBlendMode::OPAQUE,
                            &[&quad_layer, &hud_layer],
                        );
                    } else {
                        let _ =
                            stream.end(display_time, EnvironmentBlendMode::OPAQUE, &[&quad_layer]);
                    }
                } else {
                    let _ = stream.end(display_time, EnvironmentBlendMode::OPAQUE, &[&quad_layer]);
                }
            }
        } else if let Some(stream_mutex) = &self.frame_stream {
            let mut stream = stream_mutex.lock();
            let _ = stream.end(display_time, EnvironmentBlendMode::OPAQUE, &[]);
        }

        Ok(())
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
