//! Native modal dialog for capturing a configurable Windows hotkey.
//!
//! The dialog uses a low-level keyboard hook so Windows, Alt, Ctrl, and Shift
//! can be captured consistently. It owns no application state outside this
//! module and restores the owner window and hook on every return path.

#![cfg(windows)]

use std::{
    cell::Cell,
    ffi::c_void,
    mem::size_of,
    sync::atomic::{AtomicUsize, Ordering},
};

use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{
            Input::KeyboardAndMouse::{
                EnableWindow, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU,
                VK_RSHIFT, VK_RWIN,
            },
            WindowsAndMessaging::{
                CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
                GWLP_USERDATA, GetForegroundWindow, GetMessageW, GetSystemMetrics, GetWindowRect,
                HHOOK, HMENU, KBDLLHOOKSTRUCT, MSG, PostMessageW, RegisterClassExW, SM_CXSCREEN,
                SM_CYSCREEN, SW_SHOW, SetForegroundWindow, SetWindowLongPtrW, SetWindowTextW,
                SetWindowsHookExW, ShowWindow, TranslateMessage, UnhookWindowsHookEx,
                UnregisterClassW, WH_KEYBOARD_LL, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_COMMAND,
                WM_CREATE, WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_NCCREATE, WM_SYSKEYDOWN,
                WM_SYSKEYUP, WNDCLASSEXW, WS_CHILD, WS_EX_DLGMODALFRAME, WS_EX_TOOLWINDOW,
                WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
            },
        },
    },
    core::PCWSTR,
};

const WINDOW_WIDTH: i32 = 440;
const WINDOW_HEIGHT: i32 = 190;
const FINISH_MESSAGE: u32 = WM_APP + 0x3a1;
const UPDATE_MESSAGE: u32 = WM_APP + 0x3a2;
const ID_ACCEPT: usize = 1;
const ID_CANCEL: usize = 2;

thread_local! {
    /// A WH_KEYBOARD_LL callback is dispatched on the thread that installed
    /// the hook, so this pointer is only accessed on the dialog's UI thread.
    static ACTIVE_CAPTURE: Cell<*mut CaptureState> = const { Cell::new(std::ptr::null_mut()) };
}

static NEXT_CLASS_ID: AtomicUsize = AtomicUsize::new(1);

/// Display a modal native dialog and return the selected hotkey in canonical
/// form, for example `Win+Alt+V`. Returns `Ok(None)` when the user cancels.
///
/// Unmodified Enter accepts the current selection and unmodified Esc cancels.
/// Modified Enter and Esc can themselves be selected as the final key.
pub fn capture_hotkey(owner_hwnd: isize) -> Result<Option<String>, String> {
    let owner = if owner_hwnd == 0 {
        None
    } else {
        let candidate = HWND(owner_hwnd as *mut c_void);
        if !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(candidate)) }.as_bool()
        {
            return Err("the supplied owner HWND is not a valid window".to_owned());
        }
        Some(candidate)
    };

    let mut session = DialogSession::new(owner)?;
    let state_ptr = (&mut *session.state) as *mut CaptureState;
    session.previous_capture = ACTIVE_CAPTURE.with(|active| active.replace(state_ptr));
    session.tls_active = true;

    session.hook = Some(
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) }
            .map_err(|error| format!("could not install the hotkey capture hook: {error}"))?,
    );

    let x = session.initial_position.0;
    let y = session.initial_position.1;
    let window = unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_TOOLWINDOW,
            PCWSTR(session.class_name.as_ptr()),
            PCWSTR(wide("Choose hotkey").as_ptr()),
            WS_OVERLAPPED | WS_SYSMENU,
            x,
            y,
            WINDOW_WIDTH,
            WINDOW_HEIGHT,
            owner,
            None,
            Some(session.instance),
            Some(state_ptr.cast()),
        )
    }
    .map_err(|error| format!("could not create the hotkey dialog: {error}"))?;
    session.state.hwnd = window;

    unsafe {
        let _ = ShowWindow(window, SW_SHOW);
        let _ = SetForegroundWindow(window);
    }

    let mut pump_error = None;
    while !session.state.finished {
        let mut message = MSG::default();
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 < 0 {
            pump_error = Some(format!(
                "the hotkey dialog message loop failed: {}",
                std::io::Error::last_os_error()
            ));
            break;
        }
        if result.0 == 0 {
            // Preserve WM_QUIT for the application's outer message loop.
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(message.wParam.0 as i32)
            };
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    if let Some(error) = pump_error {
        return Err(error);
    }

    Ok(if session.state.accepted {
        session.state.candidate.clone()
    } else {
        None
    })
}

