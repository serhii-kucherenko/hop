use anyhow::Result;
use hop_protocol::datagram::InputEvent;

use crate::layout::{CursorPosition, ScreenSize};

pub trait LocalInputCapture: Send {
    fn poll_cursor_position(&mut self) -> Result<Option<CursorPosition>>;
    fn poll_input_events(&mut self) -> Result<Vec<InputEvent>>;
    fn set_remote_focus(&mut self, _active: bool) -> Result<()> {
        Ok(())
    }
}

pub trait RemoteInputInjector: Send {
    fn inject_event(&mut self, event: &InputEvent) -> Result<()>;
}

pub trait ScreenInfoProvider: Send + Sync {
    fn screen_size(&self) -> Result<ScreenSize>;
}

pub trait CursorController: Send {
    fn hide_cursor(&mut self) -> Result<()>;
    fn show_cursor(&mut self) -> Result<()>;
    fn warp_cursor_to_safe_point(
        &mut self,
        edge: hop_protocol::control::Edge,
        screen: ScreenSize,
    ) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStatus {
    Granted,
    Missing,
    Unknown,
}

pub struct PlatformAdapters {
    pub input_capture: Box<dyn LocalInputCapture>,
    pub input_injector: Box<dyn RemoteInputInjector>,
    pub screen_provider: Box<dyn ScreenInfoProvider>,
    pub cursor_controller: Box<dyn CursorController>,
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn mouse_move_event_from_points(
    start: CursorPosition,
    end: CursorPosition,
) -> Option<InputEvent> {
    mouse_move_event_from_delta(end.x - start.x, end.y - start.y)
}

#[cfg(any(target_os = "windows", test))]
fn mouse_move_event_from_delta(dx: i32, dy: i32) -> Option<InputEvent> {
    let dx = saturating_i32_to_i16(dx);
    let dy = saturating_i32_to_i16(dy);
    (dx != 0 || dy != 0).then_some(InputEvent::MouseMove { dx, dy })
}

#[cfg(any(target_os = "windows", test))]
fn saturating_i32_to_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

#[cfg(any(target_os = "macos", target_os = "windows", test))]
pub(crate) mod keycodes;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{build_platform_adapters, permission_status};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{build_platform_adapters, permission_status};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::{build_platform_adapters, permission_status};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_move_from_points_uses_direct_difference() {
        let start = CursorPosition { x: 100, y: 200 };
        let end = CursorPosition { x: 112, y: 195 };
        assert_eq!(
            mouse_move_event_from_points(start, end),
            Some(InputEvent::MouseMove { dx: 12, dy: -5 })
        );
    }

    #[test]
    fn mouse_move_from_points_saturates_i16_range() {
        let start = CursorPosition { x: 0, y: 0 };
        let end = CursorPosition {
            x: i32::MAX,
            y: i32::MIN,
        };
        assert_eq!(
            mouse_move_event_from_points(start, end),
            Some(InputEvent::MouseMove {
                dx: i16::MAX,
                dy: i16::MIN
            })
        );
    }

    #[test]
    fn remote_delta_math_stays_stable_when_cursor_is_frozen() {
        let frozen_cursor = CursorPosition { x: 800, y: 500 };
        let would_be = CursorPosition { x: 812, y: 492 };

        let first_event = mouse_move_event_from_points(frozen_cursor, would_be);
        let second_event = mouse_move_event_from_points(frozen_cursor, would_be);

        assert_eq!(first_event, Some(InputEvent::MouseMove { dx: 12, dy: -8 }));
        assert_eq!(second_event, first_event);
    }
}
