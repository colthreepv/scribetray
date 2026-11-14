use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::Duration,
};

use cpal::{
    FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const OUTPUT_RATE: u32 = 16_000;
const FILTER_LEFT: i64 = 15;
const FILTER_RIGHT: i64 = 16;
const FILTER_RADIUS: f64 = 16.0;
const INITIAL_RESERVE_SAMPLES: usize = OUTPUT_RATE as usize;
const REALTIME_CHUNK_BYTES: usize = 3_200;
const PROBE_MS: u64 = 150;
const PROBE_ALIVE_DB: f32 = -80.0;
const PROBE_FLOOR_DB: f32 = -120.0;

/// How often the input callback folds accumulated energy into the envelope.
const LEVEL_WINDOW_MS: u32 = 20;
/// Level below which the meter reports silence, in dBFS.
const LEVEL_FLOOR_DB: f32 = -52.0;
/// dBFS span mapped onto the 0..=1 level range above the floor.
const LEVEL_RANGE_DB: f32 = 40.0;
/// Per-window envelope attack/release smoothing factors.
const LEVEL_ATTACK: f32 = 0.6;
const LEVEL_RELEASE: f32 = 0.15;

/// A completed mono recording encoded as signed 16-bit little-endian PCM.
pub struct Recording {
    pub pcm_16k_mono: Vec<u8>,
    pub duration_seconds: u32,
}

/// The selected input, its probe results, and any extra debug recording.
pub struct PrioritizedCapture {
    pub recorder: AudioRecorder,
    pub receiver: Option<UnboundedReceiver<Vec<u8>>>,
    pub probes: Vec<(String, f32)>,
    pub used_default: bool,
    pub raw: Option<(String, AudioRecorder)>,
}

/// A cheap, cloneable handle to a recorder's live output level.
///
/// The value is the smoothed RMS envelope of the mono input, already mapped
/// onto the `0.0..=1.0` range. It is updated inside the CPAL callback through
/// a single atomic store, so reading it never blocks audio capture.
#[derive(Clone, Debug)]
pub struct LevelMeter {
    bits: Arc<AtomicU32>,
}

impl LevelMeter {
    /// Returns the current envelope level clamped to `0.0..=1.0`.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Relaxed)).clamp(0.0, 1.0)
    }
}

/// Errors that can occur while selecting an input device or recording audio.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no input device is available")]
    NoInputDevice,
    #[error("input device not found: {0}")]
    DeviceNotFound(String),
    #[error("maximum recording duration must be greater than zero")]
    InvalidMaxSeconds,
    #[error("input device has no supported PCM input configuration")]
    NoSupportedInputConfig,
    #[error("unsupported input sample format: {0}")]
    UnsupportedSampleFormat(SampleFormat),
    #[error("audio capture state was poisoned")]
    CaptureStatePoisoned,
    #[error("audio stream error: {0}")]
    Stream(String),
    #[error("could not stop the audio stream: {0}")]
    StreamControl(String),
    #[error("recording contains no audio samples")]
    EmptyRecording,
    #[error(transparent)]
    Cpal(#[from] cpal::Error),
    #[error("recording duration exceeds this platform's addressable memory")]
    CapacityOverflow,
}

/// A live input stream. Dropping it cancels capture and releases the device.
pub struct AudioRecorder {
    stream: Option<cpal::Stream>,
    device_name: String,
    capture: Arc<Mutex<CaptureState>>,
    stream_error: Arc<StreamErrors>,
    limit_reached: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    max_seconds: u32,
}

/// Errors reported by the CPAL error callback during capture.
///
/// WASAPI reports a data discontinuity as `ErrorKind::Xrun` while the stream
/// keeps delivering audio. This happens routinely when another app, such as
/// Microsoft Teams, shares the same microphone. Those events are counted
/// instead of discarding the whole recording.
#[derive(Default)]
struct StreamErrors {
    fatal: Mutex<Option<String>>,
    glitches: AtomicU32,
}

