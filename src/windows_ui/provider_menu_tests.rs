use super::*;
use crate::{config::Provider, openai::SessionStatus, transcription::openai_account_state};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMenuItemCount, GetMenuState, GetMenuStringW, GetSubMenu, HMENU, MF_BYPOSITION, MF_GRAYED,
};

fn labels(menu: HMENU) -> Vec<String> {
    let count = unsafe { GetMenuItemCount(Some(menu)) };
    assert!(count >= 0);
    (0..count)
        .map(|position| {
            let mut text = [0_u16; 512];
            let len =
                unsafe { GetMenuStringW(menu, position as u32, Some(&mut text), MF_BYPOSITION) };
            String::from_utf16_lossy(&text[..len.max(0) as usize])
        })
        .collect()
}

pub(super) fn submenu(menu: HMENU, title: &str) -> HMENU {
    let position = labels(menu)
        .iter()
        .position(|item| item == title)
        .expect("missing submenu");
    let child = unsafe { GetSubMenu(menu, position as i32) };
    assert!(!child.0.is_null());
    child
}

fn assert_disabled(menu: HMENU, position: usize) {
    let state = unsafe { GetMenuState(menu, position as u32, MF_BYPOSITION) };
    assert_ne!(state, u32::MAX);
    assert_ne!(state & MF_GRAYED.0, 0);
}

#[test]
fn openai_ready_menu_shows_identity_and_enables_recording() {
    let settings = UiSettings {
        provider: Provider::Openai,
        account: openai_account_state(SessionStatus::Ready {
            email: Some("user@example.com".into()),
            plan: Some("plus".into()),
        }),
        realtime_enabled: true,
        ..UiSettings::default()
    };
    let (menu, actions) = build_tray_menu(&settings, false, None).unwrap();
    let root = labels(menu.0);
    assert_eq!(&root[..2], ["OpenAI · Plus", "user@example.com"]);
    assert_disabled(menu.0, 0);
    assert_disabled(menu.0, 1);
    assert!(
        root.iter()
            .any(|label| label.starts_with("Start recording"))
    );
    assert!(
        actions
            .values()
            .any(|event| *event == UiEvent::ToggleRecord)
    );
    assert!(
        !labels(submenu(menu.0, "Recording"))
            .iter()
            .any(|item| item == "Realtime transcription")
    );
    assert!(!root.iter().any(|label| label.contains("credits")));
}

#[test]
fn openai_missing_and_expired_disable_recording_keep_settings() {
    for (status, detail) in [
        (SessionStatus::Missing, "Open Codex to sign in"),
        (SessionStatus::Expired, "Session expired · open Codex"),
    ] {
        let settings = UiSettings {
            provider: Provider::Openai,
            account: openai_account_state(status),
            ..UiSettings::default()
        };
        let (menu, actions) = build_tray_menu(&settings, false, None).unwrap();
        let root = labels(menu.0);
        assert_eq!(&root[..2], ["OpenAI", detail]);
        let start = root
            .iter()
            .position(|label| label.starts_with("Start recording"))
            .unwrap();
        assert_disabled(menu.0, start);
        assert!(
            !actions
                .values()
                .any(|event| *event == UiEvent::ToggleRecord)
        );
        assert!(actions.values().any(|event| *event == UiEvent::OpenConfig));
        assert!(
            !labels(submenu(menu.0, "Recording"))
                .iter()
                .any(|item| item == "Realtime transcription")
        );
    }
}

#[test]
fn elevenlabs_menu_exposes_realtime_setting() {
    let settings = UiSettings {
        provider: Provider::Elevenlabs,
        realtime_enabled: true,
        ..UiSettings::default()
    };
    let (menu, actions) = build_tray_menu(&settings, false, None).unwrap();
    assert!(
        labels(submenu(menu.0, "Recording"))
            .iter()
            .any(|item| item == "Realtime transcription")
    );
    assert!(
        actions
            .values()
            .any(|event| *event == UiEvent::ToggleRealtime)
    );
}

#[test]
fn openai_identity_is_single_line_and_has_no_mnemonics() {
    let settings = UiSettings {
        provider: Provider::Openai,
        account: openai_account_state(SessionStatus::Ready {
            email: Some(format!("first\0\n\t&{}", "x".repeat(100))),
            plan: Some("pl&us\nmore".into()),
        }),
        ..UiSettings::default()
    };
    let (menu, _) = build_tray_menu(&settings, false, None).unwrap();
    let root = labels(menu.0);
    assert_eq!(root[0], "OpenAI · Pl&&us more");
    assert!(root[1].starts_with("first   &&"));
    assert!(root[1].ends_with('…'));
    assert!(root[1].encode_utf16().count() <= 73);
    assert!(!root[1].chars().any(char::is_control));
}
