use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use hop_protocol::control::Edge;
use hop_protocol::datagram::{InputEvent, MouseButton};
use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_MOVE_NOCOALESCE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, PeekMessageW,
    PostThreadMessageW, SetCursorPos, SetWindowsHookExW, ShowCursor, TranslateMessage,
    UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLKHF_LOWER_IL_INJECTED,
    LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, SM_CXSCREEN, SM_CYSCREEN, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use crate::layout::{CursorPosition, ScreenSize};
use crate::platform::keycodes::{windows_vk_to_wire, wire_to_windows_vk};
use crate::platform::{
    mouse_move_event_from_points, CursorController, LocalInputCapture, PermissionStatus,
    PlatformAdapters, RemoteInputInjector, ScreenInfoProvider,
};

const MAX_QUEUED_EVENTS: usize = 2_048;
const WHEEL_DELTA: i32 = 120;
const HOOK_START_TIMEOUT: Duration = Duration::from_secs(2);
const XBUTTON1_DATA: u16 = 0x0001;
const XBUTTON2_DATA: u16 = 0x0002;

static CAPTURE_STATE: OnceLock<Arc<WindowsCaptureState>> = OnceLock::new();
static HOOK_THREAD_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn build_platform_adapters() -> PlatformAdapters {
    PlatformAdapters {
        input_capture: Box::new(WindowsInputCapture::new()),
        input_injector: Box::new(WindowsInputInjector),
        screen_provider: Box::new(WindowsScreenProvider),
        cursor_controller: Box::new(WindowsCursorController::new()),
    }
}

pub fn permission_status() -> PermissionStatus {
    PermissionStatus::Unknown
}

struct WindowsCaptureState {
    events: Mutex<VecDeque<InputEvent>>,
    last_mouse_point: Mutex<Option<(i32, i32)>>,
    remote_focus_active: AtomicBool,
}

impl WindowsCaptureState {
    fn new() -> Self {
        Self {
            events: Mutex::new(VecDeque::with_capacity(256)),
            last_mouse_point: Mutex::new(None),
            remote_focus_active: AtomicBool::new(false),
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
            .map_err(|_| anyhow!("failed to lock Windows event queue"))?;
        let mut drained = Vec::with_capacity(queue.len());
        while let Some(event) = queue.pop_front() {
            drained.push(event);
        }
        Ok(drained)
    }

    fn set_remote_focus(&self, active: bool) {
        self.remote_focus_active.store(active, Ordering::SeqCst);
        if !active {
            if let Ok(mut queue) = self.events.lock() {
                queue.clear();
            }
        }
        if let Ok(mut last_point) = self.last_mouse_point.lock() {
            *last_point = None;
        }
    }

    fn remote_focus_active(&self) -> bool {
        self.remote_focus_active.load(Ordering::SeqCst)
    }

    fn record_local_mouse_move(&self, point: POINT) -> Option<InputEvent> {
        let mut last_point = self.last_mouse_point.lock().ok()?;
        let event = (*last_point).and_then(|(last_x, last_y)| {
            mouse_move_event_from_points(
                CursorPosition {
                    x: last_x,
                    y: last_y,
                },
                point_to_cursor(point),
            )
        });
        *last_point = Some((point.x, point.y));
        event
    }

    fn record_remote_mouse_move(
        &self,
        would_be_point: POINT,
        actual_point: POINT,
    ) -> Option<InputEvent> {
        mouse_move_event_from_points(
            point_to_cursor(actual_point),
            point_to_cursor(would_be_point),
        )
    }
}

struct HookThread {
    thread_id: u32,
    join_handle: Option<JoinHandle<()>>,
}

struct WindowsInputCapture {
    state: Arc<WindowsCaptureState>,
    hook_thread: Option<HookThread>,
}

impl WindowsInputCapture {
    fn new() -> Self {
        let state = CAPTURE_STATE
            .get_or_init(|| Arc::new(WindowsCaptureState::new()))
            .clone();

        let hook_thread = if HOOK_THREAD_ACTIVE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            match start_hook_thread() {
                Ok(thread) => Some(thread),
                Err(error) => {
                    HOOK_THREAD_ACTIVE.store(false, Ordering::SeqCst);
                    eprintln!(
                        "Windows capture disabled: {error}. Low-level hooks require running hop in the interactive desktop session."
                    );
                    None
                }
            }
        } else {
            None
        };

        Self { state, hook_thread }
    }
}

impl LocalInputCapture for WindowsInputCapture {
    fn poll_cursor_position(&mut self) -> Result<Option<CursorPosition>> {
        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point) }.context("GetCursorPos failed")?;
        Ok(Some(CursorPosition {
            x: point.x,
            y: point.y,
        }))
    }

    fn poll_input_events(&mut self) -> Result<Vec<InputEvent>> {
        self.state.drain_events()
    }

    fn set_remote_focus(&mut self, active: bool) -> Result<()> {
        self.state.set_remote_focus(active);
        Ok(())
    }
}

