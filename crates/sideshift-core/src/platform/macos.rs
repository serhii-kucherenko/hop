use anyhow::Result;
use core_graphics::display::CGDisplay;
use sideshift_protocol::control::Edge;
use sideshift_protocol::datagram::InputEvent;

use crate::layout::{CursorPosition, ScreenSize};
use crate::platform::{
    CursorController, LocalInputCapture, PlatformAdapters, RemoteInputInjector, ScreenInfoProvider,
};

pub fn build_platform_adapters() -> PlatformAdapters {
    PlatformAdapters {
        input_capture: Box::new(MacosInputCapture),
        input_injector: Box::new(MacosInputInjector),
        screen_provider: Box::new(MacosScreenProvider),
        cursor_controller: Box::new(MacosCursorController),
    }
}

#[derive(Debug)]
struct MacosInputCapture;

impl LocalInputCapture for MacosInputCapture {
    fn poll_cursor_position(&mut self) -> Result<Option<CursorPosition>> {
        Ok(None)
    }

    fn poll_input_events(&mut self) -> Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }
}

#[derive(Debug)]
struct MacosInputInjector;

impl RemoteInputInjector for MacosInputInjector {
    fn inject_event(&mut self, _event: &InputEvent) -> Result<()> {
        // TODO: use CGEventCreateKeyboardEvent / CGEventPost for keyboard and pointer events.
        Ok(())
    }
}

#[derive(Debug)]
struct MacosScreenProvider;

impl ScreenInfoProvider for MacosScreenProvider {
    fn screen_size(&self) -> Result<ScreenSize> {
        let bounds = CGDisplay::main().bounds();
        Ok(ScreenSize {
            width: bounds.size.width as u32,
            height: bounds.size.height as u32,
        })
    }
}

#[derive(Debug)]
struct MacosCursorController;

impl CursorController for MacosCursorController {
    fn hide_cursor(&mut self) -> Result<()> {
        // TODO: use CGDisplayHideCursor.
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
        // TODO: use CGDisplayShowCursor.
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
        // TODO: use CGWarpMouseCursorPosition.
        Ok(())
    }
}
