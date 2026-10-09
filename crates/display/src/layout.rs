use serde::{Deserialize, Serialize};

use crate::config::VirtualMonitorConfig;

/// Layout arrangement mode for multi-monitor desktop workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayLayoutMode {
    /// Monitors arranged horizontally side-by-side from left to right.
    Horizontal,

    /// Monitors stacked vertically from top to bottom.
    Vertical,

    /// 2x2 grid arrangement for 3 or 4 displays.
    Grid,
}

/// A positioned monitor in global desktop coordinate space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacedMonitor {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub dpi: u32,
}

/// Computes multi-monitor positioning and spatial arrangement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayLayout {
    pub mode: DisplayLayoutMode,
    pub monitors: Vec<VirtualMonitorConfig>,
}

impl DisplayLayout {
    pub fn new(mode: DisplayLayoutMode, monitors: Vec<VirtualMonitorConfig>) -> Self {
        Self { mode, monitors }
    }

    /// Computes discrete X and Y desktop offsets for every active monitor.
    pub fn compute_placements(&self) -> Vec<PlacedMonitor> {
        let mut placements = Vec::new();
        let active_monitors: Vec<_> = self.monitors.iter().filter(|m| m.enabled).collect();

        match self.mode {
            DisplayLayoutMode::Horizontal => {
                let mut current_x = 0;
                for m in active_monitors {
                    placements.push(PlacedMonitor {
                        id: m.id,
                        name: m.name.clone(),
                        x: current_x,
                        y: 0,
                        width: m.width,
                        height: m.height,
                        refresh_rate: m.refresh_rate,
                        dpi: m.dpi,
                    });
                    current_x += m.width as i32;
                }
            }
            DisplayLayoutMode::Vertical => {
                let mut current_y = 0;
                for m in active_monitors {
                    placements.push(PlacedMonitor {
                        id: m.id,
                        name: m.name.clone(),
                        x: 0,
                        y: current_y,
                        width: m.width,
                        height: m.height,
                        refresh_rate: m.refresh_rate,
                        dpi: m.dpi,
                    });
                    current_y += m.height as i32;
                }
            }
            DisplayLayoutMode::Grid => {
                // Arranges up to 4 monitors in 2 columns:
                // [0] [1]
                // [2] [3]
                let mut row_max_height = 0;
                let mut current_x = 0;
                let mut current_y = 0;

                for (idx, m) in active_monitors.iter().enumerate() {
                    if idx > 0 && idx % 2 == 0 {
                        current_x = 0;
                        current_y += row_max_height;
                        row_max_height = 0;
                    }

                    placements.push(PlacedMonitor {
                        id: m.id,
                        name: m.name.clone(),
                        x: current_x,
                        y: current_y,
                        width: m.width,
                        height: m.height,
                        refresh_rate: m.refresh_rate,
                        dpi: m.dpi,
                    });

                    current_x += m.width as i32;
                    row_max_height = row_max_height.max(m.height as i32);
                }
            }
        }

        placements
    }

    /// Total bounding rectangle dimensions covering all placed monitors.
    pub fn bounding_box(&self) -> (u32, u32) {
        let placements = self.compute_placements();
        if placements.is_empty() {
            return (0, 0);
        }

        let max_x = placements
            .iter()
            .map(|p| p.x + p.width as i32)
            .max()
            .unwrap_or(0);
        let max_y = placements
            .iter()
            .map(|p| p.y + p.height as i32)
            .max()
            .unwrap_or(0);

        (max_x.max(0) as u32, max_y.max(0) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_horizontal_layout_computation() {
        let m1 = VirtualMonitorConfig::preset_1080p(1, 60);
        let m2 = VirtualMonitorConfig::preset_1080p(2, 90);
        let m3 = VirtualMonitorConfig::preset_1440p(3, 120);

        let layout = DisplayLayout::new(DisplayLayoutMode::Horizontal, vec![m1, m2, m3]);
        let placements = layout.compute_placements();

        assert_eq!(placements.len(), 3);
        assert_eq!(placements[0].x, 0);
        assert_eq!(placements[0].y, 0);
        assert_eq!(placements[1].x, 1920);
        assert_eq!(placements[1].y, 0);
        assert_eq!(placements[2].x, 3840);
        assert_eq!(placements[2].y, 0);

        let (total_w, total_h) = layout.bounding_box();
        assert_eq!(total_w, 1920 + 1920 + 2560);
        assert_eq!(total_h, 1440);
    }

    #[test]
    fn test_vertical_layout_computation() {
        let m1 = VirtualMonitorConfig::preset_1080p(1, 60);
        let m2 = VirtualMonitorConfig::preset_1080p(2, 90);

        let layout = DisplayLayout::new(DisplayLayoutMode::Vertical, vec![m1, m2]);
        let placements = layout.compute_placements();

        assert_eq!(placements.len(), 2);
        assert_eq!(placements[0].x, 0);
        assert_eq!(placements[0].y, 0);
        assert_eq!(placements[1].x, 0);
        assert_eq!(placements[1].y, 1080);

        let (total_w, total_h) = layout.bounding_box();
        assert_eq!(total_w, 1920);
        assert_eq!(total_h, 2160);
    }

    #[test]
    fn test_grid_layout_computation() {
        let m1 = VirtualMonitorConfig::preset_1080p(1, 60);
        let m2 = VirtualMonitorConfig::preset_1080p(2, 60);
        let m3 = VirtualMonitorConfig::preset_1080p(3, 60);

        let layout = DisplayLayout::new(DisplayLayoutMode::Grid, vec![m1, m2, m3]);
        let placements = layout.compute_placements();

        assert_eq!(placements.len(), 3);
        assert_eq!(placements[0].x, 0);
        assert_eq!(placements[0].y, 0);
        assert_eq!(placements[1].x, 1920);
        assert_eq!(placements[1].y, 0);
        assert_eq!(placements[2].x, 0);
        assert_eq!(placements[2].y, 1080);
    }
}
