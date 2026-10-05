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

    // Initialize Android context for OpenXR and MediaCodec
    unsafe {
        ndk_context::initialize_android_context(app.vm_as_ptr(), app.activity_as_ptr());
    }

    let config = ClientConfig::default();
    let mut runtime = match QuestClientRuntime::new(config) {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = ?e, "Failed to initialize Quest client runtime");
            unsafe {
                ndk_context::release_android_context();
            }
            return;
        }
    };

    // Background transport packet receiver
    let (packet_tx, packet_rx) = std::sync::mpsc::channel();
    let running_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let bg_running = running_flag.clone();

    let _receiver_handle = std::thread::spawn(move || {
        let socket = match std::net::UdpSocket::bind("0.0.0.0:48440") {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = ?e, "Failed to bind UDP socket on 0.0.0.0:48440");
                return;
            }
        };
        let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
        let mut buf = [0u8; 65536];

        while bg_running.load(std::sync::atomic::Ordering::Relaxed) {
            match socket.recv_from(&mut buf) {
                Ok((len, _src)) => {
                    match linux_quest_protocol::packet::Packet::from_bytes(&buf[..len]) {
                        Ok(packet) => {
                            let _ = packet_tx.send(packet);
                        }
                        Err(e) => {
                            tracing::trace!(error = ?e, "Failed to parse incoming packet");
                        }
                    }
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    tracing::trace!(error = ?e, "UDP socket recv error");
                }
            }
        }
    });

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

        // Ingest any received transport packets
        while let Ok(pkt) = packet_rx.try_recv() {
            runtime.ingest_packet(pkt, std::time::Instant::now());
        }

        if is_active {
            if let Err(e) = runtime.step_frame() {
                tracing::warn!(error = ?e, "Error stepping client frame");
            }
        }
    }

    running_flag.store(false, std::sync::atomic::Ordering::Relaxed);

    unsafe {
        ndk_context::release_android_context();
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
