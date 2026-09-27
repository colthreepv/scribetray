//! Windows tray shell and caret-anchored status overlay.
//!
//! This module owns the native UI thread only. It emits user actions through
//! [`UiRuntime::events`] and accepts state changes through [`UiRuntime::send`];
//! recording, configuration persistence, clipboard work, and caret discovery
//! remain the responsibility of the parent application.

#![cfg(windows)]

use std::{
    collections::HashMap,
    mem::size_of,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use tracing::info;
use windows::{
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, Ellipse, EndPaint, FillRect,
            HDC, HGDIOBJ, InvalidateRect, LineTo, MoveToEx, PAINTSTRUCT, PS_SOLID, RoundRect,
            SelectObject, SetBkMode, SetTextColor, TRANSPARENT, TextOutW,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{
                MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
                UnregisterHotKey, VK_ESCAPE,
            },
            Shell::{
                NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
                NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFY_ICON_DATA_FLAGS, NOTIFYICONDATAW,
                NOTIFYICONDATAW_0, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetCursorPos, GetMessageW,
                GetSystemMetrics, GetWindowLongPtrW, HWND_TOPMOST, IDI_APPLICATION, KillTimer,
                LWA_COLORKEY, LoadIconW, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING,
                PostMessageW, PostQuitMessage, RegisterClassExW, SM_CXVIRTUALSCREEN,
                SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOWNOACTIVATE,
                SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_SHOWWINDOW, SetForegroundWindow,
                SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
                TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
                UnregisterClassW, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE,
                WM_HOTKEY, WM_LBUTTONUP, WM_NCCREATE, WM_PAINT, WM_RBUTTONUP, WM_TIMER,
                WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
                WS_POPUP,
            },
        },
    },
    core::PCWSTR,
};

const WINDOW_CLASS: &str = "Scribetray.NativeShell.0";
const TITLE: &str = "Scribetray";
const TRAY_ICON_ID: u32 = 1;
const TRAY_CALLBACK: u32 = WM_APP + 1;
const WAKE_COMMANDS: u32 = WM_APP + 2;
const HOTKEY_TOGGLE_ID: i32 = 0x5343;
const HOTKEY_SUBMIT_ID: i32 = 0x5344;
const HOTKEY_ESCAPE_ID: i32 = 0x5345;
const TIMER_RECORDING: usize = 0x5343;
const OVERLAY_WIDTH: i32 = 152;
const OVERLAY_HEIGHT: i32 = 44;
const COLOR_KEY: COLORREF = COLORREF(0x00ff00ff);

/// Modifier keys for a configurable global hotkey.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotkeyModifiers {
    pub win: bool,
    pub alt: bool,
    pub control: bool,
    pub shift: bool,
}

impl Default for HotkeyModifiers {
    fn default() -> Self {
        Self {
            win: true,
            alt: true,
            control: false,
            shift: false,
        }
    }
}

impl HotkeyModifiers {
    /// Creates a Win+Alt hotkey with optional Shift and Ctrl modifiers.
    /// The arguments are ordered as Shift, then Ctrl.
    pub const fn new(shift: bool, control: bool) -> Self {
        Self {
            win: true,
            alt: true,
            control,
            shift,
        }
    }

    /// Creates a hotkey with explicit Win, Alt, Shift, and Ctrl modifiers.
    pub const fn with_all_modifiers(win: bool, alt: bool, shift: bool, control: bool) -> Self {
        Self {
            win,
            alt,
            control,
            shift,
        }
    }
}

/// A configurable Win+Alt hotkey. `virtual_key` is a Win32 virtual-key code;
/// `label` is its display name, such as `V` or `F8`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub virtual_key: u32,
    pub label: String,
    pub modifiers: HotkeyModifiers,
}

impl Hotkey {
    /// Creates a Win+Alt+key hotkey without optional modifiers.
    pub fn new(virtual_key: u32, label: impl Into<String>) -> Self {
        Self {
            virtual_key,
            label: label.into(),
            modifiers: HotkeyModifiers::default(),
        }
    }

