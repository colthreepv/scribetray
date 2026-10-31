use super::types::{Hotkey, UiSettings};
use super::{TRAY_CALLBACK, TRAY_ICON_ID, copy_wide, wide};
use std::mem::size_of;
use std::time::Duration;
use windows::Win32::Foundation::{ERROR_SUCCESS, HINSTANCE, HWND, LPARAM};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Controls::{LIM_SMALL, LoadIconMetric};
use windows::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NOTIFYICONDATAW};
use windows::Win32::UI::WindowsAndMessaging::{HICON, LoadIconW};
use windows::core::PCWSTR;

/// Visual state shown by the system tray icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TrayState {
    Idle,
    Recording,
    Working,
    Error,
    Off,
}

/// Taskbar theme the tray icon is drawn against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TrayTheme {
    Dark,
    Light,
}

/// Numeric resource ID of an embedded tray icon. Keep in sync with `build.rs`.
pub(super) const fn tray_icon_id(state: TrayState, theme: TrayTheme) -> u32 {
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

pub(super) fn tray_icon_data(hwnd: HWND, icon: HICON, tooltip: &str) -> NOTIFYICONDATAW {
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
pub(super) fn load_tray_icon(instance: HINSTANCE, id: u32) -> Option<(HICON, bool)> {
    let resource = PCWSTR(id as usize as *const u16);
    if let Ok(icon) = unsafe { LoadIconMetric(Some(instance), resource, LIM_SMALL) } {
        return Some((icon, true));
    }
    unsafe { LoadIconW(Some(instance), resource) }
        .ok()
        .map(|icon| (icon, false))
}

/// Reads the taskbar light/dark flag from the current user's personalization key.
pub(super) fn system_uses_light_theme() -> bool {
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
pub(super) fn setting_change_is_immersive_color(lparam: LPARAM) -> bool {
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
pub(super) fn tray_tooltip_text(
    state: TrayState,
    hotkey: &Hotkey,
    cancel_hotkey: &Hotkey,
    elapsed: Option<Duration>,
    off_reason: &str,
) -> String {
    let version = display_version();
    match state {
        TrayState::Idle => format!(
            "Scribetray {version} — Ready ({}) · click: copy last dictation",
            hotkey.combo_label()
        ),
        TrayState::Recording => {
            let total = elapsed.unwrap_or_default().as_secs();
            format!(
                "Scribetray {version} — Recording {:02}:{:02} · Enter to send · {} to cancel",
                (total / 60) % 100,
                total % 60,
                cancel_hotkey.combo_label()
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
pub(super) fn tray_off_reason(settings: &UiSettings, toggle_registered: bool) -> &'static str {
    if !settings.account.ready {
        settings.account.unavailable_reason
    } else if settings.microphones.is_empty() {
        "No input device found · open settings"
    } else if !(settings.push_to_talk || toggle_registered) {
        "Recording hotkey unavailable · open settings"
    } else {
        ""
    }
}

/// Version shown in the tray. Local candidate builds carry a `build.json`
/// beside the executable whose `candidateLabel` (for example
/// `0.8.1-test.3`) is shown instead; published downloads have no such file.
pub(super) fn display_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| std::fs::read(exe.with_file_name("build.json")).ok())
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|build| build.get("candidateLabel")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
    })
}
