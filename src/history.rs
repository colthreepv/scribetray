//! Local recording and transcription history.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use directories::BaseDirs;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{broadcast::BroadcastSnapshot, signal_stats::SignalStats};

const APP_DIRECTORY: &str = "Scribetray";
const HISTORY_DIRECTORY: &str = "history";
const SAMPLE_RATE_HZ: u64 = 16_000;
const BYTES_PER_SAMPLE: usize = 2;
pub const MAX_RECORDINGS: usize = 20;

static ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Processing state persisted beside each recording's PCM data.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStatus {
    Pending,
    Succeeded,
    Failed,
}

/// Optional capture and transcription diagnostics.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct RecordingMetadata {
    pub app_version: String,
    pub device: Option<String>,
    pub device_is_default: bool,
    pub probes: Vec<(String, f32)>,
    pub signal: Option<SignalStats>,
    pub provider: Option<String>,
    pub transcription_mode: Option<String>,
    pub stop_to_text_ms: Option<u64>,
    pub broadcast: Option<BroadcastSnapshot>,
    pub raw_capture: Option<RawCaptureInfo>,
}

/// Diagnostics for the capture before processing.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct RawCaptureInfo {
    pub device: String,
    pub signal: SignalStats,
}

/// Public metadata contract for one 16 kHz mono recording.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Recording {
    /// Stable filename identifier, also accepted by the audio/read/update APIs.
    pub id: String,
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Duration computed from the number of 16 kHz samples.
    pub duration_ms: u64,
    /// Capture duration reported by the audio layer, rounded to whole seconds.
    #[serde(default)]
    pub duration_seconds: u32,
    pub status: RecordingStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_metadata",
        skip_serializing_if = "Option::is_none"
    )]
    pub metadata: Option<RecordingMetadata>,
}

