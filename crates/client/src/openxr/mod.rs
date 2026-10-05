//! OpenXR runtime integration for Meta Quest headsets.

pub mod context;
pub mod frame_loop;
pub mod layers;
pub mod refresh_rate;

pub use context::OpenXrContext;
pub use frame_loop::{FrameLoopEngine, FramePacingMetrics};
pub use layers::{build_cylinder_layer, build_hud_layer, build_quad_layer, DesktopLayerConfig};
pub use refresh_rate::{RefreshRateManager, QUEST_REFRESH_RATES};
