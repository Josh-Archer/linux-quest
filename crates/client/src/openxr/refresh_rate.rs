//! Display refresh rate management for Meta Quest headsets (72Hz, 90Hz, 120Hz).

use crate::error::{ClientError, ClientResult};

/// Standard refresh rates supported by Meta Quest 2, 3, 3S, and Pro headsets.
pub const QUEST_REFRESH_RATES: [f32; 3] = [72.0, 90.0, 120.0];

/// Manages headset display refresh rates and frame timing budgets.
#[derive(Debug, Clone)]
pub struct RefreshRateManager {
    supported_rates: Vec<f32>,
    current_rate: f32,
    target_rate: f32,
}

impl RefreshRateManager {
    /// Creates a new refresh rate manager with initial rate settings.
    pub fn new(mut supported_rates: Vec<f32>, initial_rate: f32) -> Self {
        if supported_rates.is_empty() {
            supported_rates = QUEST_REFRESH_RATES.to_vec();
        }
        supported_rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let current_rate = if supported_rates.contains(&initial_rate) {
            initial_rate
        } else {
            supported_rates.first().copied().unwrap_or(90.0)
        };

        Self {
            supported_rates,
            current_rate,
            target_rate: current_rate,
        }
    }

    /// Returns the list of supported refresh rates reported by the headset.
    pub fn supported_rates(&self) -> &[f32] {
        &self.supported_rates
    }

    /// Returns the currently active display refresh rate in Hz.
    pub fn current_rate(&self) -> f32 {
        self.current_rate
    }

    /// Returns the target display refresh rate in Hz.
    pub fn target_rate(&self) -> f32 {
        self.target_rate
    }

    /// Returns the frame budget duration in microseconds for the current refresh rate.
    pub fn frame_budget_us(&self) -> u64 {
        if self.current_rate <= 0.0 {
            11_111 // Default 90Hz (11.1ms)
        } else {
            (1_000_000.0 / self.current_rate) as u64
        }
    }

    /// Selects the closest supported refresh rate to the requested target.
    pub fn find_closest_supported_rate(&self, requested: f32) -> f32 {
        if self.supported_rates.is_empty() {
            return requested;
        }

        self.supported_rates
            .iter()
            .copied()
            .min_by(|&a, &b| {
                let diff_a = (a - requested).abs();
                let diff_b = (b - requested).abs();
                diff_a
                    .partial_cmp(&diff_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(requested)
    }

    /// Requests a refresh rate switch via the OpenXR session.
    pub fn request_rate<G: openxr::Graphics>(
        &mut self,
        session: &openxr::Session<G>,
        target: f32,
    ) -> ClientResult<f32> {
        let best_rate = self.find_closest_supported_rate(target);
        self.target_rate = best_rate;

        // Attempt requesting rate change if extension is active
        match session.request_display_refresh_rate(best_rate) {
            Ok(()) => {
                tracing::info!(
                    requested = target,
                    selected = best_rate,
                    "Requested display refresh rate change"
                );
                self.current_rate = best_rate;
                Ok(best_rate)
            }
            Err(e) => {
                tracing::warn!(
                    requested = best_rate,
                    error = ?e,
                    "Display refresh rate request not supported or rejected by runtime"
                );
                Err(ClientError::OpenXr(format!(
                    "Failed to set refresh rate {best_rate}Hz: {e:?}"
                )))
            }
        }
    }

    /// Updates internal state when the OpenXR runtime notifies of a rate change.
    pub fn on_refresh_rate_changed(&mut self, new_rate: f32) {
        tracing::info!(new_rate, "Headset display refresh rate changed");
        self.current_rate = new_rate;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_refresh_rate_budget_calculation() {
        let mgr = RefreshRateManager::new(vec![72.0, 90.0, 120.0], 90.0);
        assert_eq!(mgr.current_rate(), 90.0);
        // 1_000_000 / 90 = 11111 us
        assert_eq!(mgr.frame_budget_us(), 11_111);

        let mgr_120 = RefreshRateManager::new(vec![72.0, 90.0, 120.0], 120.0);
        assert_eq!(mgr_120.frame_budget_us(), 8_333);

        let mgr_72 = RefreshRateManager::new(vec![72.0, 90.0, 120.0], 72.0);
        assert_eq!(mgr_72.frame_budget_us(), 13_888);
    }

    #[test]
    fn test_find_closest_supported_rate() {
        let mgr = RefreshRateManager::new(vec![72.0, 90.0, 120.0], 90.0);
        assert_eq!(mgr.find_closest_supported_rate(88.0), 90.0);
        assert_eq!(mgr.find_closest_supported_rate(75.0), 72.0);
        assert_eq!(mgr.find_closest_supported_rate(144.0), 120.0);
    }
}
