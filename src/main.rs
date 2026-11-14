#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_os = "windows"))]
compile_error!("Scribetray is a Windows-only desktop application");

mod app_settings;
mod audio;
mod broadcast;
mod autostart;
mod config;
mod delivery;
mod dictation;
mod feedback;
mod history;
mod hotkey_dialog;
mod openai;
mod openai_streaming;
mod realtime;
mod scribe;
mod silence;
mod signal_stats;
mod subscription;
mod transcription;
mod usage;
mod windows_ui;
mod wininput;

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    sync::mpsc::{self, Sender},
    time::{Duration, Instant},
};

use crate::audio::AudioRecorder;
use crate::config::{Config, Provider};
use crate::history::History;
use crate::windows_ui::{UiCommand, UiEvent, UiRuntime, UiSettings};
use directories::BaseDirs;
use tracing::{error, info, warn};

use crate::app_settings::{
    ConfigMonitor, make_ui_settings, open_config_file, parse_cancel_hotkey, parse_hotkey,
    save_config, validate_config,
};
use crate::delivery::{Delivery, PENDING_TICK, has_pending_delivery, poll_pending_delivery};
use crate::dictation::{
    ActiveRecording, WorkerFinished, drain_worker_events, recover_last_dictation, start_recording,
    start_transcription, stop_recording, update_timers,
};
use crate::feedback::{Cue, play_cue, show_error_dialog, show_notice};
use crate::usage::SubscriptionCache;

const APP_DIRECTORY: &str = "Scribetray";
const TICK: Duration = Duration::from_millis(200);
const ACCOUNT_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

fn main() {
    initialize_logging();
    if let Err(message) = run() {
        error!("application stopped: {message}");
        show_error_dialog(&message);
    }
}

