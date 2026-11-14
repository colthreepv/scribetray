//! Statistics for 16 kHz mono signed 16-bit PCM.

use serde::{Deserialize, Serialize};

/// Signal levels and silence measurements for a capture.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct SignalStats {
    pub peak_db: f32,
    pub clipped_samples: usize,
    pub floor_db: f32,
    pub median_db: f32,
    pub speech_db: f32,
    pub leading_zero_ms: u64,
    pub zero_windows: usize,
    pub windows: usize,
}

/// Computes levels over 100 ms windows; a final partial window is included.
pub fn stats(pcm_16k_mono_le: &[u8]) -> SignalStats {
    let samples: Vec<i16> = pcm_16k_mono_le
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
        .collect();
    let peak = samples
        .iter()
        .map(|&sample| (sample as i32).abs())
        .max()
        .unwrap_or(0);
    let clipped_samples = samples
        .iter()
        .filter(|&&sample| (sample as i32).abs() >= 32_000)
        .count();
    let leading_zero_ms =
        (samples.iter().take_while(|&&sample| sample == 0).count() as u64) * 1_000 / 16_000;
    let mut levels = Vec::new();
    let mut zero_windows = 0;
    for window in samples.chunks(1_600) {
        if window.iter().all(|&sample| sample == 0) {
            zero_windows += 1;
        }
        let mean_square = window
            .iter()
            .map(|&sample| (sample as f64).powi(2))
            .sum::<f64>()
            / window.len() as f64;
        levels.push(db(mean_square.sqrt()));
    }
    levels.sort_by(|a, b| a.total_cmp(b));
    let percentile = |p: f64| -> f32 {
        if levels.is_empty() {
            -120.0
        } else {
            levels[((levels.len() - 1) as f64 * p).round() as usize]
        }
    };
    SignalStats {
        peak_db: db(peak as f64),
        clipped_samples,
        floor_db: percentile(0.1),
        median_db: percentile(0.5),
        speech_db: percentile(0.9),
        leading_zero_ms,
        zero_windows,
        windows: levels.len(),
    }
}

fn db(amplitude: f64) -> f32 {
    if amplitude == 0.0 {
        -120.0
    } else {
        ((20.0 * (amplitude / 32_768.0).log10() * 10.0).round() / 10.0).max(-120.0) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    #[test]
    fn silence_and_empty_input() {
        let result = stats(&pcm(&vec![0; 1_700]));
        assert_eq!(
            (
                result.peak_db,
                result.floor_db,
                result.median_db,
                result.speech_db
            ),
            (-120.0, -120.0, -120.0, -120.0)
        );
        assert_eq!(
            (result.leading_zero_ms, result.zero_windows, result.windows),
            (106, 2, 2)
        );
        assert_eq!(stats(&[]).windows, 0);
    }

    #[test]
    fn levels_and_clipping() {
        let mut samples = vec![0; 1_600];
        samples.extend(vec![16_384; 1_600]);
        samples.extend(vec![i16::MIN; 1_600]);
        let result = stats(&pcm(&samples));
        assert_eq!(
            (
                result.peak_db,
                result.floor_db,
                result.median_db,
                result.speech_db
            ),
            (0.0, -120.0, -6.0, 0.0)
        );
        assert_eq!(
            (
                result.clipped_samples,
                result.leading_zero_ms,
                result.zero_windows,
                result.windows
            ),
            (1_600, 100, 1, 3)
        );
    }
}