impl StreamErrors {
    fn report(&self, error: &cpal::Error) {
        if matches!(
            error.kind(),
            cpal::ErrorKind::Xrun
                | cpal::ErrorKind::DeviceChanged
                | cpal::ErrorKind::RealtimeDenied
        ) {
            self.glitches.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut fatal = lock_recover(&self.fatal);
        if fatal.is_none() {
            *fatal = Some(error.to_string());
        }
    }
}

impl AudioRecorder {
    /// Starts shared-mode capture from the named device, or the system default.
    pub fn start(device_name: Option<&str>, max_seconds: u32) -> Result<Self, AudioError> {
        Self::start_capture(device_name, max_seconds, false)
    }

    /// Starts capture and forwards resampled PCM in roughly 100 ms chunks.
    pub fn start_realtime(
        device_name: Option<&str>,
        max_seconds: u32,
    ) -> Result<(Self, UnboundedReceiver<Vec<u8>>), AudioError> {
        let recorder = Self::start(device_name, max_seconds)?;
        let receiver = recorder.attach_realtime()?;
        Ok((recorder, receiver))
    }

    /// Probes inputs in priority order, with the last input as an unconditional fallback.
    pub fn start_prioritized(
        candidates: &[String],
        raw_device: Option<&str>,
        max_seconds: u32,
        realtime: bool,
    ) -> Result<PrioritizedCapture, AudioError> {
        if max_seconds == 0 {
            return Err(AudioError::InvalidMaxSeconds);
        }

        let raw_device = raw_device.map(str::trim).filter(|name| !name.is_empty());
        let (mut opened, mut raw) = thread::scope(|scope| {
            let raw_worker = raw_device.map(|name| {
                scope.spawn(move || {
                    Self::start(Some(name), max_seconds)
                        .inspect_err(|error| tracing::warn!(device = name, %error, "could not open debug microphone"))
                        .ok()
                })
            });
            let workers = candidates
                .iter()
                .map(|name| {
                    scope.spawn(move || {
                        Self::start_capture(Some(name), max_seconds, true)
                            .inspect_err(|error| tracing::warn!(device = %name, %error, "could not open microphone candidate"))
                            .ok()
                    })
                })
                .collect::<Vec<_>>();
            let opened = workers
                .into_iter()
                .map(|worker| worker.join().expect("microphone startup panicked"))
                .collect::<Vec<_>>();
            let raw = raw_worker
                .and_then(|worker| worker.join().expect("debug microphone startup panicked"));
            (opened, raw)
        });

        let named_fallback = opened.pop().flatten();
        let using_default = named_fallback.is_none()
            || candidates.last().is_some_and(|name| name.trim().is_empty());
        let fallback = named_fallback.map_or_else(|| Self::start_probe_default(max_seconds), Ok);
        let mut opened = opened.into_iter().flatten().collect::<Vec<_>>();

        thread::sleep(Duration::from_millis(PROBE_MS));

        let mut probes = Vec::new();
        opened.retain(|recorder| match recorder.probe_db() {
            Ok(db) => {
                probes.push((recorder.device_name().to_owned(), db));
                true
            }
            Err(error) => {
                tracing::warn!(device = recorder.device_name(), %error, "microphone candidate failed during probe");
                false
            }
        });
        let fallback = fallback.and_then(|recorder| match recorder.probe_db() {
            Ok(db) => Ok((recorder, db)),
            Err(error) => {
                tracing::warn!(device = recorder.device_name(), %error, "fallback microphone failed during probe");
                Err(error)
            }
        });
        let fallback = match fallback {
            Ok((recorder, db)) => Some((recorder, db, using_default)),
            Err(_error) if !using_default && !probes.iter().any(|(_, db)| *db > PROBE_ALIVE_DB) => {
                match Self::start_probe_default(max_seconds) {
                    Ok(recorder) => {
                        thread::sleep(Duration::from_millis(PROBE_MS));
                        match recorder.probe_db() {
                            Ok(db) => Some((recorder, db, true)),
                            Err(error) => {
                                tracing::warn!(%error, "system default microphone failed during probe");
                                None
                            }
                        }
                    }
                    Err(_) => None,
                }
            }
            Err(error) if opened.is_empty() => return Err(error),
            Err(_) => None,
        };
        let mut rms_db: Vec<_> = probes.iter().map(|(_, db)| *db).collect();
        let fallback_index = fallback.as_ref().map(|_| opened.len());
        if let Some((recorder, db, _)) = fallback.as_ref() {
            probes.push((recorder.device_name().to_owned(), *db));
            rms_db.push(*db);
        }
        let selected =
            select_working_probe(&rms_db, fallback_index).ok_or(AudioError::NoInputDevice)?;
        let used_default = fallback
            .as_ref()
            .is_some_and(|(_, _, is_default)| fallback_index == Some(selected) && *is_default);
        if let Some((recorder, _, _)) = fallback {
            opened.push(recorder);
        }
        let recorder = opened.swap_remove(selected);
        let device = recorder.device_name().to_owned();

        if let Some(name) = raw_device {
            if device.eq_ignore_ascii_case(name) {
                raw = None;
            } else if raw.is_none() {
                raw = opened
                    .iter()
                    .position(|recorder| recorder.device_name().eq_ignore_ascii_case(name))
                    .map(|index| opened.swap_remove(index));
            }
        }
        drop(opened);
        let receiver = realtime.then(|| recorder.attach_realtime()).transpose()?;
        let raw = raw.and_then(|recorder| {
            let name = recorder.device_name().to_owned();
            if let Some(error) = lock_recover(&recorder.stream_error.fatal).as_ref() {
                tracing::warn!(device = %name, %error, "debug microphone failed during probe");
                return None;
            }
            Some((name, recorder))
        });
        Ok(PrioritizedCapture {
            recorder,
            receiver,
            probes,
            used_default,
            raw,
        })
    }

