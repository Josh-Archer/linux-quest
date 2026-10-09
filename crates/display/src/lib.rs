//! Virtual multi-monitor management and dynamic EDID generation for Linux.

pub mod backend;
pub mod config;
pub mod edid;
pub mod error;
pub mod layout;
pub mod manager;
pub mod timing;

pub use backend::{VirtualDisplayBackend, VirtualMonitor};
pub use config::VirtualMonitorConfig;
pub use edid::EdidGenerator;
pub use error::DisplayError;
pub use layout::{DisplayLayout, DisplayLayoutMode, PlacedMonitor};
pub use manager::VirtualDisplayManager;
pub use timing::CvtTiming;

/// Returns the private runtime directory for Linux Quest display descriptors and EDIDs.
pub fn display_runtime_dir() -> std::path::PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        std::path::Path::new(&runtime_dir).join("linux-quest")
    } else {
        let uid = unsafe { libc::getuid() };
        std::path::PathBuf::from(format!("/tmp/linux-quest-{uid}"))
    }
}

/// Ensures the private runtime directory exists with strict 0700 permissions.
pub fn ensure_display_runtime_dir() -> Result<std::path::PathBuf, DisplayError> {
    let dir = display_runtime_dir();
    std::fs::create_dir_all(&dir).map_err(|e| {
        DisplayError::CommandFailed(format!(
            "Failed to create display runtime directory {dir:?}: {e}"
        ))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            DisplayError::CommandFailed(format!(
                "Failed to set 0700 permissions on display runtime directory {dir:?}: {e}"
            ))
        })?;
    }

    Ok(dir)
}