fn deserialize_metadata<'de, D>(deserializer: D) -> Result<Option<RecordingMetadata>, D::Error>
where
    D: Deserializer<'de>,
{
    // Consume the entire optional block before checking its shape so bad
    // diagnostics do not prevent the rest of the recording from loading.
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// Filesystem-backed recording history.
#[derive(Clone, Debug)]
pub struct History {
    root: PathBuf,
}

impl History {
    /// Opens `%LOCALAPPDATA%/Scribetray/history`, creating it when needed.
    pub fn open() -> Result<Self, HistoryError> {
        let base_dirs = BaseDirs::new().ok_or(HistoryError::UnavailableDirectory)?;
        Self::with_root(
            base_dirs
                .data_local_dir()
                .join(APP_DIRECTORY)
                .join(HISTORY_DIRECTORY),
        )
    }

    /// Opens a caller-selected history directory. Useful for embedding the
    /// history store under an app-managed location.
    pub fn with_root(root: impl Into<PathBuf>) -> Result<Self, HistoryError> {
        let history = Self { root: root.into() };
        fs::create_dir_all(&history.root)?;
        history.prune()?;
        Ok(history)
    }

    /// Persists already-encoded 16 kHz mono signed-16 little-endian PCM and
    /// records a pending item. Duration milliseconds are calculated from the
    /// byte count; `duration_seconds` preserves the audio layer's rounded
    /// capture duration for display.
    pub fn create_pending_pcm(
        &self,
        pcm_s16le: &[u8],
        duration_seconds: u32,
    ) -> Result<Recording, HistoryError> {
        if pcm_s16le.len() % BYTES_PER_SAMPLE != 0 {
            return Err(HistoryError::InvalidAudio);
        }

        let timestamp_ms = now_millis();
        let sequence = ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let id = format!("{timestamp_ms}-{}-{sequence}", std::process::id());
        let sample_count = pcm_s16le.len() / BYTES_PER_SAMPLE;
        let recording = Recording {
            id: id.clone(),
            timestamp_ms,
            duration_ms: ((sample_count as u128 * 1_000) / SAMPLE_RATE_HZ as u128)
                .min(u64::MAX as u128) as u64,
            duration_seconds,
            status: RecordingStatus::Pending,
            delivered: None,
            transcript: None,
            detected_language: None,
            error: None,
            metadata: None,
        };

        let audio_path = self.audio_path(&id)?;
        let mut audio_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&audio_path)?;
        let audio_result = (|| {
            audio_file.write_all(pcm_s16le)?;
            audio_file.sync_all()
        })();
        if let Err(error) = audio_result {
            let _ = fs::remove_file(&audio_path);
            return Err(error.into());
        }

        if let Err(error) = self.write_recording(&recording) {
            let _ = fs::remove_file(&audio_path);
            return Err(error);
        }

        self.prune()?;
        Ok(recording)
    }

    /// Returns entries newest first.
    pub fn list(&self) -> Result<Vec<Recording>, HistoryError> {
        let mut recordings = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                recordings.push(read_recording(&entry.path())?);
            }
        }
        recordings.sort_by(|left, right| {
            right
                .timestamp_ms
                .cmp(&left.timestamp_ms)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(recordings)
    }

    pub fn get(&self, id: &str) -> Result<Recording, HistoryError> {
        let path = self.metadata_path(id)?;
        read_recording(&path)
    }

    /// Returns the exact persisted signed 16-bit little-endian PCM bytes.
    pub fn read_audio(&self, id: &str) -> Result<Vec<u8>, HistoryError> {
        let bytes = fs::read(self.audio_path(id)?)?;
        if bytes.len() % BYTES_PER_SAMPLE != 0 {
            return Err(HistoryError::InvalidAudio);
        }
        Ok(bytes)
    }

    pub fn mark_succeeded(
        &self,
        id: &str,
        transcript: impl Into<String>,
        detected_language: Option<String>,
    ) -> Result<Recording, HistoryError> {
        let mut recording = self.get(id)?;
        recording.status = RecordingStatus::Succeeded;
        recording.transcript = Some(transcript.into());
        recording.detected_language = detected_language;
        recording.error = None;
        self.write_recording(&recording)?;
        Ok(recording)
    }

    pub fn mark_delivered(&self, id: &str, delivered: bool) -> Result<Recording, HistoryError> {
        let mut recording = self.get(id)?;
        recording.delivered = Some(delivered);
        self.write_recording(&recording)?;
        Ok(recording)
    }

    pub fn mark_pending(&self, id: &str) -> Result<Recording, HistoryError> {
        let mut recording = self.get(id)?;
        recording.status = RecordingStatus::Pending;
        recording.transcript = None;
        recording.detected_language = None;
        recording.error = None;
        recording.delivered = None;
        self.write_recording(&recording)?;
        Ok(recording)
    }

    pub fn mark_failed(
        &self,
        id: &str,
        safe_error: impl Into<String>,
    ) -> Result<Recording, HistoryError> {
        let mut recording = self.get(id)?;
        recording.status = RecordingStatus::Failed;
        recording.transcript = None;
        recording.detected_language = None;
        recording.error = Some(safe_error.into());
        self.write_recording(&recording)?;
        Ok(recording)
    }

    /// Updates the optional diagnostics for a recording.
    pub fn set_metadata(&self, id: &str, metadata: RecordingMetadata) -> Result<(), HistoryError> {
        let mut recording = self.get(id)?;
        recording.metadata = Some(metadata);
        self.write_recording(&recording)
    }

    /// Saves the original PCM capture alongside the recording.
    pub fn save_raw_pcm(&self, id: &str, bytes: &[u8]) -> Result<(), HistoryError> {
        if !bytes.len().is_multiple_of(BYTES_PER_SAMPLE) {
            return Err(HistoryError::InvalidAudio);
        }
        self.get(id)?;
        atomic_write(&self.raw_audio_path(id)?, bytes)
    }

    /// Removes audio and metadata for all but the newest 20 recordings.
    pub fn prune(&self) -> Result<(), HistoryError> {
        let recordings = self.list()?;
        for recording in recordings.into_iter().skip(MAX_RECORDINGS) {
            remove_if_present(&self.audio_path(&recording.id)?)?;
            remove_if_present(&self.raw_audio_path(&recording.id)?)?;
            remove_if_present(&self.metadata_path(&recording.id)?)?;
        }
        Ok(())
    }

    fn metadata_path(&self, id: &str) -> Result<PathBuf, HistoryError> {
        validate_id(id)?;
        Ok(self.root.join(format!("{id}.json")))
    }

    fn audio_path(&self, id: &str) -> Result<PathBuf, HistoryError> {
        validate_id(id)?;
        Ok(self.root.join(format!("{id}.pcm")))
    }

    fn raw_audio_path(&self, id: &str) -> Result<PathBuf, HistoryError> {
        validate_id(id)?;
        Ok(self.root.join(format!("{id}.raw.pcm")))
    }

    fn write_recording(&self, recording: &Recording) -> Result<(), HistoryError> {
        let path = self.metadata_path(&recording.id)?;
        let contents = serde_json::to_vec_pretty(recording)?;
        atomic_write(&path, &contents)?;
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), HistoryError> {
    if !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-') {
        Ok(())
    } else {
        Err(HistoryError::InvalidId)
    }
}