struct DialogSession {
    state: Box<CaptureState>,
    instance: HINSTANCE,
    class_name: Vec<u16>,
    owner: Option<HWND>,
    owner_was_enabled: bool,
    class_registered: bool,
    hook: Option<HHOOK>,
    tls_active: bool,
    previous_capture: *mut CaptureState,
    initial_position: (i32, i32),
}

impl DialogSession {
    fn new(owner: Option<HWND>) -> Result<Self, String> {
        let module = unsafe { GetModuleHandleW(None) }
            .map_err(|error| format!("could not locate the application module: {error}"))?;
        let instance = HINSTANCE(module.0);
        let class_id = NEXT_CLASS_ID.fetch_add(1, Ordering::Relaxed);
        let class_name = wide(&format!(
            "Scribetray.HotkeyCapture.{}.{}",
            unsafe { GetCurrentThreadId() },
            class_id
        ));

        let mut state = Box::new(CaptureState::new(instance));
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        if unsafe { RegisterClassExW(&class) } == 0 {
            return Err(format!(
                "could not register the hotkey dialog class: {}",
                std::io::Error::last_os_error()
            ));
        }

        let owner_was_enabled = owner
            // EnableWindow returns nonzero when the window was previously
            // disabled, so invert it to remember whether we should restore it.
            .map(|hwnd| !unsafe { EnableWindow(hwnd, false) }.as_bool())
            .unwrap_or(false);
        let initial_position = dialog_position(owner);

        // Keep the state allocation stable: both the window procedure and the
        // low-level hook refer to it until DialogSession is dropped.
        state.instance = instance;
        Ok(Self {
            state,
            instance,
            class_name,
            owner,
            owner_was_enabled,
            class_registered: true,
            hook: None,
            tls_active: false,
            previous_capture: std::ptr::null_mut(),
            initial_position,
        })
    }
}

impl Drop for DialogSession {
    fn drop(&mut self) {
        if self.tls_active {
            ACTIVE_CAPTURE.with(|active| {
                if active.get() == (&mut *self.state as *mut CaptureState) {
                    active.set(self.previous_capture);
                }
            });
        }
        if let Some(hook) = self.hook.take() {
            let _ = unsafe { UnhookWindowsHookEx(hook) };
        }
        if self.state.hwnd.0 != std::ptr::null_mut() {
            let _ = unsafe { DestroyWindow(self.state.hwnd) };
            self.state.hwnd = HWND::default();
        }
        if self.class_registered {
            let _ =
                unsafe { UnregisterClassW(PCWSTR(self.class_name.as_ptr()), Some(self.instance)) };
            self.class_registered = false;
        }
        if self.owner_was_enabled {
            if let Some(owner) = self.owner {
                if unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(owner)) }
                    .as_bool()
                {
                    let _ = unsafe { EnableWindow(owner, true) };
                }
            }
        }
    }
}

struct CaptureState {
    hwnd: HWND,
    label_hwnd: HWND,
    instance: HINSTANCE,
    modifiers: ModifierState,
    captured_key_down: Option<u32>,
    candidate: Option<String>,
    accepted: bool,
    finished: bool,
}

impl CaptureState {
    fn new(instance: HINSTANCE) -> Self {
        Self {
            hwnd: HWND::default(),
            label_hwnd: HWND::default(),
            instance,
            modifiers: ModifierState::default(),
            captured_key_down: None,
            candidate: None,
            accepted: false,
            finished: false,
        }
    }
}

#[derive(Default)]
struct ModifierState {
    left_win: bool,
    right_win: bool,
    left_alt: bool,
    right_alt: bool,
    left_ctrl: bool,
    right_ctrl: bool,
    left_shift: bool,
    right_shift: bool,
}

