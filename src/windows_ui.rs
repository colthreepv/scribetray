//! Windows tray shell and caret-anchored status overlay.
//!
//! This module owns the native UI thread only. It emits user actions through
//! [`UiRuntime::events`] and accepts state changes through [`UiRuntime::send`];
//! recording, configuration persistence, clipboard work, and caret discovery
//! remain the responsibility of the parent application.

#![cfg(windows)]

use std::{
    cell::RefCell,
    collections::HashMap,
    mem::size_of,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::subscription::{UsageSnapshot, format_count, format_reset_date};
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Stroke, Transform,
};
use tracing::{info, warn};
use windows::{
    Win32::{
        Foundation::{
            COLORREF, ERROR_SUCCESS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
        },
        Graphics::Gdi::{
            AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
            BLENDFUNCTION, CLIP_DEFAULT_PRECIS, COLOR_GRAYTEXT, COLOR_MENU, COLOR_MENUTEXT,
            CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, CreateFontW, CreatePen,
            CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_END_ELLIPSIS,
            DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW,
            FF_DONTCARE, FW_SEMIBOLD, FillRect, GetDC, GetMonitorInfoW, GetSysColor,
            GetSysColorBrush, GetTextExtentPoint32W, GetTextFaceW, GetTextMetricsW, HBRUSH, HDC,
            HFONT, HGDIOBJ, HPEN, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect,
            OUT_DEFAULT_PRECIS, PS_SOLID, ReleaseDC, RoundRect, SelectObject, SetBkMode,
            SetTextColor, TEXTMETRICW, TRANSPARENT, TextOutW,
        },
        System::LibraryLoader::GetModuleHandleW,
        System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
        UI::{
            Controls::{DRAWITEMSTRUCT, LIM_SMALL, LoadIconMetric, MEASUREITEMSTRUCT, ODT_MENU},
            HiDpi::{
                GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI, SystemParametersInfoForDpi,
            },
            Input::KeyboardAndMouse::{
                MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
                UnregisterHotKey, VK_ESCAPE, VK_RETURN,
            },
            Shell::{
                NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD,
                NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFY_ICON_DATA_FLAGS,
                NOTIFYICONDATAW, NOTIFYICONDATAW_0, Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CallNextHookEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
                DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
                GetCursorPos, GetMessageW, GetWindowLongPtrW, HHOOK, HICON, HWND_TOPMOST,
                IDI_APPLICATION, InsertMenuItemW, KBDLLHOOKSTRUCT, KillTimer, LoadIconW,
                MENUITEMINFOW, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MFS_DISABLED,
                MFT_OWNERDRAW, MIIM_DATA, MIIM_FTYPE, MIIM_ID, MIIM_STATE, NONCLIENTMETRICSW,
                PostMessageW, PostQuitMessage, RegisterClassExW, SPI_GETNONCLIENTMETRICS, SW_HIDE,
                SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_SHOWWINDOW,
                SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, SetWindowsHookExW,
                ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
                TranslateMessage, ULW_ALPHA, UnhookWindowsHookEx, UnregisterClassW,
                UpdateLayeredWindow, WH_KEYBOARD_LL, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY,
                WM_DISPLAYCHANGE, WM_DPICHANGED, WM_DRAWITEM, WM_HOTKEY, WM_KEYDOWN, WM_KEYUP,
                WM_LBUTTONUP, WM_MEASUREITEM, WM_NCCREATE, WM_RBUTTONUP, WM_SETTINGCHANGE,
                WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
                WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
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
const HOTKEY_ESCAPE_ID: i32 = 0x5345;
const HOTKEY_ENTER_ID: i32 = 0x5346;
/// Ticks the tray tooltip once per second while recording.
const TIMER_RECORDING: usize = 0x5343;
/// Advances the caret overlay animation while it is visible.
const TIMER_OVERLAY_ANIMATION: usize = 0x5344;
/// Samples the live level meter while the caret overlay is visible.
const TIMER_OVERLAY_METER: usize = 0x5345;
const OVERLAY_ANIMATION_MS: u32 = 33;
const OVERLAY_METER_MS: u32 = 50;
const OVERLAY_RECORDING_TICK_MS: u32 = 1000;
const USAGE_HEADER_WIDTH_DIP: i32 = 300;
const USAGE_HEADER_HEIGHT_DIP: i32 = 64;

/// Caret overlay geometry in device-independent pixels, scaled per monitor.
const PILL_WIDTH_DIP: f32 = 58.0;
const PILL_COUNTDOWN_WIDTH_DIP: f32 = 90.0;
const PILL_HEIGHT_DIP: f32 = 22.0;
const PILL_RADIUS_DIP: f32 = 11.0;
/// Transparent margin around the pill that carries the soft shadow.
const OVERLAY_SHADOW_PAD_DIP: f32 = 8.0;
const OVERLAY_GAP_DIP: f32 = 8.0;
const OVERLAY_CARET_INSET_DIP: f32 = 4.0;
const OVERLAY_WAVE_BARS: usize = 7;
const OVERLAY_BAR_WIDTH_DIP: f32 = 2.0;
const OVERLAY_BAR_PITCH_DIP: f32 = 4.0;
const OVERLAY_BAR_START_DIP: f32 = 21.0;
const OVERLAY_BAR_MIN_DIP: f32 = 2.0;
const OVERLAY_BAR_MAX_DIP: f32 = 14.0;
const OVERLAY_DOT_X_DIP: f32 = 11.0;
const OVERLAY_DOT_RADIUS_DIP: f32 = 3.0;
/// Per-frame easing applied to each waveform bar toward its sampled level.
const OVERLAY_BAR_EASING: f32 = 0.45;
const OVERLAY_FADE_IN_SECONDS: f32 = 0.12;
const OVERLAY_ERROR_HOLD_SECONDS: f32 = 2.0;
const OVERLAY_ERROR_FADE_SECONDS: f32 = 0.2;
const DEFAULT_DPI: f32 = 96.0;
const USAGE_TRACK_RGB: (u8, u8, u8) = (0xE0, 0xE0, 0xE0);
const USAGE_NORMAL_RGB: (u8, u8, u8) = (0xF0, 0x56, 0x4A);
const USAGE_WARN_RGB: (u8, u8, u8) = (0xF5, 0xA5, 0x24);
const USAGE_CRITICAL_RGB: (u8, u8, u8) = (0xDC, 0x26, 0x26);

/// Colors shared by the tray, recording overlay, and menu status states.
const REC_RGB: (u8, u8, u8) = (0xF0, 0x44, 0x38);
const WORK_RGB: (u8, u8, u8) = (0xF5, 0xA5, 0x24);
const BAR_RGB: (u8, u8, u8) = (0xF0, 0xF2, 0xF5);
const PILL_FILL_RGB: (u8, u8, u8) = (22, 24, 29);
const PILL_BORDER_RGB: (u8, u8, u8) = (255, 255, 255);
/// Cubic handle used to approximate circular corners and dots.
const CIRCLE_KAPPA: f32 = 0.552_284_75;

/// Visual state shown by the system tray icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayState {
    Idle,
    Recording,
    Working,
    Error,
    Off,
}

/// Taskbar theme the tray icon is drawn against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayTheme {
    Dark,
    Light,
}

/// Numeric resource ID of an embedded tray icon. Keep in sync with `build.rs`.
const fn tray_icon_id(state: TrayState, theme: TrayTheme) -> u32 {
    match (state, theme) {
        (TrayState::Idle, TrayTheme::Dark) => 2,
        (TrayState::Idle, TrayTheme::Light) => 3,
        (TrayState::Recording, TrayTheme::Dark) => 4,
        (TrayState::Recording, TrayTheme::Light) => 5,
        (TrayState::Working, TrayTheme::Dark) => 6,
        (TrayState::Working, TrayTheme::Light) => 7,
        (TrayState::Error, TrayTheme::Dark) => 8,
        (TrayState::Error, TrayTheme::Light) => 9,
        (TrayState::Off, TrayTheme::Dark) => 10,
        (TrayState::Off, TrayTheme::Light) => 11,
    }
}

thread_local! {
    static PUSH_TO_TALK_HOOK_STATE: RefCell<Option<PushToTalkHookState>> = const { RefCell::new(None) };
}

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
    pub push_to_talk: bool,
    pub max_seconds: u32,
    pub realtime_enabled: bool,
    /// Whether an ElevenLabs API key is resolved from the environment or config.
    pub api_key_configured: bool,
    /// Estimated batch Scribe credits spent per recording hour.
    pub scribe_credits_per_hour: u32,
    pub selected_microphone: Option<String>,
    pub microphones: Vec<String>,
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
            push_to_talk: false,
            max_seconds: 600,
            realtime_enabled: false,
            api_key_configured: false,
            scribe_credits_per_hour: 585,
            selected_microphone: None,
            microphones: Vec::new(),
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
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyPurpose {
    ToggleRecording,
}

/// Actions initiated by the user through the tray or registered hotkeys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiEvent {
    UsageRefreshRequested,
    ToggleRecord,
    EnterPressed,
    RecoverLast,
    PushToTalkPressed,
    PushToTalkReleased,
    CancelRecord,
    Quit,
    OpenConfig,
    CaptureToggleHotkey,
    ToggleRecordingMode,
    ToggleRealtime,
    MicrophoneSelected(Option<String>),
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
    PushToTalkHookFailed {
        error: String,
    },
}

/// Commands sent to the native UI thread by the parent application.
#[derive(Clone, Debug)]
pub enum UiCommand {
    SetSettings(UiSettings),
    SetUsage(Option<UsageSnapshot>),
    UpdateHistory(Vec<HistoryMenuItem>),
    SetRecording(bool),
    /// Live input level for the overlay waveform; `None` when not capturing.
    SetLevelMeter(Option<crate::audio::LevelMeter>),
    /// Number of transcriptions currently in flight, including history retries.
    SetTranscribing(usize),
    /// Controls the persistent failure badge shown in the tray.
    SetTrayError(bool),
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

struct UiState {
    commands: Receiver<UiCommand>,
    events: Sender<UiEvent>,
    settings: UiSettings,
    usage: Option<UsageSnapshot>,
    tray_hwnd: HWND,
    overlay_hwnd: HWND,
    instance: HINSTANCE,
    class_name: Vec<u16>,
    icon_added: bool,
    tray_icon: HICON,
    tray_icon_owned: bool,
    tray_state: TrayState,
    tray_theme: TrayTheme,
    tray_tooltip: String,
    toggle_registered: bool,
    escape_registered: bool,
    enter_registered: bool,
    recording: bool,
    shutting_down: bool,
    anchor_rect: Option<CaretRect>,
    anchor_status: Option<AnchorStatus>,
    recording_started: Option<Instant>,
    tray_error: bool,
    /// Count of in-flight transcriptions; any positive value shows Working.
    transcribing: usize,
    /// Live recorder level handle, supplied while capture is running.
    level_meter: Option<crate::audio::LevelMeter>,
    /// Newest sampled levels, right-most bar last, scrolled left each sample.
    overlay_levels: [f32; OVERLAY_WAVE_BARS],
    /// Eased bar heights actually drawn, in the same order as `overlay_levels`.
    overlay_shown: [f32; OVERLAY_WAVE_BARS],
    overlay_visible: bool,
    overlay_dpi_scale: f32,
    overlay_shown_at: Instant,
    overlay_state_since: Instant,
    push_to_talk_hook: Option<HHOOK>,
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
    state.update_push_to_talk_hook();
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
        enter_registered: false,
        recording: false,
        shutting_down: false,
        anchor_rect: None,
        anchor_status: None,
        recording_started: None,
        push_to_talk_hook: None,
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

impl UiState {
    fn register_hotkeys(&mut self) {
        self.unregister_record_hotkeys();
        let toggle = self.settings.toggle_hotkey.clone();
        self.toggle_registered = if self.settings.push_to_talk {
            false
        } else {
            self.register_record_hotkey(
                HOTKEY_TOGGLE_ID,
                HotkeyPurpose::ToggleRecording,
                toggle.clone(),
            )
        };
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
    }

    fn set_recording(&mut self, recording: bool) {
        if self.recording == recording {
            return;
        }
        self.recording = recording;
        PUSH_TO_TALK_HOOK_STATE.with(|state| {
            if let Some(state) = state.borrow_mut().as_mut() {
                state.recording = recording;
            }
        });
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
            match unsafe {
                RegisterHotKey(
                    Some(self.tray_hwnd),
                    HOTKEY_ENTER_ID,
                    MOD_NOREPEAT,
                    VK_RETURN.0 as u32,
                )
            } {
                Ok(()) => {
                    self.enter_registered = true;
                    info!("registered Enter to stop and send");
                }
                Err(error) => {
                    self.enter_registered = false;
                    warn!("could not register Enter to stop and send: {error}");
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
        if self.enter_registered {
            let _ = unsafe { UnregisterHotKey(Some(self.tray_hwnd), HOTKEY_ENTER_ID) };
            self.enter_registered = false;
        }
    }

    fn apply_command(&mut self, command: UiCommand) {
        match command {
            UiCommand::SetSettings(settings) => {
                let hotkeys_changed = self.settings.toggle_hotkey != settings.toggle_hotkey;
                let mode_changed = self.settings.push_to_talk != settings.push_to_talk;
                self.settings = settings;
                if hotkeys_changed || mode_changed {
                    self.register_hotkeys();
                    self.update_push_to_talk_hook();
                }
                self.refresh_tray();
            }
            UiCommand::SetUsage(usage) => self.usage = usage,
            UiCommand::UpdateHistory(history) => self.settings.history = history,
            UiCommand::SetRecording(recording) => {
                self.set_recording(recording);
                self.refresh_tray();
            }
            UiCommand::SetLevelMeter(meter) => {
                if meter.is_none() {
                    self.overlay_levels = [0.0; OVERLAY_WAVE_BARS];
                }
                self.level_meter = meter;
            }
            UiCommand::SetTranscribing(count) => {
                self.transcribing = count;
                self.refresh_tray();
            }
            UiCommand::SetTrayError(error) => {
                if error {
                    self.tray_error = true;
                } else {
                    self.clear_tray_error();
                }
                self.refresh_tray();
            }
            UiCommand::UpdateAnchor { rect, status } => {
                self.show_anchor(rect, status);
                self.refresh_tray();
            }
            UiCommand::HideAnchor => {
                self.hide_anchor();
                self.refresh_tray();
            }
            UiCommand::Notice { title, message } => self.show_notice(&title, &message),
            UiCommand::Exit => self.shutdown(),
        }
    }

    fn show_anchor(&mut self, rect: CaretRect, status: AnchorStatus) {
        if !rect.is_valid() {
            self.hide_anchor();
            return;
        }
        // Restart the entry animation only when the overlay appears or changes
        // kind, so the ~200 ms caret poll does not make the pill blink.
        let kind_changed = self.anchor_status.map(overlay_kind) != Some(overlay_kind(status));
        let appearing = !self.overlay_visible;
        if kind_changed {
            self.overlay_state_since = Instant::now();
            self.overlay_levels = [0.0; OVERLAY_WAVE_BARS];
            self.overlay_shown = [0.0; OVERLAY_WAVE_BARS];
        }
        if appearing || kind_changed {
            self.overlay_shown_at = Instant::now();
        }
        self.anchor_rect = Some(rect);
        self.anchor_status = Some(status);
        if matches!(status, AnchorStatus::Error) {
            self.tray_error = true;
        }
        self.recording_started = match status {
            AnchorStatus::Recording { elapsed } => Instant::now().checked_sub(elapsed),
            _ => None,
        };
        self.overlay_dpi_scale = overlay_dpi_scale(rect);
        self.overlay_visible = true;
        self.reposition_overlay();
        unsafe {
            let _ = ShowWindow(self.overlay_hwnd, SW_SHOWNOACTIVATE);
            if SetTimer(
                Some(self.overlay_hwnd),
                TIMER_OVERLAY_ANIMATION,
                OVERLAY_ANIMATION_MS,
                None,
            ) == 0
            {
                self.show_notice(TITLE, "The overlay animation timer could not be started.");
            }
            let _ = SetTimer(
                Some(self.overlay_hwnd),
                TIMER_OVERLAY_METER,
                OVERLAY_METER_MS,
                None,
            );
            if self.recording_started.is_some() {
                if SetTimer(
                    Some(self.overlay_hwnd),
                    TIMER_RECORDING,
                    OVERLAY_RECORDING_TICK_MS,
                    None,
                ) == 0
                {
                    self.show_notice(TITLE, "The recording timer could not be started.");
                }
            } else {
                let _ = KillTimer(Some(self.overlay_hwnd), TIMER_RECORDING);
            }
        }
        self.render_overlay();
    }

    fn hide_anchor(&mut self) {
        self.anchor_rect = None;
        self.anchor_status = None;
        self.recording_started = None;
        self.overlay_visible = false;
        self.overlay_levels = [0.0; OVERLAY_WAVE_BARS];
        self.overlay_shown = [0.0; OVERLAY_WAVE_BARS];
        unsafe {
            let _ = KillTimer(Some(self.overlay_hwnd), TIMER_OVERLAY_ANIMATION);
            let _ = KillTimer(Some(self.overlay_hwnd), TIMER_OVERLAY_METER);
            let _ = KillTimer(Some(self.overlay_hwnd), TIMER_RECORDING);
            let _ = ShowWindow(self.overlay_hwnd, SW_HIDE);
        }
    }

    /// Re-applies the DPI-aware screen position and size of the overlay window.
    fn reposition_overlay(&mut self) {
        let Some(rect) = self.anchor_rect else {
            return;
        };
        let scale = self.overlay_dpi_scale.max(0.5);
        let expanded = overlay_countdown(self).is_some();
        let (left, top) = overlay_window_position(rect, scale, expanded);
        unsafe {
            let _ = SetWindowPos(
                self.overlay_hwnd,
                Some(HWND_TOPMOST),
                left,
                top,
                overlay_window_width(scale, expanded),
                overlay_window_height(scale),
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
            );
        }
    }

    /// Eases every waveform bar toward its sampled target and repaints.
    fn advance_overlay_animation(&mut self) {
        if !self.overlay_visible {
            return;
        }
        if matches!(self.anchor_status, Some(AnchorStatus::Error))
            && self.overlay_state_since.elapsed().as_secs_f32()
                >= OVERLAY_ERROR_HOLD_SECONDS + OVERLAY_ERROR_FADE_SECONDS
        {
            self.hide_anchor();
            self.refresh_tray();
            return;
        }
        for index in 0..OVERLAY_WAVE_BARS {
            let shown = self.overlay_shown[index];
            self.overlay_shown[index] =
                shown + (self.overlay_levels[index] - shown) * OVERLAY_BAR_EASING;
        }
        self.render_overlay();
    }

    /// Scrolls one fresh level from the recorder into the waveform history.
    fn sample_overlay_meter(&mut self) {
        if !self.overlay_visible {
            return;
        }
        let level = self
            .level_meter
            .as_ref()
            .map(|meter| meter.level())
            .unwrap_or(0.0);
        self.overlay_levels.rotate_left(1);
        self.overlay_levels[OVERLAY_WAVE_BARS - 1] = level;
    }

    /// Rasterizes the current state and presents it as a premultiplied layer.
    fn render_overlay(&mut self) {
        if !self.overlay_visible {
            return;
        }
        let scale = self.overlay_dpi_scale.max(0.5);
        let width = overlay_window_width(scale, overlay_countdown(self).is_some()).max(1) as u32;
        let height = overlay_window_height(scale).max(1) as u32;
        let Some(mut pixmap) = Pixmap::new(width, height) else {
            return;
        };
        draw_overlay(&mut pixmap, self, scale);
        if let Err(error) = present_layered_window(self.overlay_hwnd, &pixmap) {
            // Presentation can fail transiently during display changes; keep the
            // last good frame instead of tearing the overlay down.
            let _ = error;
        }
    }

    /// Whether Scribetray has the minimum setup needed to start a recording.
    fn tray_setup_ready(&self) -> bool {
        self.settings.api_key_configured
            && !self.settings.microphones.is_empty()
            && (self.settings.push_to_talk || self.toggle_registered)
    }

    /// Maps the current application state to the tray icon and tooltip state.
    fn desired_tray_state(&self) -> TrayState {
        if self.recording {
            return TrayState::Recording;
        }
        if self.transcribing > 0 {
            return TrayState::Working;
        }
        match self.anchor_status {
            Some(AnchorStatus::Error) => return TrayState::Error,
            Some(AnchorStatus::Working) => return TrayState::Working,
            Some(AnchorStatus::Recording { .. }) => return TrayState::Recording,
            None => {}
        }
        if self.tray_error {
            return TrayState::Error;
        }
        if !self.tray_setup_ready() {
            return TrayState::Off;
        }
        TrayState::Idle
    }

    /// Recomputes state, icon, and tooltip, touching the shell only when they change.
    fn refresh_tray(&mut self) {
        if !self.icon_added {
            return;
        }
        let theme = if system_uses_light_theme() {
            TrayTheme::Light
        } else {
            TrayTheme::Dark
        };
        let state = self.desired_tray_state();
        let tooltip = tray_tooltip_text(
            state,
            &self.settings.toggle_hotkey,
            self.recording_started.map(|started| started.elapsed()),
            tray_off_reason(&self.settings, self.toggle_registered),
        );
        if state == self.tray_state && theme == self.tray_theme && tooltip == self.tray_tooltip {
            return;
        }
        if state != self.tray_state || theme != self.tray_theme {
            if let Some((icon, owned)) = load_tray_icon(self.instance, tray_icon_id(state, theme)) {
                self.replace_tray_icon(icon, owned);
            }
        }
        self.tray_state = state;
        self.tray_theme = theme;
        self.tray_tooltip = tooltip;
        let data = self.tray_notify_data();
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }

    fn replace_tray_icon(&mut self, icon: HICON, owned: bool) {
        let previous = self.tray_icon;
        let previous_owned = self.tray_icon_owned;
        self.tray_icon = icon;
        self.tray_icon_owned = owned;
        if previous_owned && !previous.0.is_null() {
            let _ = unsafe { DestroyIcon(previous) };
        }
    }

    fn tray_notify_data(&self) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW::default();
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.tray_hwnd;
        data.uID = TRAY_ICON_ID;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = TRAY_CALLBACK;
        data.hIcon = self.tray_icon;
        copy_wide(&self.tray_tooltip, &mut data.szTip);
        data
    }

    fn clear_tray_error(&mut self) {
        self.tray_error = false;
        if matches!(self.anchor_status, Some(AnchorStatus::Error)) {
            self.hide_anchor();
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
        self.clear_tray_error();
        self.refresh_tray();
        self.handle_user_event(UiEvent::UsageRefreshRequested);
        let usage_header = self.usage.as_ref().map(|usage| {
            UsageHeaderDrawData::new(
                usage,
                self.settings.scribe_credits_per_hour,
                self.settings.realtime_enabled,
                tray_menu_dpi(self.tray_hwnd),
            )
        });
        // Keep this stack value alive until TrackPopupMenu returns; its address
        // is carried by dwItemData for the owner's measure/draw callbacks.
        let usage_header_data = usage_header
            .as_ref()
            .map(|header| std::ptr::from_ref(header) as usize);
        let (menu, actions) =
            match build_tray_menu(&self.settings, self.recording, usage_header_data) {
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
        self.remove_push_to_talk_hook();
        self.unregister_escape();
        self.unregister_record_hotkeys();
        self.level_meter = None;
        self.overlay_visible = false;
        unsafe {
            if !self.overlay_hwnd.0.is_null() {
                let _ = KillTimer(Some(self.overlay_hwnd), TIMER_OVERLAY_ANIMATION);
                let _ = KillTimer(Some(self.overlay_hwnd), TIMER_OVERLAY_METER);
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
            if self.tray_icon_owned && !self.tray_icon.0.is_null() {
                let _ = DestroyIcon(self.tray_icon);
                self.tray_icon = HICON::default();
                self.tray_icon_owned = false;
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

    fn update_push_to_talk_hook(&mut self) {
        if self.settings.push_to_talk {
            if self.push_to_talk_hook.is_some() {
                PUSH_TO_TALK_HOOK_STATE.with(|state| {
                    if let Some(state) = state.borrow_mut().as_mut() {
                        state.hotkey = self.settings.toggle_hotkey.clone();
                        state.engaged = false;
                        state.pressed_modifiers = 0;
                        state.enter_pressed = false;
                        state.recording = false;
                    }
                });
                return;
            }

            PUSH_TO_TALK_HOOK_STATE.with(|state| {
                *state.borrow_mut() = Some(PushToTalkHookState {
                    events: self.events.clone(),
                    hotkey: self.settings.toggle_hotkey.clone(),
                    pressed_modifiers: 0,
                    engaged: false,
                    enter_pressed: false,
                    recording: self.recording,
                });
            });
            match unsafe {
                SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(push_to_talk_keyboard_hook),
                    Some(self.instance),
                    0,
                )
            } {
                Ok(hook) => self.push_to_talk_hook = Some(hook),
                Err(error) => {
                    PUSH_TO_TALK_HOOK_STATE.with(|state| *state.borrow_mut() = None);
                    let _ = self.events.send(UiEvent::PushToTalkHookFailed {
                        error: format!("Could not enable push-to-talk keyboard capture: {error}"),
                    });
                    self.show_notice(
                        TITLE,
                        "Could not enable push-to-talk keyboard capture. Toggle mode remains available.",
                    );
                }
            }
        } else {
            self.remove_push_to_talk_hook();
        }
    }

    fn remove_push_to_talk_hook(&mut self) {
        if let Some(hook) = self.push_to_talk_hook.take() {
            let _ = unsafe { UnhookWindowsHookEx(hook) };
        }
        PUSH_TO_TALK_HOOK_STATE.with(|state| *state.borrow_mut() = None);
    }
}

struct PushToTalkHookState {
    events: Sender<UiEvent>,
    hotkey: Hotkey,
    pressed_modifiers: u8,
    engaged: bool,
    enter_pressed: bool,
    recording: bool,
}

unsafe extern "system" fn push_to_talk_keyboard_hook(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let keyboard = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let message = wparam.0 as u32;
    let key_down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
    let key_up = matches!(message, WM_KEYUP | WM_SYSKEYUP);
    if !key_down && !key_up {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let mut swallow = false;
    PUSH_TO_TALK_HOOK_STATE.with(|state| {
        let mut slot = state.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return;
        };
        if let Some(bit) = modifier_bit(keyboard.vkCode) {
            if key_down {
                state.pressed_modifiers |= bit;
            } else {
                state.pressed_modifiers &= !bit;
            }
        }

        let is_enter_key = keyboard.vkCode == VK_RETURN.0 as u32;
        let is_trigger_key = keyboard.vkCode == state.hotkey.virtual_key;
        if key_down && is_enter_key && state.engaged && state.recording {
            if !state.enter_pressed {
                state.enter_pressed = true;
                swallow = true;
                let _ = state.events.send(UiEvent::EnterPressed);
            }
        } else if key_up && is_enter_key && state.enter_pressed {
            state.enter_pressed = false;
            swallow = true;
        } else if key_down && is_trigger_key {
            if state.engaged {
                swallow = true;
            } else if modifiers_match(state) {
                state.engaged = true;
                swallow = true;
                let _ = state.events.send(UiEvent::PushToTalkPressed);
            }
        } else if state.engaged
            && ((key_up && is_trigger_key) || (key_up && !modifiers_match(state)))
        {
            state.engaged = false;
            if is_trigger_key {
                swallow = true;
            }
            let _ = state.events.send(UiEvent::PushToTalkReleased);
        }
    });

    if swallow {
        LRESULT(1)
    } else {
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
}

fn modifier_bit(virtual_key: u32) -> Option<u8> {
    match virtual_key {
        0x5B => Some(1 << 0), // Left Windows
        0x5C => Some(1 << 1), // Right Windows
        0xA4 => Some(1 << 2), // Left Alt
        0xA5 => Some(1 << 3), // Right Alt
        0xA2 => Some(1 << 4), // Left Ctrl
        0xA3 => Some(1 << 5), // Right Ctrl
        0xA0 => Some(1 << 6), // Left Shift
        0xA1 => Some(1 << 7), // Right Shift
        0x12 => Some((1 << 2) | (1 << 3)),
        0x11 => Some((1 << 4) | (1 << 5)),
        0x10 => Some((1 << 6) | (1 << 7)),
        _ => None,
    }
}

fn modifiers_match(state: &PushToTalkHookState) -> bool {
    let pressed = state.pressed_modifiers;
    let actual = [
        pressed & 0b0000_0011 != 0,
        pressed & 0b0000_1100 != 0,
        pressed & 0b0011_0000 != 0,
        pressed & 0b1100_0000 != 0,
    ];
    let required = [
        state.hotkey.modifiers.win,
        state.hotkey.modifiers.alt,
        state.hotkey.modifiers.control,
        state.hotkey.modifiers.shift,
    ];
    actual == required
}

/// Coarse overlay shape used to detect state changes without comparing timing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OverlayKind {
    Recording,
    Working,
    Error,
}

fn overlay_kind(status: AnchorStatus) -> OverlayKind {
    match status {
        AnchorStatus::Recording { .. } => OverlayKind::Recording,
        AnchorStatus::Working => OverlayKind::Working,
        AnchorStatus::Error => OverlayKind::Error,
    }
}

fn overlay_countdown(state: &UiState) -> Option<u32> {
    let Some(AnchorStatus::Recording { elapsed }) = state.anchor_status else {
        return None;
    };
    let elapsed = elapsed.as_secs().min(u32::MAX as u64) as u32;
    let remaining = state.settings.max_seconds.saturating_sub(elapsed);
    (remaining <= 60).then_some(remaining)
}

/// Monitor that contains the caret, falling back to the nearest one.
fn caret_monitor(caret: CaretRect) -> Option<windows::Win32::Graphics::Gdi::HMONITOR> {
    let rect = RECT {
        left: caret.left,
        top: caret.top,
        right: caret.left.saturating_add(caret.width.max(1)),
        bottom: caret.top.saturating_add(caret.height.max(1)),
    };
    let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST) };
    (!monitor.0.is_null()).then_some(monitor)
}

/// DPI scale factor (1.0 at 96 DPI) of the monitor that holds the caret.
fn overlay_dpi_scale(caret: CaretRect) -> f32 {
    let Some(monitor) = caret_monitor(caret) else {
        return 1.0;
    };
    let mut dpi_x = DEFAULT_DPI as u32;
    let mut dpi_y = DEFAULT_DPI as u32;
    if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_ok()
        && dpi_x > 0
    {
        dpi_x as f32 / DEFAULT_DPI
    } else {
        1.0
    }
}

/// Work area (taskbar-excluded) of the monitor that holds the caret.
fn caret_work_area(caret: CaretRect) -> Option<RECT> {
    let monitor = caret_monitor(caret)?;
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        Some(info.rcWork)
    } else {
        None
    }
}

fn overlay_window_width(scale: f32, expanded: bool) -> i32 {
    let pill_width = if expanded {
        PILL_COUNTDOWN_WIDTH_DIP
    } else {
        PILL_WIDTH_DIP
    };
    ((pill_width + OVERLAY_SHADOW_PAD_DIP * 2.0) * scale)
        .round()
        .max(1.0) as i32
}

fn overlay_window_height(scale: f32) -> i32 {
    ((PILL_HEIGHT_DIP + OVERLAY_SHADOW_PAD_DIP * 2.0) * scale)
        .round()
        .max(1.0) as i32
}

/// Places the pill above the caret, flipping below and clamping into the work
/// area when the caret sits close to a screen edge.
fn overlay_window_position(caret: CaretRect, scale: f32, expanded: bool) -> (i32, i32) {
    let window_width = overlay_window_width(scale, expanded);
    let window_height = overlay_window_height(scale);
    let pad = (OVERLAY_SHADOW_PAD_DIP * scale).round() as i32;
    let gap = (OVERLAY_GAP_DIP * scale).round() as i32;
    let inset = (OVERLAY_CARET_INSET_DIP * scale).round() as i32;
    let pill_height = (PILL_HEIGHT_DIP * scale).round() as i32;
    let caret_bottom = caret.top.saturating_add(caret.height.max(1));

    let above = caret
        .top
        .saturating_sub(gap)
        .saturating_sub(pill_height)
        .saturating_sub(pad);
    let below = caret_bottom.saturating_add(gap).saturating_sub(pad);
    let mut left = caret.left.saturating_sub(inset).saturating_sub(pad);
    let mut top = above;

    if let Some(work) = caret_work_area(caret) {
        if top < work.top {
            top = below;
        }
        let max_left = work.right.saturating_sub(window_width).max(work.left);
        let max_top = work.bottom.saturating_sub(window_height).max(work.top);
        left = left.clamp(work.left, max_left);
        top = top.clamp(work.top, max_top);
    }
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

struct UsageHeaderDrawData {
    dpi: u32,
    hours: Option<String>,
    details: String,
    usage_ratio: f64,
}

impl UsageHeaderDrawData {
    fn new(usage: &UsageSnapshot, credits_per_hour: u32, realtime: bool, dpi: u32) -> Self {
        let mut details = format!(
            "{} / {} credits",
            format_count(usage.used),
            format_count(usage.limit)
        );
        if let Some(reset) = usage.reset_unix.and_then(format_reset_date) {
            details.push_str(" · resets ");
            details.push_str(&reset);
        }
        if let Some(overage) = &usage.overage {
            details.push_str(" · overage $");
            details.push_str(overage);
        }

        let usage_ratio = if usage.limit == 0 {
            if usage.used > 0 { 1.0 } else { 0.0 }
        } else {
            (usage.used as f64 / usage.limit as f64).clamp(0.0, 1.0)
        };
        Self {
            dpi,
            hours: (!realtime).then(|| estimate_hours_remaining(usage, credits_per_hour)),
            details,
            usage_ratio,
        }
    }
}

fn estimate_hours_remaining(usage: &UsageSnapshot, credits_per_hour: u32) -> String {
    let remaining = usage.limit.saturating_sub(usage.used);
    if remaining == 0 || credits_per_hour == 0 {
        return "no credits left".to_owned();
    }
    let hours = remaining as f64 / f64::from(credits_per_hour);
    if hours >= 10.0 {
        format!("≈ {} h left", hours.floor() as u64)
    } else if hours >= 1.0 {
        let whole_hours = hours.floor() as u64;
        let minutes = (((hours - whole_hours as f64) * 60.0) as u64 / 10) * 10;
        format!("≈ {whole_hours} h {minutes} m left")
    } else {
        let minutes = (hours * 60.0).round().clamp(1.0, 59.0) as u64;
        format!("≈ {minutes} m left")
    }
}

fn tray_menu_dpi(hwnd: HWND) -> u32 {
    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) }.is_ok() {
        let rect = RECT {
            left: cursor.x,
            top: cursor.y,
            right: cursor.x.saturating_add(1),
            bottom: cursor.y.saturating_add(1),
        };
        let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST) };
        if !monitor.0.is_null() {
            let mut x_dpi = 0;
            let mut y_dpi = 0;
            if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x_dpi, &mut y_dpi) }
                .is_ok()
                && x_dpi > 0
            {
                return x_dpi;
            }
        }
    }
    unsafe { GetDpiForWindow(hwnd) }.max(1)
}

fn scale_menu_dip(value: i32, dpi: u32) -> i32 {
    ((i64::from(value) * i64::from(dpi) + 48) / 96) as i32
}

unsafe fn measure_usage_header(item: &mut MEASUREITEMSTRUCT) -> bool {
    if item.CtlType != ODT_MENU || item.itemData == 0 {
        return false;
    }
    // SAFETY: this menu item stores a pointer to the UsageHeaderDrawData local
    // kept alive by show_menu for the full TrackPopupMenu call.
    let header = unsafe { &*(item.itemData as *const UsageHeaderDrawData) };
    item.itemWidth = scale_menu_dip(USAGE_HEADER_WIDTH_DIP, header.dpi) as u32;
    item.itemHeight = scale_menu_dip(USAGE_HEADER_HEIGHT_DIP, header.dpi) as u32;
    true
}

unsafe fn draw_usage_header(item: &DRAWITEMSTRUCT) -> bool {
    if item.CtlType != ODT_MENU || item.itemData == 0 || item.hDC.0.is_null() {
        return false;
    }
    // SAFETY: this menu item stores a pointer to the UsageHeaderDrawData local
    // kept alive by show_menu for the full TrackPopupMenu call.
    let header = unsafe { &*(item.itemData as *const UsageHeaderDrawData) };
    let dpi = header.dpi.max(1);
    let scale = |value| scale_menu_dip(value, dpi);
    let rect = item.rcItem;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    let background = unsafe { GetSysColorBrush(COLOR_MENU) };
    unsafe { FillRect(item.hDC, &rect, background) };

    let mut metrics = NONCLIENTMETRICSW {
        cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let got_metrics = unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some((&mut metrics as *mut NONCLIENTMETRICSW).cast()),
            0,
            dpi,
        )
    }
    .is_ok();
    let mut menu_font = if got_metrics {
        metrics.lfMenuFont
    } else {
        fallback_menu_font(dpi)
    };
    let normal_font = unsafe { CreateFontIndirectW(&menu_font) };
    menu_font.lfWeight = FW_SEMIBOLD.0 as i32;
    let semibold_font = unsafe { CreateFontIndirectW(&menu_font) };
    let old_mode = unsafe { SetBkMode(item.hDC, TRANSPARENT) };
    let old_font = if !semibold_font.0.is_null() {
        unsafe { SelectObject(item.hDC, HGDIOBJ(semibold_font.0)) }
    } else if !normal_font.0.is_null() {
        unsafe { SelectObject(item.hDC, HGDIOBJ(normal_font.0)) }
    } else {
        HGDIOBJ::default()
    };
    let menu_color = COLORREF(unsafe { GetSysColor(COLOR_MENUTEXT) });
    let old_text_color = unsafe { SetTextColor(item.hDC, menu_color) };

    let padding = scale(12);
    let top_row = RECT {
        left: rect.left + padding,
        top: rect.top + scale(6),
        right: rect.right - padding,
        bottom: rect.top + scale(26),
    };
    let mut title_rect = top_row;
    title_rect.right = rect.left + width / 2;
    let mut title = "ElevenLabs".encode_utf16().collect::<Vec<_>>();
    unsafe {
        DrawTextW(
            item.hDC,
            &mut title,
            &mut title_rect,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }
    if let Some(hours) = &header.hours {
        let mut hour_rect = top_row;
        hour_rect.left = rect.left + width / 2;
        let mut hours = hours.encode_utf16().collect::<Vec<_>>();
        unsafe {
            DrawTextW(
                item.hDC,
                &mut hours,
                &mut hour_rect,
                DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
        }
    }

    if !normal_font.0.is_null() {
        unsafe { SelectObject(item.hDC, HGDIOBJ(normal_font.0)) };
    }
    let gray_color = COLORREF(unsafe { GetSysColor(COLOR_GRAYTEXT) });
    unsafe { SetTextColor(item.hDC, gray_color) };
    let track_left = rect.left + padding;
    let track_top = rect.top + scale(29);
    let track_width = (width - 2 * padding).max(1);
    let track_height = scale(4).max(1);
    draw_usage_round_rect(
        item.hDC,
        track_left,
        track_top,
        track_left + track_width,
        track_top + track_height,
        scale(2),
        rgb_color(USAGE_TRACK_RGB),
    );
    if header.usage_ratio > 0.0 {
        let minimum = scale(4).min(track_width);
        let fill_width = ((track_width as f64 * header.usage_ratio).round() as i32)
            .max(minimum)
            .min(track_width);
        let fill_color = if header.usage_ratio >= 0.95 {
            USAGE_CRITICAL_RGB
        } else if header.usage_ratio >= 0.80 {
            USAGE_WARN_RGB
        } else {
            USAGE_NORMAL_RGB
        };
        draw_usage_round_rect(
            item.hDC,
            track_left,
            track_top,
            track_left + fill_width,
            track_top + track_height,
            scale(2),
            rgb_color(fill_color),
        );
    }

    let mut details_rect = RECT {
        left: rect.left + padding,
        top: rect.top + scale(40),
        right: rect.right - padding,
        bottom: rect.top + height - scale(3),
    };
    let mut details = header.details.encode_utf16().collect::<Vec<_>>();
    unsafe {
        DrawTextW(
            item.hDC,
            &mut details,
            &mut details_rect,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }

    unsafe {
        if !old_font.0.is_null() {
            SelectObject(item.hDC, old_font);
        }
        SetTextColor(item.hDC, old_text_color);
        SetBkMode(
            item.hDC,
            windows::Win32::Graphics::Gdi::BACKGROUND_MODE(old_mode as u32),
        );
        if !normal_font.0.is_null() {
            let _ = DeleteObject(HGDIOBJ(normal_font.0));
        }
        if !semibold_font.0.is_null() {
            let _ = DeleteObject(HGDIOBJ(semibold_font.0));
        }
    }
    true
}

fn fallback_menu_font(dpi: u32) -> windows::Win32::Graphics::Gdi::LOGFONTW {
    use windows::Win32::Graphics::Gdi::LOGFONTW;
    let mut font = LOGFONTW {
        lfHeight: -scale_menu_dip(12, dpi),
        lfWeight: 400,
        ..Default::default()
    };
    for (target, source) in font.lfFaceName.iter_mut().zip("Segoe UI".encode_utf16()) {
        *target = source;
    }
    font
}

fn rgb_color((red, green, blue): (u8, u8, u8)) -> COLORREF {
    COLORREF(u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16))
}

fn draw_usage_round_rect(
    hdc: HDC,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    radius: i32,
    color: COLORREF,
) {
    let brush: HBRUSH = unsafe { CreateSolidBrush(color) };
    let pen: HPEN = unsafe { CreatePen(PS_SOLID, 0, color) };
    let old_brush = unsafe { SelectObject(hdc, HGDIOBJ(brush.0)) };
    let old_pen = unsafe { SelectObject(hdc, HGDIOBJ(pen.0)) };
    unsafe {
        let _ = RoundRect(hdc, left, top, right, bottom, radius * 2, radius * 2);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(HGDIOBJ(pen.0));
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
}

fn build_tray_menu(
    settings: &UiSettings,
    recording: bool,
    usage_header_data: Option<usize>,
) -> Result<(MenuGuard, HashMap<u32, UiEvent>), String> {
    use windows::Win32::UI::WindowsAndMessaging::MF_GRAYED;

    let root = MenuGuard(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);
    let mut actions = HashMap::new();
    let mut next_dynamic_id = 100_u32;
    let setup_ready = settings.api_key_configured;
    let toggle_label = settings.toggle_hotkey.combo_label();

    if let Some(item_data) = usage_header_data {
        let item = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STATE | MIIM_DATA | MIIM_ID,
            fType: MFT_OWNERDRAW,
            fState: MFS_DISABLED,
            wID: 0,
            dwItemData: item_data,
            ..Default::default()
        };
        unsafe { InsertMenuItemW(root.0, 0, true, &item) }
            .map_err(|error| format!("could not add the usage header: {error}"))?;
        append_separator(root.0)?;
    }

    // The first item is the default action when the full menu opens.
    let default_id;
    if setup_ready {
        default_id = 1;
        append_action(
            root.0,
            &format!(
                "{}\t{toggle_label}",
                if recording {
                    "Stop recording".to_owned() + " · Esc"
                } else {
                    "Start recording".to_owned()
                }
            ),
            default_id,
            UiEvent::ToggleRecord,
            &mut actions,
        )?;
    } else {
        // Nothing can record until an API key exists; make that the primary action.
        default_id = 3;
        append_action(
            root.0,
            "Set API key…",
            default_id,
            UiEvent::OpenConfig,
            &mut actions,
        )?;
        append_flags(
            root.0,
            MF_GRAYED,
            0,
            &format!("Start recording\t{toggle_label}"),
        )?;
    }
    append_separator(root.0)?;

    let microphone_menu =
        MenuGuard(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);
    let default_mic_id = take_menu_id(&mut next_dynamic_id)?;
    append_radio_action(
        microphone_menu.0,
        "System default",
        default_mic_id,
        settings.selected_microphone.is_none(),
        Some(UiEvent::MicrophoneSelected(None)),
        &mut actions,
    )?;
    if settings.microphones.is_empty() {
        append_flags(microphone_menu.0, MF_GRAYED, 0, "No input devices found")?;
    } else {
        for microphone in &settings.microphones {
            let id = take_menu_id(&mut next_dynamic_id)?;
            append_radio_action(
                microphone_menu.0,
                &truncate_menu_label(microphone),
                id,
                settings.selected_microphone.as_deref() == Some(microphone),
                Some(UiEvent::MicrophoneSelected(Some(microphone.clone()))),
                &mut actions,
            )?;
        }
    }
    append_popup(root.0, microphone_menu, "Microphone")?;

    let language_menu = MenuGuard(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);
    if settings.languages.is_empty() {
        append_flags(language_menu.0, MF_GRAYED, 0, "No languages configured")?;
    } else {
        for language in &settings.languages {
            let id = take_menu_id(&mut next_dynamic_id)?;
            append_radio_action(
                language_menu.0,
                &language.label,
                id,
                language.code == settings.language_code,
                Some(UiEvent::LanguageSelected(language.code.clone())),
                &mut actions,
            )?;
        }
    }
    append_popup(root.0, language_menu, "Language")?;

    let history_menu = MenuGuard(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);
    if settings.history.is_empty() {
        append_flags(history_menu.0, MF_GRAYED, 0, "No recordings yet")?;
    } else {
        // Flat list: successful entries copy on click, failed entries retry on click.
        for item in settings.history.iter().take(10) {
            let id = take_menu_id(&mut next_dynamic_id)?;
            if item.can_copy {
                append_action(
                    history_menu.0,
                    &truncate_menu_label(&item.label),
                    id,
                    UiEvent::HistoryCopy(item.id.clone()),
                    &mut actions,
                )?;
            } else if item.can_retry {
                append_action(
                    history_menu.0,
                    &truncate_menu_label(&item.label),
                    id,
                    UiEvent::HistoryRetry(item.id.clone()),
                    &mut actions,
                )?;
            } else {
                append_flags(
                    history_menu.0,
                    MF_GRAYED,
                    0,
                    &truncate_menu_label(&item.label),
                )?;
            }
        }
    }
    append_popup(root.0, history_menu, "History")?;
    append_separator(root.0)?;

    let recording_menu =
        MenuGuard(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);
    append_radio_action(
        recording_menu.0,
        "Toggle (press again to stop)",
        8,
        !settings.push_to_talk,
        settings
            .push_to_talk
            .then_some(UiEvent::ToggleRecordingMode),
        &mut actions,
    )?;
    append_radio_action(
        recording_menu.0,
        "Push-to-talk",
        9,
        settings.push_to_talk,
        (!settings.push_to_talk).then_some(UiEvent::ToggleRecordingMode),
        &mut actions,
    )?;
    append_separator(recording_menu.0)?;
    append_check_action(
        recording_menu.0,
        "Realtime transcription",
        10,
        settings.realtime_enabled,
        UiEvent::ToggleRealtime,
        &mut actions,
    )?;
    append_check_action(
        recording_menu.0,
        "Sound cues",
        11,
        settings.sound_enabled,
        UiEvent::ToggleSound,
        &mut actions,
    )?;
    append_popup(root.0, recording_menu, "Recording")?;

    let insert_menu = MenuGuard(unsafe { CreatePopupMenu() }.map_err(|error| error.to_string())?);
    append_radio_action(
        insert_menu.0,
        "Type keystrokes",
        12,
        settings.type_mode,
        (!settings.type_mode).then_some(UiEvent::ToggleTypeMode),
        &mut actions,
    )?;
    append_radio_action(
        insert_menu.0,
        "Paste via clipboard",
        13,
        !settings.type_mode,
        settings.type_mode.then_some(UiEvent::ToggleTypeMode),
        &mut actions,
    )?;
    append_separator(insert_menu.0)?;
    append_check_action(
        insert_menu.0,
        "Add 🎙️ prefix",
        14,
        settings.prefix_enabled,
        UiEvent::TogglePrefix,
        &mut actions,
    )?;
    append_check_action(
        insert_menu.0,
        "Press Enter after inserting",
        15,
        settings.auto_enter,
        UiEvent::ToggleAutoEnter,
        &mut actions,
    )?;
    append_popup(root.0, insert_menu, "Insert")?;
    append_separator(root.0)?;

    append_action(
        root.0,
        &format!("Change hotkey ({toggle_label})…"),
        16,
        UiEvent::CaptureToggleHotkey,
        &mut actions,
    )?;
    append_action(
        root.0,
        "Open settings file…",
        17,
        UiEvent::OpenConfig,
        &mut actions,
    )?;
    append_check_action(
        root.0,
        "Start with Windows",
        18,
        settings.autostart_enabled,
        UiEvent::ToggleAutostart,
        &mut actions,
    )?;
    append_separator(root.0)?;
    append_action(root.0, "Quit Scribetray", 19, UiEvent::Quit, &mut actions)?;

    let _ = unsafe {
        windows::Win32::UI::WindowsAndMessaging::SetMenuDefaultItem(root.0, default_id, 0)
    };
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

/// Appends a bulleted radio item; the currently selected item is not clickable.
fn append_radio_action(
    menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    label: &str,
    id: u32,
    selected: bool,
    action: Option<UiEvent>,
    actions: &mut HashMap<u32, UiEvent>,
) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMenuItemCount, InsertMenuItemW, MENUITEMINFOW, MFS_CHECKED, MFS_UNCHECKED,
        MFT_RADIOCHECK, MFT_STRING, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING,
    };

    let mut text = wide(label);
    let item = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_ID | MIIM_STRING | MIIM_FTYPE | MIIM_STATE,
        fType: MFT_STRING | MFT_RADIOCHECK,
        fState: if selected { MFS_CHECKED } else { MFS_UNCHECKED },
        wID: id,
        dwTypeData: windows::core::PWSTR(text.as_mut_ptr()),
        cch: text.len() as u32,
        ..Default::default()
    };
    let position = unsafe { GetMenuItemCount(Some(menu)) };
    if position < 0 {
        return Err("could not append a radio menu item".to_owned());
    }
    unsafe { InsertMenuItemW(menu, position as u32, true, &item) }
        .map_err(|error| error.to_string())?;
    if let Some(action) = action {
        actions.insert(id, action);
    }
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
                    HOTKEY_ENTER_ID => state.handle_user_event(UiEvent::EnterPressed),
                    HOTKEY_ESCAPE_ID => {
                        state.unregister_escape();
                        state.recording = false;
                        PUSH_TO_TALK_HOOK_STATE.with(|hook_state| {
                            if let Some(hook_state) = hook_state.borrow_mut().as_mut() {
                                hook_state.recording = false;
                            }
                        });
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

/// Rasterizes the current overlay state into a premultiplied RGBA pixmap.
///
/// The geometry is written in device-independent pixels and multiplied by the
/// monitor scale factor.
fn draw_overlay(pixmap: &mut Pixmap, state: &UiState, scale: f32) {
    let pad = OVERLAY_SHADOW_PAD_DIP * scale;
    let countdown = overlay_countdown(state);
    let pill_width = if countdown.is_some() {
        PILL_COUNTDOWN_WIDTH_DIP * scale
    } else {
        PILL_WIDTH_DIP * scale
    };
    let pill_height = PILL_HEIGHT_DIP * scale;
    let radius = PILL_RADIUS_DIP * scale;
    let center_y = pad + pill_height / 2.0;

    let since_show = state.overlay_shown_at.elapsed().as_secs_f32();
    let mut alpha = (since_show / OVERLAY_FADE_IN_SECONDS).clamp(0.0, 1.0);
    let kind = state.anchor_status.map(overlay_kind);
    let state_seconds = state.overlay_state_since.elapsed().as_secs_f32();
    if kind == Some(OverlayKind::Error) {
        alpha *= error_fade(state_seconds);
    }
    alpha = alpha.clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return;
    }

    let shadow_offset = 2.0 * scale;
    if kind == Some(OverlayKind::Error) {
        // Error collapses to a 22x22 circle with a red cross.
        let compact_radius = pill_height / 2.0;
        let center_x = pad + compact_radius;
        for step in 1..=3 {
            let grow = step as f32 * 2.5 * scale;
            if let Some(path) =
                circle_path(center_x, center_y + shadow_offset, compact_radius + grow)
            {
                fill_path(pixmap, &path, rgba8(0, 0, 0, (26.0 * alpha) as u8));
            }
        }
        if let Some(path) = circle_path(center_x, center_y, compact_radius) {
            fill_path(pixmap, &path, pill_fill(240.0 * alpha));
        }
        if let Some(path) = circle_path(center_x, center_y, compact_radius - 0.5 * scale) {
            stroke_path(pixmap, &path, pill_border(alpha), scale);
        }
        let reach = 3.5 * scale;
        let color = record_color(alpha);
        if let Some(path) = line_path(
            center_x - reach,
            center_y - reach,
            center_x + reach,
            center_y + reach,
        ) {
            stroke_path(pixmap, &path, color, 2.0 * scale);
        }
        if let Some(path) = line_path(
            center_x - reach,
            center_y + reach,
            center_x + reach,
            center_y - reach,
        ) {
            stroke_path(pixmap, &path, color, 2.0 * scale);
        }
        return;
    }

    // Soft shadow: nested translucent rounded rectangles stand in for the 8 px
    // blur, which keeps the 33 ms animation cheap.
    for step in 1..=3 {
        let grow = step as f32 * 2.5 * scale;
        if let Some(path) = round_rect_path(
            pad - grow,
            pad - grow + shadow_offset,
            pill_width + grow * 2.0,
            pill_height + grow * 2.0,
            radius + grow,
        ) {
            fill_path(pixmap, &path, rgba8(0, 0, 0, (26.0 * alpha) as u8));
        }
    }
    if let Some(path) = round_rect_path(pad, pad, pill_width, pill_height, radius) {
        fill_path(pixmap, &path, pill_fill(240.0 * alpha));
    }
    if let Some(path) = round_rect_path(
        pad + 0.5 * scale,
        pad + 0.5 * scale,
        (pill_width - scale).max(0.0),
        (pill_height - scale).max(0.0),
        (radius - 0.5 * scale).max(0.0),
    ) {
        stroke_path(pixmap, &path, pill_border(alpha), scale);
    }

    match kind {
        Some(OverlayKind::Recording) => {
            let elapsed = match state.anchor_status {
                Some(AnchorStatus::Recording { elapsed }) => elapsed.as_secs_f32(),
                _ => state_seconds,
            };
            let pulse =
                0.55 + 0.45 * (0.5 + 0.5 * (2.0 * std::f32::consts::PI * elapsed / 1.6).cos());
            let dot_x = pad + OVERLAY_DOT_X_DIP * scale;
            let dot_radius = OVERLAY_DOT_RADIUS_DIP * scale;
            let dot_color = record_color(alpha * pulse);
            if state.settings.push_to_talk {
                if let Some(path) = circle_path(dot_x, center_y, dot_radius) {
                    stroke_path(pixmap, &path, dot_color, 1.5 * scale);
                }
            } else if let Some(path) = circle_path(dot_x, center_y, dot_radius) {
                fill_path(pixmap, &path, dot_color);
            }
            let bar_width = OVERLAY_BAR_WIDTH_DIP * scale;
            for index in 0..OVERLAY_WAVE_BARS {
                let level = state.overlay_shown[index].clamp(0.0, 1.0);
                let bar_height = (OVERLAY_BAR_MIN_DIP
                    + level * (OVERLAY_BAR_MAX_DIP - OVERLAY_BAR_MIN_DIP))
                    * scale;
                let x =
                    pad + (OVERLAY_BAR_START_DIP + index as f32 * OVERLAY_BAR_PITCH_DIP) * scale;
                let y = center_y - bar_height / 2.0;
                if let Some(path) = round_rect_path(x, y, bar_width, bar_height, bar_width / 2.0) {
                    fill_path(pixmap, &path, bar_color(alpha));
                }
            }
            if let Some(remaining) = countdown {
                draw_countdown(pixmap, remaining, pad, pill_width, center_y, scale, alpha);
            }
        }
        Some(OverlayKind::Working) => {
            let dot_x = pad + OVERLAY_DOT_X_DIP * scale;
            if let Some(path) = circle_path(dot_x, center_y, OVERLAY_DOT_RADIUS_DIP * scale) {
                fill_path(pixmap, &path, work_color(alpha));
            }
            let period = state_seconds / 0.9;
            for index in 0..3 {
                let phase = 2.0 * std::f32::consts::PI * (period - index as f32 / 6.0);
                let offset = 3.0 * phase.sin().max(0.0);
                let x = pad + (27.0 + index as f32 * 6.0) * scale;
                let y = center_y - offset * scale;
                if let Some(path) = circle_path(x, y, 1.75 * scale) {
                    fill_path(pixmap, &path, bar_color(alpha));
                }
            }
        }
        _ => {}
    }
}

fn draw_countdown(
    pixmap: &mut Pixmap,
    remaining: u32,
    pad: f32,
    pill_width: f32,
    center_y: f32,
    scale: f32,
    alpha: f32,
) {
    let text = format!("{}:{:02}", remaining / 60, remaining % 60);
    let width = match i32::try_from(pixmap.width()) {
        Ok(width) => width,
        Err(_) => return,
    };
    let height = match i32::try_from(pixmap.height()) {
        Ok(height) => height,
        Err(_) => return,
    };
    let Some(mask_len) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return;
    };

    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.0.is_null() {
        return;
    }

    let mut info = BITMAPINFO::default();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut mask_pixels: *mut core::ffi::c_void = core::ptr::null_mut();
    let bitmap = match unsafe {
        CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut mask_pixels, None, 0)
    } {
        Ok(bitmap) => bitmap,
        Err(_) => {
            unsafe {
                let _ = DeleteDC(dc);
            }
            return;
        }
    };
    if mask_pixels.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
        }
        return;
    }
    unsafe { std::slice::from_raw_parts_mut(mask_pixels.cast::<u8>(), mask_len).fill(0) };

    let previous_bitmap = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
    if previous_bitmap.0.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
        }
        return;
    }

    let font_height = (11.0 * scale).round().max(1.0) as i32;
    let Some((font, previous_font)) = select_overlay_countdown_font(dc, font_height) else {
        unsafe {
            let _ = SelectObject(dc, previous_bitmap);
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
        }
        return;
    };

    let text_wide = wide(&text);
    let mut extent = SIZE::default();
    let mut metrics = TEXTMETRICW::default();
    let measured = unsafe {
        GetTextExtentPoint32W(
            dc,
            &text_wide[..text_wide.len().saturating_sub(1)],
            &mut extent,
        )
        .as_bool()
            && GetTextMetricsW(dc, &mut metrics).as_bool()
    };
    if measured {
        let right = pad + pill_width - 10.0 * scale;
        let x = (right - extent.cx as f32).round() as i32;
        let y = (center_y - metrics.tmHeight as f32 / 2.0).round() as i32;
        unsafe {
            let _ = SetBkMode(dc, TRANSPARENT);
            let _ = SetTextColor(dc, COLORREF(0x00FF_FFFF));
        }
        if unsafe { TextOutW(dc, x, y, &text_wide[..text_wide.len().saturating_sub(1)]) }.as_bool()
        {
            composite_countdown_mask(pixmap, mask_pixels, alpha);
        }
    }

    unsafe {
        let _ = SelectObject(dc, previous_font);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = SelectObject(dc, previous_bitmap);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(dc);
    }
}

