use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum MouseButton {
    Left = 1,
    Right = 2,
    Middle = 3,
    Back = 4,
    Forward = 5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ElementState {
    Pressed = 1,
    Released = 2,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    MouseMoveRelative {
        dx: i32,
        dy: i32,
    },
    MouseMoveAbsolute {
        x: u32,
        y: u32,
        display_id: u16,
    },
    MouseButton {
        button: MouseButton,
        state: ElementState,
    },
    MouseScroll {
        delta_x: f32,
        delta_y: f32,
    },
    KeyboardKey {
        keycode: u32,
        state: ElementState,
    },
    ClipboardSync {
        mime_type: String,
        content: Vec<u8>,
    },
}
