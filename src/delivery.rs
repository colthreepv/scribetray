use crate::config::Config;
use crate::feedback::{Cue, play_cue, show_notice, ui_caret};
use crate::history::History;
use crate::scribe::Transcription;
use crate::windows_ui::{AnchorStatus, UiCommand, UiRuntime};
use crate::wininput::{self, InsertMethod, TargetSnapshot};
use std::{
    thread,
    time::{Duration, Instant},
};
use tracing::{error, info, warn};

pub(crate) const DONE_DISPLAY: Duration = Duration::from_millis(2_300);
/// How long a transcript waits for the user to return to the original field.
const PENDING_TIMEOUT: Duration = Duration::from_secs(20);
/// Time the original field must keep focus before a pending transcript is inserted.
const PENDING_SETTLE: Duration = Duration::from_millis(50);
/// Main loop wake-up interval while a transcript is pending.
pub(crate) const PENDING_TICK: Duration = Duration::from_millis(25);

/// A transcript whose target lost focus before it was ready. It is already on
/// the clipboard and is inserted when the original field is focused again.
struct PendingDelivery {
    target: TargetSnapshot,
    text: String,
    enter_after: bool,
    history_id: String,
    deadline: Instant,
    settle_since: Option<Instant>,
}

thread_local! {
    static PENDING: std::cell::RefCell<Option<PendingDelivery>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) enum Delivery {
    Paste(TargetSnapshot),
    Clipboard,
}

pub(crate) fn complete_transcription(
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
            "Transcription returned no text for this recording.",
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
                    play_cue(config.sounds, Cue::Normal);
                }
                Err(error) => {
                    if matches!(error, wininput::WinInputError::TargetChanged) {
                        mark_history_delivery(history, history_id, false);
                        if let Err(copy_error) = wininput::copy_text(&text) {
                            warn!("could not copy pending transcript: {copy_error}");
                        }
                        info!("focus moved; transcript copied and waiting for the original field");
                        PENDING.set(Some(PendingDelivery {
                            target,
                            text,
                            enter_after: enter_after || config.auto_enter,
                            history_id: history_id.to_owned(),
                            deadline: Instant::now() + PENDING_TIMEOUT,
                            settle_since: None,
                        }));
                        let _ = ui.send(UiCommand::SetTrayError(false));
                        if !recording_active {
                            let _ = ui.hide_caret_anchor();
                            *done_until = None;
                        }
                        play_cue(config.sounds, Cue::Pending);
                        return;
                    }
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
                    play_cue(config.sounds, Cue::Error);
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
            play_cue(config.sounds, Cue::Normal);
        }
    }
}

/// Inserts a pending transcript once its original field has been focused
/// again for `PENDING_SETTLE`.
pub(crate) fn poll_pending_delivery(config: &Config, ui: &UiRuntime, history: &History) {
    let ready = PENDING.with_borrow_mut(|slot| {
        let pending = slot.as_mut()?;
        let now = Instant::now();
        if now >= pending.deadline {
            info!("pending transcript expired; it stays on the clipboard");
            *slot = None;
            return None;
        }
        if !wininput::target_is_current(&pending.target) {
            pending.settle_since = None;
            return None;
        }
        let since = *pending.settle_since.get_or_insert(now);
        if since.elapsed() < PENDING_SETTLE {
            return None;
        }
        slot.take()
    });
    let Some(pending) = ready else {
        return;
    };

    let method = if config.insert_method.eq_ignore_ascii_case("type") {
        InsertMethod::UnicodeTyping
    } else {
        InsertMethod::Paste
    };
    match wininput::insert_text(
        &pending.target,
        &pending.text,
        method,
        config.restore_clipboard,
    ) {
        Ok(()) => {
            mark_history_delivery(history, &pending.history_id, true);
            if pending.enter_after {
                thread::sleep(Duration::from_millis(80));
                if let Err(error) = wininput::send_enter(&pending.target) {
                    warn!("auto-enter failed after pending insertion: {error}");
                }
            }
            info!("pending transcript inserted after focus returned; method={method:?}");
            play_cue(config.sounds, Cue::Normal);
        }
        Err(wininput::WinInputError::TargetChanged) => {
            PENDING.set(Some(PendingDelivery {
                settle_since: None,
                ..pending
            }));
        }
        Err(error) => {
            warn!("pending transcript was not inserted: {error}");
            show_notice(
                ui,
                "Scribetray",
                &format!("Text was not inserted ({error}); it is still on the clipboard."),
            );
            play_cue(config.sounds, Cue::Error);
        }
    }
}

pub(crate) fn set_anchor_error(ui: &UiRuntime, delivery: Option<&Delivery>) {
    if let Some(Delivery::Paste(target)) = delivery {
        if let Some(caret) = target.caret {
            let _ = ui.update_caret_anchor(ui_caret(caret.rect), AnchorStatus::Error);
        }
    }
}

fn mark_history_delivery(history: &History, history_id: &str, delivered: bool) {
    if let Err(error) = history.mark_delivered(history_id, delivered) {
        error!("could not update dictation delivery state: {error}");
    }
}

pub(crate) fn has_pending_delivery() -> bool {
    PENDING.with_borrow(Option::is_some)
}

pub(crate) fn discard_pending_delivery() -> bool {
    PENDING.take().is_some()
}
