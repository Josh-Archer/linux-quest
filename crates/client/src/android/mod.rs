//! Android NativeActivity integration and main entry point.

#[cfg(target_os = "android")]
use std::time::Duration;

#[cfg(target_os = "android")]
use android_activity::{AndroidApp, MainEvent, PollEvent};

use crate::config::ClientConfig;
use crate::runtime::QuestClientRuntime;

/// Native entry point for Android NativeActivity on Meta Quest.
#[cfg(target_os = "android")]
#[no_mangle]
pub fn android_main(app: AndroidApp) {
    // Initialize tracing logging for Android logcat
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    tracing::info!("linux-quest Quest client starting up on Android / Horizon OS");

    let config = ClientConfig::default();
    let mut runtime = match QuestClientRuntime::new(config) {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = ?e, "Failed to initialize Quest client runtime");
            return;
        }
    };

    let mut is_active = false;

    // Main Android event and render loop
    while runtime.is_running() {
        app.poll_events(
            Some(if is_active {
                Duration::ZERO
            } else {
                Duration::from_millis(100)
            }),
            |event| {
                if let PollEvent::Main(main_event) = event {
                    match main_event {
                        MainEvent::Resume { .. } => {
                            tracing::info!("Android activity resumed");
                            is_active = true;
                        }
                        MainEvent::Pause => {
                            tracing::info!("Android activity paused");
                            is_active = false;
                        }
                        MainEvent::Destroy => {
                            tracing::info!("Android activity destroying");
                            runtime.stop();
                        }
                        _ => {}
                    }
                }
            },
        );

        if is_active {
            if let Err(e) = runtime.step_frame() {
                tracing::warn!(error = ?e, "Error stepping client frame");
            }
        }
    }

    tracing::info!("linux-quest client runtime exited cleanly");
}

/// Simulated Android entry helper for cross-platform unit tests.
#[cfg(not(target_os = "android"))]
pub fn simulated_android_entry(
    config: ClientConfig,
) -> crate::error::ClientResult<QuestClientRuntime> {
    QuestClientRuntime::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simulated_android_entry() {
        let config = ClientConfig::default();
        #[cfg(not(target_os = "android"))]
        {
            let runtime = simulated_android_entry(config).unwrap();
            assert!(runtime.is_running());
        }
    }
}
