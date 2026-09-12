use std::collections::VecDeque;
use std::env;

use anyhow::Result;
use hop_protocol::control::Edge;
use hop_protocol::datagram::InputEvent;

use crate::layout::{CursorPosition, ScreenSize};
use crate::platform::{
    CursorController, LocalInputCapture, PermissionStatus, PlatformAdapters, RemoteInputInjector,
    ScreenInfoProvider,
};

const DEFAULT_SCREEN_WIDTH: u32 = 1920;
const DEFAULT_SCREEN_HEIGHT: u32 = 1080;

pub fn build_platform_adapters() -> PlatformAdapters {
    PlatformAdapters {
        input_capture: Box::new(LinuxMockInputCapture::from_environment()),
        input_injector: Box::new(LinuxMockInputInjector),
        screen_provider: Box::new(LinuxMockScreenProvider),
        cursor_controller: Box::new(LinuxMockCursorController),
    }
}

pub fn permission_status() -> PermissionStatus {
    PermissionStatus::Unknown
}

#[derive(Debug)]
struct LinuxMockInputCapture {
    scripted_positions: VecDeque<CursorPosition>,
}

impl LinuxMockInputCapture {
    fn from_environment() -> Self {
        let scripted_positions = env::var("HOP_MOCK_CURSOR_PATH")
            .ok()
            .and_then(|raw| parse_cursor_path(&raw))
            .unwrap_or_default();
        Self { scripted_positions }
    }
}

impl LocalInputCapture for LinuxMockInputCapture {
    fn poll_cursor_position(&mut self) -> Result<Option<CursorPosition>> {
        Ok(self.scripted_positions.pop_front())
    }

    fn poll_input_events(&mut self) -> Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }
}

#[derive(Debug)]
struct LinuxMockInputInjector;

impl RemoteInputInjector for LinuxMockInputInjector {
    fn inject_event(&mut self, _event: &InputEvent) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct LinuxMockScreenProvider;

impl ScreenInfoProvider for LinuxMockScreenProvider {
    fn screen_size(&self) -> Result<ScreenSize> {
        Ok(ScreenSize {
            width: DEFAULT_SCREEN_WIDTH,
            height: DEFAULT_SCREEN_HEIGHT,
        })
    }
}

#[derive(Debug)]
struct LinuxMockCursorController;

impl CursorController for LinuxMockCursorController {
    fn hide_cursor(&mut self) -> Result<()> {
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
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
        Ok(())
    }
}

fn parse_cursor_path(raw: &str) -> Option<VecDeque<CursorPosition>> {
    let mut positions = VecDeque::new();
    for entry in raw.split(';').filter(|value| !value.trim().is_empty()) {
        let (x, y) = entry.split_once(',')?;
        let x = x.trim().parse::<i32>().ok()?;
        let y = y.trim().parse::<i32>().ok()?;
        positions.push_back(CursorPosition { x, y });
    }
    Some(positions)
}
