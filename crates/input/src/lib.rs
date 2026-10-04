use linux_quest_protocol::InputEvent;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum InputError {
    #[error("Failed to open uinput device: {0}")]
    DeviceOpenFailed(String),

    #[error("Failed to emit event: {0}")]
    EmitFailed(String),

    #[error("Clipboard sync error: {0}")]
    ClipboardError(String),
}

pub trait InputInjector: Send + Sync {
    fn inject_event(&mut self, event: InputEvent) -> Result<(), InputError>;
}

/// In-memory mock injector for testing remote input processing.
#[derive(Default)]
pub struct MockInputInjector {
    pub received_events: Vec<InputEvent>,
}

impl MockInputInjector {
    pub fn new() -> Self {
        Self::default()
    }
}

impl InputInjector for MockInputInjector {
    fn inject_event(&mut self, event: InputEvent) -> Result<(), InputError> {
        self.received_events.push(event);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linux_quest_protocol::{ElementState, MouseButton};

    #[test]
    fn test_mock_input_injector() {
        let mut injector = MockInputInjector::new();
        let ev1 = InputEvent::MouseMoveRelative { dx: 10, dy: -5 };
        let ev2 = InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ElementState::Pressed,
        };

        injector.inject_event(ev1.clone()).unwrap();
        injector.inject_event(ev2.clone()).unwrap();

        assert_eq!(injector.received_events.len(), 2);
        assert_eq!(injector.received_events[0], ev1);
        assert_eq!(injector.received_events[1], ev2);
    }
}
