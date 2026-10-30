use super::keyboard_hook::KEYBOARD_HOOK_STATE;
use super::overlay::OVERLAY_WAVE_BARS;
use super::tray_icon::{
    TrayState, TrayTheme, load_tray_icon, system_uses_light_theme, tray_icon_id, tray_off_reason,
    tray_tooltip_text,
};
use super::types::{AnchorStatus, CaretRect, UiCommand, UiEvent, UiSettings};
use super::{
    TIMER_OVERLAY_ANIMATION, TIMER_OVERLAY_METER, TIMER_RECORDING, TRAY_CALLBACK, TRAY_ICON_ID,
    copy_wide,
};
use crate::subscription::UsageSnapshot;
use std::mem::size_of;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;
use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, DestroyWindow, HHOOK, HICON, KillTimer, PostQuitMessage, UnregisterClassW,
};
use windows::core::PCWSTR;

pub(super) struct UiState {
    pub(super) commands: Receiver<UiCommand>,
    pub(super) events: Sender<UiEvent>,
    pub(super) settings: UiSettings,
    pub(super) usage: Option<UsageSnapshot>,
    pub(super) tray_hwnd: HWND,
    pub(super) overlay_hwnd: HWND,
    pub(super) instance: HINSTANCE,
    pub(super) class_name: Vec<u16>,
    pub(super) icon_added: bool,
    pub(super) tray_icon: HICON,
    pub(super) tray_icon_owned: bool,
    pub(super) tray_state: TrayState,
    pub(super) tray_theme: TrayTheme,
    pub(super) tray_tooltip: String,
    pub(super) toggle_registered: bool,
    pub(super) escape_registered: bool,
    pub(super) recording: bool,
    pub(super) shutting_down: bool,
    pub(super) anchor_rect: Option<CaretRect>,
    pub(super) anchor_status: Option<AnchorStatus>,
    pub(super) recording_started: Option<Instant>,
    pub(super) tray_error: bool,
    /// Count of in-flight transcriptions; any positive value shows Working.
    pub(super) transcribing: usize,
    /// Live recorder level handle, supplied while capture is running.
    pub(super) level_meter: Option<crate::audio::LevelMeter>,
    /// Newest sampled levels, right-most bar last, scrolled left each sample.
    pub(super) overlay_levels: [f32; OVERLAY_WAVE_BARS],
    /// Eased bar heights actually drawn, in the same order as `overlay_levels`.
    pub(super) overlay_shown: [f32; OVERLAY_WAVE_BARS],
    pub(super) overlay_visible: bool,
    pub(super) overlay_dpi_scale: f32,
    pub(super) overlay_shown_at: Instant,
    pub(super) overlay_state_since: Instant,
    pub(super) keyboard_hook: Option<HHOOK>,
}

impl UiState {
    pub(super) fn apply_command(&mut self, command: UiCommand) {
        match command {
            UiCommand::SetSettings(settings) => {
                let hotkeys_changed = self.settings.toggle_hotkey != settings.toggle_hotkey;
                let mode_changed = self.settings.push_to_talk != settings.push_to_talk;
                if settings.provider != crate::config::Provider::Elevenlabs {
                    self.usage = None;
                }
                self.settings = settings;
                KEYBOARD_HOOK_STATE.with(|state| {
                    if let Some(state) = state.borrow_mut().as_mut() {
                        if hotkeys_changed || mode_changed {
                            state.hotkey = self.settings.toggle_hotkey.clone();
                            state.engaged = false;
                            state.pressed_modifiers = 0;
                        }
                        state.push_to_talk = self.settings.push_to_talk;
                        state.recording = self.recording;
                        state.intercept_enter = self.settings.intercept_enter;
                    }
                });
                if hotkeys_changed || mode_changed {
                    self.register_hotkeys();
                }
                self.update_keyboard_hook();
                self.refresh_tray();
            }
            UiCommand::SetUsage(usage) => {
                if self.settings.provider == crate::config::Provider::Elevenlabs {
                    self.usage = usage;
                }
            }
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

    /// Whether Scribetray has the minimum setup needed to start a recording.
    fn tray_setup_ready(&self) -> bool {
        self.settings.account.ready
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
    pub(super) fn refresh_tray(&mut self) {
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

    pub(super) fn clear_tray_error(&mut self) {
        self.tray_error = false;
        if matches!(self.anchor_status, Some(AnchorStatus::Error)) {
            self.hide_anchor();
        }
    }

    pub(super) fn show_notice(&self, title: &str, message: &str) {
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

    pub(super) fn handle_user_event(&mut self, event: UiEvent) {
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

    pub(super) fn cleanup(&mut self) {
        self.remove_keyboard_hook();
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
}
