//! Swapchain texture management for OpenXR composition layers.

use ash::vk;

use crate::error::ClientResult;
use crate::graphics::vulkan::VulkanContext;

/// A texture image in an OpenXR swapchain.
#[derive(Debug, Clone)]
pub struct SwapchainTexture {
    /// Raw Vulkan image handle from OpenXR.
    pub image: vk::Image,
    /// Vulkan image view for sampling or attachment.
    pub image_view: vk::ImageView,
    /// Texture width in pixels.
    pub width: u32,
    /// Texture height in pixels.
    pub height: u32,
    /// Texture format.
    pub format: vk::Format,
}

impl SwapchainTexture {
    /// Wraps an OpenXR swapchain image with an image view.
    pub fn new(
        context: &VulkanContext,
        image: vk::Image,
        width: u32,
        height: u32,
        format: vk::Format,
    ) -> ClientResult<Self> {
        let image_view = context.create_image_view(image, format)?;
        Ok(Self {
            image,
            image_view,
            width,
            height,
            format,
        })
    }

    /// Destroys the image view associated with this swapchain texture.
    pub fn destroy(&self, context: &VulkanContext) {
        context.destroy_image_view(self.image_view);
    }
}