/// Selects Segoe UI Variable Semibold when installed, falling back to Segoe UI.
fn select_overlay_countdown_font(dc: HDC, height: i32) -> Option<(HFONT, HGDIOBJ)> {
    for family in ["Segoe UI Variable", "Segoe UI"] {
        let family_wide = wide(family);
        let font = unsafe {
            CreateFontW(
                -height,
                0,
                0,
                0,
                FW_SEMIBOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
                PCWSTR(family_wide.as_ptr()),
            )
        };
        if font.0.is_null() {
            continue;
        }

        let previous = unsafe { SelectObject(dc, HGDIOBJ(font.0)) };
        if previous.0.is_null() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(font.0));
            }
            continue;
        }

        let mut actual_face = [0_u16; 64];
        let face_length = unsafe { GetTextFaceW(dc, Some(&mut actual_face)) };
        let face_length = actual_face
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(face_length.max(0) as usize)
            .min(actual_face.len());
        let actual_face = String::from_utf16_lossy(&actual_face[..face_length]);
        let matches_requested_family = if family == "Segoe UI Variable" {
            actual_face.starts_with("Segoe UI Variable")
                || actual_face.eq_ignore_ascii_case("Segoe UI")
        } else {
            actual_face.eq_ignore_ascii_case("Segoe UI")
        };
        if matches_requested_family {
            return Some((font, previous));
        }

        unsafe {
            let _ = SelectObject(dc, previous);
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
    }
    None
}

