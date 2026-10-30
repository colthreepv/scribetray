const SAMPLE_RATE_HZ: usize = 16_000;
const FRAME_MS: usize = 20;
const FRAME_SAMPLES: usize = SAMPLE_RATE_HZ * FRAME_MS / 1_000;
const MIN_RECORDING_MS: usize = 3_000;
const NOISE_PERCENTILE: usize = 15;
const SPEECH_PERCENTILE: usize = 90;
const MIN_SPEECH_CONTRAST_DB: f64 = 10.0;
const MIN_ENTER_ABOVE_NOISE_DB: f64 = 6.0;
const ENTER_DYNAMIC_DIVISOR: f64 = 3.0;
const EXIT_MARGIN_DB: f64 = 4.0;
const HIGH_ZCR: f64 = 0.22;
const MIN_DB_FS: f64 = -96.0;
const PRE_PAD_MS: usize = 100;
const POST_PAD_MS: usize = 200;
const MIN_PAUSE_MS: usize = 700;
const SHORTENED_PAUSE_MS: usize = 250;
const FADE_MS: usize = 10;

pub struct TrimStats {
    pub original_ms: u32,
    pub trimmed_ms: u32,
    pub pauses_shortened: u32,
}

#[derive(Clone, Copy)]
struct Frame {
    start_sample: usize,
    end_sample: usize,
    dbfs: f64,
    zcr: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SampleRange {
    start: usize,
    end: usize,
}

pub fn trim_silence(pcm_s16le: &[u8]) -> Option<(Vec<u8>, TrimStats)> {
    let sample_count = pcm_s16le.len() / 2;
    if sample_count < ms_to_samples(MIN_RECORDING_MS) {
        return None;
    }

    let frames = analyze_frames(pcm_s16le, sample_count);
    let frames_db: Vec<f64> = frames.iter().map(|frame| frame.dbfs).collect();
    let noise_floor_db = percentile(&frames_db, NOISE_PERCENTILE);
    let speech_level_db = percentile(&frames_db, SPEECH_PERCENTILE);
    let contrast_db = speech_level_db - noise_floor_db;
    if contrast_db < MIN_SPEECH_CONTRAST_DB {
        return None;
    }

    let speech_runs = detect_speech_runs(&frames, noise_floor_db, speech_level_db, contrast_db);
    if speech_runs.is_empty() {
        return None;
    }

    let cuts = plan_cuts(sample_count, &speech_runs);
    if cuts.is_empty() {
        return None;
    }

    let output = render_with_fades(pcm_s16le, sample_count, &cuts);
    let trimmed_samples = output.len() / 2;
    let stats = TrimStats {
        original_ms: samples_to_ms(sample_count),
        trimmed_ms: samples_to_ms(trimmed_samples),
        pauses_shortened: cuts.len() as u32,
    };
    Some((output, stats))
}

fn analyze_frames(pcm_s16le: &[u8], sample_count: usize) -> Vec<Frame> {
    let mut frames = Vec::with_capacity(sample_count.div_ceil(FRAME_SAMPLES));
    let mut start_sample = 0;
    while start_sample < sample_count {
        let end_sample = (start_sample + FRAME_SAMPLES).min(sample_count);
        let mut sum_squares = 0_i64;
        let mut crossings = 0_usize;
        let mut last_sign = 0_i16;

        for sample_index in start_sample..end_sample {
            let sample = pcm_sample(pcm_s16le, sample_index);
            sum_squares += i64::from(sample) * i64::from(sample);
            let sign = sample.signum();
            if sign != 0 {
                if last_sign != 0 && sign != last_sign {
                    crossings += 1;
                }
                last_sign = sign;
            }
        }

        let frame_len = end_sample - start_sample;
        let rms = (sum_squares as f64 / frame_len as f64).sqrt();
        let dbfs = if rms > 0.0 {
            (20.0 * (rms / 32_768.0).log10()).max(MIN_DB_FS)
        } else {
            MIN_DB_FS
        };
        let zcr = if frame_len > 1 {
            crossings as f64 / (frame_len - 1) as f64
        } else {
            0.0
        };

        frames.push(Frame {
            start_sample,
            end_sample,
            dbfs,
            zcr,
        });
        start_sample = end_sample;
    }
    frames
}

fn percentile(values: &[f64], percentile: usize) -> f64 {
    debug_assert!(!values.is_empty());
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = ((sorted.len() - 1) * percentile + 50) / 100;
    sorted[index]
}

fn detect_speech_runs(
    frames: &[Frame],
    noise_floor_db: f64,
    speech_level_db: f64,
    contrast_db: f64,
) -> Vec<SampleRange> {
    let enter_threshold =
        noise_floor_db + (contrast_db / ENTER_DYNAMIC_DIVISOR).max(MIN_ENTER_ABOVE_NOISE_DB);
    let exit_threshold = enter_threshold - EXIT_MARGIN_DB;

    let mut in_speech = false;
    let mut run_start = 0;
    let mut runs = Vec::new();
    for frame in frames {
        let high_zcr_unvoiced = frame.zcr >= HIGH_ZCR && frame.dbfs >= exit_threshold;
        let keep_speaking = frame.dbfs >= exit_threshold || high_zcr_unvoiced;

        if in_speech && !keep_speaking {
            runs.push(SampleRange {
                start: run_start,
                end: frame.start_sample,
            });
            in_speech = false;
        } else if !in_speech && (frame.dbfs >= enter_threshold || high_zcr_unvoiced) {
            run_start = frame.start_sample;
            in_speech = true;
        }
    }
    if in_speech {
        runs.push(SampleRange {
            start: run_start,
            end: frames.last().map_or(run_start, |frame| frame.end_sample),
        });
    }

    debug_assert!(speech_level_db > noise_floor_db);
    runs
}

fn plan_cuts(sample_count: usize, speech_runs: &[SampleRange]) -> Vec<SampleRange> {
    let min_pause_samples = ms_to_samples(MIN_PAUSE_MS);
    let shortened_pause_samples = ms_to_samples(SHORTENED_PAUSE_MS);
    let pre_pad_samples = ms_to_samples(PRE_PAD_MS);
    let post_pad_samples = ms_to_samples(POST_PAD_MS);
    let mut cuts = Vec::new();

    let first = speech_runs[0];
    if first.start > min_pause_samples {
        cuts.push(SampleRange {
            start: 0,
            end: first.start - shortened_pause_samples,
        });
    }

    for pair in speech_runs.windows(2) {
        let previous = pair[0];
        let next = pair[1];
        let pause_samples = next.start - previous.end;
        if pause_samples > min_pause_samples {
            let after_previous = shortened_pause_samples.saturating_sub(pre_pad_samples);
            let cut_start = previous.end + after_previous.min(post_pad_samples);
            let cut_end = next.start - pre_pad_samples.min(shortened_pause_samples);
            if cut_end > cut_start {
                cuts.push(SampleRange {
                    start: cut_start,
                    end: cut_end,
                });
            }
        }
    }

    let last = *speech_runs.last().expect("speech_runs is not empty");
    let trailing_pause = sample_count - last.end;
    if trailing_pause > min_pause_samples {
        cuts.push(SampleRange {
            start: last.end + shortened_pause_samples,
            end: sample_count,
        });
    }

    cuts.sort_unstable_by_key(|cut| cut.start);
    cuts
}

fn render_with_fades(pcm_s16le: &[u8], sample_count: usize, cuts: &[SampleRange]) -> Vec<u8> {
    let mut segments = Vec::with_capacity(cuts.len() + 1);
    let mut cursor = 0;
    for cut in cuts {
        if cut.start > cursor {
            segments.push(SampleRange {
                start: cursor,
                end: cut.start,
            });
        }
        cursor = cut.end;
    }
    if cursor < sample_count {
        segments.push(SampleRange {
            start: cursor,
            end: sample_count,
        });
    }

    let fade_samples = ms_to_samples(FADE_MS);
    let output_sample_count: usize = segments
        .iter()
        .map(|segment| segment.end - segment.start)
        .sum();
    let mut output = Vec::with_capacity(output_sample_count * 2);

    for (segment_index, segment) in segments.iter().enumerate() {
        let segment_len = segment.end - segment.start;
        let fade_in = segment_index > 0;
        let fade_out = segment_index + 1 < segments.len();

        for offset in 0..segment_len {
            let mut gain = 1.0;
            if fade_in && offset < fade_samples {
                gain *= offset as f64 / fade_samples as f64;
            }
            if fade_out && segment_len - 1 - offset < fade_samples {
                gain *= (segment_len - 1 - offset) as f64 / fade_samples as f64;
            }
            let original = pcm_sample(pcm_s16le, segment.start + offset);
            let faded = (f64::from(original) * gain).round() as i16;
            output.extend_from_slice(&faded.to_le_bytes());
        }
    }

    output
}

fn pcm_sample(pcm_s16le: &[u8], sample_index: usize) -> i16 {
    let byte_index = sample_index * 2;
    i16::from_le_bytes([pcm_s16le[byte_index], pcm_s16le[byte_index + 1]])
}

fn ms_to_samples(milliseconds: usize) -> usize {
    SAMPLE_RATE_HZ * milliseconds / 1_000
}

fn samples_to_ms(samples: usize) -> u32 {
    u32::try_from(samples.saturating_mul(1_000) / SAMPLE_RATE_HZ).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: usize = SAMPLE_RATE_HZ / 1_000;

    struct SimpleRng(u32);

    impl SimpleRng {
        fn new(seed: u32) -> Self {
            Self(seed)
        }

        fn next_noise(&mut self, amplitude: i32) -> i16 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            let width = amplitude * 2 + 1;
            ((self.0 as i32 % width) - amplitude) as i16
        }
    }