    fn start_probe_default(max_seconds: u32) -> Result<Self, AudioError> {
        Self::start_capture(None, max_seconds, true).inspect_err(|error| {
            tracing::warn!(%error, "could not open system default microphone for probe");
        })
    }

    fn probe_db(&self) -> Result<f32, AudioError> {
        if let Some(error) = lock_recover(&self.stream_error.fatal).as_ref() {
            return Err(AudioError::Stream(error.clone()));
        }
        let capture = self
            .capture
            .lock()
            .map_err(|_| AudioError::CaptureStatePoisoned)?;
        Ok(capture
            .probe
            .as_ref()
            .map_or(PROBE_FLOOR_DB, ProbeEnergy::db))
    }

    fn attach_realtime(&self) -> Result<UnboundedReceiver<Vec<u8>>, AudioError> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| AudioError::CaptureStatePoisoned)?;
        capture.attach_realtime(sender);
        Ok(receiver)
    }

    fn start_capture(
        device_name: Option<&str>,
        max_seconds: u32,
        probe: bool,
    ) -> Result<Self, AudioError> {
        if max_seconds == 0 {
            return Err(AudioError::InvalidMaxSeconds);
        }

        let host = cpal::default_host();
        let device = match device_name.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => find_input_device(&host, name)?,
            None => host
                .default_input_device()
                .ok_or(AudioError::NoInputDevice)?,
        };
        let device_name = input_device_name(&device);

        let (config, unsupported_format) = select_input_config(&device)?;
        let sample_rate = config.sample_rate();
        let channels = config.channels();
        if sample_rate == 0 || channels == 0 {
            return Err(AudioError::NoSupportedInputConfig);
        }

        let max_output_samples = u64::from(max_seconds)
            .checked_mul(u64::from(OUTPUT_RATE))
            .ok_or(AudioError::CapacityOverflow)?;
        let max_output_bytes = usize::try_from(max_output_samples)
            .ok()
            .and_then(|samples| samples.checked_mul(2))
            .ok_or(AudioError::CapacityOverflow)?;
        let max_input_frames = u64::from(max_seconds) * u64::from(sample_rate);

        let level = Arc::new(AtomicU32::new(0));
        let capture = Arc::new(Mutex::new(CaptureState::new(
            sample_rate,
            channels,
            max_input_frames,
            max_output_samples,
            max_output_bytes,
            probe,
            level.clone(),
        )));
        let stream_error = Arc::new(StreamErrors::default());
        let limit_reached = Arc::new(AtomicBool::new(false));

        let stream = match config.sample_format() {
            SampleFormat::F32 => build_stream::<f32>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            SampleFormat::F64 => build_stream::<f64>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            SampleFormat::I8 => build_stream::<i8>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            SampleFormat::I16 => build_stream::<i16>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            SampleFormat::I24 => build_stream::<cpal::I24>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            SampleFormat::I32 => build_stream::<i32>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            SampleFormat::I64 => build_stream::<i64>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            SampleFormat::U8 => build_stream::<u8>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            SampleFormat::U16 => build_stream::<u16>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            SampleFormat::U24 => build_stream::<cpal::U24>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            SampleFormat::U32 => build_stream::<u32>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            SampleFormat::U64 => build_stream::<u64>(
                &device,
                config.config(),
                capture.clone(),
                stream_error.clone(),
                limit_reached.clone(),
            )?,
            format => {
                return Err(AudioError::UnsupportedSampleFormat(
                    unsupported_format.unwrap_or(format),
                ));
            }
        };

        stream.play()?;

        Ok(Self {
            stream: Some(stream),
            device_name,
            capture,
            stream_error,
            limit_reached,
            level,
            max_seconds,
        })
    }

    /// Stops capture and returns the collected PCM data.
    pub fn stop(mut self) -> Result<Recording, AudioError> {
        self.finish_recording()
    }

    /// Signals a failed stop before dropping capture and closing its audio channel.
    pub fn stop_with_error_handler(
        mut self,
        on_error: impl FnOnce(),
    ) -> Result<Recording, AudioError> {
        let result = self.finish_recording();
        if result.is_err() {
            on_error();
        }
        result
    }

    pub fn has_failed(&self) -> bool {
        lock_recover(&self.stream_error.fatal).is_some()
    }

    fn finish_recording(&mut self) -> Result<Recording, AudioError> {
        if let Some(stream) = self.stream.take() {
            let pause_result = stream.pause();
            drop(stream);
            if let Err(error) = pause_result {
                return Err(AudioError::StreamControl(error.to_string()));
            }
        }

        let glitches = self.stream_error.glitches.load(Ordering::Relaxed);
        if glitches > 0 {
            tracing::warn!("audio capture reported {glitches} non-fatal stream glitch(es)");
        }
        if let Some(error) = lock_recover(&self.stream_error.fatal).take() {
            return Err(AudioError::Stream(error));
        }

        let mut capture = self
            .capture
            .lock()
            .map_err(|_| AudioError::CaptureStatePoisoned)?;
        capture.finish();
        if capture.pcm.is_empty() {
            return Err(AudioError::EmptyRecording);
        }

        let sample_count = capture.output_samples as u64;
        let duration_seconds = sample_count
            .div_ceil(u64::from(OUTPUT_RATE))
            .min(u64::from(self.max_seconds)) as u32;

        Ok(Recording {
            pcm_16k_mono: std::mem::take(&mut capture.pcm),
            duration_seconds,
        })
    }

    /// Returns true once capture has reached its configured duration limit.
    pub fn limit_reached(&self) -> bool {
        self.limit_reached.load(Ordering::Acquire)
    }

    /// Returns a cheap, cloneable handle to the live input level meter.
    pub fn level_meter(&self) -> LevelMeter {
        LevelMeter {
            bits: self.level.clone(),
        }
    }

    /// Returns the resolved name of the opened input device.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Returns the available input device names for the tray microphone menu.
    pub fn input_device_names() -> Result<Vec<String>, AudioError> {
        let host = cpal::default_host();
        let mut names = host
            .input_devices()?
            .map(|device| input_device_name(&device))
            .filter(|name| !name.trim().is_empty())
            .collect::<Vec<_>>();
        names.sort_by_key(|name| name.to_lowercase());
        names.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
        Ok(names)
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
            drop(stream);
        }
    }
}