    /// Creates a hotkey with optional Ctrl and Shift modifiers.
    pub fn with_modifiers(
        virtual_key: u32,
        label: impl Into<String>,
        modifiers: HotkeyModifiers,
    ) -> Self {
        Self {
            virtual_key,
            label: label.into(),
            modifiers,
        }
    }

    /// Returns the complete configured chord as it appears in notices.
    pub fn combo_label(&self) -> String {
        let mut parts = Vec::with_capacity(5);
        if self.modifiers.win {
            parts.push("Win");
        }
        if self.modifiers.alt {
            parts.push("Alt");
        }
        if self.modifiers.control {
            parts.push("Ctrl");
        }
        if self.modifiers.shift {
            parts.push("Shift");
        }
        parts.push(&self.label);
        parts.join("+")
    }

    fn win32_modifiers(&self) -> windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS {
        let mut modifiers = MOD_NOREPEAT;
        if self.modifiers.win {
            modifiers |= MOD_WIN;
        }
        if self.modifiers.alt {
            modifiers |= MOD_ALT;
        }
        if self.modifiers.control {
            modifiers |= MOD_CONTROL;
        }
        if self.modifiers.shift {
            modifiers |= MOD_SHIFT;
        }
        modifiers
    }
}

impl Default for Hotkey {
    fn default() -> Self {
        Self::new(b'V' as u32, "V")
    }
}

/// One entry in the tray's language submenu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageOption {
    pub code: String,
    pub label: String,
}

/// A compact history-menu snapshot supplied by the parent application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryMenuItem {
    pub id: String,
    pub label: String,
    pub can_copy: bool,
    pub can_retry: bool,
}

/// Initial UI state and the data used to build tray menus.
#[derive(Clone, Debug)]
pub struct UiSettings {
    pub toggle_hotkey: Hotkey,
    pub submit_hotkey: Hotkey,
    pub prefix_enabled: bool,
    pub auto_enter: bool,
    pub sound_enabled: bool,
    pub type_mode: bool,
    pub autostart_enabled: bool,
    pub language_code: String,
    pub languages: Vec<LanguageOption>,
    pub history: Vec<HistoryMenuItem>,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            toggle_hotkey: Hotkey::default(),
            submit_hotkey: Hotkey::with_modifiers(
                b'V' as u32,
                "V",
                HotkeyModifiers::new(true, false),
            ),
            prefix_enabled: true,
            auto_enter: false,
            sound_enabled: true,
            type_mode: false,
            autostart_enabled: false,
            language_code: "auto".to_owned(),
            languages: vec![LanguageOption {
                code: "auto".to_owned(),
                label: "Automatic detection".to_owned(),
            }],
            history: Vec::new(),
        }
    }
}

/// Screen-pixel caret rectangle supplied by the parent caret detector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaretRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl CaretRect {
    pub fn from_edges(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            width: right.saturating_sub(left),
            height: bottom.saturating_sub(top),
        }
    }

    fn is_valid(self) -> bool {
        self.width >= 0 && self.height >= 0
    }
}

/// Visual state displayed by the caret anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorStatus {
    Recording { elapsed: Duration },
    Working,
    Done,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyPurpose {
    ToggleRecording,
    SubmitRecording,
}

/// Actions initiated by the user through the tray or registered hotkeys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiEvent {
    ToggleRecord,
    SubmitHotkeyRecord,
    CancelRecord,
    Quit,
    OpenConfig,
    TogglePrefix,
    ToggleAutoEnter,
    ToggleSound,
    ToggleTypeMode,
    ToggleAutostart,
    LanguageSelected(String),
    HistoryCopy(String),
    HistoryRetry(String),
    HotkeyRegistrationFailed {
        purpose: HotkeyPurpose,
        hotkey: Hotkey,
        error: String,
    },
    EscapeRegistrationFailed {
        error: String,
    },
}

/// Commands sent to the native UI thread by the parent application.
#[derive(Clone, Debug)]
pub enum UiCommand {
    SetSettings(UiSettings),
    UpdateHistory(Vec<HistoryMenuItem>),
    SetRecording(bool),
    UpdateAnchor {
        rect: CaretRect,
        status: AnchorStatus,
    },
    HideAnchor,
    Notice {
        title: String,
        message: String,
    },
    Exit,
}

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

