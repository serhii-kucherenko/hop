use anyhow::Result;
use sideshift_protocol::control::Edge;
use sideshift_protocol::datagram::InputEvent;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use crate::layout::{CursorPosition, ScreenSize};
use crate::platform::{
    CursorController, LocalInputCapture, PlatformAdapters, RemoteInputInjector, ScreenInfoProvider,
};

pub fn build_platform_adapters() -> PlatformAdapters {
    PlatformAdapters {
        input_capture: Box::new(WindowsInputCapture),
        input_injector: Box::new(WindowsInputInjector),
        screen_provider: Box::new(WindowsScreenProvider),
        cursor_controller: Box::new(WindowsCursorController),
    }
}

#[derive(Debug)]
struct WindowsInputCapture;

impl LocalInputCapture for WindowsInputCapture {
    fn poll_cursor_position(&mut self) -> Result<Option<CursorPosition>> {
        Ok(None)
    }

    fn poll_input_events(&mut self) -> Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }
}

#[derive(Debug)]
struct WindowsInputInjector;

impl RemoteInputInjector for WindowsInputInjector {
    fn inject_event(&mut self, _event: &InputEvent) -> Result<()> {
        // TODO: map InputEvent to INPUT and call SendInput.
        Ok(())
    }
}

#[derive(Debug)]
struct WindowsScreenProvider;

impl ScreenInfoProvider for WindowsScreenProvider {
    fn screen_size(&self) -> Result<ScreenSize> {
        let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        Ok(ScreenSize {
            width: width as u32,
            height: height as u32,
        })
    }
}

#[derive(Debug)]
struct WindowsCursorController;

impl CursorController for WindowsCursorController {
    fn hide_cursor(&mut self) -> Result<()> {
        // TODO: call ShowCursor(FALSE) while tracking visibility refcount.
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
        // TODO: call ShowCursor(TRUE) while tracking visibility refcount.
        Ok(())
    }

    fn warp_cursor_to_safe_point(&mut self, edge: Edge, screen: ScreenSize) -> Result<()> {
        let _target = match edge {
            Edge::Left => CursorPosition {
                x: (screen.width as i32) - 2,
                y: (screen.height as i32) / 2,
            },
            Edge::Right => CursorPosition {
                x: 1,
                y: (screen.height as i32) / 2,
            },
            Edge::Top => CursorPosition {
                x: (screen.width as i32) / 2,
                y: (screen.height as i32) - 2,
            },
            Edge::Bottom => CursorPosition {
                x: (screen.width as i32) / 2,
                y: 1,
            },
        };
        // TODO: call SetCursorPos.
        Ok(())
    }
}
