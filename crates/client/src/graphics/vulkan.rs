//! Vulkan graphics device and resource management for OpenXR.

use ash::vk;
use ash::vk::Handle;

use crate::error::{ClientError, ClientResult};

/// Parameters describing an image layout transition barrier.
#[derive(Debug, Clone, Copy)]
pub struct ImageLayoutTransition {
    pub old_layout: vk::ImageLayout,
    pub new_layout: vk::ImageLayout,
    pub src_access: vk::AccessFlags,
    pub dst_access: vk::AccessFlags,
    pub src_stage: vk::PipelineStageFlags,
    pub dst_stage: vk::PipelineStageFlags,
}

/// Vulkan instance, physical device, and logical device wrapper.
pub struct VulkanContext {
    entry: Option<ash::Entry>,
    instance: Option<ash::Instance>,
    physical_device: vk::PhysicalDevice,
    device: Option<ash::Device>,
    graphics_queue: vk::Queue,
    queue_family_index: u32,
    command_pool: vk::CommandPool,
    owns_device: bool,
    owns_instance: bool,
    is_simulated: bool,
}

// Vulkan handles can be transferred across thread boundaries (Send).
// Queue and command pool require external synchronization, so Sync is not implemented.
unsafe impl Send for VulkanContext {}

impl VulkanContext {
    /// Creates a simulated Vulkan context for testing or headless CI environments.
    pub fn new_simulated() -> Self {
        Self {
            entry: None,
            instance: None,
            physical_device: vk::PhysicalDevice::null(),
            device: None,
            graphics_queue: vk::Queue::null(),
            queue_family_index: 0,
            command_pool: vk::CommandPool::null(),
            owns_device: false,
            owns_instance: false,
            is_simulated: true,
        }
    }

    /// Initializes a live Vulkan context using OpenXR physical device parameters.
    pub fn new_from_openxr(
        raw_instance: vk::Instance,
        physical_device: vk::PhysicalDevice,
        raw_device: vk::Device,
        queue_family_index: u32,
        queue_index: u32,
    ) -> ClientResult<Self> {
        let entry = unsafe {
            ash::Entry::load().map_err(|e| {
                ClientError::Vulkan(format!("Failed to load Vulkan entry point: {e:?}"))
            })?
        };

        let instance = unsafe { ash::Instance::load(entry.static_fn(), raw_instance) };
        let device = unsafe { ash::Device::load(instance.fp_v1_0(), raw_device) };
        let graphics_queue = unsafe { device.get_device_queue(queue_family_index, queue_index) };

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);

        let command_pool = unsafe {
            device
                .create_command_pool(&pool_info, None)
                .map_err(|e| ClientError::Vulkan(format!("Failed to create command pool: {e:?}")))?
        };

