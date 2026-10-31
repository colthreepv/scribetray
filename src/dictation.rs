use crate::app_settings::{
    format_duration, history_item_visible, history_preview, refresh_history_menu,
};
use crate::audio::AudioRecorder;
use crate::config::{Config, Provider};
use crate::delivery::{
    DONE_DISPLAY, Delivery, complete_transcription, discard_pending_delivery, set_anchor_error,
};
use crate::feedback::{Cue, play_cue, show_notice, ui_caret};
use crate::history::{History, Recording, RecordingStatus};
use crate::scribe::Transcription;
use crate::windows_ui::{AnchorStatus, UiCommand, UiRuntime};
use crate::wininput::{self, TargetSnapshot};
use crate::{openai_streaming, realtime, transcription};
use std::{
    collections::HashMap,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};
use tracing::{error, info, warn};

const CARET_REFRESH: Duration = Duration::from_millis(250);

const OPENAI_STREAM_STOP_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug)]
pub(crate) struct WorkerFinished {
    history_id: String,
    result: Result<Transcription, String>,
}

/// Must outlive capture and finalization. An abandoned recording never flushes.
struct StreamCancellation(Option<tokio::sync::watch::Sender<bool>>);

impl StreamCancellation {
    fn cancel(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(true);
        }
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for StreamCancellation {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(crate) struct ActiveRecording {
    // Fields drop in declaration order: signal cancellation before closing audio.
    stream_cancel: Option<StreamCancellation>,
    pub(crate) recorder: AudioRecorder,
    pub(crate) config: Config,
    stream_result: Option<Receiver<Result<Transcription, String>>>,
    target: TargetSnapshot,
    started: Instant,
    next_caret_refresh: Instant,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn recover_last_dictation(
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

pub(crate) fn start_recording(
    active: &mut Option<ActiveRecording>,
    config: &Config,
    ui: &UiRuntime,
    done_until: &mut Option<Instant>,
) {
    let account = transcription::account_state(config);
    if !account.ready {
        show_notice(ui, "Scribetray setup", account.unavailable_reason);
        return;
    }
    if discard_pending_delivery() {
        info!("new recording replaces the pending transcript; it stays on the clipboard");
    }
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
    let mut stream_cancel = None;
    let (recorder, stream_result) = if config.provider == Provider::Openai {
        let (recorder, audio_chunks) =
            match AudioRecorder::start_realtime(config.microphone.as_deref(), config.max_seconds) {
                Ok(started) => started,
                Err(error) => {
                    show_notice(ui, "Scribetray microphone", &error.to_string());
                    return;
                }
            };
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        stream_cancel = Some(StreamCancellation(Some(cancel_tx)));
        let (result_tx, result_rx) = mpsc::channel();
        let language = language_for_api(&config.language);
        let task = thread::Builder::new()
            .name("scribetray-openai-stream".to_owned())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "Could not start the OpenAI streaming runtime.".to_owned())
                    .and_then(|runtime| {
                        runtime.block_on(openai_streaming::transcribe_stream(
                            language.as_deref(),
                            audio_chunks,
                            cancel_rx,
                        ))
                    });
                // Only normal stop creates a finalization worker to receive this result.
                let _ = result_tx.send(result);
            });
        if let Err(error) = task {
            warn!("Could not start OpenAI streaming ({error}); batch transcription will be used");
            stream_cancel.take();
            (recorder, None)
        } else {
            (recorder, Some(result_rx))
        }
    } else if config.provider == Provider::Elevenlabs
        && config.realtime
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
        if stream_result.is_some() {
            "streaming"
        } else {
            "batch"
        },
        caret.map(|item| item.method)
    );
    *active = Some(ActiveRecording {
        stream_cancel,
        recorder,
        config: config.clone(),
        stream_result,
        target,
        started: Instant::now(),
        next_caret_refresh: Instant::now(),
    });
    *done_until = None;
    let _ = ui.send(UiCommand::SetRecording(true));
    let _ = ui.send(UiCommand::SetLevelMeter(Some(level_meter)));
    play_cue(config.sounds, Cue::Normal);
    update_timers(ui, active.as_mut(), done_until);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn stop_recording(
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
    let stream_result = active_recording.stream_result;
    let mut stream_cancel = active_recording.stream_cancel;
    let recording_config = active_recording.config;
    let enter_after = send_enter_after;
    let captured = if let Some(cancel) = stream_cancel.as_mut() {
        active_recording
            .recorder
            .stop_with_error_handler(|| cancel.cancel())
    } else {
        active_recording.recorder.stop()
    };
    let captured = match captured {
        Ok(captured) => captured,
        Err(error) => {
            let _ = ui.send(UiCommand::SetRecording(false));
            let _ = ui.send(UiCommand::SetLevelMeter(None));
            let _ = ui.send(UiCommand::SetTrayError(true));
            let _ = ui.hide_caret_anchor();
            show_notice(ui, "Scribetray recording", &error.to_string());
            play_cue(config.sounds, Cue::Error);
            return;
        }
    };

    let _ = ui.send(UiCommand::SetRecording(false));
    let _ = ui.send(UiCommand::SetLevelMeter(None));
    play_cue(config.sounds, Cue::Normal);
    match history.create_pending_pcm(&captured.pcm_16k_mono, captured.duration_seconds) {
        Ok(recording) => {
            let anchor = target.caret.map(|caret| ui_caret(caret.rect));
            if let Some(rect) = anchor {
                let _ = ui.update_caret_anchor(rect, AnchorStatus::Working);
            }
            if let Some(stream_result) = stream_result {
                start_streaming_transcription(
                    recording,
                    captured.pcm_16k_mono,
                    stream_result,
                    stream_cancel.take(),
                    Delivery::Paste(target),
                    enter_after,
                    &recording_config,
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
                    &recording_config,
                    ui,
                    history,
                    worker_tx,
                    deliveries,
                    submit_after_transcription,
                );
            }
        }
        Err(error) => {
            if let Some(cancel) = stream_cancel.as_mut() {
                cancel.cancel();
            }
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
    stream_result: Receiver<Result<Transcription, String>>,
    stream_cancel: Option<StreamCancellation>,
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
    let job_config = config.clone();
    let language = language_for_api(&config.language);
    let job_id = id.clone();
    let task = thread::Builder::new()
        .name("scribetray-transcription".to_owned())
        .spawn(move || {
            let timeout =
                (job_config.provider == Provider::Openai).then_some(OPENAI_STREAM_STOP_TIMEOUT);
            let result = finish_streaming(stream_result, stream_cancel, timeout, || {
                transcription::transcribe(&job_config, &pcm, language.as_deref())
            });
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

/// Called only after a successful stop and history write, never on cancellation.
/// The sole fallback boundary also makes timeout/disconnect/remote errors mutually exclusive.
fn finish_streaming(
    stream_result: Receiver<Result<Transcription, String>>,
    mut cancel: Option<StreamCancellation>,
    timeout: Option<Duration>,
    batch: impl FnOnce() -> Result<Transcription, String>,
) -> Result<Transcription, String> {
    let streamed = match timeout {
        Some(timeout) => stream_result.recv_timeout(timeout).unwrap_or_else(|error| {
            Err(match error {
                mpsc::RecvTimeoutError::Timeout => {
                    "Streaming transcription timed out after stop.".to_owned()
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    "Streaming transcription worker stopped unexpectedly.".to_owned()
                }
            })
        }),
        None => stream_result.recv().unwrap_or_else(|_| {
            Err("Streaming transcription worker stopped unexpectedly.".to_owned())
        }),
    };
    match streamed {
        Ok(transcription) => {
            if let Some(cancel) = cancel.as_mut() {
                cancel.disarm();
            }
            Ok(transcription)
        }
        Err(stream_error) => {
            if let Some(cancel) = cancel.as_mut() {
                cancel.cancel();
            }
            warn!(
                "Streaming transcription failed ({stream_error}); retrying the complete audio with the selected provider's batch transcription"
            );
            batch().map_err(|batch_error| format!(
                "Streaming transcription failed ({stream_error}); batch retry failed ({batch_error})."
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn start_transcription(
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
    let job_config = config.clone();
    let language = language_for_api(&config.language);
    let job_id = id.clone();
    let task = thread::Builder::new()
        .name("scribetray-transcription".to_owned())
        .spawn(move || {
            let result = transcription::transcribe(&job_config, &pcm, language.as_deref());
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

pub(crate) fn drain_worker_events(
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
                play_cue(config.sounds, Cue::Error);
            }
        }
        let _ = ui.send(UiCommand::SetTranscribing(deliveries.len()));
        refresh_history_menu(ui, history);
    }
}

pub(crate) fn update_timers(
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

fn language_for_api(language: &str) -> Option<String> {
    let language = language.trim();
    if language.is_empty() || language.eq_ignore_ascii_case("auto") {
        None
    } else {
        Some(language.to_owned())
    }
}

#[cfg(test)]
mod provider_validation_tests {
    use super::*;

    fn stream_guard() -> (StreamCancellation, tokio::sync::watch::Receiver<bool>) {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        (StreamCancellation(Some(sender)), receiver)
    }

    fn transcript() -> Transcription {
        Transcription {
            text: "Final text".into(),
            detected_language: None,
        }
    }

    #[test]
    fn abandoned_stream_signals_cancel_before_audio_closes() {
        struct CaptureDrop(tokio::sync::watch::Receiver<bool>);
        impl Drop for CaptureDrop {
            fn drop(&mut self) {
                assert!(*self.0.borrow(), "audio must not close before cancellation");
            }
        }
        struct AbandonedRecording {
            _cancel: StreamCancellation,
            _capture: CaptureDrop,
        }
        let (guard, cancel) = stream_guard();
        drop(AbandonedRecording {
            _cancel: guard,
            _capture: CaptureDrop(cancel.clone()),
        });
        assert!(*cancel.borrow());
    }

    #[test]
    fn normal_stop_keeps_guard_alive_until_final_result_without_batch() {
        let (guard, cancel) = stream_guard();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            finish_streaming(receiver, Some(guard), Some(Duration::from_secs(1)), || {
                panic!("successful streaming must not invoke batch")
            })
        });
        assert!(!*cancel.borrow());
        sender.send(Ok(transcript())).unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), transcript());
        assert!(!*cancel.borrow(), "normal completion must disarm the guard");
        assert!(
            cancel.has_changed().is_err(),
            "finalizer must release cancellation ownership"
        );
    }

    #[test]
    fn stream_error_and_disconnect_each_fall_back_once_after_cancelling() {
        for disconnect in [false, true] {
            let (guard, cancel) = stream_guard();
            let (sender, receiver) = mpsc::channel();
            if !disconnect {
                sender.send(Err("handshake failed".into())).unwrap();
            }
            drop(sender);
            let mut calls = 0;
            let result = finish_streaming(receiver, Some(guard), Some(Duration::ZERO), || {
                assert!(*cancel.borrow(), "abort upstream before batch");
                calls += 1;
                Err("batch failed".into())
            });
            assert_eq!(calls, 1);
            assert!(result.unwrap_err().contains("batch failed"));
        }
    }

    #[test]
    fn stop_during_connection_times_out_and_rejects_late_result() {
        let (guard, cancel) = stream_guard();
        let (sender, receiver) = mpsc::channel();
        let mut calls = 0;
        let result = finish_streaming(receiver, Some(guard), Some(Duration::ZERO), || {
            assert!(*cancel.borrow());
            calls += 1;
            Ok(transcript())
        });
        assert_eq!(calls, 1);
        assert_eq!(result.unwrap(), transcript());
        assert!(
            sender.send(Ok(transcript())).is_err(),
            "late results cannot reach insertion"
        );
    }
}
