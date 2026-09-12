use std::collections::{HashSet, VecDeque};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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

use crate::layout::{CursorPosition, ScreenBounds};
use crate::platform::keycodes::{mac_keycode_to_wire, wire_to_mac_keycode_for_injection};
use crate::platform::macos_input::{
    ClickCountTracker, InputPoint, ModifierFlagsState, ModifierSnapshot,
};
use crate::platform::{
    CursorController, LocalInputCapture, PermissionStatus, PlatformAdapters, RemoteInputInjector,
    ScreenInfoProvider,
};

pub fn build_platform_adapters() -> PlatformAdapters {
    PlatformAdapters {
        input_capture: Box::new(MacosInputCapture::new()),
        input_injector: Box::new(MacosInputInjector::new()),
        screen_provider: Box::new(MacosScreenProvider),
        cursor_controller: Box::new(MacosCursorController::new()),
    }
}

pub fn permission_status() -> PermissionStatus {
    let probe = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![CGEventType::KeyDown, CGEventType::MouseMoved],
        |_proxy, _event_type, _event| None,
    );
    if probe.is_ok() {
        PermissionStatus::Granted
    } else {
        PermissionStatus::Missing
    }
}

const MAX_QUEUED_EVENTS: usize = 2_048;
const TAP_START_TIMEOUT: Duration = Duration::from_secs(2);
const VIRTUAL_CURSOR_RESYNC_IDLE_THRESHOLD: Duration = Duration::from_millis(250);
const OTHER_MOUSE_BUTTON_MIDDLE: i64 = 2;
const OTHER_MOUSE_BUTTON_X1: i64 = 3;
const OTHER_MOUSE_BUTTON_X2: i64 = 4;

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
    x1: bool,
    x2: bool,
}

impl MouseButtonsDown {
    fn update(&mut self, button: MouseButton, pressed: bool) {
        match button {
            MouseButton::Left => self.left = pressed,
            MouseButton::Right => self.right = pressed,
            MouseButton::Middle => self.middle = pressed,
            MouseButton::X1 => self.x1 = pressed,
            MouseButton::X2 => self.x2 = pressed,
        }
    }

    fn any_pressed(&self) -> bool {
        self.left || self.right || self.middle || self.x1 || self.x2
    }
}

struct MacosInputInjector {
    buttons_down: MouseButtonsDown,
    virtual_cursor: Option<CGPoint>,
    backing_scale_x: f64,
    backing_scale_y: f64,
    last_move_at: Option<Instant>,
    modifier_flags: ModifierFlagsState,
    swap_ctrl_cmd: bool,
    unknown_wire_keycodes: HashSet<u16>,
    click_tracker: ClickCountTracker,
    click_clock_start: Instant,
}

impl MacosInputInjector {
    fn new() -> Self {
        let (backing_scale_x, backing_scale_y) = display_backing_scale();
        Self {
            buttons_down: MouseButtonsDown {
                left: false,
                right: false,
                middle: false,
                x1: false,
                x2: false,
            },
            virtual_cursor: None,
            backing_scale_x,
            backing_scale_y,
            last_move_at: None,
            modifier_flags: ModifierFlagsState::default(),
            swap_ctrl_cmd: true,
            unknown_wire_keycodes: HashSet::new(),
            click_tracker: ClickCountTracker::default(),
            click_clock_start: Instant::now(),
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
        delta: Option<(i64, i64)>,
        click_state: Option<i64>,
        other_button_number: Option<i64>,
    ) -> Result<()> {
        let source =
            create_event_source().ok_or_else(|| anyhow!("failed to create CGEventSource"))?;
        let event = CGEvent::new_mouse_event(source, event_type, point, button)
            .map_err(|_| anyhow!("failed to build macOS mouse event"))?;
        if let Some((delta_x, delta_y)) = delta {
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, delta_x);
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, delta_y);
        }
        if let Some(click_state) = click_state {
            event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state);
        }
        if let Some(button_number) = other_button_number {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, button_number);
        }
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn maybe_resync_virtual_cursor(&mut self, now: Instant) -> Result<()> {
        let should_resync = match self.last_move_at {
            Some(last) => now.duration_since(last) > VIRTUAL_CURSOR_RESYNC_IDLE_THRESHOLD,
            None => self.virtual_cursor.is_none(),
        };
        if should_resync && !self.buttons_down.any_pressed() {
            self.virtual_cursor = Some(self.current_cursor_location()?);
        }
        Ok(())
    }

    fn scaled_mouse_delta(&self, dx: i16, dy: i16) -> (f64, f64) {
        (
            f64::from(dx) / self.backing_scale_x,
            f64::from(dy) / self.backing_scale_y,
        )
    }

    fn virtual_cursor_or_system(&mut self) -> Result<CGPoint> {
        if let Some(cursor) = self.virtual_cursor {
            return Ok(cursor);
        }
        let current = self.current_cursor_location()?;
        self.virtual_cursor = Some(current);
        Ok(current)
    }
}