struct UiState {
    commands: Receiver<UiCommand>,
    events: Sender<UiEvent>,
    settings: UiSettings,
    tray_hwnd: HWND,
    overlay_hwnd: HWND,
    instance: HINSTANCE,
    class_name: Vec<u16>,
    icon_added: bool,
    toggle_registered: bool,
    submit_registered: bool,
    escape_registered: bool,
    recording: bool,
    shutting_down: bool,
    anchor_rect: Option<CaretRect>,
    anchor_status: Option<AnchorStatus>,
    recording_started: Option<Instant>,
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

    // `winresource::set_icon` embeds the application's icon as resource ID 1.
    let icon_resource = PCWSTR(1_usize as *const u16);
    let icon = match unsafe { LoadIconW(Some(instance), icon_resource) }
        .or_else(|_| unsafe { LoadIconW(None, IDI_APPLICATION) })
    {
        Ok(icon) => icon,
        Err(error) => {
            let _ = unsafe { UnregisterClassW(PCWSTR(class_name.as_ptr()), Some(instance)) };
            return Err(format!("could not load the tray icon: {error}"));
        }
    };

    let mut state = Box::new(UiState {
        commands,
        events,
        settings,
        tray_hwnd: HWND::default(),
        overlay_hwnd: HWND::default(),
        instance,
        class_name,
        icon_added: false,
        toggle_registered: false,
        submit_registered: false,
        escape_registered: false,
        recording: false,
        shutting_down: false,
        anchor_rect: None,
        anchor_status: None,
        recording_started: None,
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
            OVERLAY_WIDTH,
            OVERLAY_HEIGHT,
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

    unsafe { SetLayeredWindowAttributes(overlay_hwnd, COLOR_KEY, 255, LWA_COLORKEY) }.map_err(
        |error| {
            unsafe {
                let _ = DestroyWindow(overlay_hwnd);
                let _ = DestroyWindow(tray_hwnd);
                let _ = UnregisterClassW(PCWSTR(state.class_name.as_ptr()), Some(instance));
            }
            format!("could not configure the status overlay: {error}")
        },
    )?;

    if !unsafe { Shell_NotifyIconW(NIM_ADD, &tray_icon_data(tray_hwnd, icon)) }.as_bool() {
        unsafe {
            let _ = DestroyWindow(overlay_hwnd);
            let _ = DestroyWindow(tray_hwnd);
            let _ = UnregisterClassW(PCWSTR(state.class_name.as_ptr()), Some(instance));
        }
        return Err("Windows could not add the Scribetray tray icon".to_owned());
    }
    state.icon_added = true;

    let mut version = tray_icon_data(tray_hwnd, icon);
    version.uFlags = NOTIFY_ICON_DATA_FLAGS(0);
    version.Anonymous = NOTIFYICONDATAW_0 { uVersion: 4 };
    let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &version) };
    Ok(state)
}

impl UiState {
    fn register_hotkeys(&mut self) {
        self.unregister_record_hotkeys();
        let toggle = self.settings.toggle_hotkey.clone();
        let submit = self.settings.submit_hotkey.clone();
        self.toggle_registered = self.register_record_hotkey(
            HOTKEY_TOGGLE_ID,
            HotkeyPurpose::ToggleRecording,
            toggle.clone(),
        );

        if toggle.virtual_key == submit.virtual_key && toggle.modifiers == submit.modifiers {
            self.hotkey_failure(
                HotkeyPurpose::SubmitRecording,
                submit,
                "the submit hotkey matches the toggle hotkey".to_owned(),
            );
            self.submit_registered = false;
        } else {
            self.submit_registered = self.register_record_hotkey(
                HOTKEY_SUBMIT_ID,
                HotkeyPurpose::SubmitRecording,
                submit,
            );
        }
    }