fn read_recording(path: &Path) -> Result<Recording, HistoryError> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), HistoryError> {
    let parent = path.parent().ok_or(HistoryError::UnavailableDirectory)?;
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(HistoryError::InvalidId)?;
    let temporary_path = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary_path);
    }
    result.map_err(HistoryError::Io)
}

fn remove_if_present(path: &Path) -> Result<(), HistoryError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("could not determine the user data directory")]
    UnavailableDirectory,
    #[error("could not read or write recording history: {0}")]
    Io(#[from] std::io::Error),
    #[error("recording metadata is invalid")]
    Metadata(#[from] serde_json::Error),
    #[error("recording identifier is invalid")]
    InvalidId,
    #[error("recording audio is not signed 16-bit PCM")]
    InvalidAudio,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_recording_round_trips_without_metadata() {
        let old = r#"{"id":"1","timestamp_ms":1,"duration_ms":0,"status":"pending"}"#;
        let recording: Recording = serde_json::from_str(old).unwrap();
        assert_eq!(recording.metadata, None);
        let saved = serde_json::to_value(&recording).unwrap();
        assert!(saved.get("metadata").is_none());
        assert_eq!(
            serde_json::from_value::<Recording>(saved).unwrap(),
            recording
        );
    }

    #[test]
    fn malformed_metadata_does_not_hide_recording() {
        let json = r#"{"id":"1","timestamp_ms":1,"duration_ms":0,"status":"succeeded",
            "metadata":{"probes":[["x",null]]},"transcript":"Hello"}"#;
        let recording: Recording = serde_json::from_str(json).unwrap();
        assert_eq!(recording.metadata, None);
        assert_eq!(recording.status, RecordingStatus::Succeeded);
        assert_eq!(recording.transcript.as_deref(), Some("Hello"));
    }

    #[test]
    fn partial_metadata_uses_defaults() {
        let metadata: RecordingMetadata =
            serde_json::from_str(r#"{"raw_capture":{"device":"x"}}"#).unwrap();
        assert_eq!(
            metadata,
            RecordingMetadata {
                raw_capture: Some(RawCaptureInfo {
                    device: "x".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }
        );
    }

    #[test]
    fn metadata_round_trips_and_prune_removes_raw() {
        let root =
            std::env::temp_dir().join(format!("scribetray-history-{}", uuid::Uuid::new_v4()));
        let history = History::with_root(&root).unwrap();
        let first = history.create_pending_pcm(&[0, 0], 0).unwrap();
        let metadata = RecordingMetadata {
            app_version: "test".into(),
            ..Default::default()
        };
        history.set_metadata(&first.id, metadata.clone()).unwrap();
        assert_eq!(history.get(&first.id).unwrap().metadata, Some(metadata));
        history.save_raw_pcm(&first.id, &[1, 0]).unwrap();
        assert!(history.raw_audio_path(&first.id).unwrap().exists());
        // Ensure the first item is older even when all entries share a timestamp.
        let mut old = history.get(&first.id).unwrap();
        old.timestamp_ms = 0;
        history.write_recording(&old).unwrap();
        for _ in 0..MAX_RECORDINGS {
            history.create_pending_pcm(&[0, 0], 0).unwrap();
        }
        assert!(!history.raw_audio_path(&first.id).unwrap().exists());
        assert!(!history.audio_path(&first.id).unwrap().exists());
        assert!(!history.metadata_path(&first.id).unwrap().exists());
        fs::remove_dir_all(root).unwrap();
    }
}
