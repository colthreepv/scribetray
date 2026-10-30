use super::state::UiState;
use super::types::{Hotkey, UiEvent};
use super::{HOTKEY_ESCAPE_ID, HOTKEY_TOGGLE_ID, HotkeyPurpose, RELEASE_ENTER_HOOK, TITLE};
use std::cell::RefCell;
use std::sync::mpsc::Sender;
use tracing::{info, warn};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_ESCAPE, VK_RETURN,
    VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, KBDLLHOOKSTRUCT, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

thread_local! {
    pub(super) static KEYBOARD_HOOK_STATE: RefCell<Option<KeyboardHookState>> = const { RefCell::new(None) };
}

impl UiState {
    pub(super) fn register_hotkeys(&mut self) {
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

    pub(super) fn unregister_record_hotkeys(&mut self) {
        if self.toggle_registered {
            let _ = unsafe { UnregisterHotKey(Some(self.tray_hwnd), HOTKEY_TOGGLE_ID) };
            self.toggle_registered = false;
        }
    }

    pub(super) fn set_recording(&mut self, recording: bool) {
        if self.recording == recording {
            return;
        }
        self.recording = recording;
        KEYBOARD_HOOK_STATE.with(|state| {
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
        } else {
            self.unregister_escape();
        }
        self.update_keyboard_hook();
    }

    pub(super) fn unregister_escape(&mut self) {
        if self.escape_registered {
            let _ = unsafe { UnregisterHotKey(Some(self.tray_hwnd), HOTKEY_ESCAPE_ID) };
            self.escape_registered = false;
        }
    }

    pub(super) fn update_keyboard_hook(&mut self) {
        let held_enter_needs_pairing = KEYBOARD_HOOK_STATE.with(|state| {
            state
                .borrow()
                .as_ref()
                .is_some_and(|state| state.enter_pressed || state.shifted_enter_pressed)
        });
        let should_install = self.settings.push_to_talk
            || (self.recording && self.settings.intercept_enter)
            || held_enter_needs_pairing;

        if !should_install {
            self.remove_keyboard_hook();
            return;
        }

        if self.keyboard_hook.is_some() {
            KEYBOARD_HOOK_STATE.with(|state| {
                if let Some(state) = state.borrow_mut().as_mut() {
                    state.hotkey = self.settings.toggle_hotkey.clone();
                    state.push_to_talk = self.settings.push_to_talk;
                    state.recording = self.recording;
                    state.intercept_enter = self.settings.intercept_enter;
                }
            });
            return;
        }

        KEYBOARD_HOOK_STATE.with(|state| {
            *state.borrow_mut() = Some(KeyboardHookState {
                events: self.events.clone(),
                tray_hwnd: self.tray_hwnd,
                hotkey: self.settings.toggle_hotkey.clone(),
                pressed_modifiers: 0,
                engaged: false,
                push_to_talk: self.settings.push_to_talk,
                enter_pressed: false,
                shifted_enter_pressed: false,
                recording: self.recording,
                intercept_enter: self.settings.intercept_enter,
            });
        });
        match unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(self.instance), 0)
        } {
            Ok(hook) => self.keyboard_hook = Some(hook),
            Err(error) => {
                KEYBOARD_HOOK_STATE.with(|state| *state.borrow_mut() = None);
                if self.settings.push_to_talk {
                    let _ = self.events.send(UiEvent::PushToTalkHookFailed {
                        error: format!("Could not enable push-to-talk keyboard capture: {error}"),
                    });
                    self.show_notice(
                        TITLE,
                        "Could not enable push-to-talk keyboard capture. Toggle mode remains available.",
                    );
                } else {
                    warn!("could not enable Enter interception keyboard hook: {error}");
                    self.show_notice(
                        TITLE,
                        "Could not enable Enter interception. Enter will pass through while recording.",
                    );
                }
            }
        }
    }

    pub(super) fn remove_keyboard_hook(&mut self) {
        if let Some(hook) = self.keyboard_hook.take() {
            let _ = unsafe { UnhookWindowsHookEx(hook) };
        }
        KEYBOARD_HOOK_STATE.with(|state| *state.borrow_mut() = None);
    }
}

pub(super) struct KeyboardHookState {
    events: Sender<UiEvent>,
    tray_hwnd: HWND,
    pub(super) hotkey: Hotkey,
    pub(super) pressed_modifiers: u8,
    pub(super) engaged: bool,
    pub(super) push_to_talk: bool,
    enter_pressed: bool,
    shifted_enter_pressed: bool,
    pub(super) recording: bool,
    pub(super) intercept_enter: bool,
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
    let (release_idle_hook, tray_hwnd) = KEYBOARD_HOOK_STATE.with(|state| {
        let mut slot = state.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return (false, HWND::default());
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
        let shift_is_pressed = state.pressed_modifiers & 0b1100_0000 != 0
            || unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
        if is_enter_key && state.enter_pressed {
            // Consume repeats and pair the key-up with an intercepted key-down,
            // even if the option was disabled while this key was held.
            if key_up {
                state.enter_pressed = false;
            }
            swallow = true;
        } else if is_enter_key && state.shifted_enter_pressed {
            // Keep the whole Shift+Enter stroke passed through if Shift is
            // released before Windows delivers the key-up or a repeat.
            if key_up {
                state.shifted_enter_pressed = false;
            }
        } else if key_down && is_enter_key && state.recording && shift_is_pressed {
            state.shifted_enter_pressed = true;
        } else if key_down
            && is_enter_key
            && state.intercept_enter
            && state.recording
            && (!state.push_to_talk || state.engaged)
        {
            state.enter_pressed = true;
            swallow = true;
            let _ = state.events.send(UiEvent::EnterPressed);
        } else if state.push_to_talk && key_down && is_trigger_key {
            if state.engaged {
                swallow = true;
            } else if modifiers_match(state) {
                state.engaged = true;
                swallow = true;
                let _ = state.events.send(UiEvent::PushToTalkPressed);
            }
        } else if state.push_to_talk
            && state.engaged
            && ((key_up && is_trigger_key) || (key_up && !modifiers_match(state)))
        {
            state.engaged = false;
            if is_trigger_key {
                swallow = true;
            }
            let _ = state.events.send(UiEvent::PushToTalkReleased);
        }

        let release_idle_hook = !state.push_to_talk
            && !state.recording
            && !state.enter_pressed
            && !state.shifted_enter_pressed;
        (release_idle_hook, state.tray_hwnd)
    });

    if release_idle_hook {
        let _ = unsafe { PostMessageW(Some(tray_hwnd), RELEASE_ENTER_HOOK, WPARAM(0), LPARAM(0)) };
    }

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

fn modifiers_match(state: &KeyboardHookState) -> bool {
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