    fn register_record_hotkey(&mut self, id: i32, purpose: HotkeyPurpose, hotkey: Hotkey) -> bool {
        match unsafe {
            RegisterHotKey(
                Some(self.tray_hwnd),
                id,
                hotkey.win32_modifiers(),
                hotkey.virtual_key,
            )
        } {
            Ok(()) => {
                info!("registered {} hotkey", hotkey.combo_label());
                true
            }
            Err(error) => {
                self.hotkey_failure(purpose, hotkey, error.to_string());
                false
            }
        }
    }

    fn hotkey_failure(&self, purpose: HotkeyPurpose, hotkey: Hotkey, error: String) {
        let purpose_label = match purpose {
            HotkeyPurpose::ToggleRecording => "record toggle",
            HotkeyPurpose::SubmitRecording => "record submit",
        };
        let details = format!(
            "Could not register {purpose_label} hotkey {}: {error}. Choose another key in settings.",
            hotkey.combo_label()
        );
        let _ = self.events.send(UiEvent::HotkeyRegistrationFailed {
            purpose,
            hotkey,
            error: details.clone(),
        });
        self.show_notice(TITLE, &details);
    }

    fn unregister_record_hotkeys(&mut self) {
        if self.toggle_registered {
            let _ = unsafe { UnregisterHotKey(Some(self.tray_hwnd), HOTKEY_TOGGLE_ID) };
            self.toggle_registered = false;
        }
        if self.submit_registered {
            let _ = unsafe { UnregisterHotKey(Some(self.tray_hwnd), HOTKEY_SUBMIT_ID) };
            self.submit_registered = false;
        }
    }

    fn set_recording(&mut self, recording: bool) {
        if self.recording == recording {
            return;
        }
        self.recording = recording;
        if recording {
            match unsafe {
                RegisterHotKey(
                    Some(self.tray_hwnd),
                    HOTKEY_ESCAPE_ID,
                    MOD_NOREPEAT,
                    VK_ESCAPE.0 as u32,
                )
            } {
                Ok(()) => {
                    self.escape_registered = true;
                    info!("registered Escape recording cancel hotkey");
                }
                Err(error) => {
                    let message = format!(
                        "Could not register Escape to cancel recording: {error}. Use the tray menu to stop recording."
                    );
                    let _ = self.events.send(UiEvent::EscapeRegistrationFailed {
                        error: error.to_string(),
                    });
                    self.show_notice(TITLE, &message);
                }
            }
        } else {
            self.unregister_escape();
        }
    }

    fn unregister_escape(&mut self) {
        if self.escape_registered {
            let _ = unsafe { UnregisterHotKey(Some(self.tray_hwnd), HOTKEY_ESCAPE_ID) };
            self.escape_registered = false;
        }
    }

    fn apply_command(&mut self, command: UiCommand) {
        match command {
            UiCommand::SetSettings(settings) => {
                let hotkeys_changed = self.settings.toggle_hotkey != settings.toggle_hotkey
                    || self.settings.submit_hotkey != settings.submit_hotkey;
                self.settings = settings;
                if hotkeys_changed {
                    self.register_hotkeys();
                }
            }
            UiCommand::UpdateHistory(history) => self.settings.history = history,
            UiCommand::SetRecording(recording) => self.set_recording(recording),
            UiCommand::UpdateAnchor { rect, status } => self.show_anchor(rect, status),
            UiCommand::HideAnchor => self.hide_anchor(),
            UiCommand::Notice { title, message } => self.show_notice(&title, &message),
            UiCommand::Exit => self.shutdown(),
        }
    }

    fn show_anchor(&mut self, rect: CaretRect, status: AnchorStatus) {
        if !rect.is_valid() {
            self.hide_anchor();
            return;
        }
        self.anchor_rect = Some(rect);
        self.anchor_status = Some(status);
        self.recording_started = match status {
            AnchorStatus::Recording { elapsed } => Instant::now().checked_sub(elapsed),
            _ => None,
        };
        unsafe {
            if self.recording_started.is_some() {
                if SetTimer(Some(self.overlay_hwnd), TIMER_RECORDING, 1000, None) == 0 {
                    self.show_notice(TITLE, "The recording timer could not be started.");
                }
            } else {
                let _ = KillTimer(Some(self.overlay_hwnd), TIMER_RECORDING);
            }
            let (left, top) = overlay_position(rect);
            let _ = SetWindowPos(
                self.overlay_hwnd,
                Some(HWND_TOPMOST),
                left,
                top,
                OVERLAY_WIDTH,
                OVERLAY_HEIGHT,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
            );
            let _ = ShowWindow(self.overlay_hwnd, SW_SHOWNOACTIVATE);
            let _ = InvalidateRect(Some(self.overlay_hwnd), None, true);
        }
    }