fn run() -> Result<(), String> {
    let mut config = Config::load_or_create().map_err(|error| error.to_string())?;
    validate_config(&config)?;
    let mut config_monitor = ConfigMonitor::new();
    if config.start_with_windows {
        if let Err(error) = autostart::set_enabled(true) {
            warn!("could not apply Start with Windows setting: {error}");
        }
    }
    let history = History::open().map_err(|error| error.to_string())?;
    let microphones = match AudioRecorder::input_device_names() {
        Ok(microphones) => microphones,
        Err(error) => {
            warn!("could not enumerate input devices: {error}");
            Vec::new()
        }
    };
    let mut ui_settings =
        make_ui_settings(&config, history.list().unwrap_or_default(), microphones);
    let ui = UiRuntime::start(ui_settings.clone())?;
    if config.provider == Provider::Elevenlabs && config.resolved_api_key().is_none() {
        show_notice(
            &ui,
            "Scribetray setup",
            "Scribetray needs an ElevenLabs API key. Right-click the tray icon and choose Set API key…",
        );
    }
    let (worker_tx, worker_rx) = mpsc::channel();
    let (subscription_tx, subscription_rx) = mpsc::channel();
    let mut subscription_cache = SubscriptionCache::default();
    subscription_cache.reset_for_config(&config, &ui, &subscription_tx);

    let mut active: Option<ActiveRecording> = None;
    let mut deliveries: HashMap<String, Delivery> = HashMap::new();
    let mut submit_after_transcription: HashMap<String, bool> = HashMap::new();
    let mut done_until: Option<Instant> = None;
    let mut account_refresh = Instant::now();

    info!("Scribetray started");
    loop {
        let tick = if has_pending_delivery() {
            PENDING_TICK
        } else {
            TICK
        };
        let event = ui.events.recv_timeout(tick);
        if active.is_none()
            && let Some(updated) = config_monitor.load_if_changed(&config)
        {
            config = updated;
            subscription_cache.reset_for_config(&config, &ui, &subscription_tx);
            let microphones = ui_settings.microphones.clone();
            ui_settings =
                make_ui_settings(&config, history.list().unwrap_or_default(), microphones);
            if let Err(error) = ui.send(UiCommand::SetSettings(Box::new(ui_settings.clone()))) {
                warn!("could not apply reloaded settings: {error}");
            }
        }
        match event {
            Ok(UiEvent::UsageRefreshRequested) => {
                subscription_cache.refresh(&config, &subscription_tx, false);
            }
            Ok(event) => {
                if !handle_ui_event(
                    event,
                    &mut config,
                    &mut ui_settings,
                    &ui,
                    &history,
                    &worker_tx,
                    &mut active,
                    &mut deliveries,
                    &mut submit_after_transcription,
                    &mut done_until,
                ) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        while let Ok(finished) = subscription_rx.try_recv() {
            subscription_cache.apply_finished(finished, &ui);
        }

        let recorder_failed = active.as_ref().is_some_and(|recording| {
            recording.config.provider == Provider::Openai && recording.recorder.has_failed()
        });
        if recorder_failed
            || active
                .as_ref()
                .is_some_and(|recording| recording.recorder.limit_reached())
        {
            stop_recording(
                &mut active,
                false,
                &config,
                &ui,
                &history,
                &worker_tx,
                &mut deliveries,
                &mut submit_after_transcription,
            );
            if !recorder_failed {
                show_notice(&ui, "Scribetray", "Maximum recording length reached.");
            }
        }
        drain_worker_events(
            &worker_rx,
            &mut config,
            &ui,
            &history,
            &mut deliveries,
            &mut submit_after_transcription,
            active.is_some(),
            &mut done_until,
        );
        update_timers(&ui, active.as_mut(), &mut done_until);
        poll_pending_delivery(&config, &ui, &history);
        if config.provider == Provider::Openai
            && account_refresh.elapsed() >= ACCOUNT_REFRESH_INTERVAL
        {
            account_refresh = Instant::now();
            let account = transcription::account_state(&config);
            if account != ui_settings.account {
                ui_settings.account = account;
                let _ = ui.send(UiCommand::SetSettings(Box::new(ui_settings.clone())));
            }
        }
    }

    if let Some(recording) = active.take() {
        drop(recording);
        let _ = ui.send(UiCommand::SetRecording(false));
        let _ = ui.send(UiCommand::SetLevelMeter(None));
        let _ = ui.hide_caret_anchor();
    }
    info!("Scribetray stopped");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_ui_event(
    event: UiEvent,
    config: &mut Config,
    ui_settings: &mut UiSettings,
    ui: &UiRuntime,
    history: &History,
    worker_tx: &Sender<WorkerFinished>,
    active: &mut Option<ActiveRecording>,
    deliveries: &mut HashMap<String, Delivery>,
    submit_after_transcription: &mut HashMap<String, bool>,
    done_until: &mut Option<Instant>,
) -> bool {
    match event {
        UiEvent::UsageRefreshRequested => return true,
        UiEvent::ToggleRecord => {
            if active.is_some() {
                stop_recording(
                    active,
                    false,
                    config,
                    ui,
                    history,
                    worker_tx,
                    deliveries,
                    submit_after_transcription,
                );
            } else {
                start_recording(active, config, ui, done_until);
            }
        }
        UiEvent::EnterPressed => {
            if active.is_some() {
                stop_recording(
                    active,
                    true,
                    config,
                    ui,
                    history,
                    worker_tx,
                    deliveries,
                    submit_after_transcription,
                );
            }
        }
        UiEvent::RecoverLast => {
            recover_last_dictation(
                config,
                ui,
                history,
                worker_tx,
                deliveries,
                submit_after_transcription,
            );
        }
        UiEvent::PushToTalkPressed => {
            if active.is_none() {
                start_recording(active, config, ui, done_until);
            }
        }
        UiEvent::PushToTalkReleased => {
            if active.is_some() {
                stop_recording(
                    active,
                    false,
                    config,
                    ui,
                    history,
                    worker_tx,
                    deliveries,
                    submit_after_transcription,
                );
            }
        }
        UiEvent::CancelRecord => {
            if active.take().is_some() {
                info!("recording cancelled");
                let _ = ui.send(UiCommand::SetRecording(false));
                let _ = ui.send(UiCommand::SetLevelMeter(None));
                let _ = ui.hide_caret_anchor();
                play_cue(config.sounds, Cue::Cancel);
            }
        }
        UiEvent::Quit => return false,
        UiEvent::OpenConfig => {
            if let Err(error) = open_config_file() {
                show_notice(ui, "Scribetray settings", &error);
            }
        }
        UiEvent::CaptureToggleHotkey => {
            match hotkey_dialog::capture_hotkey(ui.native_window_handle()) {
                Ok(Some(hotkey)) => match parse_hotkey(&hotkey) {
                    Ok(parsed) => {
                        let cancel = parse_cancel_hotkey(&config.cancel_hotkey)
                            .expect("validated cancel hotkey");
                        if parsed.virtual_key == cancel.virtual_key
                            && parsed.modifiers == cancel.modifiers
                        {
                            show_notice(
                                ui,
                                "Scribetray hotkey",
                                "Recording and cancel hotkeys must differ.",
                            );
                        } else {
                            config.hotkey = hotkey;
                            save_config(config, ui);
                        }
                    }
                    Err(error) => show_notice(ui, "Scribetray hotkey", &error),
                },
                Ok(None) => {}
                Err(error) => show_notice(ui, "Scribetray hotkey", &error),
            }
        }
        UiEvent::ToggleRecordingMode => {
            config.mode = if config.mode.eq_ignore_ascii_case("push_to_talk") {
                "toggle".to_owned()
            } else {
                "push_to_talk".to_owned()
            };
            save_config(config, ui);
        }
        UiEvent::ToggleRealtime => {
            if config.provider == Provider::Elevenlabs {
                config.realtime = !config.realtime;
                save_config(config, ui);
            }
        }
        UiEvent::MicrophoneSelected(microphone) => {
            config.microphone = microphone.filter(|name| !name.trim().is_empty());
            save_config(config, ui);
        }
        UiEvent::TogglePrefix => {
            config.prefix_enabled = !config.prefix_enabled;
            save_config(config, ui);
        }
        UiEvent::ToggleAutoEnter => {
            config.auto_enter = !config.auto_enter;
            save_config(config, ui);
        }
        UiEvent::ToggleEnterInterception => {
            config.intercept_enter = !config.intercept_enter;
            save_config(config, ui);
        }
        UiEvent::SoundsSelected(sounds) => {
            config.sounds = sounds;
            save_config(config, ui);
        }
        UiEvent::ToggleTypeMode => {
            config.insert_method = if config.insert_method.eq_ignore_ascii_case("type") {
                "paste".to_owned()
            } else {
                "type".to_owned()
            };
            save_config(config, ui);
        }
        UiEvent::ToggleAutostart => {
            let enable = !config.start_with_windows;
            match autostart::set_enabled(enable) {
                Ok(()) => {
                    config.start_with_windows = enable;
                    save_config(config, ui);
                }
                Err(error) => show_notice(ui, "Scribetray startup", &error.to_string()),
            }
        }
        UiEvent::LanguageSelected(language) => {
            config.language = language;
            save_config(config, ui);
        }
        UiEvent::HistoryCopy(id) => {
            let _ = ui.send(UiCommand::SetTrayError(false));
            match history.get(&id) {
                Ok(recording) => match recording.transcript {
                    Some(transcript) => match wininput::copy_text(&transcript) {
                        Ok(()) => {
                            show_notice(ui, "Scribetray history", "Transcript copied to clipboard.")
                        }
                        Err(error) => show_notice(
                            ui,
                            "Scribetray history",
                            &format!("Could not copy transcript: {error}"),
                        ),
                    },
                    None => show_notice(
                        ui,
                        "Scribetray history",
                        "This recording has no transcript yet.",
                    ),
                },
                Err(error) => show_notice(ui, "Scribetray history", &error.to_string()),
            }
        }
        UiEvent::HistoryRetry(id) => {
            let _ = ui.send(UiCommand::SetTrayError(false));
            match history.get(&id) {
                Ok(recording) => match history.read_audio(&id) {
                    Ok(pcm) => start_transcription(
                        recording,
                        pcm,
                        Delivery::Clipboard,
                        false,
                        config,
                        ui,
                        history,
                        worker_tx,
                        deliveries,
                        submit_after_transcription,
                    ),
                    Err(error) => show_notice(ui, "Scribetray history", &error.to_string()),
                },
                Err(error) => show_notice(ui, "Scribetray history", &error.to_string()),
            }
        }
        UiEvent::HotkeyRegistrationFailed { error, .. } => warn!("{error}"),
        UiEvent::CancelRegistrationFailed { error } => warn!("{error}"),
        UiEvent::PushToTalkHookFailed { error } => {
            warn!("{error}");
            config.mode = "toggle".to_owned();
            save_config(config, ui);
        }
    }

    let microphones = ui_settings.microphones.clone();
    *ui_settings = make_ui_settings(config, history.list().unwrap_or_default(), microphones);
    if let Err(error) = ui.send(UiCommand::SetSettings(Box::new(ui_settings.clone()))) {
        warn!("could not refresh tray settings: {error}");
    }
    true
}

const LOG_RETENTION_DAYS: u64 = 7;

/// Returns the newest suffix of a chronological log starting at the cutoff date.
fn trim_log_text<'a>(log: &'a str, cutoff: &str) -> &'a str {
    if !log.lines().any(|line| leading_timestamp(line).is_some()) {
        return log;
    }
    let mut offset = 0;
    for line in log.split_inclusive('\n') {
        let Some(timestamp) = leading_timestamp(line) else {
            offset += line.len();
            continue;
        };
        if timestamp >= cutoff {
            return &log[offset..];
        }
        offset += line.len();
    }
    ""
}

/// Trims the log before it is opened, preserving it if replacement fails.
fn trim_log_file(path: &std::path::Path) {
    let Some(cutoff) = utc_cutoff() else {
        return;
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };
    let kept = trim_log_text(&contents, &cutoff);
    if kept.len() == contents.len() {
        return;
    }

    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let temp_path = path.with_file_name(format!("{file_name}.trim-{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut temp = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)?;
        std::io::Write::write_all(&mut temp, kept.as_bytes())?;
        temp.sync_all()?;
        drop(temp);
        fs::rename(&temp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp_path);
    }
}

fn leading_timestamp(line: &str) -> Option<&str> {
    let bytes = line.as_bytes().get(..19)?;
    let valid = bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7 | 10 | 13 | 16) || byte.is_ascii_digit());
    valid.then(|| &line[..19])
}