/// Blends the grayscale GDI glyph mask into tiny-skia's premultiplied RGBA data.
fn composite_countdown_mask(
    pixmap: &mut Pixmap,
    mask_pixels: *const core::ffi::c_void,
    alpha: f32,
) {
    let mask = unsafe { std::slice::from_raw_parts(mask_pixels.cast::<u8>(), pixmap.data().len()) };
    let pixmap_data = pixmap.data_mut();
    for (mask_pixel, target_pixel) in mask.chunks_exact(4).zip(pixmap_data.chunks_exact_mut(4)) {
        let coverage =
            (u16::from(mask_pixel[0]) + u16::from(mask_pixel[1]) + u16::from(mask_pixel[2])) as f32
                / (3.0 * 255.0);
        let source_alpha = (coverage * alpha).clamp(0.0, 1.0);
        if source_alpha <= 0.0 {
            continue;
        }
        let inverse_alpha = 1.0 - source_alpha;
        target_pixel[0] = (WORK_RGB.0 as f32 * source_alpha
            + target_pixel[0] as f32 * inverse_alpha)
            .round() as u8;
        target_pixel[1] = (WORK_RGB.1 as f32 * source_alpha
            + target_pixel[1] as f32 * inverse_alpha)
            .round() as u8;
        target_pixel[2] = (WORK_RGB.2 as f32 * source_alpha
            + target_pixel[2] as f32 * inverse_alpha)
            .round() as u8;
        target_pixel[3] =
            (255.0 * source_alpha + target_pixel[3] as f32 * inverse_alpha).round() as u8;
    }
}