    fn hide_anchor(&mut self) {
        self.anchor_rect = None;
        self.anchor_status = None;
        self.recording_started = None;
        unsafe {
            let _ = KillTimer(Some(self.overlay_hwnd), TIMER_RECORDING);
            let _ = ShowWindow(
                self.overlay_hwnd,
                windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
            );
        }
    }

    fn show_notice(&self, title: &str, message: &str) {
        let mut data = NOTIFYICONDATAW::default();
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.tray_hwnd;
        data.uID = TRAY_ICON_ID;
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_INFO;
        copy_wide(title, &mut data.szInfoTitle);
        copy_wide(message, &mut data.szInfo);
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }

    fn show_menu(&mut self) {
        let (menu, actions) = match build_tray_menu(&self.settings, self.recording) {
            Ok(menu) => menu,
            Err(error) => {
                self.show_notice(TITLE, &format!("Could not build tray menu: {error}"));
                return;
            }
        };
        let mut cursor = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut cursor);
            let _ = SetForegroundWindow(self.tray_hwnd);
        }
        let selected = unsafe {
            TrackPopupMenu(
                menu.0,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                cursor.x,
                cursor.y,
                None,
                self.tray_hwnd,
                None,
            )
        };
        unsafe {
            let _ = PostMessageW(
                Some(self.tray_hwnd),
                windows::Win32::UI::WindowsAndMessaging::WM_NULL,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(event) = actions.get(&(selected.0 as u32)) {
            self.handle_user_event(event.clone());
        }
    }

    fn handle_user_event(&mut self, event: UiEvent) {
        if matches!(event, UiEvent::Quit) {
            self.shutting_down = true;
            let _ = self.events.send(event);
            unsafe { PostQuitMessage(0) };
            return;
        }
        let _ = self.events.send(event);
    }

    fn shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        unsafe { PostQuitMessage(0) };
    }

    fn cleanup(&mut self) {
        self.unregister_escape();
        self.unregister_record_hotkeys();
        unsafe {
            if !self.overlay_hwnd.0.is_null() {
                let _ = KillTimer(Some(self.overlay_hwnd), TIMER_RECORDING);
            }
            if self.icon_added {
                let mut data = NOTIFYICONDATAW::default();
                data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
                data.hWnd = self.tray_hwnd;
                data.uID = TRAY_ICON_ID;
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                self.icon_added = false;
            }
            if !self.overlay_hwnd.0.is_null() {
                let _ = DestroyWindow(self.overlay_hwnd);
                self.overlay_hwnd = HWND::default();
            }
            if !self.tray_hwnd.0.is_null() {
                let _ = DestroyWindow(self.tray_hwnd);
                self.tray_hwnd = HWND::default();
            }
            let _ = UnregisterClassW(PCWSTR(self.class_name.as_ptr()), Some(self.instance));
        }
    }
}

fn overlay_position(caret: CaretRect) -> (i32, i32) {
    let left_edge = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top_edge = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let screen_width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(OVERLAY_WIDTH);
    let screen_height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(OVERLAY_HEIGHT);
    let right_edge = left_edge.saturating_add(screen_width);
    let bottom_edge = top_edge.saturating_add(screen_height);
    let mut left = caret.left;
    let mut top = caret.top.saturating_add(caret.height).saturating_add(10);
    if top.saturating_add(OVERLAY_HEIGHT) > bottom_edge {
        top = caret.top.saturating_sub(OVERLAY_HEIGHT + 10);
    }
    left = left.clamp(left_edge, right_edge.saturating_sub(OVERLAY_WIDTH));
    top = top.clamp(top_edge, bottom_edge.saturating_sub(OVERLAY_HEIGHT));
    (left, top)
}

