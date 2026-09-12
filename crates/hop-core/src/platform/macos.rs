use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use hop_protocol::control::Edge;
use hop_protocol::datagram::{InputEvent, MouseButton};

use crate::layout::{CursorPosition, ScreenSize};
use crate::platform::keycodes::{mac_keycode_to_wire, wire_to_mac_keycode};
use crate::platform::{
    CursorController, LocalInputCapture, PlatformAdapters, RemoteInputInjector, ScreenInfoProvider,
};

pub fn build_platform_adapters() -> PlatformAdapters {
    PlatformAdapters {
        input_capture: Box::new(MacosInputCapture::new()),
        input_injector: Box::new(MacosInputInjector::new()),
        screen_provider: Box::new(MacosScreenProvider),
        cursor_controller: Box::new(MacosCursorController::new()),
    }
}

const MAX_QUEUED_EVENTS: usize = 2_048;
const TAP_START_TIMEOUT: Duration = Duration::from_secs(2);

struct MacosCaptureState {
    events: Mutex<VecDeque<InputEvent>>,
    last_cursor: Mutex<Option<CursorPosition>>,
    modifier_states: Mutex<[bool; 128]>,
}

impl MacosCaptureState {
    fn new() -> Self {
        Self {
            events: Mutex::new(VecDeque::with_capacity(256)),
            last_cursor: Mutex::new(None),
            modifier_states: Mutex::new([false; 128]),
        }
    }

    fn push_event(&self, event: InputEvent) {
        if let Ok(mut queue) = self.events.lock() {
            if queue.len() >= MAX_QUEUED_EVENTS {
                let _ = queue.pop_front();
            }
            queue.push_back(event);
        }
    }

    fn drain_events(&self) -> Result<Vec<InputEvent>> {
        let mut queue = self
            .events
            .lock()
            .map_err(|_| anyhow!("failed to lock macOS event queue"))?;
        let mut drained = Vec::with_capacity(queue.len());
        while let Some(event) = queue.pop_front() {
            drained.push(event);
        }
        Ok(drained)
    }

    fn set_cursor(&self, cursor: CursorPosition) {
        if let Ok(mut current) = self.last_cursor.lock() {
            *current = Some(cursor);
        }
    }

    fn cursor(&self) -> Option<CursorPosition> {
        self.last_cursor.lock().ok().and_then(|cursor| *cursor)
    }

    fn modifier_transition(&self, keycode: u16, desired_state: bool) -> bool {
        let Ok(mut states) = self.modifier_states.lock() else {
            return desired_state;
        };

        let index = usize::from(keycode);
        if index >= states.len() {
            return desired_state;
        }

        let current = states[index];
        let next = if desired_state == current {
            !current
        } else {
            desired_state
        };
        states[index] = next;
        next
    }
}

struct MacosInputCapture {
    state: Arc<MacosCaptureState>,
}

impl MacosInputCapture {
    fn new() -> Self {
        let state = Arc::new(MacosCaptureState::new());
        if let Err(error) = start_event_tap(state.clone()) {
            eprintln!(
                "macOS capture disabled: {error}. Grant Input Monitoring + Accessibility and relaunch hop."
            );
        }

        Self { state }
    }
}

impl LocalInputCapture for MacosInputCapture {
    fn poll_cursor_position(&mut self) -> Result<Option<CursorPosition>> {
        if let Some(cursor) = self.state.cursor() {
            return Ok(Some(cursor));
        }

        let cursor = query_cursor_position()?;
        self.state.set_cursor(cursor);
        Ok(Some(cursor))
    }

    fn poll_input_events(&mut self) -> Result<Vec<InputEvent>> {
        self.state.drain_events()
    }
}

struct MouseButtonsDown {
    left: bool,
    right: bool,
    middle: bool,
}

impl MouseButtonsDown {
    fn update(&mut self, button: MouseButton, pressed: bool) {
        match button {
            MouseButton::Left => self.left = pressed,
            MouseButton::Right => self.right = pressed,
            MouseButton::Middle => self.middle = pressed,
        }
    }
}

struct MacosInputInjector {
    buttons_down: MouseButtonsDown,
}

impl MacosInputInjector {
    fn new() -> Self {
        Self {
            buttons_down: MouseButtonsDown {
                left: false,
                right: false,
                middle: false,
            },
        }
    }

    fn current_cursor_location(&self) -> Result<CGPoint> {
        let source =
            create_event_source().ok_or_else(|| anyhow!("failed to create CGEventSource"))?;
        let event = CGEvent::new(source)
            .map_err(|_| anyhow!("failed to read cursor location before injection"))?;
        Ok(event.location())
    }

    fn movement_event_type(&self) -> (CGEventType, CGMouseButton) {
        if self.buttons_down.left {
            return (CGEventType::LeftMouseDragged, CGMouseButton::Left);
        }
        if self.buttons_down.right {
            return (CGEventType::RightMouseDragged, CGMouseButton::Right);
        }
        if self.buttons_down.middle {
            return (CGEventType::OtherMouseDragged, CGMouseButton::Center);
        }
        (CGEventType::MouseMoved, CGMouseButton::Left)
    }