fn select_probe(rms_db: &[f32]) -> Option<usize> {
    let (_, higher_priority) = rms_db.split_last()?;
    Some(
        higher_priority
            .iter()
            .position(|&db| db > PROBE_ALIVE_DB)
            .unwrap_or(higher_priority.len()),
    )
}

fn select_working_probe(rms_db: &[f32], fallback_index: Option<usize>) -> Option<usize> {
    match fallback_index {
        Some(_) => select_probe(rms_db),
        None => rms_db
            .iter()
            .position(|&db| db > PROBE_ALIVE_DB)
            .or_else(|| (!rms_db.is_empty()).then_some(0)),
    }
}

fn input_device_name(device: &cpal::Device) -> String {
    device
        .description()
        .ok()
        .map(|description| description.name().to_owned())
        .unwrap_or_else(|| device.to_string())
}

fn find_input_device(host: &cpal::Host, requested_name: &str) -> Result<cpal::Device, AudioError> {
    let mut case_insensitive_match = None;
    for device in host.input_devices()? {
        let name = input_device_name(&device);
        if name == requested_name {
            return Ok(device);
        }
        if case_insensitive_match.is_none() && name.eq_ignore_ascii_case(requested_name) {
            case_insensitive_match = Some(device);
        }
    }

    case_insensitive_match.ok_or_else(|| AudioError::DeviceNotFound(requested_name.to_owned()))
}