impl RemoteInputInjector for MacosInputInjector {
    fn set_swap_ctrl_cmd(&mut self, enabled: bool) {
        self.swap_ctrl_cmd = enabled;
    }

    fn inject_event(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMove { dx, dy } => {
                if *dx == 0 && *dy == 0 {
                    return Ok(());
                }
                let now = Instant::now();
                self.maybe_resync_virtual_cursor(now)?;
                let current = self.virtual_cursor_or_system()?;
                let (scaled_dx, scaled_dy) = self.scaled_mouse_delta(*dx, *dy);
                let target = CGPoint::new(current.x + scaled_dx, current.y + scaled_dy);
                self.virtual_cursor = Some(target);
                let (event_type, button) = self.movement_event_type();
                self.post_mouse_event(
                    event_type,
                    button,
                    target,
                    Some((scaled_dx.round() as i64, scaled_dy.round() as i64)),
                    None,
                    None,
                )?;
                self.last_move_at = Some(now);
            }
            InputEvent::MouseButton { button, pressed } => {
                let current = self.virtual_cursor_or_system()?;
                let now = self.click_clock_start.elapsed();
                let point = InputPoint {
                    x: current.x,
                    y: current.y,
                };
                let click_state = match button {
                    MouseButton::Left | MouseButton::Right | MouseButton::Middle => Some(
                        self.click_tracker
                            .click_state_for_event(*button, *pressed, point, now),
                    ),
                    MouseButton::X1 | MouseButton::X2 => None,
                };

                let (event_type, native_button, other_button_number) = match (button, pressed) {
                    (MouseButton::Left, true) => {
                        (CGEventType::LeftMouseDown, CGMouseButton::Left, None)
                    }
                    (MouseButton::Left, false) => {
                        (CGEventType::LeftMouseUp, CGMouseButton::Left, None)
                    }
                    (MouseButton::Right, true) => {
                        (CGEventType::RightMouseDown, CGMouseButton::Right, None)
                    }
                    (MouseButton::Right, false) => {
                        (CGEventType::RightMouseUp, CGMouseButton::Right, None)
                    }
                    (MouseButton::Middle, true) => (
                        CGEventType::OtherMouseDown,
                        CGMouseButton::Center,
                        Some(OTHER_MOUSE_BUTTON_MIDDLE),
                    ),
                    (MouseButton::Middle, false) => (
                        CGEventType::OtherMouseUp,
                        CGMouseButton::Center,
                        Some(OTHER_MOUSE_BUTTON_MIDDLE),
                    ),
                    (MouseButton::X1, true) => (
                        CGEventType::OtherMouseDown,
                        CGMouseButton::Center,
                        Some(OTHER_MOUSE_BUTTON_X1),
                    ),
                    (MouseButton::X1, false) => (
                        CGEventType::OtherMouseUp,
                        CGMouseButton::Center,
                        Some(OTHER_MOUSE_BUTTON_X1),
                    ),
                    (MouseButton::X2, true) => (
                        CGEventType::OtherMouseDown,
                        CGMouseButton::Center,
                        Some(OTHER_MOUSE_BUTTON_X2),
                    ),
                    (MouseButton::X2, false) => (
                        CGEventType::OtherMouseUp,
                        CGMouseButton::Center,
                        Some(OTHER_MOUSE_BUTTON_X2),
                    ),
                };
                self.post_mouse_event(
                    event_type,
                    native_button,
                    current,
                    None,
                    click_state,
                    other_button_number,
                )?;
                self.buttons_down.update(*button, *pressed);
            }
            InputEvent::Key { scancode, pressed } => {
                let Some(mac_keycode) =
                    wire_to_mac_keycode_for_injection(*scancode, self.swap_ctrl_cmd)
                else {
                    if self.unknown_wire_keycodes.insert(*scancode) {
                        eprintln!(
                            "macOS injector: missing injection keycode mapping for scancode 0x{scancode:04X}"
                        );
                    }
                    return Ok(());
                };
                let source = create_event_source()
                    .ok_or_else(|| anyhow!("failed to create CGEventSource"))?;
                let key_event = CGEvent::new_keyboard_event(source, mac_keycode, *pressed)
                    .map_err(|_| anyhow!("failed to build macOS keyboard event"))?;
                let snapshot = self.modifier_flags.apply_key_event(mac_keycode, *pressed);
                key_event.set_flags(cg_event_flags_from_snapshot(snapshot));
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
    fn screen_bounds(&self) -> Result<ScreenBounds> {
        Ok(detect_display_union_bounds())
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

    fn warp_cursor_to_safe_point(&mut self, edge: Edge, screen: ScreenBounds) -> Result<()> {
        let target = match edge {
            Edge::Left => CursorPosition {
                x: screen.max_x() - 1,
                y: screen.origin_y + (screen.height as i32) / 2,
            },
            Edge::Right => CursorPosition {
                x: screen.origin_x + 1,
                y: screen.origin_y + (screen.height as i32) / 2,
            },
            Edge::Top => CursorPosition {
                x: screen.origin_x + (screen.width as i32) / 2,
                y: screen.max_y() - 1,
            },
            Edge::Bottom => CursorPosition {
                x: screen.origin_x + (screen.width as i32) / 2,
                y: screen.origin_y + 1,
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

fn display_backing_scale() -> (f64, f64) {
    let display = CGDisplay::main();
    let bounds = display.bounds();
    let width = bounds.size.width.max(1.0);
    let height = bounds.size.height.max(1.0);
    let scale_x = display.pixels_wide() as f64 / width;
    let scale_y = display.pixels_high() as f64 / height;
    (scale_x.max(1.0), scale_y.max(1.0))
}

fn detect_display_union_bounds() -> ScreenBounds {
    let display_ids = CGDisplay::active_displays()
        .ok()
        .filter(|displays| !displays.is_empty());
    let mut iter = display_ids
        .map(|display_ids| {
            display_ids
                .into_iter()
                .map(CGDisplay::new)
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec![CGDisplay::main()])
        .into_iter();
    let Some(first_display) = iter.next() else {
        return ScreenBounds::from_size(1, 1);
    };

    let first_bounds = first_display.bounds();
    let mut min_x = first_bounds.origin.x;
    let mut min_y = first_bounds.origin.y;
    let mut max_x = first_bounds.origin.x + first_bounds.size.width;
    let mut max_y = first_bounds.origin.y + first_bounds.size.height;

    for display in iter {
        let bounds = display.bounds();
        min_x = min_x.min(bounds.origin.x);
        min_y = min_y.min(bounds.origin.y);
        max_x = max_x.max(bounds.origin.x + bounds.size.width);
        max_y = max_y.max(bounds.origin.y + bounds.size.height);
    }

    let width = (max_x - min_x).max(1.0).round() as u32;
    let height = (max_y - min_y).max(1.0).round() as u32;
    ScreenBounds {
        origin_x: min_x.round() as i32,
        origin_y: min_y.round() as i32,
        width,
        height,
    }
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
        .map_err(|_| anyhow!("failed to create CFRunLoop source for event tap"))?;
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
            let button_number =
                event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match button_number {
                OTHER_MOUSE_BUTTON_MIDDLE => Some(MouseButton::Middle),
                OTHER_MOUSE_BUTTON_X1 => Some(MouseButton::X1),
                OTHER_MOUSE_BUTTON_X2 => Some(MouseButton::X2),
                _ => None,
            };
            if let Some(button) = button {
                state.push_event(InputEvent::MouseButton {
                    button,
                    pressed: true,
                });
            }
            state.set_cursor(point_to_cursor(event.location()));
        }
        CGEventType::OtherMouseUp => {
            let button_number =
                event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match button_number {
                OTHER_MOUSE_BUTTON_MIDDLE => Some(MouseButton::Middle),
                OTHER_MOUSE_BUTTON_X1 => Some(MouseButton::X1),
                OTHER_MOUSE_BUTTON_X2 => Some(MouseButton::X2),
                _ => None,
            };
            if let Some(button) = button {
                state.push_event(InputEvent::MouseButton {
                    button,
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

fn cg_event_flags_from_snapshot(snapshot: ModifierSnapshot) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    if snapshot.command {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    if snapshot.shift {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if snapshot.alternate {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if snapshot.control {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if snapshot.alpha_shift {
        flags |= CGEventFlags::CGEventFlagAlphaShift;
    }
    flags
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