impl Drop for WindowsInputCapture {
    fn drop(&mut self) {
        let Some(mut hook_thread) = self.hook_thread.take() else {
            return;
        };

        let _ = unsafe { PostThreadMessageW(hook_thread.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(join_handle) = hook_thread.join_handle.take() {
            let _ = join_handle.join();
        }
        HOOK_THREAD_ACTIVE.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct WindowsInputInjector;

impl RemoteInputInjector for WindowsInputInjector {
    fn inject_event(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMove { dx, dy } => {
                if *dx == 0 && *dy == 0 {
                    return Ok(());
                }
                let input = mouse_input(
                    i32::from(*dx),
                    i32::from(*dy),
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE,
                    0,
                );
                send_inputs(&[input])?;
            }
            InputEvent::MouseButton { button, pressed } => {
                let (flags, mouse_data) = match (button, pressed) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON1_DATA)),
                    (MouseButton::X1, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON1_DATA)),
                    (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON2_DATA)),
                    (MouseButton::X2, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON2_DATA)),
                };
                send_inputs(&[mouse_input(0, 0, flags, mouse_data)])?;
            }
            InputEvent::Key { scancode, pressed } => {
                let vk = wire_to_windows_vk(*scancode);
                send_inputs(&[keyboard_input(vk, *pressed)])?;
            }
            InputEvent::Scroll { dx, dy } => match (*dx != 0, *dy != 0) {
                (false, false) => {}
                (false, true) => {
                    let data = wheel_data(*dy);
                    send_inputs(&[mouse_input(0, 0, MOUSEEVENTF_WHEEL, data)])?;
                }
                (true, false) => {
                    let data = wheel_data(*dx);
                    send_inputs(&[mouse_input(0, 0, MOUSEEVENTF_HWHEEL, data)])?;
                }
                (true, true) => {
                    let vertical = mouse_input(0, 0, MOUSEEVENTF_WHEEL, wheel_data(*dy));
                    let horizontal = mouse_input(0, 0, MOUSEEVENTF_HWHEEL, wheel_data(*dx));
                    send_inputs(&[vertical, horizontal])?;
                }
            },
        }
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

impl WindowsCursorController {
    fn new() -> Self {
        Self
    }
}

impl CursorController for WindowsCursorController {
    fn hide_cursor(&mut self) -> Result<()> {
        for _ in 0..32 {
            let counter = unsafe { ShowCursor(false) };
            if counter < 0 {
                return Ok(());
            }
        }
        Err(anyhow!("ShowCursor(false) did not converge"))
    }

    fn show_cursor(&mut self) -> Result<()> {
        for _ in 0..32 {
            let counter = unsafe { ShowCursor(true) };
            if counter >= 0 {
                return Ok(());
            }
        }
        Err(anyhow!("ShowCursor(true) did not converge"))
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
        unsafe { SetCursorPos(target.x, target.y) }.context("SetCursorPos failed")?;
        Ok(())
    }
}

fn start_hook_thread() -> Result<HookThread> {
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);
    let join_handle = thread::Builder::new()
        .name("hop-win-capture".to_owned())
        .spawn(move || {
            let startup = run_hook_loop(&startup_tx);
            if let Err(error) = startup {
                let _ = startup_tx.send(Err(error.to_string()));
            }
        })
        .context("failed to spawn Windows capture thread")?;

    match startup_rx.recv_timeout(HOOK_START_TIMEOUT) {
        Ok(Ok(thread_id)) => Ok(HookThread {
            thread_id,
            join_handle: Some(join_handle),
        }),
        Ok(Err(error)) => Err(anyhow!(error)),
        Err(_) => Err(anyhow!(
            "timed out while waiting for Windows low-level hook startup"
        )),
    }
}