fn select_input_config(
    device: &cpal::Device,
) -> Result<(cpal::SupportedStreamConfig, Option<SampleFormat>), AudioError> {
    let mut selected = None;
    let mut unsupported_format = None;

    for range in device.supported_input_configs()? {
        let config = range
            .try_with_standard_sample_rate()
            .unwrap_or_else(|| range.with_max_sample_rate());
        if is_pcm_format(config.sample_format()) {
            if selected.as_ref().is_none_or(
                |(current_range, _): &(
                    cpal::SupportedStreamConfigRange,
                    cpal::SupportedStreamConfig,
                )| { range.cmp_default_heuristics(current_range).is_gt() },
            ) {
                selected = Some((range, config));
            }
        } else {
            unsupported_format = Some(config.sample_format());
        }
    }

    selected
        .map(|(_, config)| (config, unsupported_format))
        .ok_or_else(|| {
            unsupported_format.map_or(AudioError::NoSupportedInputConfig, |format| {
                AudioError::UnsupportedSampleFormat(format)
            })
        })
}

fn is_pcm_format(format: SampleFormat) -> bool {
    matches!(
        format,
        SampleFormat::F32
            | SampleFormat::F64
            | SampleFormat::I8
            | SampleFormat::I16
            | SampleFormat::I24
            | SampleFormat::I32
            | SampleFormat::I64
            | SampleFormat::U8
            | SampleFormat::U16
            | SampleFormat::U24
            | SampleFormat::U32
            | SampleFormat::U64
    )
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    capture: Arc<Mutex<CaptureState>>,
    stream_error: Arc<StreamErrors>,
    limit_reached: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device.build_input_stream(
        config,
        move |samples: &[T], _| {
            let mut capture = lock_recover(&capture);
            let reached_limit = capture.push_samples(samples);
            if reached_limit {
                limit_reached.store(true, Ordering::Release);
            }
        },
        move |error| stream_error.report(&error),
        None,
    )
}

struct CaptureState {
    sample_rate: u32,
    channels: usize,
    max_input_frames: u64,
    max_output_samples: usize,
    max_output_bytes: usize,
    realtime_sender: Option<UnboundedSender<Vec<u8>>>,
    realtime_chunk: Vec<u8>,
    input: VecDeque<f32>,
    input_base: u64,
    input_count: u64,
    output_samples: usize,
    pcm: Vec<u8>,
    probe: Option<ProbeEnergy>,
    level: Arc<AtomicU32>,
    level_window_frames: u64,
    level_square_sum: f64,
    level_frames: u64,
    level_envelope: f32,
}

struct ProbeEnergy {
    max_frames: u64,
    frames: u64,
    square_sum: f64,
}

impl ProbeEnergy {
    fn db(&self) -> f32 {
        if self.frames == 0 {
            return PROBE_FLOOR_DB;
        }
        let db = (10.0 * (self.square_sum / self.frames as f64).log10()) as f32;
        if db.is_finite() {
            db.max(PROBE_FLOOR_DB)
        } else {
            PROBE_FLOOR_DB
        }
    }
}

