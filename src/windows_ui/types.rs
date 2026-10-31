use crate::subscription::UsageSnapshot;
use std::time::Duration;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};

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

    pub(super) fn win32_modifiers(
        &self,
    ) -> windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS {
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
    pub cancel_hotkey: Hotkey,
    pub push_to_talk: bool,
    pub max_seconds: u32,
    pub realtime_enabled: bool,
    pub provider: crate::config::Provider,
    pub account: crate::transcription::AccountState,
    /// Estimated batch Scribe credits spent per recording hour.
    pub scribe_credits_per_hour: u32,
    pub selected_microphone: Option<String>,
    pub microphones: Vec<String>,
    pub prefix_enabled: bool,
    pub auto_enter: bool,
    pub intercept_enter: bool,
    pub sounds: crate::config::Sounds,
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
            cancel_hotkey: Hotkey::with_modifiers(
                0x1B,
                "Esc",
                HotkeyModifiers::with_all_modifiers(false, false, false, false),
            ),
            push_to_talk: false,
            max_seconds: 600,
            realtime_enabled: false,
            provider: crate::config::Provider::Elevenlabs,
            account: crate::transcription::AccountState {
                ready: false,
                header: None,
                detail: None,
                unavailable_reason: "Set an API key in settings",
            },
            scribe_credits_per_hour: 585,
            selected_microphone: None,
            microphones: Vec::new(),
            prefix_enabled: true,
            auto_enter: false,
            intercept_enter: true,
            sounds: crate::config::Sounds::Important,
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
    pub(super) fn is_valid(self) -> bool {
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
    ToggleEnterInterception,
    SoundsSelected(crate::config::Sounds),
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
    CancelRegistrationFailed {
        error: String,
    },
    PushToTalkHookFailed {
        error: String,
    },
}

/// Commands sent to the native UI thread by the parent application.
#[derive(Clone, Debug)]
pub enum UiCommand {
    SetSettings(Box<UiSettings>),
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