fn utc_cutoff() -> Option<String> {
    let retention_seconds = LOG_RETENTION_DAYS.checked_mul(86_400)?;
    let cutoff =
        std::time::SystemTime::now().checked_sub(Duration::from_secs(retention_seconds))?;
    let seconds = cutoff.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let days = i64::try_from(seconds / 86_400).ok()?;
    let time = seconds % 86_400;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);

    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        time / 3_600,
        time % 3_600 / 60,
        time % 60
    ))
}

fn initialize_logging() {
    let Some(base_dirs) = BaseDirs::new() else {
        return;
    };
    let log_directory = base_dirs.data_local_dir().join(APP_DIRECTORY).join("logs");
    if fs::create_dir_all(&log_directory).is_err() {
        return;
    }
    trim_log_file(&log_directory.join("scribetray.log"));
    let file = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_directory.join("scribetray.log"))
    {
        Ok(file) => file,
        Err(_) => return,
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(file)
        .with_ansi(false)
        .with_env_filter("info")
        .try_init();
}

#[cfg(test)]
mod log_retention_tests {
    use super::trim_log_text;

    #[test]
    fn keeps_entries_from_cutoff_with_their_continuations() {
        let log = concat!(
            "2026-11-06T23:59:59.999999Z old\n",
            "  continuation of old\n",
            "2026-11-07T00:00:00.000001Z new\n",
            "  continuation of new\n",
        );

        assert_eq!(
            trim_log_text(log, "2026-11-07T00:00:00"),
            "2026-11-07T00:00:00.000001Z new\n  continuation of new\n"
        );
    }

    #[test]
    fn drops_an_all_old_log() {
        let log = concat!(
            "2026-11-06T23:59:59.999999Z old\n",
            "  continuation of old\n",
        );

        assert!(trim_log_text(log, "2026-11-07T00:00:00").is_empty());
    }
}