/// Fades the error badge out after it has held for two seconds.
fn error_fade(seconds: f32) -> f32 {
    if seconds <= OVERLAY_ERROR_HOLD_SECONDS {
        1.0
    } else {
        (1.0 - (seconds - OVERLAY_ERROR_HOLD_SECONDS) / OVERLAY_ERROR_FADE_SECONDS).clamp(0.0, 1.0)
    }
}

fn rgba8(red: u8, green: u8, blue: u8, alpha: u8) -> Color {
    Color::from_rgba8(red, green, blue, alpha)
}

fn scaled_rgba(rgb: (u8, u8, u8), alpha: f32) -> Color {
    rgba8(rgb.0, rgb.1, rgb.2, (255.0 * alpha).clamp(0.0, 255.0) as u8)
}

fn pill_fill(alpha: f32) -> Color {
    rgba8(
        PILL_FILL_RGB.0,
        PILL_FILL_RGB.1,
        PILL_FILL_RGB.2,
        alpha.clamp(0.0, 255.0) as u8,
    )
}

fn pill_border(alpha: f32) -> Color {
    rgba8(
        PILL_BORDER_RGB.0,
        PILL_BORDER_RGB.1,
        PILL_BORDER_RGB.2,
        (33.0 * alpha).clamp(0.0, 255.0) as u8,
    )
}

