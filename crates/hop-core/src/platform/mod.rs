use anyhow::Result;
use hop_protocol::datagram::InputEvent;

use crate::layout::{CursorPosition, ScreenSize};

pub trait LocalInputCapture: Send {
    fn poll_cursor_position(&mut self) -> Result<Option<CursorPosition>>;
    fn poll_input_events(&mut self) -> Result<Vec<InputEvent>>;
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
