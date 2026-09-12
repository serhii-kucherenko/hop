use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sideshift_protocol::control::Edge;
use sideshift_protocol::datagram::{InputEvent, MouseButton};
use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_MOVE_NOCOALESCE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, PeekMessageW,
    PostThreadMessageW, SetCursorPos, SetWindowsHookExW, ShowCursor, TranslateMessage,
    UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    LLKHF_LOWER_IL_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, SM_CXSCREEN,
    SM_CYSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::layout::{CursorPosition, ScreenSize};
use crate::platform::keycodes::{windows_vk_to_wire, wire_to_windows_vk};
use crate::platform::{
    CursorController, LocalInputCapture, PlatformAdapters, RemoteInputInjector, ScreenInfoProvider,
};

const MAX_QUEUED_EVENTS: usize = 2_048;
const WHEEL_DELTA: i32 = 120;
const HOOK_START_TIMEOUT: Duration = Duration::from_secs(2);

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

struct WindowsCaptureState {
    events: Mutex<VecDeque<InputEvent>>,
    last_mouse_point: Mutex<Option<(i32, i32)>>,
}

impl WindowsCaptureState {
    fn new() -> Self {
        Self {
            events: Mutex::new(VecDeque::with_capacity(256)),
            last_mouse_point: Mutex::new(None),
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

    fn record_mouse_move(&self, point: POINT) -> Option<InputEvent> {
        let mut last_point = self.last_mouse_point.lock().ok()?;
        let event = (*last_point).map(|(last_x, last_y)| InputEvent::MouseMove {
            dx: saturating_i32_to_i16(point.x - last_x),
            dy: saturating_i32_to_i16(point.y - last_y),
        });
        *last_point = Some((point.x, point.y));
        event.filter(|movement| {
            matches!(
                movement,
                InputEvent::MouseMove {
                    dx: non_zero_dx,
                    dy: non_zero_dy
                } if *non_zero_dx != 0 || *non_zero_dy != 0
            )
        })
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
                        "Windows capture disabled: {error}. Low-level hooks require running SideShift in the interactive desktop session."
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
                let flags = match (button, pressed) {
                    (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                    (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                    (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                    (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                    (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                };
                send_inputs(&[mouse_input(0, 0, flags, 0)])?;
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
        .name("sideshift-win-capture".to_owned())
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
            TranslateMessage(&message);
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
    if code == HC_ACTION as i32 {
        if let Some(state) = CAPTURE_STATE.get() {
            let hook = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            if (hook.flags & LLMHF_INJECTED) == 0 {
                let message = wparam.0 as u32;
                if let Some(event) = mouse_event_from_hook(state, message, hook) {
                    state.push_event(event);
                }
            }
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn mouse_event_from_hook(
    state: &WindowsCaptureState,
    message: u32,
    hook: &MSLLHOOKSTRUCT,
) -> Option<InputEvent> {
    match message {
        WM_MOUSEMOVE => state.record_mouse_move(hook.pt),
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
    if code == HC_ACTION as i32 {
        if let Some(state) = CAPTURE_STATE.get() {
            let hook = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let injected =
                hook.flags.contains(LLKHF_INJECTED) || hook.flags.contains(LLKHF_LOWER_IL_INJECTED);
            if !injected {
                let message = wparam.0 as u32;
                if let Some(event) = keyboard_event_from_hook(message, hook) {
                    state.push_event(event);
                }
            }
        }
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

fn saturating_i32_to_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}
