#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_os = "windows"))]
compile_error!("Scribetray is a Windows-only desktop application");

mod audio;
mod autostart;
mod config;
mod history;
mod hotkey_dialog;
mod realtime;
mod scribe;
mod subscription;
mod windows_ui;
mod wininput;

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    mem::size_of,
    sync::{
        OnceLock,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use audio::AudioRecorder;
use config::{Config, config_path};
use directories::BaseDirs;
use history::{History, Recording, RecordingStatus};
use scribe::{ScribeClient, Transcription};
use tracing::{error, info, warn};
use windows::{
    Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT},
    core::PCWSTR,
};
use windows_ui::{
    AnchorStatus, CaretRect as UiCaretRect, HistoryMenuItem, Hotkey, HotkeyModifiers,
    LanguageOption, UiCommand, UiEvent, UiRuntime, UiSettings,
};
use wininput::{InsertMethod, TargetSnapshot};

const APP_DIRECTORY: &str = "Scribetray";
const TICK: Duration = Duration::from_millis(200);
const CARET_REFRESH: Duration = Duration::from_millis(250);
const DONE_DISPLAY: Duration = Duration::from_millis(2_300);
const CUE_SAMPLE_RATE: u32 = 22_050;
const SUBSCRIPTION_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

static SOUND_WAVES: OnceLock<[Vec<u8>; 2]> = OnceLock::new();

#[derive(Debug)]
struct WorkerFinished {
    history_id: String,
    result: Result<Transcription, String>,
}

struct SubscriptionFinished {
    generation: u64,
    result: Result<subscription::UsageSnapshot, subscription::UsageError>,
}

#[derive(Default)]
struct SubscriptionCache {
    generation: u64,
    api_key_fingerprint: Option<u64>,
    last_success: Option<Instant>,
    last_attempt: Option<Instant>,
    fetching: bool,
    permission_denied: bool,
}

impl SubscriptionCache {
    fn reset_for_config(
        &mut self,
        config: &Config,
        ui: &UiRuntime,
        finished_tx: &Sender<SubscriptionFinished>,
    ) {
        self.generation = self.generation.wrapping_add(1);
        self.last_attempt = None;
        self.fetching = false;
        self.permission_denied = false;
        let fingerprint = config
            .resolved_api_key()
            .as_deref()
            .map(api_key_fingerprint);
        if fingerprint != self.api_key_fingerprint {
            self.last_success = None;
            let _ = ui.send(UiCommand::SetUsage(None));
        }
        self.api_key_fingerprint = fingerprint;
        self.refresh(config, finished_tx, true);
    }

    fn refresh(
        &mut self,
        config: &Config,
        finished_tx: &Sender<SubscriptionFinished>,
        force: bool,
    ) {
        if self.fetching || self.permission_denied {
            return;
        }
        let Some(api_key) = config.resolved_api_key() else {
            return;
        };
        if !force {
            let stale = self
                .last_success
                .is_none_or(|last_success| last_success.elapsed() >= SUBSCRIPTION_REFRESH_INTERVAL);
            let recently_attempted = self
                .last_attempt
                .is_some_and(|last_attempt| last_attempt.elapsed() < SUBSCRIPTION_REFRESH_INTERVAL);
            if !stale || recently_attempted {
                return;
            }
        }

        self.fetching = true;
        self.last_attempt = Some(Instant::now());
        let generation = self.generation;
        let finished_tx = finished_tx.clone();
        thread::spawn(move || {
            let result = subscription::fetch_usage(&api_key);
            let _ = finished_tx.send(SubscriptionFinished { generation, result });
        });
    }