impl CaptureState {
    fn new(
        sample_rate: u32,
        channels: u16,
        max_input_frames: u64,
        max_output_samples: u64,
        max_output_bytes: usize,
        probe: bool,
        level: Arc<AtomicU32>,
    ) -> Self {
        let max_output_samples = max_output_samples as usize;
        let level_window_frames =
            (u64::from(sample_rate) / (1_000 / u64::from(LEVEL_WINDOW_MS))).max(1);
        Self {
            sample_rate,
            channels: usize::from(channels),
            max_input_frames,
            max_output_samples,
            max_output_bytes,
            realtime_sender: None,
            realtime_chunk: Vec::new(),
            input: VecDeque::with_capacity(64),
            input_base: 0,
            input_count: 0,
            output_samples: 0,
            pcm: Vec::with_capacity(max_output_bytes.min(INITIAL_RESERVE_SAMPLES * 2)),
            probe: probe.then(|| ProbeEnergy {
                max_frames: (u64::from(sample_rate) * PROBE_MS).div_ceil(1_000),
                frames: 0,
                square_sum: 0.0,
            }),
            level,
            level_window_frames,
            level_square_sum: 0.0,
            level_frames: 0,
            level_envelope: 0.0,
        }
    }

    fn attach_realtime(&mut self, sender: UnboundedSender<Vec<u8>>) {
        debug_assert!(self.realtime_sender.is_none());
        for chunk in self.pcm.chunks(REALTIME_CHUNK_BYTES) {
            let _ = sender.send(chunk.to_vec());
        }
        self.realtime_chunk = Vec::with_capacity(REALTIME_CHUNK_BYTES);
        self.realtime_sender = Some(sender);
    }

    fn push_samples<T>(&mut self, samples: &[T]) -> bool
    where
        T: Sample,
        f32: FromSample<T>,
    {
        for frame in samples.chunks_exact(self.channels) {
            if self.input_count >= self.max_input_frames
                || self.output_samples >= self.max_output_samples
            {
                return true;
            }

            let probe = self
                .probe
                .as_mut()
                .filter(|probe| probe.frames < probe.max_frames);
            let mut sum = 0.0;
            let mut square_sum = 0.0;
            for sample in frame {
                let value: f32 = (*sample).to_sample();
                if value.is_finite() {
                    let value = f64::from(value);
                    sum += value;
                    if probe.is_some() {
                        square_sum += value * value;
                    }
                }
            }
            if let Some(probe) = probe {
                probe.square_sum += square_sum / self.channels as f64;
                probe.frames += 1;
            }
            let mono = (sum / self.channels as f64).clamp(-1.0, 1.0) as f32;
            self.input.push_back(mono);
            self.input_count += 1;
            self.accumulate_level(mono);
            self.produce_until(None);

            if self.input_count >= self.max_input_frames
                || self.output_samples >= self.max_output_samples
            {
                return true;
            }
        }

        false
    }

    /// Folds one mono sample into the RMS envelope, publishing roughly every
    /// `LEVEL_WINDOW_MS`. The callback only ever performs arithmetic and one
    /// relaxed atomic store; no allocation or cross-thread send happens here.
    fn accumulate_level(&mut self, mono: f32) {
        self.level_square_sum += f64::from(mono) * f64::from(mono);
        self.level_frames += 1;
        if self.level_frames < self.level_window_frames {
            return;
        }

        let rms = (self.level_square_sum / self.level_frames as f64).sqrt() as f32;
        let db = 20.0 * (rms + 1e-9).log10();
        let target = ((db - LEVEL_FLOOR_DB) / LEVEL_RANGE_DB).clamp(0.0, 1.0);
        let smoothing = if target > self.level_envelope {
            LEVEL_ATTACK
        } else {
            LEVEL_RELEASE
        };
        self.level_envelope =
            (self.level_envelope + (target - self.level_envelope) * smoothing).clamp(0.0, 1.0);
        self.level
            .store(self.level_envelope.to_bits(), Ordering::Relaxed);

        self.level_square_sum = 0.0;
        self.level_frames = 0;
    }

