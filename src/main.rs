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
mod windows_ui;
mod wininput;

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use audio::AudioRecorder;
use config::{Config, config_path};
use directories::BaseDirs;
use history::{History, Recording, RecordingStatus};
use scribe::{ScribeClient, Transcription};
use tracing::{error, info, warn};
use windows_ui::{
    AnchorStatus, CaretRect as UiCaretRect, HistoryMenuItem, Hotkey, HotkeyModifiers,
    LanguageOption, UiCommand, UiEvent, UiRuntime, UiSettings,
};
use wininput::{InsertMethod, TargetSnapshot};

const APP_DIRECTORY: &str = "Scribetray";
const TICK: Duration = Duration::from_millis(200);
const DONE_DISPLAY: Duration = Duration::from_millis(1_200);

#[derive(Debug)]
struct WorkerFinished {
    history_id: String,
    result: Result<Transcription, String>,
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
    submit_on_complete: bool,
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
    parse_hotkey(&config.hotkey)?;
    parse_hotkey(&config.hotkey_submit)?;
    if config.start_with_windows {
        if let Err(error) = autostart::set_enabled(true) {
            warn!("could not apply Start with Windows setting: {error}");
        }
    }
    let history = History::open().map_err(|error| error.to_string())?;
    let mut ui_settings = make_ui_settings(&config, history.list().unwrap_or_default());
    let ui = UiRuntime::start(ui_settings.clone())?;
    let (worker_tx, worker_rx) = mpsc::channel();

    let mut active: Option<ActiveRecording> = None;
    let mut deliveries: HashMap<String, Delivery> = HashMap::new();
    let mut submit_after_transcription: HashMap<String, bool> = HashMap::new();
    let mut done_until: Option<Instant> = None;

    info!("Scribetray started");
    loop {
        match ui.events.recv_timeout(TICK) {
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
        update_timers(&ui, active.as_ref(), &mut done_until);
    }

    if let Some(recording) = active.take() {
        drop(recording);
        let _ = ui.send(UiCommand::SetRecording(false));
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
                start_recording(active, false, config, ui, done_until);
            }
        }
        UiEvent::SubmitHotkeyRecord => {
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
            } else {
                start_recording(active, true, config, ui, done_until);
            }
        }
        UiEvent::PushToTalkPressed => {
            if active.is_none() {
                start_recording(active, false, config, ui, done_until);
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
        UiEvent::HistoryCopy(id) => match history.get(&id) {
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
        },
        UiEvent::HistoryRetry(id) => match history.get(&id) {
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
        },
        UiEvent::HotkeyRegistrationFailed { error, .. } => warn!("{error}"),
        UiEvent::EscapeRegistrationFailed { error } => warn!("Escape hotkey unavailable: {error}"),
        UiEvent::PushToTalkHookFailed { error } => {
            warn!("{error}");
            config.mode = "toggle".to_owned();
            save_config(config, ui);
        }
    }

    *ui_settings = make_ui_settings(config, history.list().unwrap_or_default());
    if let Err(error) = ui.send(UiCommand::SetSettings(ui_settings.clone())) {
        warn!("could not refresh tray settings: {error}");
    }
    true
}

fn start_recording(
    active: &mut Option<ActiveRecording>,
    submit_on_complete: bool,
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

    let caret = target.caret;
    info!(
        "recording started; caret method={:?}",
        caret.map(|item| item.method)
    );
    *active = Some(ActiveRecording {
        recorder,
        realtime_result,
        target,
        started: Instant::now(),
        submit_on_complete,
    });
    *done_until = None;
    let _ = ui.send(UiCommand::SetRecording(true));
    play_cue(config.sound_cues, 0x40);
    update_timers(ui, active.as_ref(), done_until);
}