    fn apply_finished(&mut self, finished: SubscriptionFinished, ui: &UiRuntime) {
        if finished.generation != self.generation {
            return;
        }
        self.fetching = false;
        match finished.result {
            Ok(usage) => {
                self.last_success = Some(Instant::now());
                let _ = ui.send(UiCommand::SetUsage(Some(usage)));
            }
            Err(subscription::UsageError::PermissionDenied) => {
                self.permission_denied = true;
                let _ = ui.send(UiCommand::SetUsage(None));
                warn!("ElevenLabs usage line hidden: API key is missing the user_read permission");
            }
            Err(subscription::UsageError::HttpStatus(status)) => {
                warn!("could not refresh ElevenLabs usage: HTTP {status}");
            }
            Err(subscription::UsageError::RequestFailed) => {
                warn!("could not refresh ElevenLabs usage: request failed");
            }
            Err(subscription::UsageError::InvalidResponse) => {
                warn!("could not refresh ElevenLabs usage: invalid subscription response");
            }
            Err(subscription::UsageError::ClientInitialization) => {
                warn!("could not initialize the ElevenLabs usage HTTP client");
            }
        }
    }
}

enum Delivery {
    Paste(TargetSnapshot),
    Clipboard,
}

struct ActiveRecording {
    recorder: AudioRecorder,
    realtime_result: Option<Receiver<Result<Transcription, String>>>,
    target: TargetSnapshot,
    started: Instant,
    next_caret_refresh: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConfigStamp {
    modified: Option<SystemTime>,
    length: u64,
}

struct ConfigMonitor {
    observed: Option<ConfigStamp>,
}

impl ConfigMonitor {
    fn new() -> Self {
        Self {
            observed: config_stamp(),
        }
    }

