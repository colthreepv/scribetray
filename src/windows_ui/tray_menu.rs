use super::state::UiState;
use super::types::{UiEvent, UiSettings};
use super::{TITLE, wide};
use crate::subscription::{UsageSnapshot, format_count, format_reset_date};
use std::collections::HashMap;
use std::mem::size_of;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_GRAYTEXT, COLOR_MENU, COLOR_MENUTEXT, CreateFontIndirectW, CreatePen, CreateSolidBrush,
    DT_END_ELLIPSIS, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW,
    FW_SEMIBOLD, FillRect, GetSysColor, GetSysColorBrush, HBRUSH, HDC, HGDIOBJ, HPEN,
    MONITOR_DEFAULTTONEAREST, MonitorFromRect, PS_SOLID, RoundRect, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODT_MENU};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI, SystemParametersInfoForDpi,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, InsertMenuItemW, MENUITEMINFOW,
    MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MFS_DISABLED, MFT_OWNERDRAW, MIIM_DATA,
    MIIM_FTYPE, MIIM_ID, MIIM_STATE, NONCLIENTMETRICSW, PostMessageW, SPI_GETNONCLIENTMETRICS,
    SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
};
use windows::core::PCWSTR;

const USAGE_HEADER_WIDTH_DIP: i32 = 300;
const USAGE_HEADER_HEIGHT_DIP: i32 = 64;

const USAGE_TRACK_RGB: (u8, u8, u8) = (0xE0, 0xE0, 0xE0);
const USAGE_NORMAL_RGB: (u8, u8, u8) = (0xF0, 0x56, 0x4A);
const USAGE_WARN_RGB: (u8, u8, u8) = (0xF5, 0xA5, 0x24);
const USAGE_CRITICAL_RGB: (u8, u8, u8) = (0xDC, 0x26, 0x26);

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

pub(super) unsafe fn measure_usage_header(item: &mut MEASUREITEMSTRUCT) -> bool {
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

pub(super) unsafe fn draw_usage_header(item: &DRAWITEMSTRUCT) -> bool {
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
    let setup_ready = settings.account.ready || recording;
    let toggle_label = settings.toggle_hotkey.combo_label();

    if let Some(header) = &settings.account.header {
        append_flags(root.0, MF_GRAYED, 0, &identity_menu_label(header))?;
        if let Some(detail) = &settings.account.detail {
            append_flags(root.0, MF_GRAYED, 0, &identity_menu_label(detail))?;
        }
        append_separator(root.0)?;
    } else if let Some(item_data) = usage_header_data {
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
            if settings.provider == crate::config::Provider::Openai {
                "Open settings file…"
            } else {
                "Set API key…"
            },
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
    if settings.provider == crate::config::Provider::Elevenlabs {
        append_check_action(
            recording_menu.0,
            "Realtime transcription",
            10,
            settings.realtime_enabled,
            UiEvent::ToggleRealtime,
            &mut actions,
        )?;
    }
    append_check_action(
        recording_menu.0,
        "Intercept Enter while recording",
        20,
        settings.intercept_enter,
        UiEvent::ToggleEnterInterception,
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
    append_flags(
        root.0,
        MF_GRAYED,
        0,
        &format!("Scribetray {}", super::tray_icon::display_version()),
    )?;
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

/// Keep externally supplied account text on one menu line, with no mnemonic.
fn identity_menu_label(label: &str) -> String {
    const MAX_UNITS: usize = 72;
    let mut output = String::new();
    let mut units = 0;
    for character in label.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        let width = if character == '&' {
            2
        } else {
            character.len_utf16()
        };
        if units + width > MAX_UNITS {
            output.push('…');
            break;
        }
        output.push(character);
        if character == '&' {
            output.push('&');
        }
        units += width;
    }
    output
}

impl UiState {
    pub(super) fn show_menu(&mut self) {
        if self.settings.provider == crate::config::Provider::Openai {
            self.settings.account =
                crate::transcription::openai_account_state(crate::openai::session_status());
        }
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
}

#[cfg(test)]
#[path = "provider_menu_tests.rs"]
mod provider_menu_tests;
