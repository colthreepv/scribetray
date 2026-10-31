use crate::autostart;
use crate::config::{Config, Provider, config_path};
use crate::feedback::show_notice;
use crate::history::{History, Recording, RecordingStatus};
use crate::windows_ui::{
    HistoryMenuItem, Hotkey, HotkeyModifiers, LanguageOption, UiCommand, UiRuntime, UiSettings,
};
use crate::{realtime, transcription};
use std::{fs, time::SystemTime};
use tracing::{info, warn};

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConfigStamp {
    modified: Option<SystemTime>,
    length: u64,
}

pub(crate) struct ConfigMonitor {
    observed: Option<ConfigStamp>,
}

impl ConfigMonitor {
    pub(crate) fn new() -> Self {
        Self {
            observed: config_stamp(),
        }
    }

    pub(crate) fn load_if_changed(&mut self, current: &Config) -> Option<Config> {
        let stamp = config_stamp()?;
        if self.observed.as_ref() == Some(&stamp) {
            return None;
        }
        self.observed = Some(stamp);

        let mut updated = match Config::load() {
            Ok(config) => config,
            Err(error) => {
                warn!("could not reload settings: {error}");
                return None;
            }
        };
        if let Err(error) = validate_config(&updated) {
            warn!("ignored invalid settings update: {error}");
            return None;
        }
        if updated == *current {
            return None;
        }
        if updated.start_with_windows != current.start_with_windows
            && let Err(error) = autostart::set_enabled(updated.start_with_windows)
        {
            warn!("could not update Start with Windows setting: {error}");
            updated.start_with_windows = current.start_with_windows;
        }
        info!("settings reloaded from disk");
        Some(updated)
    }
}

pub(crate) fn make_ui_settings(
    config: &Config,
    recordings: Vec<Recording>,
    microphones: Vec<String>,
) -> UiSettings {
    let toggle_hotkey = parse_hotkey(&config.hotkey).unwrap_or_default();
    let cancel_hotkey =
        parse_cancel_hotkey(&config.cancel_hotkey).expect("validated cancel hotkey");
    let languages = [
        ("auto", "Automatic detection"),
        ("en", "English"),
        ("it", "Italian"),
        ("th", "Thai"),
        ("es", "Spanish"),
        ("fr", "French"),
        ("de", "German"),
        ("ja", "Japanese"),
        ("zh", "Chinese"),
    ]
    .into_iter()
    .map(|(code, label)| LanguageOption {
        code: code.to_owned(),
        label: label.to_owned(),
    })
    .collect();
    let history = recordings
        .into_iter()
        .filter(history_item_visible)
        .take(10)
        .map(|recording| HistoryMenuItem {
            label: history_label(&recording),
            id: recording.id,
            can_copy: recording.status == RecordingStatus::Succeeded
                && recording
                    .transcript
                    .as_deref()
                    .is_some_and(|transcript| !transcript.trim().is_empty()),
            can_retry: recording.status == RecordingStatus::Failed,
        })
        .collect();
    UiSettings {
        toggle_hotkey,
        cancel_hotkey,
        push_to_talk: config.mode.eq_ignore_ascii_case("push_to_talk"),
        max_seconds: config.max_seconds,
        realtime_enabled: config.realtime,
        provider: config.provider,
        account: transcription::account_state(config),
        scribe_credits_per_hour: config.scribe_credits_per_hour,
        selected_microphone: config.microphone.clone(),
        microphones,
        prefix_enabled: config.prefix_enabled,
        auto_enter: config.auto_enter,
        intercept_enter: config.intercept_enter,
        sounds: config.sounds,
        type_mode: config.insert_method.eq_ignore_ascii_case("type"),
        autostart_enabled: config.start_with_windows,
        language_code: config.language.clone(),
        languages,
        history,
    }
}

fn config_stamp() -> Option<ConfigStamp> {
    let path = config_path().ok()?;
    let metadata = fs::metadata(path).ok()?;
    Some(ConfigStamp {
        modified: metadata.modified().ok(),
        length: metadata.len(),
    })
}