impl ModifierState {
    fn update(&mut self, vk: u32, down: bool) -> bool {
        let slot = match vk {
            value if value == VK_LWIN.0 as u32 => &mut self.left_win,
            value if value == VK_RWIN.0 as u32 => &mut self.right_win,
            value if value == VK_LMENU.0 as u32 => &mut self.left_alt,
            value if value == VK_RMENU.0 as u32 => &mut self.right_alt,
            value if value == VK_LCONTROL.0 as u32 => &mut self.left_ctrl,
            value if value == VK_RCONTROL.0 as u32 => &mut self.right_ctrl,
            value if value == VK_LSHIFT.0 as u32 => &mut self.left_shift,
            value if value == VK_RSHIFT.0 as u32 => &mut self.right_shift,
            _ => return false,
        };
        *slot = down;
        true
    }

    fn label(&self, key: &str) -> String {
        let mut parts = Vec::with_capacity(5);
        if self.left_win || self.right_win {
            parts.push("Win");
        }
        if self.left_alt || self.right_alt {
            parts.push("Alt");
        }
        if self.left_ctrl || self.right_ctrl {
            parts.push("Ctrl");
        }
        if self.left_shift || self.right_shift {
            parts.push("Shift");
        }
        parts.push(key);
        parts.join("+")
    }

    fn any(&self) -> bool {
        self.left_win
            || self.right_win
            || self.left_alt
            || self.right_alt
            || self.left_ctrl
            || self.right_ctrl
            || self.left_shift
            || self.right_shift
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let hook_info = lparam.0 as *const KBDLLHOOKSTRUCT;
    if hook_info.is_null() {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let info = unsafe { *hook_info };
    let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let up = matches!(wparam.0 as u32, WM_KEYUP | WM_SYSKEYUP);
    if !down && !up {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let mut swallow = false;
    ACTIVE_CAPTURE.with(|active| {
        let state_ptr = active.get();
        if state_ptr.is_null() {
            return;
        }
        let state = unsafe { &mut *state_ptr };
        if state.finished {
            return;
        }

        let foreground = unsafe { GetForegroundWindow() } == state.hwnd;
        let is_modifier = state.modifiers.update(info.vkCode, down);
        if !foreground {
            return;
        }

        if is_modifier {
            swallow = true;
            return;
        }

        if up {
            if state.captured_key_down == Some(info.vkCode) {
                state.captured_key_down = None;
                swallow = true;
            }
            return;
        }

        if info.vkCode == 0x0d && !state.modifiers.any() {
            if state.candidate.is_some() {
                let _ =
                    unsafe { PostMessageW(Some(state.hwnd), FINISH_MESSAGE, WPARAM(1), LPARAM(0)) };
            }
            swallow = true;
            return;
        }
        if info.vkCode == 0x1b && !state.modifiers.any() {
            let _ = unsafe { PostMessageW(Some(state.hwnd), FINISH_MESSAGE, WPARAM(0), LPARAM(0)) };
            swallow = true;
            return;
        }

        if let Some(key) = key_label(info.vkCode) {
            if state.modifiers.any() {
                state.candidate = Some(state.modifiers.label(&key));
            }
            state.captured_key_down = Some(info.vkCode);
            let _ = unsafe { PostMessageW(Some(state.hwnd), UPDATE_MESSAGE, WPARAM(0), LPARAM(0)) };
            swallow = true;
        }
    });

    if swallow {
        LRESULT(1)
    } else {
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe {
            &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW)
        };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
    }

    let state_ptr =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA) }
            as *mut CaptureState;
    if state_ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    match message {
        WM_CREATE => {
            let instance = unsafe { (*state_ptr).instance };
            unsafe { (*state_ptr).hwnd = hwnd };
            if !create_controls(hwnd, instance, state_ptr) {
                unsafe {
                    (*state_ptr).finished = true;
                    (*state_ptr).accepted = false;
                }
                return LRESULT(-1);
            }
            LRESULT(0)
        }
        UPDATE_MESSAGE => {
            let (label_hwnd, candidate) =
                unsafe { ((*state_ptr).label_hwnd, (*state_ptr).candidate.clone()) };
            if !label_hwnd.0.is_null() {
                let text = candidate
                    .map(|value| format!("Current shortcut: {value}"))
                    .unwrap_or_else(|| "Current shortcut: (none)".to_owned());
                let text = wide(&text);
                let _ = unsafe { SetWindowTextW(label_hwnd, PCWSTR(text.as_ptr())) };
            }
            LRESULT(0)
        }
        FINISH_MESSAGE => {
            finish_capture(state_ptr, wparam.0 != 0);
            LRESULT(0)
        }
        WM_COMMAND => {
            match wparam.0 & 0xffff {
                ID_ACCEPT => finish_capture(state_ptr, true),
                ID_CANCEL => finish_capture(state_ptr, false),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            finish_capture(state_ptr, false);
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                if !(*state_ptr).finished {
                    (*state_ptr).finished = true;
                    (*state_ptr).accepted = false;
                }
                (*state_ptr).hwnd = HWND::default();
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn finish_capture(state_ptr: *mut CaptureState, accepted: bool) {
    let hwnd = unsafe {
        let state = &mut *state_ptr;
        state.accepted = accepted && state.candidate.is_some();
        state.finished = true;
        state.hwnd
    };
    if !hwnd.0.is_null() {
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}

fn create_controls(hwnd: HWND, instance: HINSTANCE, state_ptr: *mut CaptureState) -> bool {
    let instruction = wide(
        "Press a supported key with Win, Alt, Ctrl or Shift. Enter accepts; Esc cancels.\nUse modified Enter/Esc to bind those keys.",
    );
    let current = wide("Current shortcut: (none)");
    let accept_text = wide("OK");
    let cancel_text = wide("Cancel");
    let static_class = wide("STATIC");
    let button_class = wide("BUTTON");

    let instruction_hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(static_class.as_ptr()),
            PCWSTR(instruction.as_ptr()),
            WS_CHILD | WS_VISIBLE,
            18,
            16,
            400,
            42,
            Some(hwnd),
            None,
            Some(instance),
            None,
        )
    };
    let Ok(_instruction_hwnd) = instruction_hwnd else {
        return false;
    };

    let label = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(static_class.as_ptr()),
            PCWSTR(current.as_ptr()),
            WS_CHILD | WS_VISIBLE,
            18,
            72,
            400,
            24,
            Some(hwnd),
            None,
            Some(instance),
            None,
        )
    };
    let Ok(label) = label else {
        return false;
    };
    unsafe { (*state_ptr).label_hwnd = label };

    let accept = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(button_class.as_ptr()),
            PCWSTR(accept_text.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            245,
            112,
            78,
            28,
            Some(hwnd),
            Some(HMENU(ID_ACCEPT as *mut c_void)),
            Some(instance),
            None,
        )
    };
    if accept.is_err() {
        return false;
    }

    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(button_class.as_ptr()),
            PCWSTR(cancel_text.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            332,
            112,
            78,
            28,
            Some(hwnd),
            Some(HMENU(ID_CANCEL as *mut c_void)),
            Some(instance),
            None,
        )
    }
    .is_ok()
}