fn record_color(alpha: f32) -> Color {
    scaled_rgba(REC_RGB, alpha)
}

fn work_color(alpha: f32) -> Color {
    scaled_rgba(WORK_RGB, alpha)
}

fn bar_color(alpha: f32) -> Color {
    scaled_rgba(BAR_RGB, alpha)
}

fn fill_path(pixmap: &mut Pixmap, path: &Path, color: Color) {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
}

fn stroke_path(pixmap: &mut Pixmap, path: &Path, color: Color, width: f32) {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let mut stroke = Stroke::default();
    stroke.width = width.max(0.1);
    stroke.line_cap = LineCap::Round;
    stroke.line_join = LineJoin::Round;
    pixmap.stroke_path(path, &paint, &stroke, Transform::identity(), None);
}

/// Builds a rounded rectangle path with cubic corners.
fn round_rect_path(x: f32, y: f32, width: f32, height: f32, radius: f32) -> Option<Path> {
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let radius = radius.max(0.0).min(width / 2.0).min(height / 2.0);
    let mut builder = PathBuilder::new();
    if radius <= 0.0 {
        builder.move_to(x, y);
        builder.line_to(x + width, y);
        builder.line_to(x + width, y + height);
        builder.line_to(x, y + height);
        builder.close();
        return builder.finish();
    }
    let right = x + width;
    let bottom = y + height;
    let handle = radius * CIRCLE_KAPPA;
    builder.move_to(x + radius, y);
    builder.line_to(right - radius, y);
    builder.cubic_to(
        right - radius + handle,
        y,
        right,
        y + radius - handle,
        right,
        y + radius,
    );
    builder.line_to(right, bottom - radius);
    builder.cubic_to(
        right,
        bottom - radius + handle,
        right - radius + handle,
        bottom,
        right - radius,
        bottom,
    );
    builder.line_to(x + radius, bottom);
    builder.cubic_to(
        x + radius - handle,
        bottom,
        x,
        bottom - radius + handle,
        x,
        bottom - radius,
    );
    builder.line_to(x, y + radius);
    builder.cubic_to(
        x,
        y + radius - handle,
        x + radius - handle,
        y,
        x + radius,
        y,
    );
    builder.close();
    builder.finish()
}