pub(crate) fn validate_config(config: &Config) -> Result<(), String> {
    let hotkey = parse_hotkey(&config.hotkey)?;
    let cancel = parse_cancel_hotkey(&config.cancel_hotkey)?;
    if hotkey.virtual_key == cancel.virtual_key && hotkey.modifiers == cancel.modifiers {
        return Err("cancel_hotkey must differ from hotkey".to_owned());
    }
    if config.max_seconds == 0 {
        return Err("max_seconds must be greater than zero".to_owned());
    }
    if config.provider == Provider::Elevenlabs && config.scribe_credits_per_hour == 0 {
        return Err("scribe_credits_per_hour must be greater than zero".to_owned());
    }
    if !config.mode.eq_ignore_ascii_case("toggle")
        && !config.mode.eq_ignore_ascii_case("push_to_talk")
    {
        return Err("mode must be either toggle or push_to_talk".to_owned());
    }
    if !config.insert_method.eq_ignore_ascii_case("paste")
        && !config.insert_method.eq_ignore_ascii_case("type")
    {
        return Err("insert_method must be either paste or type".to_owned());
    }
    if config.provider == Provider::Elevenlabs {
        realtime::validate_keyterms(&config.keyterms).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn history_label(recording: &Recording) -> String {
    let preview = match recording.status {
        RecordingStatus::Pending => "Transcribing…".to_owned(),
        RecordingStatus::Failed => "✕ Transcription failed — click to retry".to_owned(),
        RecordingStatus::Succeeded => {
            let text = recording.transcript.as_deref().unwrap_or_default();
            let preview = history_preview(text);
            if recording.delivered == Some(false) {
                format!("⚠ {preview}")
            } else {
                preview
            }
        }
    };
    format!("{preview}\t{}", format_duration(recording.duration_seconds))
}

pub(crate) fn history_item_visible(recording: &Recording) -> bool {
    match recording.status {
        RecordingStatus::Pending | RecordingStatus::Failed => true,
        RecordingStatus::Succeeded => recording
            .transcript
            .as_deref()
            .is_some_and(|transcript| !transcript.trim().is_empty()),
    }
}

pub(crate) fn history_preview(transcript: &str) -> String {
    let normalized = transcript.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview: String = normalized.chars().take(40).collect();
    if normalized.chars().count() > 40 {
        preview.push('…');
    }
    preview
}

pub(crate) fn format_duration(seconds: u32) -> String {
    format!("{:02}m{:02}s", seconds / 60, seconds % 60)
}

pub(crate) fn parse_hotkey(value: &str) -> Result<Hotkey, String> {
    parse_hotkey_inner(value, false)
}

pub(crate) fn parse_cancel_hotkey(value: &str) -> Result<Hotkey, String> {
    parse_hotkey_inner(value, true)
}

fn parse_hotkey_inner(value: &str, allow_unmodified_escape: bool) -> Result<Hotkey, String> {
    let mut win = false;
    let mut alt = false;
    let mut shift = false;
    let mut control = false;
    let mut key: Option<(u32, String)> = None;
    for part in value
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if part.eq_ignore_ascii_case("win") || part.eq_ignore_ascii_case("windows") {
            win = true;
        } else if part.eq_ignore_ascii_case("alt") {
            alt = true;
        } else if part.eq_ignore_ascii_case("shift") {
            shift = true;
        } else if part.eq_ignore_ascii_case("ctrl") || part.eq_ignore_ascii_case("control") {
            control = true;
        } else {
            if key.is_some() {
                return Err(format!(
                    "Hotkey {value:?} has more than one non-modifier key."
                ));
            }
            key = Some(parse_key(part).ok_or_else(|| {
                format!("Hotkey key {part:?} is unsupported. Use a letter, digit, F1–F24, Space, Enter, Tab, or Esc.")
            })?);
        }
    }
    if !(win || alt || shift || control)
        && !(allow_unmodified_escape && key.as_ref().is_some_and(|(key, _)| *key == 0x1B))
    {
        return Err(format!("Hotkey {value:?} needs at least one modifier."));
    }
    let (virtual_key, label) = key.ok_or_else(|| format!("Hotkey {value:?} has no key."))?;
    Ok(Hotkey::with_modifiers(
        virtual_key,
        label,
        HotkeyModifiers::with_all_modifiers(win, alt, shift, control),
    ))
}

fn parse_key(value: &str) -> Option<(u32, String)> {
    if value.len() == 1 {
        let character = value.as_bytes()[0].to_ascii_uppercase();
        if character.is_ascii_alphanumeric() {
            return Some((u32::from(character), char::from(character).to_string()));
        }
    }
    if let Some(number) = value.strip_prefix('F').or_else(|| value.strip_prefix('f')) {
        if let Ok(number) = number.parse::<u32>()
            && (1..=24).contains(&number)
        {
            return Some((0x70 + number - 1, format!("F{number}")));
        }
    }
    match value.to_ascii_lowercase().as_str() {
        "space" => Some((0x20, "Space".to_owned())),
        "enter" => Some((0x0D, "Enter".to_owned())),
        "tab" => Some((0x09, "Tab".to_owned())),
        "esc" | "escape" => Some((0x1B, "Esc".to_owned())),
        _ => None,
    }
}

pub(crate) fn save_config(config: &Config, ui: &UiRuntime) {
    if let Err(error) = config.save() {
        show_notice(
            ui,
            "Scribetray settings",
            &format!("Could not save settings: {error}"),
        );
    }
}

pub(crate) fn refresh_history_menu(ui: &UiRuntime, history: &History) {
    match history.list() {
        Ok(recordings) => {
            let _ = ui.send(UiCommand::UpdateHistory(
                make_ui_settings(&Config::default(), recordings, Vec::new()).history,
            ));
        }
        Err(error) => warn!("could not refresh history menu: {error}"),
    }
}

pub(crate) fn open_config_file() -> Result<(), String> {
    Config::load_or_create().map_err(|error| error.to_string())?;
    let path = config_path().map_err(|error| error.to_string())?;
    std::process::Command::new("notepad.exe")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open the config file in Notepad: {error}"))
}

#[cfg(test)]
mod provider_validation_tests {
    use super::*;

    #[test]
    fn cancel_hotkey_validation_handles_defaults_aliases_and_conflicts() {
        let mut config = Config::default();
        assert!(validate_config(&config).is_ok());
        assert_eq!(parse_cancel_hotkey("escape").unwrap().virtual_key, 0x1b);
        assert!(parse_hotkey("Esc").is_err());
        config.cancel_hotkey = "Alt+Windows+v".into();
        assert!(validate_config(&config).is_err());
        config.cancel_hotkey = "Win+Alt+X".into();
        assert!(validate_config(&config).is_ok());
        config.cancel_hotkey = "bad key".into();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn elevenlabs_options_are_ignored_by_openai_validation() {
        let mut config = Config {
            provider: Provider::Openai,
            scribe_credits_per_hour: 0,
            keyterms: vec![String::new()],
            realtime: true,
            ..Config::default()
        };
        assert!(validate_config(&config).is_ok());
        config.provider = Provider::Elevenlabs;
        assert!(validate_config(&config).is_err());
        config.scribe_credits_per_hour = 585;
        assert!(validate_config(&config).is_err());
    }
}
