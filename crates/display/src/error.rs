use thiserror::Error;

/// Errors arising from virtual monitor management, display servers, or EDID generation.
#[derive(Error, Debug)]
pub enum DisplayError {
    #[error("D-Bus failure: {0}")]
    Dbus(String),

    #[error("Wayland protocol failure: {0}")]
    Wayland(String),

    #[error("X11/xrandr failure: {0}")]
    X11(String),

    #[error("Invalid display configuration: {0}")]
    InvalidConfiguration(String),

    #[error("Output not found: {0}")]
    OutputNotFound(String),

    #[error("Maximum virtual monitors exceeded (limit {0})")]
    MaxMonitorsExceeded(u32),

    #[error("Backend unavailable: {0}")]
    BackendUnavailable(String),

    #[error("Command execution failed: {0}")]
    CommandFailed(String),

    #[error("EDID generation error: {0}")]
    Edid(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