fn run_hook_loop(startup_tx: &mpsc::SyncSender<Result<u32, String>>) -> Result<()> {
    let mut queue_probe = MSG::default();
    let _ = unsafe { PeekMessageW(&mut queue_probe, None, 0, 0, PM_NOREMOVE) };

    let keyboard_hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0) }
            .context("SetWindowsHookExW(WH_KEYBOARD_LL) failed")?;
    let mouse_hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0) }
        .context("SetWindowsHookExW(WH_MOUSE_LL) failed")?;

    let thread_id = unsafe { GetCurrentThreadId() };
    let _ = startup_tx.send(Ok(thread_id));

    let mut message = MSG::default();
    let mut message_loop_error = None;
    loop {
        let get_message_result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if get_message_result.0 == -1 {
            message_loop_error = Some(anyhow!("GetMessageW failed for hook loop"));
            break;
        }
        if get_message_result.0 == 0 || message.message == WM_QUIT {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    let _ = unsafe { UnhookWindowsHookEx(mouse_hook) };
    let _ = unsafe { UnhookWindowsHookEx(keyboard_hook) };

    if let Some(error) = message_loop_error {
        return Err(error);
    }
    Ok(())
}

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let mut should_block = false;
    if code == HC_ACTION as i32 {
        if let Some(state) = CAPTURE_STATE.get() {
            let hook = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            if (hook.flags & LLMHF_INJECTED) == 0 {
                let remote_focus_active = state.remote_focus_active();
                should_block = remote_focus_active;
                let message = wparam.0 as u32;
                if let Some(event) =
                    mouse_event_from_hook(state, message, hook, remote_focus_active)
                {
                    state.push_event(event);
                }
            }
        }
    }

    if should_block {
        return LRESULT(1);
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn mouse_event_from_hook(
    state: &WindowsCaptureState,
    message: u32,
    hook: &MSLLHOOKSTRUCT,
    remote_focus_active: bool,
) -> Option<InputEvent> {
    match message {
        WM_MOUSEMOVE => {
            if remote_focus_active {
                let mut actual = POINT::default();
                if unsafe { GetCursorPos(&mut actual) }.is_ok() {
                    return state.record_remote_mouse_move(hook.pt, actual);
                }
                return None;
            }
            state.record_local_mouse_move(hook.pt)
        }
        WM_LBUTTONDOWN => Some(InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        }),
        WM_LBUTTONUP => Some(InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: false,
        }),
        WM_RBUTTONDOWN => Some(InputEvent::MouseButton {
            button: MouseButton::Right,
            pressed: true,
        }),
        WM_RBUTTONUP => Some(InputEvent::MouseButton {
            button: MouseButton::Right,
            pressed: false,
        }),
        WM_MBUTTONDOWN => Some(InputEvent::MouseButton {
            button: MouseButton::Middle,
            pressed: true,
        }),
        WM_MBUTTONUP => Some(InputEvent::MouseButton {
            button: MouseButton::Middle,
            pressed: false,
        }),
        WM_XBUTTONDOWN => {
            xbutton_from_hook_data(hook.mouseData).map(|button| InputEvent::MouseButton {
                button,
                pressed: true,
            })
        }
        WM_XBUTTONUP => {
            xbutton_from_hook_data(hook.mouseData).map(|button| InputEvent::MouseButton {
                button,
                pressed: false,
            })
        }
        WM_MOUSEWHEEL => {
            let dy = normalize_wheel_delta(hiword_signed(hook.mouseData));
            (dy != 0).then_some(InputEvent::Scroll { dx: 0, dy })
        }
        WM_MOUSEHWHEEL => {
            let dx = normalize_wheel_delta(hiword_signed(hook.mouseData));
            (dx != 0).then_some(InputEvent::Scroll { dx, dy: 0 })
        }
        _ => None,
    }
}

unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let mut should_block = false;
    if code == HC_ACTION as i32 {
        if let Some(state) = CAPTURE_STATE.get() {
            let hook = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let injected =
                hook.flags.contains(LLKHF_INJECTED) || hook.flags.contains(LLKHF_LOWER_IL_INJECTED);
            if !injected {
                should_block = state.remote_focus_active();
                let message = wparam.0 as u32;
                if let Some(event) = keyboard_event_from_hook(message, hook) {
                    state.push_event(event);
                }
            }
        }
    }

    if should_block {
        return LRESULT(1);
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn keyboard_event_from_hook(message: u32, hook: &KBDLLHOOKSTRUCT) -> Option<InputEvent> {
    let pressed = match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => true,
        WM_KEYUP | WM_SYSKEYUP => false,
        _ => return None,
    };

    let wire_keycode = windows_vk_to_wire(hook.vkCode)?;
    Some(InputEvent::Key {
        scancode: wire_keycode,
        pressed,
    })
}

fn mouse_input(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, mouse_data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn keyboard_input(vk: u16, pressed: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send_inputs(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }

    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(anyhow!(
            "SendInput submitted {sent} of {} events",
            inputs.len()
        ));
    }
    Ok(())
}

fn wheel_data(units: i16) -> u32 {
    let raw = i32::from(units).saturating_mul(WHEEL_DELTA) as i16;
    u16::from_ne_bytes(raw.to_ne_bytes()) as u32
}

fn hiword_signed(value: u32) -> i16 {
    ((value >> 16) as u16) as i16
}

fn xbutton_from_hook_data(mouse_data: u32) -> Option<MouseButton> {
    match hiword_unsigned(mouse_data) {
        XBUTTON1_DATA => Some(MouseButton::X1),
        XBUTTON2_DATA => Some(MouseButton::X2),
        _ => None,
    }
}

fn hiword_unsigned(value: u32) -> u16 {
    (value >> 16) as u16
}

fn normalize_wheel_delta(raw_delta: i16) -> i16 {
    if raw_delta == 0 {
        return 0;
    }
    let raw_delta = i32::from(raw_delta);
    if raw_delta % WHEEL_DELTA == 0 {
        return saturating_i32_to_i16(raw_delta / WHEEL_DELTA);
    }
    saturating_i32_to_i16(raw_delta.signum())
}

fn point_to_cursor(point: POINT) -> CursorPosition {
    CursorPosition {
        x: point.x,
        y: point.y,
    }
}

fn saturating_i32_to_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}
