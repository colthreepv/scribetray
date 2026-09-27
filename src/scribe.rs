//! Synchronous client for ElevenLabs Scribe speech-to-text.

use std::time::Duration;

use reqwest::{
    blocking::{Client, multipart::Form, multipart::Part},
    header::HeaderValue,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const ENDPOINT: &str = "https://api.elevenlabs.io/v1/speech-to-text";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const FILE_FORMAT: &str = "pcm_s16le_16";

/// Parent-app result contract, including the detected language when returned.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Transcription {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_language: Option<String>,
}

#[derive(Debug)]
pub struct ScribeClient {
    client: Client,
}

impl ScribeClient {
    /// Builds a client with the default 90-second request timeout.
    pub fn new() -> Result<Self, ScribeError> {
        Self::with_timeout(REQUEST_TIMEOUT)
    }

    /// Builds a client with an explicit request timeout.
    pub fn with_timeout(timeout: Duration) -> Result<Self, ScribeError> {
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|_| ScribeError::ClientInitialization)?;
        Ok(Self { client })
    }

    /// Submits 16 kHz mono signed-16 little-endian PCM. A language code of
    /// `None` or `auto` leaves detection to Scribe. Network failures and 5xx
    /// responses are retried once.
    pub fn transcribe(
        &self,
        api_key: &str,
        pcm_s16le: &[u8],
        model_id: &str,
        language_code: Option<&str>,
    ) -> Result<Transcription, ScribeError> {
        if api_key.trim().is_empty() {
            return Err(ScribeError::MissingApiKey);
        }
        if pcm_s16le.len() % 2 != 0 {
            return Err(ScribeError::InvalidAudio);
        }
        let api_key = HeaderValue::from_str(api_key).map_err(|_| ScribeError::InvalidApiKey)?;

        for attempt in 0..=1 {
            let form = request_form(pcm_s16le, model_id, language_code)?;
            let response = self
                .client
                .post(ENDPOINT)
                .header("xi-api-key", api_key.clone())
                .multipart(form)
                .send();

            let response = match response {
                Ok(response) => response,
                Err(error) if attempt == 0 && is_transient_network_error(&error) => continue,
                Err(_) => return Err(ScribeError::NetworkFailure),
            };

            let status = response.status();
            if status.is_server_error() && attempt == 0 {
                continue;
            }
            if !status.is_success() {
                return Err(ScribeError::HttpStatus(status.as_u16()));
            }

            let body = response
                .json::<ScribeResponse>()
                .map_err(|_| ScribeError::InvalidResponse)?;
            return Ok(Transcription {
                text: body.text,
                detected_language: body.language_code,
            });
        }

        // The loop either returns a result or continues exactly once.
        Err(ScribeError::NetworkFailure)
    }
}

fn request_form(
    pcm_s16le: &[u8],
    model_id: &str,
    language_code: Option<&str>,
) -> Result<Form, ScribeError> {
    let audio_part = Part::bytes(pcm_s16le.to_vec())
        .file_name("recording.pcm")
        .mime_str("application/octet-stream")
        .map_err(|_| ScribeError::RequestSetup)?;
    let mut form = Form::new()
        .text("model_id", model_id.to_owned())
        .text("file_format", FILE_FORMAT)
        .text("tag_audio_events", "false")
        .part("file", audio_part);

    if let Some(language_code) = language_code
        .map(str::trim)
        .filter(|code| !code.is_empty() && !code.eq_ignore_ascii_case("auto"))
    {
        form = form.text("language_code", language_code.to_owned());
    }
    Ok(form)
}

fn is_transient_network_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

#[derive(Deserialize)]
struct ScribeResponse {
    text: String,
    #[serde(default)]
    language_code: Option<String>,
}

#[derive(Debug, Error)]
pub enum ScribeError {
    #[error("ElevenLabs API key is missing")]
    MissingApiKey,
    #[error("ElevenLabs API key is not a valid HTTP header value")]
    InvalidApiKey,
    #[error("recording audio must contain complete signed-16 PCM samples")]
    InvalidAudio,
    #[error("could not initialize the HTTP client")]
    ClientInitialization,
    #[error("could not prepare the transcription request")]
    RequestSetup,
    #[error("transcription request failed due to a network error")]
    NetworkFailure,
    #[error("ElevenLabs returned HTTP {0}")]
    HttpStatus(u16),
    #[error("ElevenLabs returned an invalid transcription response")]
    InvalidResponse,
}