    fn append_background(
        samples: &mut Vec<i16>,
        duration_ms: usize,
        amplitude: i32,
        rng: &mut SimpleRng,
    ) {
        for _ in 0..duration_ms * MS {
            samples.push(rng.next_noise(amplitude));
        }
    }

    fn append_tone(
        samples: &mut Vec<i16>,
        duration_ms: usize,
        frequency_hz: f64,
        amplitude: f64,
        background_amplitude: i32,
        rng: &mut SimpleRng,
    ) {
        for sample_index in 0..duration_ms * MS {
            let phase =
                std::f64::consts::TAU * frequency_hz * sample_index as f64 / SAMPLE_RATE_HZ as f64;
            let value = amplitude * phase.sin() + f64::from(rng.next_noise(background_amplitude));
            samples.push(value.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
        }
    }

    fn append_noise_burst(
        samples: &mut Vec<i16>,
        duration_ms: usize,
        amplitude: i32,
        rng: &mut SimpleRng,
    ) {
        for _ in 0..duration_ms * MS {
            samples.push(rng.next_noise(amplitude));
        }
    }

    fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn sample_from_output(output: &[u8], sample_index: usize) -> i16 {
        pcm_sample(output, sample_index)
    }

    fn mid_pause_fixture() -> Vec<u8> {
        let mut rng = SimpleRng::new(0x51C3_E001);
        let mut samples = Vec::new();
        append_background(&mut samples, 500, 160, &mut rng);
        append_tone(&mut samples, 500, 440.0, 6_000.0, 160, &mut rng);
        append_background(&mut samples, 1_200, 160, &mut rng);
        append_tone(&mut samples, 500, 520.0, 6_000.0, 160, &mut rng);
        append_background(&mut samples, 300, 160, &mut rng);
        pcm_bytes(&samples)
    }

    #[test]
    fn shortens_long_mid_pause_to_250_ms() {
        let pcm = mid_pause_fixture();
        let (output, stats) = trim_silence(&pcm).expect("long pause should be shortened");

        assert_eq!(stats.original_ms, 3_000);
        assert_eq!(stats.trimmed_ms, 2_050);
        assert_eq!(stats.pauses_shortened, 1);
        assert_eq!(output.len() / 2, 2_050 * MS);
    }

    #[test]
    fn short_pauses_remain_untouched() {
        let mut rng = SimpleRng::new(0xA11C_E002);
        let mut samples = Vec::new();
        append_background(&mut samples, 400, 160, &mut rng);
        append_tone(&mut samples, 500, 440.0, 6_000.0, 160, &mut rng);
        append_background(&mut samples, 600, 160, &mut rng);
        append_tone(&mut samples, 500, 520.0, 6_000.0, 160, &mut rng);
        append_background(&mut samples, 1_000, 160, &mut rng);
        let pcm = pcm_bytes(&samples);

        let (output, stats) = trim_silence(&pcm).expect("trailing pause should be shortened");

        assert_eq!(stats.pauses_shortened, 1);
        assert_eq!(stats.trimmed_ms, 2_250);
        assert_eq!(output.len() / 2, 2_250 * MS);
    }

    #[test]
    fn quiet_but_clear_speech_is_preserved() {
        let mut rng = SimpleRng::new(0xA11C_E003);
        let mut samples = Vec::new();
        append_background(&mut samples, 400, 100, &mut rng);
        append_tone(&mut samples, 700, 390.0, 900.0, 100, &mut rng);
        append_background(&mut samples, 1_000, 100, &mut rng);
        append_tone(&mut samples, 700, 490.0, 900.0, 100, &mut rng);
        append_background(&mut samples, 400, 100, &mut rng);
        let pcm = pcm_bytes(&samples);

        let (output, stats) = trim_silence(&pcm).expect("clear quiet speech should survive");

        assert_eq!(stats.original_ms, 3_200);
        assert_eq!(stats.trimmed_ms, 2_450);
        assert_eq!(stats.pauses_shortened, 1);
        assert_eq!(output.len() / 2, 2_450 * MS);
    }

    #[test]
    fn ambiguous_signal_returns_none() {
        let mut rng = SimpleRng::new(0xA11C_E004);
        let mut samples = Vec::new();
        append_background(&mut samples, 3_500, 500, &mut rng);

        assert!(trim_silence(&pcm_bytes(&samples)).is_none());
    }

    #[test]
    fn recording_shorter_than_three_seconds_returns_none() {
        let mut rng = SimpleRng::new(0xA11C_E005);
        let mut samples = Vec::new();
        append_background(&mut samples, 200, 100, &mut rng);
        append_tone(&mut samples, 800, 440.0, 6_000.0, 100, &mut rng);
        append_background(&mut samples, 1_999, 100, &mut rng);

        assert!(trim_silence(&pcm_bytes(&samples)).is_none());
    }

    #[test]
    fn odd_trailing_byte_is_ignored() {
        let mut pcm = mid_pause_fixture();
        let even_result = trim_silence(&pcm).expect("fixture should be trimmable");
        pcm.push(0x7f);
        let odd_result = trim_silence(&pcm).expect("odd byte should not break valid PCM");

        assert_eq!(odd_result.0, even_result.0);
        assert_eq!(odd_result.1.trimmed_ms, even_result.1.trimmed_ms);
        assert_eq!(odd_result.0.len() % 2, 0);
    }

    #[test]
    fn shortens_mid_and_trailing_pauses() {
        let mut rng = SimpleRng::new(0xA11C_E007);
        let mut samples = Vec::new();
        append_background(&mut samples, 400, 160, &mut rng);
        append_tone(&mut samples, 500, 440.0, 6_000.0, 160, &mut rng);
        append_background(&mut samples, 900, 160, &mut rng);
        append_tone(&mut samples, 500, 520.0, 6_000.0, 160, &mut rng);
        append_background(&mut samples, 900, 160, &mut rng);
        let pcm = pcm_bytes(&samples);

        let (output, stats) = trim_silence(&pcm).expect("both long pauses should be shortened");

        assert_eq!(stats.original_ms, 3_200);
        assert_eq!(stats.trimmed_ms, 1_900);
        assert_eq!(stats.pauses_shortened, 2);
        assert_eq!(output.len() / 2, 1_900 * MS);
    }

    #[test]
    fn cut_samples_have_bounded_delta() {
        let mut rng = SimpleRng::new(0xA11C_E008);
        let mut samples = Vec::new();
        append_background(&mut samples, 400, 1_000, &mut rng);
        append_tone(&mut samples, 500, 440.0, 8_000.0, 1_000, &mut rng);
        append_background(&mut samples, 1_200, 1_000, &mut rng);
        append_tone(&mut samples, 500, 520.0, 8_000.0, 1_000, &mut rng);
        append_background(&mut samples, 400, 1_000, &mut rng);
        let pcm = pcm_bytes(&samples);

        let frame_data = analyze_frames(&pcm, samples.len());
        let levels: Vec<f64> = frame_data.iter().map(|frame| frame.dbfs).collect();
        let noise = percentile(&levels, NOISE_PERCENTILE);
        let speech = percentile(&levels, SPEECH_PERCENTILE);
        let runs = detect_speech_runs(&frame_data, noise, speech, speech - noise);
        let cuts = plan_cuts(samples.len(), &runs);
        assert_eq!(cuts.len(), 1);
        let seam = cuts[0].start;

        let (output, _) = trim_silence(&pcm).expect("long low-level pause should be cut");
        let left = sample_from_output(&output, seam - 1);
        let right = sample_from_output(&output, seam);
        assert!(left.abs() <= 1, "left cut edge was {left}");
        assert!(right.abs() <= 1, "right cut edge was {right}");
        assert!((i32::from(right) - i32::from(left)).abs() <= 1);
    }

    #[test]
    fn high_zcr_noise_burst_counts_as_speech() {
        let mut rng = SimpleRng::new(0xA11C_E009);
        let mut samples = Vec::new();
        append_background(&mut samples, 500, 120, &mut rng);
        append_noise_burst(&mut samples, 600, 2_000, &mut rng);
        append_background(&mut samples, 900, 120, &mut rng);
        append_tone(&mut samples, 600, 440.0, 5_000.0, 120, &mut rng);
        append_background(&mut samples, 400, 120, &mut rng);
        let pcm = pcm_bytes(&samples);

        let (_, stats) = trim_silence(&pcm).expect("noise burst and tone are speech");

        assert_eq!(stats.pauses_shortened, 1);
        assert_eq!(stats.trimmed_ms, 2_350);
    }

    #[test]
    fn high_zcr_unvoiced_frame_can_extend_speech() {
        let frames = vec![
            Frame {
                start_sample: 0,
                end_sample: FRAME_SAMPLES,
                dbfs: -20.0,
                zcr: 0.05,
            },
            Frame {
                start_sample: FRAME_SAMPLES,
                end_sample: FRAME_SAMPLES * 2,
                dbfs: -42.0,
                zcr: 0.45,
            },
            Frame {
                start_sample: FRAME_SAMPLES * 2,
                end_sample: FRAME_SAMPLES * 3,
                dbfs: -20.0,
                zcr: 0.05,
            },
        ];

        let runs = detect_speech_runs(&frames, -60.0, -15.0, 45.0);

        assert_eq!(
            runs,
            vec![SampleRange {
                start: 0,
                end: FRAME_SAMPLES * 3,
            }]
        );
    }
}