    fn finish(&mut self) {
        let rate = u64::from(self.sample_rate);
        let quotient = self.input_count / rate;
        let remainder = self.input_count % rate;
        let target_samples = quotient
            .saturating_mul(u64::from(OUTPUT_RATE))
            .saturating_add((remainder * u64::from(OUTPUT_RATE)).div_ceil(rate))
            .min(self.max_output_samples as u64) as usize;
        self.produce_until(Some(target_samples));
        if let Some(sender) = &self.realtime_sender
            && !self.realtime_chunk.is_empty()
        {
            let _ = sender.send(std::mem::take(&mut self.realtime_chunk));
        }
    }

    fn produce_until(&mut self, flush_target: Option<usize>) {
        loop {
            let target = flush_target.unwrap_or(self.max_output_samples);
            if self.output_samples >= target {
                break;
            }

            let position =
                self.output_samples as f64 * f64::from(self.sample_rate) / f64::from(OUTPUT_RATE);
            let center = position.floor() as i64;
            if flush_target.is_none() && center + FILTER_RIGHT >= self.input_count as i64 {
                break;
            }

            let sample = self.filtered_sample(position, center);
            let pcm_sample = float_to_i16(sample).to_le_bytes();
            if self.pcm.len() + pcm_sample.len() > self.max_output_bytes {
                break;
            }
            if self.pcm.len() + pcm_sample.len() > self.pcm.capacity() {
                let additional =
                    (self.max_output_bytes - self.pcm.len()).min(INITIAL_RESERVE_SAMPLES * 2);
                self.pcm.reserve_exact(additional);
            }
            self.pcm.extend_from_slice(&pcm_sample);
            if let Some(sender) = &self.realtime_sender {
                self.realtime_chunk.extend_from_slice(&pcm_sample);
                if self.realtime_chunk.len() >= REALTIME_CHUNK_BYTES {
                    let chunk = std::mem::replace(
                        &mut self.realtime_chunk,
                        Vec::with_capacity(REALTIME_CHUNK_BYTES),
                    );
                    let _ = sender.send(chunk);
                }
            }
            self.output_samples += 1;
        }

        let next_position =
            self.output_samples as f64 * f64::from(self.sample_rate) / f64::from(OUTPUT_RATE);
        let retain_from = ((next_position.floor() as i64 - FILTER_LEFT).max(0)) as u64;
        while self.input_base < retain_from {
            if self.input.pop_front().is_none() {
                break;
            }
            self.input_base += 1;
        }
    }

    fn filtered_sample(&self, position: f64, center: i64) -> f32 {
        let downsample_ratio = f64::from(OUTPUT_RATE) / f64::from(self.sample_rate);
        let cutoff = if downsample_ratio < 1.0 {
            downsample_ratio * 0.94
        } else {
            1.0
        };

        let mut weighted_sum = 0.0;
        let mut weight_sum = 0.0;
        for offset in -FILTER_LEFT..=FILTER_RIGHT {
            let source_index = center + offset;
            let delta = source_index as f64 - position;
            let normalized_delta = delta / FILTER_RADIUS;
            let window = if normalized_delta.abs() <= 1.0 {
                0.5 + 0.5 * (std::f64::consts::PI * normalized_delta).cos()
            } else {
                0.0
            };
            let scaled_delta = cutoff * delta;
            let sinc = if scaled_delta.abs() < f64::EPSILON {
                1.0
            } else {
                (std::f64::consts::PI * scaled_delta).sin() / (std::f64::consts::PI * scaled_delta)
            };
            let weight = cutoff * sinc * window;
            weighted_sum += self.source_sample(source_index) * weight;
            weight_sum += weight;
        }

        if weight_sum.abs() > f64::EPSILON {
            (weighted_sum / weight_sum).clamp(-1.0, 1.0) as f32
        } else {
            self.source_sample(center).clamp(-1.0, 1.0) as f32
        }
    }