    fn post_mouse_event(
        &self,
        event_type: CGEventType,
        button: CGMouseButton,
        point: CGPoint,
    ) -> Result<()> {
        let source =
            create_event_source().ok_or_else(|| anyhow!("failed to create CGEventSource"))?;
        let event = CGEvent::new_mouse_event(source, event_type, point, button)
            .map_err(|_| anyhow!("failed to build macOS mouse event"))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

impl RemoteInputInjector for MacosInputInjector {
    fn inject_event(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMove { dx, dy } => {
                if *dx == 0 && *dy == 0 {
                    return Ok(());
                }
                let current = self.current_cursor_location()?;
                let target = CGPoint::new(current.x + f64::from(*dx), current.y + f64::from(*dy));
                let (event_type, button) = self.movement_event_type();
                self.post_mouse_event(event_type, button, target)?;
            }
            InputEvent::MouseButton { button, pressed } => {
                let current = self.current_cursor_location()?;
                let (event_type, native_button) = match (button, pressed) {
                    (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                    (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                    (MouseButton::Right, true) => {
                        (CGEventType::RightMouseDown, CGMouseButton::Right)
                    }
                    (MouseButton::Right, false) => {
                        (CGEventType::RightMouseUp, CGMouseButton::Right)
                    }
                    (MouseButton::Middle, true) => {
                        (CGEventType::OtherMouseDown, CGMouseButton::Center)
                    }
                    (MouseButton::Middle, false) => {
                        (CGEventType::OtherMouseUp, CGMouseButton::Center)
                    }
                };
                self.post_mouse_event(event_type, native_button, current)?;
                self.buttons_down.update(*button, *pressed);
            }
            InputEvent::Key { scancode, pressed } => {
                let Some(mac_keycode) = wire_to_mac_keycode(*scancode) else {
                    return Ok(());
                };
                let source = create_event_source()
                    .ok_or_else(|| anyhow!("failed to create CGEventSource"))?;
                let key_event = CGEvent::new_keyboard_event(source, mac_keycode, *pressed)
                    .map_err(|_| anyhow!("failed to build macOS keyboard event"))?;
                key_event.post(CGEventTapLocation::HID);
            }
            InputEvent::Scroll { dx, dy } => {
                if *dx == 0 && *dy == 0 {
                    return Ok(());
                }
                let source = create_event_source()
                    .ok_or_else(|| anyhow!("failed to create CGEventSource"))?;
                let scroll = CGEvent::new_scroll_event(
                    source,
                    ScrollEventUnit::LINE,
                    2,
                    i32::from(*dy),
                    i32::from(*dx),
                    0,
                )
                .map_err(|_| anyhow!("failed to build macOS scroll event"))?;
                scroll.post(CGEventTapLocation::HID);
            }
        }
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
struct MacosCursorController {
    cursor_hidden: bool,
}

impl MacosCursorController {
    fn new() -> Self {
        Self {
            cursor_hidden: false,
        }
    }
}

impl CursorController for MacosCursorController {
    fn hide_cursor(&mut self) -> Result<()> {
        if self.cursor_hidden {
            return Ok(());
        }
        CGDisplay::main()
            .hide_cursor()
            .map_err(|code| anyhow!("CGDisplayHideCursor failed with code {code}"))?;
        self.cursor_hidden = true;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
        if !self.cursor_hidden {
            return Ok(());
        }
        CGDisplay::main()
            .show_cursor()
            .map_err(|code| anyhow!("CGDisplayShowCursor failed with code {code}"))?;
        self.cursor_hidden = false;
        Ok(())
    }

    fn warp_cursor_to_safe_point(&mut self, edge: Edge, screen: ScreenSize) -> Result<()> {
        let target = match edge {
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
        CGDisplay::warp_mouse_cursor_position(CGPoint::new(target.x as f64, target.y as f64))
            .map_err(|code| anyhow!("CGWarpMouseCursorPosition failed with code {code}"))?;
        Ok(())
    }
}

fn create_event_source() -> Option<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .or_else(|_| CGEventSource::new(CGEventSourceStateID::CombinedSessionState))
        .ok()
}

fn query_cursor_position() -> Result<CursorPosition> {
    let source = create_event_source().ok_or_else(|| anyhow!("failed to create CGEventSource"))?;
    let event =
        CGEvent::new(source).map_err(|_| anyhow!("failed to query macOS cursor location"))?;
    Ok(point_to_cursor(event.location()))
}

fn start_event_tap(state: Arc<MacosCaptureState>) -> Result<()> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("hop-macos-capture".to_owned())
        .spawn(move || {
            let setup_result = install_and_run_event_tap(state, &ready_tx);
            if let Err(error) = setup_result {
                let _ = ready_tx.send(Err(error.to_string()));
            }
        })
        .context("failed to spawn macOS capture thread")?;

    match ready_rx.recv_timeout(TAP_START_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow!(error)),
        Err(_) => Err(anyhow!(
            "timed out while waiting for macOS event tap startup"
        )),
    }
}

fn install_and_run_event_tap(
    state: Arc<MacosCaptureState>,
    ready_tx: &mpsc::SyncSender<Result<(), String>>,
) -> Result<()> {
    let callback_state = state.clone();
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::ScrollWheel,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ],
        move |_proxy, event_type, event| {
            handle_tap_event(&callback_state, event_type, event);
            None
        },
    )
    .map_err(|_| anyhow!("CGEventTapCreate returned null"))?;

    let run_loop = CFRunLoop::get_current();
    let loop_source = tap
        .mach_port
        .create_runloop_source(0)
        .ok_or_else(|| anyhow!("failed to create CFRunLoop source for event tap"))?;
    run_loop.add_source(&loop_source, unsafe { kCFRunLoopCommonModes });
    tap.enable();

    let _ = ready_tx.send(Ok(()));
    CFRunLoop::run_current();
    Ok(())
}

fn handle_tap_event(state: &MacosCaptureState, event_type: CGEventType, event: &CGEvent) {
    match event_type {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            let dx = saturating_i64_to_i16(
                event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X),
            );
            let dy = saturating_i64_to_i16(
                event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y),
            );
            if dx != 0 || dy != 0 {
                state.push_event(InputEvent::MouseMove { dx, dy });
            }
            state.set_cursor(point_to_cursor(event.location()));
        }
        CGEventType::LeftMouseDown => {
            state.push_event(InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            });
            state.set_cursor(point_to_cursor(event.location()));
        }
        CGEventType::LeftMouseUp => {
            state.push_event(InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: false,
            });
            state.set_cursor(point_to_cursor(event.location()));
        }
        CGEventType::RightMouseDown => {
            state.push_event(InputEvent::MouseButton {
                button: MouseButton::Right,
                pressed: true,
            });
            state.set_cursor(point_to_cursor(event.location()));
        }
        CGEventType::RightMouseUp => {
            state.push_event(InputEvent::MouseButton {
                button: MouseButton::Right,
                pressed: false,
            });
            state.set_cursor(point_to_cursor(event.location()));
        }
        CGEventType::OtherMouseDown => {
            if event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) == 2 {
                state.push_event(InputEvent::MouseButton {
                    button: MouseButton::Middle,
                    pressed: true,
                });
            }
            state.set_cursor(point_to_cursor(event.location()));
        }
        CGEventType::OtherMouseUp => {
            if event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) == 2 {
                state.push_event(InputEvent::MouseButton {
                    button: MouseButton::Middle,
                    pressed: false,
                });
            }
            state.set_cursor(point_to_cursor(event.location()));
        }
        CGEventType::ScrollWheel => {
            let dx = saturating_i64_to_i16(
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2),
            );
            let dy = saturating_i64_to_i16(
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
            );
            if dx != 0 || dy != 0 {
                state.push_event(InputEvent::Scroll { dx, dy });
            }
        }
        CGEventType::KeyDown => {
            enqueue_key_event(state, event, true);
        }
        CGEventType::KeyUp => {
            enqueue_key_event(state, event, false);
        }
        CGEventType::FlagsChanged => {
            if let Some(keycode) = event_keycode(event) {
                if let Some(wire_keycode) = mac_keycode_to_wire(keycode) {
                    let pressed = modifier_flag_for_key(keycode)
                        .map(|flag| {
                            let desired = event.get_flags().contains(flag);
                            state.modifier_transition(keycode, desired)
                        })
                        .unwrap_or(true);
                    state.push_event(InputEvent::Key {
                        scancode: wire_keycode,
                        pressed,
                    });
                }
            }
        }
        _ => {}
    }
}

fn enqueue_key_event(state: &MacosCaptureState, event: &CGEvent, pressed: bool) {
    let Some(keycode) = event_keycode(event) else {
        return;
    };
    if let Some(wire_keycode) = mac_keycode_to_wire(keycode) {
        state.push_event(InputEvent::Key {
            scancode: wire_keycode,
            pressed,
        });
    }
}

fn event_keycode(event: &CGEvent) -> Option<u16> {
    let value = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
    u16::try_from(value).ok()
}

fn modifier_flag_for_key(keycode: u16) -> Option<CGEventFlags> {
    match keycode {
        54 | 55 => Some(CGEventFlags::CGEventFlagCommand),
        56 | 60 => Some(CGEventFlags::CGEventFlagShift),
        58 | 61 => Some(CGEventFlags::CGEventFlagAlternate),
        59 | 62 => Some(CGEventFlags::CGEventFlagControl),
        57 => Some(CGEventFlags::CGEventFlagAlphaShift),
        _ => None,
    }
}

fn point_to_cursor(point: CGPoint) -> CursorPosition {
    CursorPosition {
        x: point.x.round() as i32,
        y: point.y.round() as i32,
    }
}

fn saturating_i64_to_i16(value: i64) -> i16 {
    value.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
}