fn dialog_position(owner: Option<HWND>) -> (i32, i32) {
    let mut rect = RECT::default();
    if let Some(owner) = owner {
        if unsafe { GetWindowRect(owner, &mut rect) }.is_ok()
            && rect.right > rect.left
            && rect.bottom > rect.top
        {
            return (
                rect.left + ((rect.right - rect.left) - WINDOW_WIDTH) / 2,
                rect.top + ((rect.bottom - rect.top) - WINDOW_HEIGHT) / 2,
            );
        }
    }

    (
        (unsafe { GetSystemMetrics(SM_CXSCREEN) } - WINDOW_WIDTH) / 2,
        (unsafe { GetSystemMetrics(SM_CYSCREEN) } - WINDOW_HEIGHT) / 2,
    )
}

fn key_label(vk: u32) -> Option<String> {
    match vk {
        0x30..=0x39 => char::from_u32(vk).map(|key| key.to_string()),
        0x41..=0x5a => char::from_u32(vk).map(|key| key.to_string()),
        0x70..=0x87 => Some(format!("F{}", vk - 0x6f)),
        0x20 => Some("Space".to_owned()),
        0x09 => Some("Tab".to_owned()),
        0x0d => Some("Enter".to_owned()),
        0x1b => Some("Esc".to_owned()),
        _ => None,
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