    fn source_sample(&self, index: i64) -> f64 {
        if index < 0 || (index as u64) < self.input_base {
            return self.input.front().copied().unwrap_or(0.0) as f64;
        }
        if index as u64 >= self.input_count {
            return self.input.back().copied().unwrap_or(0.0) as f64;
        }

        let offset = (index as u64 - self.input_base) as usize;
        self.input.get(offset).copied().unwrap_or(0.0) as f64
    }
}

fn float_to_i16(sample: f32) -> i16 {
    let sample = sample.clamp(-1.0, 1.0);
    if sample <= -1.0 {
        i16::MIN
    } else {
        (sample * f32::from(i16::MAX)).round() as i16
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_picks_first_alive_input_or_unconditional_fallback() {
        assert_eq!(select_probe(&[-96.0, -58.0, -40.0, -100.0]), Some(1));
        assert_eq!(select_probe(&[-60.0, -50.0, -40.0]), Some(0));
        assert_eq!(select_probe(&[-96.0, PROBE_ALIVE_DB, -105.0]), Some(2));
        assert_eq!(select_probe(&[PROBE_FLOOR_DB]), Some(0));
        assert_eq!(select_working_probe(&[-100.0, -90.0], None), Some(0));
        assert_eq!(select_working_probe(&[-100.0, -60.0], None), Some(1));
        assert_eq!(select_working_probe(&[], None), None);
        assert_eq!(select_probe(&[]), None);
    }

    #[test]
    fn probe_silence_has_finite_floor() {
        let mut capture = capture_state(1, true);
        assert_eq!(capture.probe.as_ref().unwrap().db(), PROBE_FLOOR_DB);
        capture.push_samples(&[0.0_f32; 10]);
        assert_eq!(capture.probe.as_ref().unwrap().db(), PROBE_FLOOR_DB);
        capture.probe.as_mut().unwrap().square_sum = 1e-30;
        assert_eq!(capture.probe.as_ref().unwrap().db(), PROBE_FLOOR_DB);
    }

    #[test]
    fn probe_measures_raw_samples_before_downmix_and_caps_window() {
        for (amplitude, expected_db) in [(0.000_015_85_f32, -96.0), (0.001, -60.0)] {
            let mut capture = capture_state(2, true);
            let window_frames = (u64::from(OUTPUT_RATE) * PROBE_MS / 1_000) as usize;
            let mut samples = [amplitude, -amplitude].repeat(window_frames);
            samples.extend_from_slice(&[1.0, 1.0]);
            assert!(!capture.push_samples(&samples));
            let db = capture.probe.as_ref().unwrap().db();
            assert!((db - expected_db).abs() < 0.01, "got {db} dBFS");
        }
    }

    #[test]
    fn realtime_handover_sends_buffer_then_live_without_gaps_or_duplicates() {
        let mut capture = capture_state(1, true);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let samples: Vec<i16> = (0..6_000).map(|i| (i * 13 % 20_000) as i16).collect();
        let split = 2_400;
        assert!(!capture.push_samples(&samples[..split]));
        let prefix = capture.pcm.clone();
        assert!(prefix.len() > REALTIME_CHUNK_BYTES);
        capture.attach_realtime(sender);

        let mut received = Vec::new();
        while let Ok(chunk) = receiver.try_recv() {
            received.extend(chunk);
        }
        assert_eq!(received, prefix);

        assert!(!capture.push_samples(&samples[split..]));
        let live = receiver.try_recv().unwrap();
        assert_eq!(live.len(), REALTIME_CHUNK_BYTES);
        received.extend(live);
        capture.finish();
        drop(capture.realtime_sender.take());
        while let Ok(chunk) = receiver.try_recv() {
            received.extend(chunk);
        }
        assert_eq!(received, capture.pcm);
        assert_eq!(received.len(), samples.len() * 2);
    }

    fn capture_state(channels: u16, probe: bool) -> CaptureState {
        let frames = u64::from(OUTPUT_RATE);
        CaptureState::new(
            OUTPUT_RATE,
            channels,
            frames,
            frames,
            frames as usize * 2,
            probe,
            Arc::new(AtomicU32::new(0)),
        )
    }
}