/// Builds a full circle from four cubic arcs.
fn circle_path(center_x: f32, center_y: f32, radius: f32) -> Option<Path> {
    if radius <= 0.0 {
        return None;
    }
    let handle = radius * CIRCLE_KAPPA;
    let mut builder = PathBuilder::new();
    builder.move_to(center_x, center_y - radius);
    builder.cubic_to(
        center_x + handle,
        center_y - radius,
        center_x + radius,
        center_y - handle,
        center_x + radius,
        center_y,
    );
    builder.cubic_to(
        center_x + radius,
        center_y + handle,
        center_x + handle,
        center_y + radius,
        center_x,
        center_y + radius,
    );
    builder.cubic_to(
        center_x - handle,
        center_y + radius,
        center_x - radius,
        center_y + handle,
        center_x - radius,
        center_y,
    );
    builder.cubic_to(
        center_x - radius,
        center_y - handle,
        center_x - handle,
        center_y - radius,
        center_x,
        center_y - radius,
    );
    builder.close();
    builder.finish()
}

fn line_path(x1: f32, y1: f32, x2: f32, y2: f32) -> Option<Path> {
    let mut builder = PathBuilder::new();
    builder.move_to(x1, y1);
    builder.line_to(x2, y2);
    builder.finish()
}

/// Presents a premultiplied RGBA pixmap through UpdateLayeredWindow.
///
/// A temporary top-down 32-bit DIB receives a channel-swapped copy because a
/// layered window expects premultiplied BGRA. The DIB and its device context are
/// released before returning, so no GDI objects survive between frames.
fn present_layered_window(hwnd: HWND, pixmap: &Pixmap) -> Result<(), String> {
    let width = pixmap.width() as i32;
    let height = pixmap.height() as i32;
    if width <= 0 || height <= 0 {
        return Ok(());
    }

    let screen_dc = unsafe { GetDC(None) };
    if screen_dc.0.is_null() {
        return Err("could not acquire a screen device context".to_owned());
    }
    let memory_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
    if memory_dc.0.is_null() {
        unsafe {
            let _ = ReleaseDC(None, screen_dc);
        }
        return Err("could not create an overlay device context".to_owned());
    }

    let mut info = BITMAPINFO::default();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // negative height requests a top-down DIB
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut pixels: *mut core::ffi::c_void = core::ptr::null_mut();
    let bitmap = match unsafe {
        CreateDIBSection(Some(screen_dc), &info, DIB_RGB_COLORS, &mut pixels, None, 0)
    } {
        Ok(bitmap) => bitmap,
        Err(error) => {
            unsafe {
                let _ = DeleteDC(memory_dc);
                let _ = ReleaseDC(None, screen_dc);
            }
            return Err(format!("could not create the overlay bitmap: {error}"));
        }
    };

    let source = pixmap.data();
    let pixel_count = width as usize * height as usize;
    if !pixels.is_null() && source.len() >= pixel_count * 4 {
        let destination =
            unsafe { std::slice::from_raw_parts_mut(pixels as *mut u8, pixel_count * 4) };
        for index in 0..pixel_count {
            let offset = index * 4;
            destination[offset] = source[offset + 2];
            destination[offset + 1] = source[offset + 1];
            destination[offset + 2] = source[offset];
            destination[offset + 3] = source[offset + 3];
        }
    }

    let previous = unsafe { SelectObject(memory_dc, HGDIOBJ(bitmap.0)) };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let size = SIZE {
        cx: width,
        cy: height,
    };
    let origin = POINT { x: 0, y: 0 };
    let result = unsafe {
        UpdateLayeredWindow(
            hwnd,
            Some(screen_dc),
            None,
            Some(&size as *const SIZE),
            Some(memory_dc),
            Some(&origin as *const POINT),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    };

    unsafe {
        let _ = SelectObject(memory_dc, previous);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory_dc);
        let _ = ReleaseDC(None, screen_dc);
    }
    result.map_err(|error| format!("UpdateLayeredWindow failed: {error}"))
}