struct MenuGuard(windows::Win32::UI::WindowsAndMessaging::HMENU);

impl Drop for MenuGuard {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            let _ = unsafe { DestroyMenu(self.0) };
        }
    }
}

fn build_tray_menu(
    settings: &UiSettings,
    recording: bool,
) -> Result<(MenuGuard, HashMap<u32, UiEvent>), String> {
    use windows::Win32::UI::WindowsAndMessaging::{MF_GRAYED, MF_UNCHECKED};

    let root = MenuGuard(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);
    let mut actions = HashMap::new();
    let mut next_dynamic_id = 100_u32;
    append_action(
        root.0,
        if recording {
            "Stop recording"
        } else {
            "Start recording"
        },
        1,
        UiEvent::ToggleRecord,
        &mut actions,
    )?;
    append_separator(root.0)?;
    append_action(
        root.0,
        "Settings and hotkeys…",
        2,
        UiEvent::OpenConfig,
        &mut actions,
    )?;
    append_check_action(
        root.0,
        "Emoji prefix",
        4,
        settings.prefix_enabled,
        UiEvent::TogglePrefix,
        &mut actions,
    )?;
    append_check_action(
        root.0,
        "Auto-Enter",
        5,
        settings.auto_enter,
        UiEvent::ToggleAutoEnter,
        &mut actions,
    )?;
    append_check_action(
        root.0,
        "Sound cues",
        6,
        settings.sound_enabled,
        UiEvent::ToggleSound,
        &mut actions,
    )?;
    append_check_action(
        root.0,
        "Type mode",
        7,
        settings.type_mode,
        UiEvent::ToggleTypeMode,
        &mut actions,
    )?;
    append_check_action(
        root.0,
        "Start with Windows",
        9,
        settings.autostart_enabled,
        UiEvent::ToggleAutostart,
        &mut actions,
    )?;

    let language_menu = MenuGuard(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);
    if settings.languages.is_empty() {
        append_flags(language_menu.0, MF_GRAYED, 0, "No languages configured")?;
    } else {
        for language in &settings.languages {
            let id = take_menu_id(&mut next_dynamic_id)?;
            let flags = if language.code == settings.language_code {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING | MF_UNCHECKED
            };
            append_flags(language_menu.0, flags, id as usize, &language.label)?;
            actions.insert(id, UiEvent::LanguageSelected(language.code.clone()));
        }
    }
    append_popup(root.0, language_menu, "Language")?;

    let history_menu = MenuGuard(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);
    if settings.history.is_empty() {
        append_flags(history_menu.0, MF_GRAYED, 0, "No recordings yet")?;
    } else {
        for item in &settings.history {
            let item_menu =
                MenuGuard(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);
            if item.can_copy {
                let id = take_menu_id(&mut next_dynamic_id)?;
                append_action(
                    item_menu.0,
                    "Copy transcript",
                    id,
                    UiEvent::HistoryCopy(item.id.clone()),
                    &mut actions,
                )?;
            }
            if item.can_retry {
                let id = take_menu_id(&mut next_dynamic_id)?;
                append_action(
                    item_menu.0,
                    "Retry transcription",
                    id,
                    UiEvent::HistoryRetry(item.id.clone()),
                    &mut actions,
                )?;
            }
            if !item.can_copy && !item.can_retry {
                append_flags(item_menu.0, MF_GRAYED, 0, "No available action")?;
            }
            append_popup(history_menu.0, item_menu, &truncate_menu_label(&item.label))?;
        }
    }
    append_popup(root.0, history_menu, "History")?;
    append_separator(root.0)?;
    append_action(root.0, "Quit", 8, UiEvent::Quit, &mut actions)?;
    Ok((root, actions))
}

fn append_action(
    menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    label: &str,
    id: u32,
    action: UiEvent,
    actions: &mut HashMap<u32, UiEvent>,
) -> Result<(), String> {
    append_flags(menu, MF_STRING, id as usize, label)?;
    actions.insert(id, action);
    Ok(())
}

