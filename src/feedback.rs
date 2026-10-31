use crate::config::Sounds;
use crate::windows_ui::{CaretRect as UiCaretRect, UiCommand, UiRuntime};
use crate::wininput;
use std::{mem::size_of, sync::OnceLock};
use tracing::warn;
use windows::{
    Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT},
    core::PCWSTR,
};

const CUE_SAMPLE_RATE: u32 = 22_050;

static SOUND_WAVES: OnceLock<[Vec<u8>; 3]> = OnceLock::new();

pub(crate) fn ui_caret(rect: wininput::CaretRect) -> UiCaretRect {
    UiCaretRect {
        left: rect.left.round() as i32,
        top: rect.top.round() as i32,
        width: rect.width.round().max(1.0) as i32,
        height: rect.height.round().max(1.0) as i32,
    }
}

pub(crate) fn show_notice(ui: &UiRuntime, title: &str, message: &str) {
    warn!("{title}: {message}");
    let _ = ui.send(UiCommand::Notice {
        title: title.to_owned(),
        message: message.to_owned(),
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cue {
    Normal,
    Cancel,
    Error,
    Pending,
}

fn cue_enabled(sounds: Sounds, cue: Cue) -> bool {
    match sounds {
        Sounds::All => true,
        Sounds::Important => matches!(cue, Cue::Error | Cue::Pending),
        Sounds::Off => false,
    }
}

pub(crate) fn play_cue(sounds: Sounds, cue: Cue) {
    if !cue_enabled(sounds, cue) {
        return;
    }

    let waves = SOUND_WAVES.get_or_init(|| {
        [
            make_cue_wave(&[660, 880], 65, 18),
            make_cue_wave(&[330], 150, 0),
            make_cue_wave(&[523, 523], 90, 70),
        ]
    });
    let (wave, fallback) = match cue {
        Cue::Normal => (&waves[0], 0x40),
        Cue::Cancel | Cue::Error => (&waves[1], 0x10),
        Cue::Pending => (&waves[2], 0x20),
    };
    let played = unsafe {
        PlaySoundW(
            PCWSTR::from_raw(wave.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        )
        .as_bool()
    };
    if !played {
        let fallback_played = unsafe { MessageBeep(fallback) != 0 };
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

pub(crate) fn show_error_dialog(message: &str) {
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

#[cfg(test)]
mod cue_policy_tests {
    use super::*;

    #[test]
    fn sound_settings_filter_cue_kinds() {
        for cue in [Cue::Normal, Cue::Cancel, Cue::Error, Cue::Pending] {
            assert!(cue_enabled(Sounds::All, cue));
            assert!(!cue_enabled(Sounds::Off, cue));
            assert_eq!(
                cue_enabled(Sounds::Important, cue),
                matches!(cue, Cue::Error | Cue::Pending)
            );
        }
    }
}
