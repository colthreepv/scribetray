//! User configuration stored in `%APPDATA%/Scribetray/config.toml`.

use std::{
    env, fmt, fs,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const APP_DIRECTORY: &str = "Scribetray";
const CONFIG_FILE: &str = "config.toml";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The default language setting requests automatic language detection.
pub const DEFAULT_LANGUAGE_CODE: &str = "auto";
pub const DEFAULT_MODEL: &str = "scribe_v2";

/// Settings persisted for the Scribetray desktop application.
///
/// `api_key` is deliberately redacted from `Debug` output.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    /// Optional fallback when `ELEVENLABS_API_KEY` is not set.
    pub api_key: Option<String>,
    /// Main global recording hotkey.
    pub hotkey: String,
    /// Optional hotkey for submitting the current recording.
    pub hotkey_submit: String,
    /// Recording interaction mode (for example, `toggle` or `push_to_talk`).
    pub mode: String,
    /// ElevenLabs speech-to-text model identifier.
    pub model: String,
    /// An ElevenLabs language code, or `auto` to detect it from the audio.
    #[serde(alias = "language_code")]
    pub language: String,
    /// Text prepended to inserted transcripts when `prefix_enabled` is true.
    pub prefix: String,
    pub prefix_enabled: bool,
    pub auto_enter: bool,
    /// Transcript insertion strategy (for example, `paste` or `type`).
    pub insert_method: String,
    pub restore_clipboard: bool,
    pub max_seconds: u32,
    /// Selected input device name, or `None` for the system default.
    pub microphone: Option<String>,
    pub sound_cues: bool,
    pub start_with_windows: bool,
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("hotkey", &self.hotkey)
            .field("hotkey_submit", &self.hotkey_submit)
            .field("mode", &self.mode)
            .field("model", &self.model)
            .field("language", &self.language)
            .field("prefix", &self.prefix)
            .field("prefix_enabled", &self.prefix_enabled)
            .field("auto_enter", &self.auto_enter)
            .field("insert_method", &self.insert_method)
            .field("restore_clipboard", &self.restore_clipboard)
            .field("max_seconds", &self.max_seconds)
            .field("microphone", &self.microphone)
            .field("sound_cues", &self.sound_cues)
            .field("start_with_windows", &self.start_with_windows)
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: None,
            hotkey: "Win+Alt+V".to_owned(),
            hotkey_submit: "Win+Alt+Shift+V".to_owned(),
            mode: "toggle".to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            language: DEFAULT_LANGUAGE_CODE.to_owned(),
            prefix: "🎙️ ".to_owned(),
            prefix_enabled: true,
            auto_enter: false,
            insert_method: "paste".to_owned(),
            restore_clipboard: true,
            max_seconds: 600,
            microphone: None,
            sound_cues: true,
            start_with_windows: false,
        }
    }
}

impl Config {
    /// Loads the saved settings, creating the config directory and default file
    /// on the first run.
    pub fn load_or_create() -> Result<Self, ConfigError> {
        let path = config_path()?;
        match fs::read_to_string(&path) {
            Ok(contents) => Ok(toml::from_str(&contents)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let config = Self::default();
                let contents = toml::to_string_pretty(&config)?;
                let parent = path.parent().ok_or(ConfigError::UnavailableDirectory)?;
                fs::create_dir_all(parent)?;

                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(mut file) => {
                        file.write_all(contents.as_bytes())?;
                        file.sync_all()?;
                        Ok(config)
                    }
                    // Another process may have created the file after our read.
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let contents = fs::read_to_string(&path)?;
                        Ok(toml::from_str(&contents)?)
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Saves settings with a same-directory temporary file and rename.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = config_path()?;
        let parent = path.parent().ok_or(ConfigError::UnavailableDirectory)?;
        fs::create_dir_all(parent)?;
        let contents = toml::to_string_pretty(self)?;
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(
            ".{CONFIG_FILE}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));

        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary_path)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary_path, &path)?;
            Ok::<_, std::io::Error>(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result.map_err(ConfigError::Io)
    }

    /// Returns the environment key when non-empty, falling back to the saved
    /// key. The key is returned only to the caller and is never logged here.
    pub fn resolved_api_key(&self) -> Option<String> {
        env::var_os("ELEVENLABS_API_KEY")
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                self.api_key
                    .as_ref()
                    .filter(|value| !value.trim().is_empty())
                    .cloned()
            })
    }
}

/// Returns the absolute config path for the current user.
pub fn config_path() -> Result<PathBuf, ConfigError> {
    let base_dirs = BaseDirs::new().ok_or(ConfigError::UnavailableDirectory)?;
    Ok(base_dirs.config_dir().join(APP_DIRECTORY).join(CONFIG_FILE))
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine the user config directory")]
    UnavailableDirectory,
    #[error("could not read or write application config: {0}")]
    Io(#[from] std::io::Error),
    #[error("config TOML is invalid")]
    Toml(#[from] toml::de::Error),
    #[error("could not serialize config TOML")]
    Serialize(#[from] toml::ser::Error),
}