fn tray_icon_data(hwnd: HWND, icon: HICON, tooltip: &str) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW::default();
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_ICON_ID;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = TRAY_CALLBACK;
    data.hIcon = icon;
    copy_wide(tooltip, &mut data.szTip);
    data
}

/// Loads an embedded tray icon for the current DPI. The boolean is true when the
/// returned icon is caller-owned and must be released with DestroyIcon.
fn load_tray_icon(instance: HINSTANCE, id: u32) -> Option<(HICON, bool)> {
    let resource = PCWSTR(id as usize as *const u16);
    if let Ok(icon) = unsafe { LoadIconMetric(Some(instance), resource, LIM_SMALL) } {
        return Some((icon, true));
    }
    unsafe { LoadIconW(Some(instance), resource) }
        .ok()
        .map(|icon| (icon, false))
}

/// Reads the taskbar light/dark flag from the current user's personalization key.
fn system_uses_light_theme() -> bool {
    let subkey = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = wide("SystemUsesLightTheme");
    let mut value: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast::<core::ffi::c_void>()),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS && value != 0
}

/// True when a WM_SETTINGCHANGE broadcast carries the ImmersiveColorSet topic.
fn setting_change_is_immersive_color(lparam: LPARAM) -> bool {
    if lparam.0 == 0 {
        return false;
    }
    let pointer = lparam.0 as *const u16;
    if pointer.is_null() {
        return false;
    }
    let mut length = 0_usize;
    unsafe {
        while length < 64 && *pointer.add(length) != 0 {
            length += 1;
        }
    }
    let units = unsafe { std::slice::from_raw_parts(pointer, length) };
    String::from_utf16_lossy(units) == "ImmersiveColorSet"
}

