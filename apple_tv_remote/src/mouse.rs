//! uinput sink for the touchpad state machine.

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, InputEvent, KeyCode, RelativeAxisCode};

use crate::touch::{Button, InputSink, Output, RemoteKey};

pub const DEVICE_NAME: &str = "Apple TV Remote Touch Mouse";

impl RemoteKey {
    pub fn code(self) -> KeyCode {
        match self {
            RemoteKey::PlayPause => KeyCode::KEY_ENTER,
            RemoteKey::Menu => KeyCode::KEY_1,
        }
    }
}

pub struct VirtualMouse {
    device: VirtualDevice,
}

impl VirtualMouse {
    pub fn create() -> Result<Self> {
        let mut keys = AttributeSet::new();
        keys.insert(KeyCode::BTN_LEFT);
        keys.insert(KeyCode::BTN_RIGHT);
        keys.insert(RemoteKey::PlayPause.code());
        keys.insert(RemoteKey::Menu.code());
        let mut axes = AttributeSet::new();
        axes.insert(RelativeAxisCode::REL_X);
        axes.insert(RelativeAxisCode::REL_Y);
        axes.insert(RelativeAxisCode::REL_WHEEL);
        axes.insert(RelativeAxisCode::REL_HWHEEL);
        let device = VirtualDevice::builder()
            .context("open /dev/uinput; is uinput enabled for this user?")?
            .name(DEVICE_NAME)
            .with_keys(&keys)?
            .with_relative_axes(&axes)?
            .build()
            .context("create uinput device")?;
        Ok(Self { device })
    }
}

fn relative(code: RelativeAxisCode, value: i32) -> InputEvent {
    InputEvent::new(evdev::EventType::RELATIVE.0, code.0, value)
}

fn key(code: KeyCode, down: bool) -> InputEvent {
    InputEvent::new(evdev::EventType::KEY.0, code.0, i32::from(down))
}

impl InputSink for VirtualMouse {
    fn emit(&mut self, output: Output) {
        let mut events: Vec<InputEvent> = Vec::with_capacity(2);
        match output {
            Output::Motion { dx, dy } => {
                if dx != 0 {
                    events.push(relative(RelativeAxisCode::REL_X, dx));
                }
                if dy != 0 {
                    events.push(relative(RelativeAxisCode::REL_Y, dy));
                }
            }
            Output::Scroll {
                horizontal,
                vertical,
            } => {
                if horizontal != 0 {
                    events.push(relative(RelativeAxisCode::REL_HWHEEL, horizontal));
                }
                if vertical != 0 {
                    events.push(relative(RelativeAxisCode::REL_WHEEL, vertical));
                }
            }
            Output::Key { button, down } => {
                let code = match button {
                    Button::Left => KeyCode::BTN_LEFT,
                    Button::Right => KeyCode::BTN_RIGHT,
                };
                events.push(key(code, down));
            }
            Output::RemoteKey { key: remote, down } => {
                events.push(key(remote.code(), down));
            }
        }
        // `emit` appends the SYN_REPORT that terminates the batch.
        if !events.is_empty() {
            if let Err(error) = self.device.emit(&events) {
                tracing::warn!(%error, "uinput write failed");
            }
        }
    }
}