fn take_menu_id(next: &mut u32) -> Result<u32, String> {
    let id = *next;
    *next = next
        .checked_add(1)
        .ok_or_else(|| "too many language or history menu entries".to_owned())?;
    Ok(id)
}

fn append_check_action(
    menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    label: &str,
    id: u32,
    checked: bool,
    action: UiEvent,
    actions: &mut HashMap<u32, UiEvent>,
) -> Result<(), String> {
    let flags = if checked {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING | windows::Win32::UI::WindowsAndMessaging::MF_UNCHECKED
    };
    append_flags(menu, flags, id as usize, label)?;
    actions.insert(id, action);
    Ok(())
}

fn append_separator(menu: windows::Win32::UI::WindowsAndMessaging::HMENU) -> Result<(), String> {
    append_flags(menu, MF_SEPARATOR, 0, "")
}

fn append_flags(
    menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    flags: windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_FLAGS,
    id: usize,
    label: &str,
) -> Result<(), String> {
    let text = wide(label);
    unsafe { AppendMenuW(menu, flags, id, PCWSTR(text.as_ptr())) }
        .map_err(|error| error.to_string())
}

fn append_popup(
    parent: windows::Win32::UI::WindowsAndMessaging::HMENU,
    mut child: MenuGuard,
    label: &str,
) -> Result<(), String> {
    let text = wide(label);
    unsafe { AppendMenuW(parent, MF_POPUP, child.0.0 as usize, PCWSTR(text.as_ptr())) }
        .map_err(|error| error.to_string())?;
    child.0 = windows::Win32::UI::WindowsAndMessaging::HMENU::default();
    Ok(())
}

