//! Windows tray shell and caret-anchored status overlay.
//!
//! This module owns the native UI thread only. It emits user actions through
//! [`UiRuntime::events`] and accepts state changes through [`UiRuntime::send`];
//! recording, configuration persistence, clipboard work, and caret discovery
//! remain the responsibility of the parent application.

#![cfg(windows)]

mod keyboard_hook;
mod overlay;
mod overlay_paint;
mod state;
mod tray_icon;
mod tray_menu;
mod types;

use std::sync::mpsc::{Receiver, Sender};
pub use types::{
    AnchorStatus, CaretRect, HistoryMenuItem, Hotkey, HotkeyModifiers, HotkeyPurpose,
    LanguageOption, UiCommand, UiEvent, UiSettings,
};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT};
use windows::Win32::UI::Shell::{
    NIM_ADD, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFY_ICON_DATA_FLAGS, NOTIFYICONDATAW_0,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW,
    GetWindowLongPtrW, IDI_APPLICATION, LoadIconW, PostMessageW, PostQuitMessage, RegisterClassExW,
    SetWindowLongPtrW, TranslateMessage, UnregisterClassW, WM_APP, WM_CLOSE, WM_CONTEXTMENU,
    WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_DRAWITEM, WM_HOTKEY, WM_LBUTTONUP,
    WM_MEASUREITEM, WM_NCCREATE, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

use self::keyboard_hook::KEYBOARD_HOOK_STATE;
use self::overlay::{
    OVERLAY_WAVE_BARS, overlay_dpi_scale, overlay_window_height, overlay_window_width,
};
use self::state::UiState;
use self::tray_icon::{
    TrayState, TrayTheme, load_tray_icon, setting_change_is_immersive_color,
    system_uses_light_theme, tray_icon_data, tray_icon_id, tray_off_reason, tray_tooltip_text,
};
use self::tray_menu::{draw_usage_header, measure_usage_header};
use std::mem::size_of;
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Instant;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::core::PCWSTR;

const WINDOW_CLASS: &str = "Scribetray.NativeShell.0";
const TITLE: &str = "Scribetray";
const TRAY_ICON_ID: u32 = 1;
const TRAY_CALLBACK: u32 = WM_APP + 1;
const WAKE_COMMANDS: u32 = WM_APP + 2;
const RELEASE_ENTER_HOOK: u32 = WM_APP + 3;
const HOTKEY_TOGGLE_ID: i32 = 0x5343;
const HOTKEY_ESCAPE_ID: i32 = 0x5345;
/// Ticks the tray tooltip once per second while recording.
const TIMER_RECORDING: usize = 0x5343;
/// Advances the caret overlay animation while it is visible.
const TIMER_OVERLAY_ANIMATION: usize = 0x5344;
/// Samples the live level meter while the caret overlay is visible.
const TIMER_OVERLAY_METER: usize = 0x5345;
const OVERLAY_ANIMATION_MS: u32 = 33;
const OVERLAY_METER_MS: u32 = 50;
const OVERLAY_RECORDING_TICK_MS: u32 = 1000;

/// Owns the native UI thread, user-event receiver, and command channel.
pub struct UiRuntime {
    command_tx: Sender<UiCommand>,
    /// Receive tray and hotkey actions on the parent application's event loop.
    pub events: Receiver<UiEvent>,
    hwnd: HWND,
    thread: Option<JoinHandle<()>>,
}

impl UiRuntime {
    /// Starts the hidden tray window, global hotkeys, and non-activating overlay.
    pub fn start(settings: UiSettings) -> Result<Self, String> {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("scribetray-windows-ui".to_owned())
            .spawn(move || run_ui_thread(settings, command_rx, event_tx, ready_tx))
            .map_err(|error| format!("could not start the Windows UI thread: {error}"))?;

        match ready_rx.recv() {
            Ok(Ok(hwnd)) => Ok(Self {
                command_tx,
                events: event_rx,
                hwnd: HWND(hwnd as *mut core::ffi::c_void),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                let _ = thread.join();
                Err(format!("Windows UI thread stopped during startup: {error}"))
            }
        }
    }

    /// Queues a command for the UI thread and wakes its native message pump.
    pub fn send(&self, command: UiCommand) -> Result<(), String> {
        self.command_tx
            .send(command)
            .map_err(|_| "Windows UI thread is no longer running".to_owned())?;
        unsafe { PostMessageW(Some(self.hwnd), WAKE_COMMANDS, WPARAM(0), LPARAM(0)) }
            .map_err(|error| format!("could not wake the Windows UI thread: {error}"))
    }

    /// Moves and updates the caret-anchored status overlay.
    pub fn update_caret_anchor(&self, rect: CaretRect, status: AnchorStatus) -> Result<(), String> {
        self.send(UiCommand::UpdateAnchor { rect, status })
    }

    /// Hides the caret-anchored status overlay.
    pub fn hide_caret_anchor(&self) -> Result<(), String> {
        self.send(UiCommand::HideAnchor)
    }

    /// Raw hidden tray HWND for native modal dialogs owned by the application.
    pub fn native_window_handle(&self) -> isize {
        self.hwnd.0 as isize
    }
}

impl Drop for UiRuntime {
    fn drop(&mut self) {
        let _ = self.command_tx.send(UiCommand::Exit);
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WAKE_COMMANDS, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_ui_thread(
    settings: UiSettings,
    commands: Receiver<UiCommand>,
    events: Sender<UiEvent>,
    ready: mpsc::SyncSender<Result<isize, String>>,
) {
    let mut state = match create_ui_state(settings, commands, events) {
        Ok(state) => state,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    let hwnd = state.tray_hwnd.0 as isize;
    state.register_hotkeys();
    state.update_keyboard_hook();
    state.refresh_tray();
    let _ = ready.send(Ok(hwnd));

    loop {
        let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    state.cleanup();
}

fn create_ui_state(
    settings: UiSettings,
    commands: Receiver<UiCommand>,
    events: Sender<UiEvent>,
) -> Result<Box<UiState>, String> {
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| format!("could not locate the application module: {error}"))?;
    let instance = HINSTANCE(module.0);
    let mut class_name = wide(WINDOW_CLASS);
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: PCWSTR(class_name.as_mut_ptr()),
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        return Err(format!(
            "could not register the Scribetray window class: {}",
            std::io::Error::last_os_error()
        ));
    }

    let theme = if system_uses_light_theme() {
        TrayTheme::Light
    } else {
        TrayTheme::Dark
    };
    // Assume the hotkey will register; only a missing input device is known here.
    let initial_state = if settings.microphones.is_empty() {
        TrayState::Off
    } else {
        TrayState::Idle
    };
    // Prefer the embedded tray icons; fall back to the shared application icon.
    let (icon, icon_owned) = match load_tray_icon(instance, tray_icon_id(initial_state, theme)) {
        Some(pair) => pair,
        None => match unsafe { LoadIconW(None, IDI_APPLICATION) } {
            Ok(icon) => (icon, false),
            Err(error) => {
                let _ = unsafe { UnregisterClassW(PCWSTR(class_name.as_ptr()), Some(instance)) };
                return Err(format!("could not load the tray icon: {error}"));
            }
        },
    };

    let initial_tooltip = tray_tooltip_text(
        initial_state,
        &settings.toggle_hotkey,
        None,
        tray_off_reason(&settings, false),
    );
    let mut state = Box::new(UiState {
        commands,
        events,
        settings,
        usage: None,
        tray_hwnd: HWND::default(),
        overlay_hwnd: HWND::default(),
        instance,
        class_name,
        icon_added: false,
        tray_icon: icon,
        tray_icon_owned: icon_owned,
        tray_state: initial_state,
        tray_theme: theme,
        tray_tooltip: initial_tooltip.clone(),
        tray_error: false,
        transcribing: 0,
        level_meter: None,
        overlay_levels: [0.0; OVERLAY_WAVE_BARS],
        overlay_shown: [0.0; OVERLAY_WAVE_BARS],
        overlay_visible: false,
        overlay_dpi_scale: 1.0,
        overlay_shown_at: Instant::now(),
        overlay_state_since: Instant::now(),
        toggle_registered: false,
        escape_registered: false,
        recording: false,
        shutting_down: false,
        anchor_rect: None,
        anchor_status: None,
        recording_started: None,
        keyboard_hook: None,
    });
    let state_ptr = (&mut *state as *mut UiState).cast::<core::ffi::c_void>();
    let class_name = PCWSTR(state.class_name.as_ptr());
    let title = wide(TITLE);

    let tray_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name,
            PCWSTR(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            Some(state_ptr),
        )
    }
    .map_err(|error| {
        let _ = unsafe { UnregisterClassW(PCWSTR(state.class_name.as_ptr()), Some(instance)) };
        format!("could not create the hidden tray window: {error}")
    })?;
    state.tray_hwnd = tray_hwnd;

    let overlay_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name,
            PCWSTR(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            overlay_window_width(1.0, false),
            overlay_window_height(1.0),
            None,
            None,
            Some(instance),
            Some(state_ptr),
        )
    }
    .map_err(|error| {
        unsafe {
            let _ = DestroyWindow(tray_hwnd);
            let _ = UnregisterClassW(PCWSTR(state.class_name.as_ptr()), Some(instance));
        }
        format!("could not create the status overlay window: {error}")
    })?;
    state.overlay_hwnd = overlay_hwnd;

    if !unsafe { Shell_NotifyIconW(NIM_ADD, &tray_icon_data(tray_hwnd, icon, &initial_tooltip)) }
        .as_bool()
    {
        unsafe {
            let _ = DestroyWindow(overlay_hwnd);
            let _ = DestroyWindow(tray_hwnd);
            let _ = UnregisterClassW(PCWSTR(state.class_name.as_ptr()), Some(instance));
        }
        return Err("Windows could not add the Scribetray tray icon".to_owned());
    }
    state.icon_added = true;

    let mut version = tray_icon_data(tray_hwnd, icon, &initial_tooltip);
    version.uFlags = NOTIFY_ICON_DATA_FLAGS(0);
    version.Anonymous = NOTIFYICONDATAW_0 { uVersion: 4 };
    let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &version) };
    // Version-4 tooltips require NIF_SHOWTIP to be applied through NIM_MODIFY.
    let refresh = tray_icon_data(tray_hwnd, icon, &initial_tooltip);
    let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &refresh) };
    Ok(state)
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
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        return LRESULT(1);
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut UiState;
    if state_ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    if message == WM_MEASUREITEM && lparam.0 != 0 {
        let item = unsafe { &mut *(lparam.0 as *mut MEASUREITEMSTRUCT) };
        if unsafe { measure_usage_header(item) } {
            return LRESULT(1);
        }
    }
    if message == WM_DRAWITEM && lparam.0 != 0 {
        let item = unsafe { &*(lparam.0 as *const DRAWITEMSTRUCT) };
        if unsafe { draw_usage_header(item) } {
            return LRESULT(1);
        }
    }
    let state = unsafe { &mut *state_ptr };
    let is_overlay = hwnd == state.overlay_hwnd;

    if is_overlay {
        match message {
            WM_TIMER if wparam.0 == TIMER_OVERLAY_ANIMATION => {
                state.advance_overlay_animation();
                return LRESULT(0);
            }
            WM_TIMER if wparam.0 == TIMER_OVERLAY_METER => {
                state.sample_overlay_meter();
                return LRESULT(0);
            }
            WM_TIMER if wparam.0 == TIMER_RECORDING => {
                // Refresh the elapsed-time tooltip; the animation timer repaints.
                state.reposition_overlay();
                state.render_overlay();
                state.refresh_tray();
                return LRESULT(0);
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED => {
                if let Some(rect) = state.anchor_rect {
                    state.overlay_dpi_scale = overlay_dpi_scale(rect);
                    state.reposition_overlay();
                    state.render_overlay();
                }
                return LRESULT(0);
            }
            _ => {}
        }
    } else {
        if message == TRAY_CALLBACK {
            match lparam.0 as u32 & 0xffff {
                WM_RBUTTONUP | WM_CONTEXTMENU => state.show_menu(),
                WM_LBUTTONUP | NIN_SELECT => state.handle_user_event(UiEvent::RecoverLast),
                _ => {}
            }
            return LRESULT(0);
        }
        match message {
            WAKE_COMMANDS => {
                while let Ok(command) = state.commands.try_recv() {
                    state.apply_command(command);
                    if state.shutting_down {
                        break;
                    }
                }
                return LRESULT(0);
            }
            WM_SETTINGCHANGE => {
                if setting_change_is_immersive_color(lparam) {
                    state.refresh_tray();
                }
                return LRESULT(0);
            }
            WM_HOTKEY => {
                match wparam.0 as i32 {
                    HOTKEY_TOGGLE_ID => state.handle_user_event(UiEvent::ToggleRecord),
                    HOTKEY_ESCAPE_ID => {
                        state.unregister_escape();
                        state.recording = false;
                        KEYBOARD_HOOK_STATE.with(|hook_state| {
                            if let Some(hook_state) = hook_state.borrow_mut().as_mut() {
                                hook_state.recording = false;
                            }
                        });
                        state.update_keyboard_hook();
                        state.handle_user_event(UiEvent::CancelRecord);
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
            RELEASE_ENTER_HOOK => {
                state.update_keyboard_hook();
                return LRESULT(0);
            }
            WM_CLOSE => {
                state.handle_user_event(UiEvent::Quit);
                return LRESULT(0);
            }
            WM_DESTROY => {
                if hwnd == state.tray_hwnd {
                    unsafe { PostQuitMessage(0) };
                }
                return LRESULT(0);
            }
            _ => {}
        }
    }

    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn copy_wide<const N: usize>(text: &str, destination: &mut [u16; N]) {
    let mut index = 0;
    for unit in text.encode_utf16() {
        if index >= N.saturating_sub(1) {
            break;
        }
        destination[index] = unit;
        index += 1;
    }
    destination[index] = 0;
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