/// Builds the tray tooltip for a state, the configured hotkey, and elapsed time.
fn tray_tooltip_text(
    state: TrayState,
    hotkey: &Hotkey,
    elapsed: Option<Duration>,
    off_reason: &str,
) -> String {
    let version = env!("CARGO_PKG_VERSION");
    match state {
        TrayState::Idle => format!(
            "Scribetray {version} — Ready ({}) · click: copy last dictation",
            hotkey.combo_label()
        ),
        TrayState::Recording => {
            let total = elapsed.unwrap_or_default().as_secs();
            format!(
                "Scribetray {version} — Recording {:02}:{:02} · Enter to send · Esc to cancel",
                (total / 60) % 100,
                total % 60
            )
        }
        TrayState::Working => format!("Scribetray {version} — Transcribing…"),
        TrayState::Error => {
            format!("Scribetray {version} — Last dictation failed · click History to retry")
        }
        TrayState::Off => format!("Scribetray {version} — {off_reason}"),
    }
}

/// Explains why the tray is in the off state, or an empty string when ready.
fn tray_off_reason(settings: &UiSettings, toggle_registered: bool) -> &'static str {
    if !settings.api_key_configured {
        "Set an API key in settings"
    } else if settings.microphones.is_empty() {
        "No input device found · open settings"
    } else if !(settings.push_to_talk || toggle_registered) {
        "Recording hotkey unavailable · open settings"
    } else {
        ""
    }
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
