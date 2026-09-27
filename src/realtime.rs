//! Realtime ElevenLabs Scribe WebSocket client.

use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

use crate::scribe::Transcription;

const ENDPOINT: &str = "wss://api.elevenlabs.io/v1/speech-to-text/realtime";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_KEYTERMS: usize = 50;
const MAX_KEYTERM_CHARACTERS: usize = 20;

/// Validates and normalizes optional Scribe vocabulary hints.
pub fn validate_keyterms(keyterms: &[String]) -> Result<Vec<String>, RealtimeError> {
    if keyterms.len() > MAX_KEYTERMS {
        return Err(RealtimeError::InvalidKeyterms(format!(
            "Scribe accepts at most {MAX_KEYTERMS} vocabulary terms."
        )));
    }

    keyterms
        .iter()
        .map(|term| {
            let term = term.trim();
            if term.is_empty() {
                return Err(RealtimeError::InvalidKeyterms(
                    "Vocabulary terms cannot be empty.".to_owned(),
                ));
            }
            if term.chars().count() > MAX_KEYTERM_CHARACTERS {
                return Err(RealtimeError::InvalidKeyterms(format!(
                    "Vocabulary terms must be at most {MAX_KEYTERM_CHARACTERS} characters: {term:?}."
                )));
            }
            Ok(term.to_owned())
        })
        .collect()
}

/// Streams 16 kHz mono signed-16 little-endian PCM to Scribe and returns the
/// final committed transcript. The last audio chunk is committed explicitly.
pub async fn transcribe_stream(
    api_key: &str,
    language_code: Option<&str>,
    keyterms: &[String],
    mut audio: UnboundedReceiver<Vec<u8>>,
) -> Result<Transcription, RealtimeError> {
    if api_key.trim().is_empty() {
        return Err(RealtimeError::MissingApiKey);
    }
    let keyterms = validate_keyterms(keyterms)?;
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query
        .append_pair("model_id", "scribe_v2_realtime")
        .append_pair("audio_format", "pcm_16000")
        .append_pair("commit_strategy", "manual");
    if let Some(language) = language_code
        .map(str::trim)
        .filter(|language| !language.is_empty() && !language.eq_ignore_ascii_case("auto"))
    {
        query.append_pair("language_code", language);
    }
    for term in &keyterms {
        query.append_pair("keyterms", term);
    }

    let endpoint = format!("{ENDPOINT}?{}", query.finish());
    let mut request = endpoint
        .into_client_request()
        .map_err(|_| RealtimeError::RequestSetup)?;
    let api_key = HeaderValue::from_str(api_key).map_err(|_| RealtimeError::InvalidApiKey)?;
    request.headers_mut().insert("xi-api-key", api_key);

    let (socket, _) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .map_err(|_| RealtimeError::ConnectTimeout)?
            .map_err(|_| RealtimeError::Connection)?;
    let (mut sink, mut incoming) = socket.split();

    wait_for_session(&mut incoming).await?;

    let mut pending_chunk: Option<Vec<u8>> = None;
    let mut commit_sent = false;
    let mut commit_timeout = Box::pin(tokio::time::sleep(Duration::from_secs(86_400)));
    loop {
        tokio::select! {
            chunk = audio.recv(), if !commit_sent => {
                match chunk {
                    Some(chunk) if !chunk.is_empty() => {
                        if chunk.len() % 2 != 0 {
                            return Err(RealtimeError::InvalidAudio);
                        }
                        if let Some(previous) = pending_chunk.replace(chunk) {
                            send_audio_chunk(&mut sink, &previous, false).await?;
                        }
                    }
                    Some(_) => {}
                    None => {
                        let final_chunk = pending_chunk.take().ok_or(RealtimeError::EmptyAudio)?;
                        send_audio_chunk(&mut sink, &final_chunk, true).await?;
                        commit_sent = true;
                        commit_timeout
                            .as_mut()
                            .reset(tokio::time::Instant::now() + COMMIT_TIMEOUT);
                    }
                }
            }
            message = incoming.next() => {
                let message = message.ok_or(RealtimeError::ConnectionClosed)?
                    .map_err(|_| RealtimeError::Connection)?;
                if let Some(transcription) = read_transcript_message(message)?
                    && commit_sent
                {
                    return Ok(transcription);
                }
            }
            _ = &mut commit_timeout, if commit_sent => {
                return Err(RealtimeError::CommitTimeout);
            }
        }
    }
}