fn truncate_menu_label(label: &str) -> String {
    const MAX_UNITS: usize = 72;
    let mut output = String::new();
    let mut units = 0;
    for character in label.chars() {
        let character_units = character.len_utf16();
        if units + character_units > MAX_UNITS {
            output.push('…');
            break;
        }
        output.push(character);
        units += character_units;
    }
    output
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
    let state = unsafe { &mut *state_ptr };
    let is_overlay = hwnd == state.overlay_hwnd;

    if is_overlay {
        match message {
            WM_PAINT => {
                paint_overlay(hwnd, state);
                return LRESULT(0);
            }
            WM_TIMER if wparam.0 == TIMER_RECORDING => {
                if let Some(started) = state.recording_started {
                    state.anchor_status = Some(AnchorStatus::Recording {
                        elapsed: started.elapsed(),
                    });
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                return LRESULT(0);
            }
            WM_DISPLAYCHANGE => {
                if let Some(rect) = state.anchor_rect {
                    let (left, top) = overlay_position(rect);
                    unsafe {
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            left,
                            top,
                            OVERLAY_WIDTH,
                            OVERLAY_HEIGHT,
                            SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                        );
                    }
                }
                return LRESULT(0);
            }
            _ => {}
        }
    } else {
        if message == TRAY_CALLBACK {
            match lparam.0 as u32 & 0xffff {
                WM_RBUTTONUP | WM_CONTEXTMENU => state.show_menu(),
                WM_LBUTTONUP | NIN_SELECT => state.handle_user_event(UiEvent::ToggleRecord),
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
            WM_HOTKEY => {
                match wparam.0 as i32 {
                    HOTKEY_TOGGLE_ID => state.handle_user_event(UiEvent::ToggleRecord),
                    HOTKEY_SUBMIT_ID => state.handle_user_event(UiEvent::SubmitHotkeyRecord),
                    HOTKEY_ESCAPE_ID => {
                        state.unregister_escape();
                        state.recording = false;
                        state.handle_user_event(UiEvent::CancelRecord);
                    }
                    _ => {}
                }
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

fn paint_overlay(hwnd: HWND, state: &UiState) {
    let mut paint = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
    if hdc.0.is_null() {
        return;
    }
    let background = unsafe { CreateSolidBrush(COLOR_KEY) };
    let full = RECT {
        left: 0,
        top: 0,
        right: OVERLAY_WIDTH,
        bottom: OVERLAY_HEIGHT,
    };
    unsafe {
        let _ = FillRect(hdc, &full, background);
        let _ = DeleteObject(HGDIOBJ(background.0));
        let _ = SetBkMode(hdc, TRANSPARENT);
    }

    if let Some(status) = state.anchor_status {
        draw_overlay_contents(hdc, status);
    }
    unsafe {
        let _ = EndPaint(hwnd, &paint);
    }
}

fn draw_overlay_contents(hdc: HDC, status: AnchorStatus) {
    let (accent, label) = match status {
        AnchorStatus::Recording { elapsed } => {
            let total = elapsed.as_secs();
            let accent = if (elapsed.as_millis() / 400).is_multiple_of(2) {
                rgb(248, 113, 113)
            } else {
                rgb(185, 28, 28)
            };
            (
                accent,
                format!("{:02}:{:02}", (total / 60) % 100, total % 60),
            )
        }
        AnchorStatus::Working => (rgb(245, 158, 11), "Working".to_owned()),
        AnchorStatus::Done => (rgb(34, 197, 94), "Done".to_owned()),
        AnchorStatus::Error => (rgb(239, 68, 68), "Error".to_owned()),
    };
    let card_brush = unsafe { CreateSolidBrush(rgb(25, 28, 36)) };
    let border_pen = unsafe { CreatePen(PS_SOLID, 1, rgb(67, 73, 85)) };
    let accent_brush = unsafe { CreateSolidBrush(accent) };
    let accent_pen = unsafe { CreatePen(PS_SOLID, 2, accent) };
    let old_brush = unsafe { SelectObject(hdc, HGDIOBJ(card_brush.0)) };
    let old_pen = unsafe { SelectObject(hdc, HGDIOBJ(border_pen.0)) };
    unsafe {
        let _ = RoundRect(hdc, 1, 1, OVERLAY_WIDTH - 1, OVERLAY_HEIGHT - 1, 14, 14);
    }

    let old_accent_brush = unsafe { SelectObject(hdc, HGDIOBJ(accent_brush.0)) };
    let old_accent_pen = unsafe { SelectObject(hdc, HGDIOBJ(accent_pen.0)) };
    unsafe {
        let _ = RoundRect(hdc, 17, 9, 27, 24, 8, 8);
        let _ = MoveToEx(hdc, 13, 20, None);
        let _ = LineTo(hdc, 13, 24);
        let _ = LineTo(hdc, 16, 28);
        let _ = LineTo(hdc, 28, 28);
        let _ = LineTo(hdc, 31, 24);
        let _ = LineTo(hdc, 31, 20);
        let _ = MoveToEx(hdc, 22, 28, None);
        let _ = LineTo(hdc, 22, 32);
        let _ = MoveToEx(hdc, 18, 32, None);
        let _ = LineTo(hdc, 26, 32);
        let _ = Ellipse(hdc, OVERLAY_WIDTH - 17, 17, OVERLAY_WIDTH - 9, 25);
    }

    unsafe {
        let _ = SelectObject(hdc, old_accent_brush);
        let _ = SelectObject(hdc, old_accent_pen);
        let _ = SelectObject(hdc, old_brush);
        let _ = SelectObject(hdc, old_pen);
        let _ = SetTextColor(hdc, rgb(245, 247, 250));
    }
    let text = wide(&label);
    unsafe {
        let _ = TextOutW(hdc, 43, 14, &text[..text.len().saturating_sub(1)]);
    }

    unsafe {
        let _ = DeleteObject(HGDIOBJ(card_brush.0));
        let _ = DeleteObject(HGDIOBJ(border_pen.0));
        let _ = DeleteObject(HGDIOBJ(accent_brush.0));
        let _ = DeleteObject(HGDIOBJ(accent_pen.0));
    }
}

fn tray_icon_data(
    hwnd: HWND,
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW::default();
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_ICON_ID;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = TRAY_CALLBACK;
    data.hIcon = icon;
    copy_wide(TITLE, &mut data.szTip);
    data
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

const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    COLORREF(red as u32 | ((green as u32) << 8) | ((blue as u32) << 16))
}