    fn load_if_changed(&mut self, current: &Config) -> Option<Config> {
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
    if config.resolved_api_key().is_none() {
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

    info!("Scribetray started");
    loop {
        match ui.events.recv_timeout(TICK) {
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

        if active
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
            show_notice(&ui, "Scribetray", "Maximum recording length reached.");
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
        if active.is_none()
            && let Some(updated) = config_monitor.load_if_changed(&config)
        {
            config = updated;
            subscription_cache.reset_for_config(&config, &ui, &subscription_tx);
            let microphones = ui_settings.microphones.clone();
            ui_settings =
                make_ui_settings(&config, history.list().unwrap_or_default(), microphones);
            if let Err(error) = ui.send(UiCommand::SetSettings(ui_settings.clone())) {
                warn!("could not apply reloaded settings: {error}");
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

fn api_key_fingerprint(api_key: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    api_key.hash(&mut hasher);
    hasher.finish()
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
                play_cue(config.sound_cues, 0x10);
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
                    Ok(_) => {
                        config.hotkey = hotkey;
                        save_config(config, ui);
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
            config.realtime = !config.realtime;
            save_config(config, ui);
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
        UiEvent::ToggleSound => {
            config.sound_cues = !config.sound_cues;
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
        UiEvent::EscapeRegistrationFailed { error } => warn!("Escape hotkey unavailable: {error}"),
        UiEvent::PushToTalkHookFailed { error } => {
            warn!("{error}");
            config.mode = "toggle".to_owned();
            save_config(config, ui);
        }
    }

    let microphones = ui_settings.microphones.clone();
    *ui_settings = make_ui_settings(config, history.list().unwrap_or_default(), microphones);
    if let Err(error) = ui.send(UiCommand::SetSettings(ui_settings.clone())) {
        warn!("could not refresh tray settings: {error}");
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn recover_last_dictation(
    config: &Config,
    ui: &UiRuntime,
    history: &History,
    worker_tx: &Sender<WorkerFinished>,
    deliveries: &mut HashMap<String, Delivery>,
    submit_after_transcription: &mut HashMap<String, bool>,
) {
    let _ = ui.send(UiCommand::SetTrayError(false));
    let recordings = match history.list() {
        Ok(recordings) => recordings,
        Err(error) => {
            show_notice(ui, "Scribetray history", &error.to_string());
            return;
        }
    };
    let Some(recording) = recordings.into_iter().find(history_item_visible) else {
        show_notice(ui, "Scribetray history", "Nothing to recover yet");
        return;
    };

    match recording.status {
        RecordingStatus::Pending => {
            show_notice(ui, "Scribetray history", "Still transcribing…");
        }
        RecordingStatus::Failed => match history.read_audio(&recording.id) {
            Ok(pcm) => {
                show_notice(
                    ui,
                    "Scribetray history",
                    &format!("Retrying {}…", format_duration(recording.duration_seconds)),
                );
                start_transcription(
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
                );
            }
            Err(error) => show_notice(ui, "Scribetray history", &error.to_string()),
        },
        RecordingStatus::Succeeded => {
            let Some(transcript) = recording
                .transcript
                .as_deref()
                .filter(|transcript| !transcript.trim().is_empty())
            else {
                show_notice(ui, "Scribetray history", "Nothing to recover yet");
                return;
            };
            match wininput::copy_text(transcript) {
                Ok(()) => show_notice(
                    ui,
                    "Scribetray history",
                    &format!(
                        "Copied: \"{}\" · {}",
                        history_preview(transcript),
                        format_duration(recording.duration_seconds)
                    ),
                ),
                Err(error) => show_notice(
                    ui,
                    "Scribetray history",
                    &format!("Could not copy transcript: {error}"),
                ),
            }
        }
    }
}

fn start_recording(
    active: &mut Option<ActiveRecording>,
    config: &Config,
    ui: &UiRuntime,
    done_until: &mut Option<Instant>,
) {
    let target = match wininput::capture_target() {
        Ok(target) => target,
        Err(error) => {
            show_notice(
                ui,
                "Scribetray",
                &format!("Could not find the focused input: {error}"),
            );
            return;
        }
    };
    let (recorder, realtime_result) = if config.realtime
        && let Some(api_key) = config.resolved_api_key()
    {
        let keyterms = match realtime::validate_keyterms(&config.keyterms) {
            Ok(keyterms) => keyterms,
            Err(error) => {
                show_notice(ui, "Scribetray vocabulary", &error.to_string());
                return;
            }
        };
        let (recorder, audio_chunks) =
            match AudioRecorder::start_realtime(config.microphone.as_deref(), config.max_seconds) {
                Ok(started) => started,
                Err(error) => {
                    show_notice(ui, "Scribetray microphone", &error.to_string());
                    return;
                }
            };
        let (result_tx, result_rx) = mpsc::channel();
        let language = language_for_api(&config.language);
        match thread::Builder::new()
            .name("scribetray-realtime".to_owned())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "Could not start the realtime transcription runtime.".to_owned())
                    .and_then(|runtime| {
                        runtime
                            .block_on(realtime::transcribe_stream(
                                &api_key,
                                language.as_deref(),
                                &keyterms,
                                audio_chunks,
                            ))
                            .map_err(|error| error.to_string())
                    });
                let _ = result_tx.send(result);
            }) {
            Ok(_) => (recorder, Some(result_rx)),
            Err(error) => {
                show_notice(
                    ui,
                    "Scribetray realtime",
                    &format!(
                        "Could not start the realtime connection; batch transcription will be used: {error}"
                    ),
                );
                (recorder, None)
            }
        }
    } else {
        match AudioRecorder::start(config.microphone.as_deref(), config.max_seconds) {
            Ok(recorder) => (recorder, None),
            Err(error) => {
                show_notice(ui, "Scribetray microphone", &error.to_string());
                return;
            }
        }
    };

    let level_meter = recorder.level_meter();
    let caret = target.caret;
    info!(
        "recording started; transcription mode={}; caret method={:?}",
        if config.realtime { "realtime" } else { "batch" },
        caret.map(|item| item.method)
    );
    *active = Some(ActiveRecording {
        recorder,
        realtime_result,
        target,
        started: Instant::now(),
        next_caret_refresh: Instant::now(),
    });
    *done_until = None;
    let _ = ui.send(UiCommand::SetRecording(true));
    let _ = ui.send(UiCommand::SetLevelMeter(Some(level_meter)));
    play_cue(config.sound_cues, 0x40);
    update_timers(ui, active.as_mut(), done_until);
}

#[allow(clippy::too_many_arguments)]
fn stop_recording(
    active: &mut Option<ActiveRecording>,
    send_enter_after: bool,
    config: &Config,
    ui: &UiRuntime,
    history: &History,
    worker_tx: &Sender<WorkerFinished>,
    deliveries: &mut HashMap<String, Delivery>,
    submit_after_transcription: &mut HashMap<String, bool>,
) {
    let Some(active_recording) = active.take() else {
        return;
    };
    let target = active_recording.target;
    let realtime_result = active_recording.realtime_result;
    let enter_after = send_enter_after;
    let captured = match active_recording.recorder.stop() {
        Ok(captured) => captured,
        Err(error) => {
            let _ = ui.send(UiCommand::SetRecording(false));
            let _ = ui.send(UiCommand::SetLevelMeter(None));
            let _ = ui.send(UiCommand::SetTrayError(true));
            let _ = ui.hide_caret_anchor();
            show_notice(ui, "Scribetray recording", &error.to_string());
            play_cue(config.sound_cues, 0x10);
            return;
        }
    };

    let _ = ui.send(UiCommand::SetRecording(false));
    let _ = ui.send(UiCommand::SetLevelMeter(None));
    play_cue(config.sound_cues, 0x40);
    match history.create_pending_pcm(&captured.pcm_16k_mono, captured.duration_seconds) {
        Ok(recording) => {
            let anchor = target.caret.map(|caret| ui_caret(caret.rect));
            if let Some(rect) = anchor {
                let _ = ui.update_caret_anchor(rect, AnchorStatus::Working);
            }
            if let Some(realtime_result) = realtime_result {
                start_streaming_transcription(
                    recording,
                    captured.pcm_16k_mono,
                    realtime_result,
                    Delivery::Paste(target),
                    enter_after,
                    config,
                    ui,
                    history,
                    worker_tx,
                    deliveries,
                    submit_after_transcription,
                );
            } else {
                start_transcription(
                    recording,
                    captured.pcm_16k_mono,
                    Delivery::Paste(target),
                    enter_after,
                    config,
                    ui,
                    history,
                    worker_tx,
                    deliveries,
                    submit_after_transcription,
                );
            }
        }
        Err(error) => {
            let _ = ui.hide_caret_anchor();
            let _ = ui.send(UiCommand::SetTrayError(true));
            show_notice(ui, "Scribetray history", &error.to_string());
        }
    }
    refresh_history_menu(ui, history);
}

#[allow(clippy::too_many_arguments)]
fn start_streaming_transcription(
    recording: Recording,
    pcm: Vec<u8>,
    realtime_result: Receiver<Result<Transcription, String>>,
    delivery: Delivery,
    enter_after: bool,
    config: &Config,
    ui: &UiRuntime,
    history: &History,
    worker_tx: &Sender<WorkerFinished>,
    deliveries: &mut HashMap<String, Delivery>,
    submit_after_transcription: &mut HashMap<String, bool>,
) {
    let id = recording.id;
    if deliveries.contains_key(&id) {
        show_notice(
            ui,
            "Scribetray history",
            "This recording is already being transcribed.",
        );
        return;
    }
    if let Err(error) = history.mark_pending(&id) {
        warn!("could not mark retry as pending: {error}");
    }
    deliveries.insert(id.clone(), delivery);
    submit_after_transcription.insert(id.clone(), enter_after);
    let _ = ui.send(UiCommand::SetTranscribing(deliveries.len()));

    let sender = worker_tx.clone();
    let api_key = config.resolved_api_key();
    let model = config.model.clone();
    let language = language_for_api(&config.language);
    let job_id = id.clone();
    let task = thread::Builder::new()
        .name("scribetray-transcription".to_owned())
        .spawn(move || {
            let realtime = realtime_result
                .recv()
                .unwrap_or_else(|_| Err("Realtime transcription worker stopped unexpectedly.".to_owned()));
            let result = match realtime {
                Ok(transcription) => Ok(transcription),
                Err(realtime_error) => {
                    warn!("Realtime transcription failed ({realtime_error}); retrying the complete audio with batch Scribe");
                    match api_key {
                        Some(api_key) => ScribeClient::new()
                            .and_then(|client| {
                                client.transcribe(&api_key, &pcm, &model, language.as_deref())
                            })
                            .map_err(|batch_error| {
                                format!(
                                    "Realtime transcription failed ({realtime_error}); batch retry failed ({batch_error})."
                                )
                            }),
                        None => Err("ELEVENLABS_API_KEY is not configured".to_owned()),
                    }
                }
            };
            let _ = sender.send(WorkerFinished {
                history_id: job_id,
                result,
            });
        });

    if let Err(error) = task {
        set_anchor_error(ui, deliveries.get(&id));
        deliveries.remove(&id);
        submit_after_transcription.remove(&id);
        let _ = ui.send(UiCommand::SetTranscribing(deliveries.len()));
        let _ = ui.send(UiCommand::SetTrayError(true));
        let _ = history.mark_failed(&id, "Could not start transcription worker");
        show_notice(
            ui,
            "Scribetray",
            &format!("Could not start transcription: {error}"),
        );
    }
    refresh_history_menu(ui, history);
}

#[allow(clippy::too_many_arguments)]
fn start_transcription(
    recording: Recording,
    pcm: Vec<u8>,
    delivery: Delivery,
    enter_after: bool,
    config: &Config,
    ui: &UiRuntime,
    history: &History,
    worker_tx: &Sender<WorkerFinished>,
    deliveries: &mut HashMap<String, Delivery>,
    submit_after_transcription: &mut HashMap<String, bool>,
) {
    let id = recording.id;
    if deliveries.contains_key(&id) {
        show_notice(
            ui,
            "Scribetray history",
            "This recording is already being transcribed.",
        );
        return;
    }
    if let Err(error) = history.mark_pending(&id) {
        warn!("could not mark retry as pending: {error}");
    }
    deliveries.insert(id.clone(), delivery);
    submit_after_transcription.insert(id.clone(), enter_after);
    let _ = ui.send(UiCommand::SetTranscribing(deliveries.len()));

    let sender = worker_tx.clone();
    let api_key = config.resolved_api_key();
    let model = config.model.clone();
    let language = language_for_api(&config.language);
    let job_id = id.clone();
    let task = thread::Builder::new()
        .name("scribetray-transcription".to_owned())
        .spawn(move || {
            let result = match api_key {
                Some(api_key) => ScribeClient::new()
                    .and_then(|client| {
                        client.transcribe(&api_key, &pcm, &model, language.as_deref())
                    })
                    .map_err(|error| error.to_string()),
                None => Err("ELEVENLABS_API_KEY is not configured".to_owned()),
            };
            let _ = sender.send(WorkerFinished {
                history_id: job_id,
                result,
            });
        });

    if let Err(error) = task {
        set_anchor_error(ui, deliveries.get(&id));
        deliveries.remove(&id);
        submit_after_transcription.remove(&id);
        let _ = ui.send(UiCommand::SetTranscribing(deliveries.len()));
        let _ = ui.send(UiCommand::SetTrayError(true));
        let _ = history.mark_failed(&id, "Could not start transcription worker");
        show_notice(
            ui,
            "Scribetray",
            &format!("Could not start transcription: {error}"),
        );
    }
    refresh_history_menu(ui, history);
}

fn drain_worker_events(
    worker_rx: &Receiver<WorkerFinished>,
    config: &mut Config,
    ui: &UiRuntime,
    history: &History,
    deliveries: &mut HashMap<String, Delivery>,
    submit_after_transcription: &mut HashMap<String, bool>,
    recording_active: bool,
    done_until: &mut Option<Instant>,
) {
    while let Ok(finished) = worker_rx.try_recv() {
        let delivery = deliveries.remove(&finished.history_id);
        let enter_after = submit_after_transcription
            .remove(&finished.history_id)
            .unwrap_or(false);

        match finished.result {
            Ok(transcription) => {
                if let Err(error) = history.mark_succeeded(
                    &finished.history_id,
                    transcription.text.clone(),
                    transcription.detected_language.clone(),
                ) {
                    error!("could not update transcription history: {error}");
                }
                complete_transcription(
                    transcription,
                    delivery,
                    enter_after,
                    history,
                    &finished.history_id,
                    config,
                    ui,
                    recording_active,
                    done_until,
                );
            }
            Err(message) => {
                let _ = ui.send(UiCommand::SetTrayError(true));
                if let Err(error) = history.mark_failed(&finished.history_id, message.clone()) {
                    error!("could not save transcription error to history: {error}");
                }
                if !recording_active {
                    set_anchor_error(ui, delivery.as_ref());
                    *done_until = Some(Instant::now() + DONE_DISPLAY);
                }
                show_notice(ui, "Scribetray transcription", &message);
                play_cue(config.sound_cues, 0x10);
            }
        }
        let _ = ui.send(UiCommand::SetTranscribing(deliveries.len()));
        refresh_history_menu(ui, history);
    }
}

fn complete_transcription(
    transcription: Transcription,
    delivery: Option<Delivery>,
    enter_after: bool,
    history: &History,
    history_id: &str,
    config: &Config,
    ui: &UiRuntime,
    recording_active: bool,
    done_until: &mut Option<Instant>,
) {
    let mut text = transcription.text;
    if text.trim().is_empty() {
        let _ = ui.send(UiCommand::SetTrayError(true));
        set_anchor_error(ui, delivery.as_ref());
        if !recording_active {
            *done_until = Some(Instant::now() + DONE_DISPLAY);
        }
        show_notice(
            ui,
            "Scribetray",
            "Scribe returned no text for this recording.",
        );
        return;
    }
    if config.prefix_enabled {
        text.insert_str(0, &config.prefix);
    }

    match delivery {
        Some(Delivery::Paste(target)) => {
            let method = if config.insert_method.eq_ignore_ascii_case("type") {
                InsertMethod::UnicodeTyping
            } else {
                InsertMethod::Paste
            };
            match wininput::insert_text(&target, &text, method, config.restore_clipboard) {
                Ok(()) => {
                    mark_history_delivery(history, history_id, true);
                    let _ = ui.send(UiCommand::SetTrayError(false));
                    if enter_after || config.auto_enter {
                        thread::sleep(Duration::from_millis(80));
                        if let Err(error) = wininput::send_enter(&target) {
                            warn!("auto-enter failed after text insertion: {error}");
                            show_notice(
                                ui,
                                "Scribetray",
                                &format!("Text was inserted, but Enter failed: {error}"),
                            );
                        } else {
                            info!("auto-enter sent after text insertion");
                        }
                    }
                    info!("transcription inserted; method={method:?}");
                    if !recording_active {
                        let _ = ui.hide_caret_anchor();
                        *done_until = None;
                    }
                    play_cue(config.sound_cues, 0x40);
                }
                Err(error) => {
                    mark_history_delivery(history, history_id, false);
                    let _ = ui.send(UiCommand::SetTrayError(true));
                    match wininput::copy_text(&text) {
                        Ok(()) => show_notice(
                            ui,
                            "Scribetray",
                            &format!(
                                "Text was not inserted ({error}); it was copied to the clipboard."
                            ),
                        ),
                        Err(copy_error) => show_notice(
                            ui,
                            "Scribetray",
                            &format!(
                                "Text was not inserted ({error}), and clipboard copy failed: {copy_error}"
                            ),
                        ),
                    }
                    if !recording_active {
                        set_anchor_error(ui, Some(&Delivery::Paste(target)));
                        *done_until = Some(Instant::now() + DONE_DISPLAY);
                    }
                    play_cue(config.sound_cues, 0x10);
                }
            }
        }
        Some(Delivery::Clipboard) | None => {
            mark_history_delivery(history, history_id, false);
            match wininput::copy_text(&text) {
                Ok(()) => {
                    let _ = ui.send(UiCommand::SetTrayError(false));
                    show_notice(
                        ui,
                        "Scribetray history",
                        "Transcription ready and copied to clipboard.",
                    )
                }
                Err(error) => {
                    let _ = ui.send(UiCommand::SetTrayError(true));
                    show_notice(
                        ui,
                        "Scribetray history",
                        &format!("Could not copy transcription: {error}"),
                    )
                }
            }
            if !recording_active {
                let _ = ui.hide_caret_anchor();
                *done_until = None;
            }
            play_cue(config.sound_cues, 0x40);
        }
    }
}

fn update_timers(
    ui: &UiRuntime,
    active: Option<&mut ActiveRecording>,
    done_until: &mut Option<Instant>,
) {
    if let Some(recording) = active {
        let now = Instant::now();
        if now >= recording.next_caret_refresh {
            if let Some(caret) = wininput::refresh_caret(&recording.target) {
                recording.target.caret = Some(caret);
            }
            recording.next_caret_refresh = now + CARET_REFRESH;
        }
        if let Some(caret) = recording.target.caret {
            let _ = ui.update_caret_anchor(
                ui_caret(caret.rect),
                AnchorStatus::Recording {
                    elapsed: recording.started.elapsed(),
                },
            );
        }
        return;
    }
    if done_until.is_some_and(|until| Instant::now() >= until) {
        let _ = ui.hide_caret_anchor();
        *done_until = None;
    }
}

fn set_anchor_error(ui: &UiRuntime, delivery: Option<&Delivery>) {
    if let Some(Delivery::Paste(target)) = delivery {
        if let Some(caret) = target.caret {
            let _ = ui.update_caret_anchor(ui_caret(caret.rect), AnchorStatus::Error);
        }
    }
}

fn ui_caret(rect: wininput::CaretRect) -> UiCaretRect {
    UiCaretRect {
        left: rect.left.round() as i32,
        top: rect.top.round() as i32,
        width: rect.width.round().max(1.0) as i32,
        height: rect.height.round().max(1.0) as i32,
    }
}

fn make_ui_settings(
    config: &Config,
    recordings: Vec<Recording>,
    microphones: Vec<String>,
) -> UiSettings {
    let toggle_hotkey = parse_hotkey(&config.hotkey).unwrap_or_default();
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
        push_to_talk: config.mode.eq_ignore_ascii_case("push_to_talk"),
        max_seconds: config.max_seconds,
        realtime_enabled: config.realtime,
        api_key_configured: config.resolved_api_key().is_some(),
        scribe_credits_per_hour: config.scribe_credits_per_hour,
        selected_microphone: config.microphone.clone(),
        microphones,
        prefix_enabled: config.prefix_enabled,
        auto_enter: config.auto_enter,
        sound_enabled: config.sound_cues,
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

fn validate_config(config: &Config) -> Result<(), String> {
    parse_hotkey(&config.hotkey)?;
    if config.max_seconds == 0 {
        return Err("max_seconds must be greater than zero".to_owned());
    }
    if config.scribe_credits_per_hour == 0 {
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
    realtime::validate_keyterms(&config.keyterms).map_err(|error| error.to_string())?;
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

fn history_item_visible(recording: &Recording) -> bool {
    match recording.status {
        RecordingStatus::Pending | RecordingStatus::Failed => true,
        RecordingStatus::Succeeded => recording
            .transcript
            .as_deref()
            .is_some_and(|transcript| !transcript.trim().is_empty()),
    }
}

fn history_preview(transcript: &str) -> String {
    let normalized = transcript.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview: String = normalized.chars().take(40).collect();
    if normalized.chars().count() > 40 {
        preview.push('…');
    }
    preview
}

fn format_duration(seconds: u32) -> String {
    format!("{:02}m{:02}s", seconds / 60, seconds % 60)
}

fn mark_history_delivery(history: &History, history_id: &str, delivered: bool) {
    if let Err(error) = history.mark_delivered(history_id, delivered) {
        error!("could not update dictation delivery state: {error}");
    }
}

fn parse_hotkey(value: &str) -> Result<Hotkey, String> {
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
    if !(win || alt || shift || control) {
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

fn language_for_api(language: &str) -> Option<String> {
    let language = language.trim();
    if language.is_empty() || language.eq_ignore_ascii_case("auto") {
        None
    } else {
        Some(language.to_owned())
    }
}

fn save_config(config: &Config, ui: &UiRuntime) {
    if let Err(error) = config.save() {
        show_notice(
            ui,
            "Scribetray settings",
            &format!("Could not save settings: {error}"),
        );
    }
}

fn refresh_history_menu(ui: &UiRuntime, history: &History) {
    match history.list() {
        Ok(recordings) => {
            let _ = ui.send(UiCommand::UpdateHistory(
                make_ui_settings(&Config::default(), recordings, Vec::new()).history,
            ));
        }
        Err(error) => warn!("could not refresh history menu: {error}"),
    }
}

fn open_config_file() -> Result<(), String> {
    Config::load_or_create().map_err(|error| error.to_string())?;
    let path = config_path().map_err(|error| error.to_string())?;
    std::process::Command::new("notepad.exe")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open the config file in Notepad: {error}"))
}

fn initialize_logging() {
    let Some(base_dirs) = BaseDirs::new() else {
        return;
    };
    let log_directory = base_dirs.data_local_dir().join(APP_DIRECTORY).join("logs");
    if fs::create_dir_all(&log_directory).is_err() {
        return;
    }
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

fn show_notice(ui: &UiRuntime, title: &str, message: &str) {
    warn!("{title}: {message}");
    let _ = ui.send(UiCommand::Notice {
        title: title.to_owned(),
        message: message.to_owned(),
    });
}

fn play_cue(enabled: bool, cue: u32) {
    if !enabled {
        return;
    }

    let waves = SOUND_WAVES.get_or_init(|| {
        [
            make_cue_wave(&[660, 880], 65, 18),
            make_cue_wave(&[330], 150, 0),
        ]
    });
    let wave = if cue == 0x10 { &waves[1] } else { &waves[0] };
    let played = unsafe {
        PlaySoundW(
            PCWSTR::from_raw(wave.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        )
        .as_bool()
    };
    if !played {
        let fallback_played = unsafe { MessageBeep(cue) != 0 };
        warn!(
            "wave sound cue playback failed; Windows message beep fallback played={fallback_played}"
        );
    }
}

fn make_cue_wave(frequencies: &[u32], tone_ms: u32, gap_ms: u32) -> Vec<u8> {
    let tone_samples = (CUE_SAMPLE_RATE * tone_ms / 1_000) as usize;
    let gap_samples = (CUE_SAMPLE_RATE * gap_ms / 1_000) as usize;
    let sample_count =
        tone_samples * frequencies.len() + gap_samples * frequencies.len().saturating_sub(1);
    let data_bytes = (sample_count * size_of::<i16>()) as u32;
    let mut wave = Vec::with_capacity(44 + data_bytes as usize);
    wave.extend_from_slice(b"RIFF");
    wave.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wave.extend_from_slice(b"WAVEfmt ");
    wave.extend_from_slice(&16_u32.to_le_bytes());
    wave.extend_from_slice(&1_u16.to_le_bytes());
    wave.extend_from_slice(&1_u16.to_le_bytes());
    wave.extend_from_slice(&CUE_SAMPLE_RATE.to_le_bytes());
    wave.extend_from_slice(&(CUE_SAMPLE_RATE * size_of::<i16>() as u32).to_le_bytes());
    wave.extend_from_slice(&(size_of::<i16>() as u16).to_le_bytes());
    wave.extend_from_slice(&16_u16.to_le_bytes());
    wave.extend_from_slice(b"data");
    wave.extend_from_slice(&data_bytes.to_le_bytes());

    let attack_samples = (CUE_SAMPLE_RATE as usize * 5 / 1_000).max(1);
    let release_samples = (CUE_SAMPLE_RATE as usize * 14 / 1_000).max(1);
    for (tone_index, frequency) in frequencies.iter().copied().enumerate() {
        for index in 0..tone_samples {
            let attack = (index + 1) as f32 / attack_samples as f32;
            let release = (tone_samples - index) as f32 / release_samples as f32;
            let envelope = attack.min(release).min(1.0);
            let phase =
                std::f32::consts::TAU * frequency as f32 * index as f32 / CUE_SAMPLE_RATE as f32;
            let sample = (phase.sin() * envelope * 0.22 * i16::MAX as f32) as i16;
            wave.extend_from_slice(&sample.to_le_bytes());
        }
        if tone_index + 1 < frequencies.len() {
            wave.resize(wave.len() + gap_samples * size_of::<i16>(), 0);
        }
    }
    wave
}

fn show_error_dialog(message: &str) {
    let title: Vec<u16> = "Scribetray".encode_utf16().chain(Some(0)).collect();
    let body: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let _ = MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), 0x10);
    }
}

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBeep(kind: u32) -> i32;
    fn MessageBoxW(
        hwnd: *mut core::ffi::c_void,
        text: *const u16,
        title: *const u16,
        kind: u32,
    ) -> i32;
}