        Ok(Self {
            entry: Some(entry),
            instance: Some(instance),
            physical_device,
            device: Some(device),
            graphics_queue,
            queue_family_index,
            command_pool,
            owns_device: true,
            owns_instance: true,
            is_simulated: false,
        })
    }

    /// Whether this context is running in simulation mode.
    pub fn is_simulated(&self) -> bool {
        self.is_simulated
    }

    /// Returns the raw Vulkan physical device handle.
    pub fn physical_device(&self) -> vk::PhysicalDevice {
        self.physical_device
    }

    /// Returns the graphics queue family index.
    pub fn queue_family_index(&self) -> u32 {
        self.queue_family_index
    }

    /// Returns the graphics queue handle.
    pub fn graphics_queue(&self) -> vk::Queue {
        self.graphics_queue
    }

    /// Returns the raw Vulkan instance pointer for OpenXR SessionCreateInfo.
    pub fn raw_instance_ptr(&self) -> *const std::ffi::c_void {
        if let Some(instance) = &self.instance {
            instance.handle().as_raw() as *const _
        } else {
            std::ptr::null()
        }
    }

    /// Returns the raw Vulkan physical device pointer for OpenXR SessionCreateInfo.
    pub fn raw_physical_device_ptr(&self) -> *const std::ffi::c_void {
        self.physical_device.as_raw() as *const _
    }

    /// Returns the raw Vulkan logical device pointer for OpenXR SessionCreateInfo.
    pub fn raw_device_ptr(&self) -> *const std::ffi::c_void {
        if let Some(device) = &self.device {
            device.handle().as_raw() as *const _
        } else {
            std::ptr::null()
        }
    }

    /// Returns a reference to the logical device, if loaded.
    pub fn device(&self) -> Option<&ash::Device> {
        self.device.as_ref()
    }

    /// Returns a reference to the Vulkan instance, if loaded.
    pub fn instance(&self) -> Option<&ash::Instance> {
        self.instance.as_ref()
    }

    /// Returns a reference to the Vulkan entry, if loaded.
    pub fn entry(&self) -> Option<&ash::Entry> {
        self.entry.as_ref()
    }

    /// Creates an image view for an OpenXR swapchain image.
    pub fn create_image_view(
        &self,
        image: vk::Image,
        format: vk::Format,
    ) -> ClientResult<vk::ImageView> {
        if self.is_simulated {
            return Ok(vk::ImageView::from_raw(image.as_raw()));
        }

        let device = self
            .device
            .as_ref()
            .ok_or_else(|| ClientError::Vulkan("Device not loaded".to_string()))?;

        let subresource_range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .base_mip_level(0)
            .level_count(1)
            .base_array_layer(0)
            .layer_count(1);

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(subresource_range);

        unsafe {
            device
                .create_image_view(&view_info, None)
                .map_err(|e| ClientError::Vulkan(format!("Failed to create image view: {e:?}")))
        }
    }

    /// Destroys a Vulkan image view.
    pub fn destroy_image_view(&self, view: vk::ImageView) {
        if !self.is_simulated {
            if let Some(device) = &self.device {
                unsafe {
                    device.destroy_image_view(view, None);
                }
            }
        }
    }

    /// Executes a pipeline barrier on a command buffer to transition image layouts.
    pub fn transition_image_layout(
        &self,
        command_buffer: vk::CommandBuffer,
        image: vk::Image,
        transition: &ImageLayoutTransition,
    ) {
        if self.is_simulated {
            return;
        }

        if let Some(device) = &self.device {
            let subresource_range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .base_mip_level(0)
                .level_count(1)
                .base_array_layer(0)
                .layer_count(1);

            let barrier = vk::ImageMemoryBarrier::default()
                .old_layout(transition.old_layout)
                .new_layout(transition.new_layout)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(subresource_range)
                .src_access_mask(transition.src_access)
                .dst_access_mask(transition.dst_access);

            unsafe {
                device.cmd_pipeline_barrier(
                    command_buffer,
                    transition.src_stage,
                    transition.dst_stage,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier],
                );
            }
        }
    }
}

impl Drop for VulkanContext {
    fn drop(&mut self) {
        if !self.is_simulated {
            if let Some(device) = self.device.take() {
                unsafe {
                    let _ = device.device_wait_idle();
                    if self.command_pool != vk::CommandPool::null() {
                        device.destroy_command_pool(self.command_pool, None);
                    }
                    if self.owns_device {
                        device.destroy_device(None);
                    }
                }
            }
            if let Some(instance) = self.instance.take() {
                unsafe {
                    if self.owns_instance {
                        instance.destroy_instance(None);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simulated_vulkan_context() {
        let ctx = VulkanContext::new_simulated();
        assert!(ctx.is_simulated());
        assert_eq!(ctx.queue_family_index(), 0);

        let dummy_img = vk::Image::from_raw(12345);
        let view = ctx
            .create_image_view(dummy_img, vk::Format::R8G8B8A8_UNORM)
            .unwrap();
        assert_eq!(view.as_raw(), 12345);
        ctx.destroy_image_view(view);
    }
}