async fn wait_for_session(
    incoming: &mut (
             impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin
         ),
) -> Result<(), RealtimeError> {
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        loop {
            let message = incoming
                .next()
                .await
                .ok_or(RealtimeError::ConnectionClosed)?
                .map_err(|_| RealtimeError::Connection)?;
            if let Some(session_ready) = read_session_message(message)?
                && session_ready
            {
                return Ok(());
            }
        }
    })
    .await
    .map_err(|_| RealtimeError::SessionTimeout)?
}

fn read_session_message(message: Message) -> Result<Option<bool>, RealtimeError> {
    let Message::Text(text) = message else {
        if message.is_close() {
            return Err(RealtimeError::ConnectionClosed);
        }
        return Ok(None);
    };
    let event: ServerEvent =
        serde_json::from_str(text.as_str()).map_err(|_| RealtimeError::InvalidResponse)?;
    match event.message_type.as_str() {
        "session_started" => Ok(Some(true)),
        "warning" | "partial_transcript" | "committed_transcript" => Ok(None),
        "rate_limited" | "error" => Err(RealtimeError::Rejected),
        _ => Ok(None),
    }
}

fn read_transcript_message(message: Message) -> Result<Option<Transcription>, RealtimeError> {
    let Message::Text(text) = message else {
        if message.is_close() {
            return Err(RealtimeError::ConnectionClosed);
        }
        return Ok(None);
    };
    let event: ServerEvent =
        serde_json::from_str(text.as_str()).map_err(|_| RealtimeError::InvalidResponse)?;
    match event.message_type.as_str() {
        "committed_transcript" => Ok(Some(Transcription {
            text: event.text.unwrap_or_default(),
            detected_language: None,
        })),
        "rate_limited" | "error" => Err(RealtimeError::Rejected),
        _ => Ok(None),
    }
}

async fn send_audio_chunk(
    sink: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    pcm: &[u8],
    commit: bool,
) -> Result<(), RealtimeError> {
    let message = serde_json::json!({
        "message_type": "input_audio_chunk",
        "audio_base_64": STANDARD.encode(pcm),
        "commit": commit,
    });
    sink.send(Message::Text(message.to_string().into()))
        .await
        .map_err(|_| RealtimeError::Connection)
}

#[derive(Deserialize)]
struct ServerEvent {
    message_type: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Error)]
pub enum RealtimeError {
    #[error("ElevenLabs API key is missing")]
    MissingApiKey,
    #[error("ElevenLabs API key is not a valid HTTP header value")]
    InvalidApiKey,
    #[error("could not prepare the realtime transcription request")]
    RequestSetup,
    #[error("could not connect to ElevenLabs realtime transcription")]
    Connection,
    #[error("ElevenLabs realtime connection timed out")]
    ConnectTimeout,
    #[error("ElevenLabs realtime session did not start")]
    SessionTimeout,
    #[error("ElevenLabs realtime connection closed unexpectedly")]
    ConnectionClosed,
    #[error("ElevenLabs realtime transcription was rate-limited or rejected")]
    Rejected,
    #[error("realtime transcription returned an invalid response")]
    InvalidResponse,
    #[error("realtime transcription did not finish in time")]
    CommitTimeout,
    #[error("recording contains no audio samples")]
    EmptyAudio,
    #[error("recording audio must contain complete signed-16 PCM samples")]
    InvalidAudio,
    #[error("invalid keyterms: {0}")]
    InvalidKeyterms(String),
}