#[allow(clippy::too_many_arguments)]
fn stop_recording(
    active: &mut Option<ActiveRecording>,
    submit_hotkey: bool,
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
    let enter_after = active_recording.submit_on_complete || submit_hotkey;
    let captured = match active_recording.recorder.stop() {
        Ok(captured) => captured,
        Err(error) => {
            let _ = ui.send(UiCommand::SetRecording(false));
            let _ = ui.hide_caret_anchor();
            show_notice(ui, "Scribetray recording", &error.to_string());
            play_cue(config.sound_cues, 0x10);
            return;
        }
    };

    let _ = ui.send(UiCommand::SetRecording(false));
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
    deliveries.insert(id.clone(), delivery);
    submit_after_transcription.insert(id.clone(), enter_after);

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
                Err(realtime_error) => match api_key {
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
                },
            };
            let _ = sender.send(WorkerFinished {
                history_id: job_id,
                result,
            });
        });

    if let Err(error) = task {
        deliveries.remove(&id);
        submit_after_transcription.remove(&id);
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
    deliveries.insert(id.clone(), delivery);
    submit_after_transcription.insert(id.clone(), enter_after);

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
        deliveries.remove(&id);
        submit_after_transcription.remove(&id);
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
                    config,
                    ui,
                    recording_active,
                    done_until,
                );
            }
            Err(message) => {
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
        refresh_history_menu(ui, history);
    }
}

fn complete_transcription(
    transcription: Transcription,
    delivery: Option<Delivery>,
    enter_after: bool,
    config: &Config,
    ui: &UiRuntime,
    recording_active: bool,
    done_until: &mut Option<Instant>,
) {
    let mut text = transcription.text;
    if text.trim().is_empty() {
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
                    if enter_after || config.auto_enter {
                        if let Err(error) = wininput::send_enter(&target) {
                            show_notice(
                                ui,
                                "Scribetray",
                                &format!("Text was inserted, but Enter failed: {error}"),
                            );
                        }
                    }
                    info!("transcription inserted; method={method:?}");
                    if !recording_active {
                        if let Some(caret) = target.caret {
                            let _ =
                                ui.update_caret_anchor(ui_caret(caret.rect), AnchorStatus::Done);
                        }
                        *done_until = Some(Instant::now() + DONE_DISPLAY);
                    }
                    play_cue(config.sound_cues, 0x40);
                }
                Err(error) => {
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
            match wininput::copy_text(&text) {
                Ok(()) => show_notice(
                    ui,
                    "Scribetray history",
                    "Transcription ready and copied to clipboard.",
                ),
                Err(error) => show_notice(
                    ui,
                    "Scribetray history",
                    &format!("Could not copy transcription: {error}"),
                ),
            }
            if !recording_active {
                *done_until = Some(Instant::now() + DONE_DISPLAY);
            }
            play_cue(config.sound_cues, 0x40);
        }
    }
}

fn update_timers(
    ui: &UiRuntime,
    active: Option<&ActiveRecording>,
    done_until: &mut Option<Instant>,
) {
    if let Some(recording) = active {
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

fn make_ui_settings(config: &Config, recordings: Vec<Recording>) -> UiSettings {
    let toggle_hotkey = parse_hotkey(&config.hotkey).unwrap_or_default();
    let submit_hotkey = parse_hotkey(&config.hotkey_submit).unwrap_or_else(|_| {
        Hotkey::with_modifiers(b'V' as u32, "V", HotkeyModifiers::new(true, false))
    });
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
        .take(history::MAX_RECORDINGS)
        .map(|recording| HistoryMenuItem {
            label: history_label(&recording),
            id: recording.id,
            can_copy: recording.transcript.is_some(),
            can_retry: recording.status != RecordingStatus::Succeeded,
        })
        .collect();
    UiSettings {
        toggle_hotkey,
        submit_hotkey,
        push_to_talk: config.mode.eq_ignore_ascii_case("push_to_talk"),
        realtime_enabled: config.realtime,
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

fn history_label(recording: &Recording) -> String {
    let minutes = recording.duration_seconds / 60;
    let seconds = recording.duration_seconds % 60;
    let status = match recording.status {
        RecordingStatus::Pending => "Pending".to_owned(),
        RecordingStatus::Succeeded => "Done".to_owned(),
        RecordingStatus::Failed => "Failed".to_owned(),
    };
    format!("{minutes:02}:{seconds:02} — {status}")
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
                make_ui_settings(&Config::default(), recordings).history,
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
    let _ = ui.send(UiCommand::Notice {
        title: title.to_owned(),
        message: message.to_owned(),
    });
}

fn play_cue(enabled: bool, cue: u32) {
    if enabled {
        unsafe {
            let _ = MessageBeep(cue);
        }
    }
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
