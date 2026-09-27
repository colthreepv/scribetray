//! Realtime ElevenLabs Scribe WebSocket client.

use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use tracing::info;

use crate::scribe::Transcription;

const ENDPOINT: &str = "wss://api.elevenlabs.io/v1/speech-to-text/realtime";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(15);
const MANUAL_COMMIT_INTERVAL_BYTES: usize = 16_000 * 2 * 25;
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

/// Streams 16 kHz mono signed-16 little-endian PCM to Scribe, committing
/// segments before the service's automatic ~36-second boundary and joining
/// every committed segment when recording stops.
pub async fn transcribe_stream(
    api_key: &str,
    language_code: Option<&str>,
    keyterms: &[String],
    audio: UnboundedReceiver<Vec<u8>>,
) -> Result<Transcription, RealtimeError> {
    transcribe_stream_at(ENDPOINT, api_key, language_code, keyterms, audio).await
}

async fn transcribe_stream_at(
    endpoint_base: &str,
    api_key: &str,
    language_code: Option<&str>,
    keyterms: &[String],
    mut audio: UnboundedReceiver<Vec<u8>>,
) -> Result<Transcription, RealtimeError> {
    if api_key.trim().is_empty() {
        return Err(RealtimeError::MissingApiKey);
    }
    let keyterms = validate_keyterms(keyterms)?;
    let query = {
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
        query.finish()
    };

    let endpoint = format!("{endpoint_base}?{query}");
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
    let mut final_commit_sent = false;
    let mut commits_sent = 0_u32;
    let mut commits_received = 0_u32;
    let mut audio_since_commit = 0_usize;
    let mut committed_text = String::new();
    let mut commit_timeout = Box::pin(tokio::time::sleep(Duration::from_secs(86_400)));
    loop {
        tokio::select! {
            chunk = audio.recv(), if !final_commit_sent => {
                match chunk {
                    Some(chunk) if !chunk.is_empty() => {
                        if chunk.len() % 2 != 0 {
                            return Err(RealtimeError::InvalidAudio);
                        }
                        if let Some(previous) = pending_chunk.replace(chunk) {
                            audio_since_commit = audio_since_commit.saturating_add(previous.len());
                            let commit = audio_since_commit >= MANUAL_COMMIT_INTERVAL_BYTES;
                            send_audio_chunk(&mut sink, &previous, commit).await?;
                            if commit {
                                commits_sent += 1;
                                audio_since_commit = 0;
                                info!("sent realtime transcript segment commit {commits_sent}");
                            }
                        }
                    }
                    Some(_) => {}
                    None => {
                        let final_chunk = pending_chunk.take().ok_or(RealtimeError::EmptyAudio)?;
                        send_audio_chunk(&mut sink, &final_chunk, true).await?;
                        commits_sent += 1;
                        final_commit_sent = true;
                        commit_timeout
                            .as_mut()
                            .reset(tokio::time::Instant::now() + COMMIT_TIMEOUT);
                    }
                }
            }
            message = incoming.next() => {
                let message = message.ok_or(RealtimeError::ConnectionClosed)?
                    .map_err(|_| RealtimeError::Connection)?;
                if let Some(transcription) = read_transcript_message(message)? {
                    commits_received += 1;
                    append_committed_segment(&mut committed_text, &transcription.text);
                    info!("received realtime transcript segment {commits_received}");
                    if final_commit_sent && commits_received >= commits_sent {
                        info!(
                            "realtime transcription complete; segments={commits_received}, characters={}",
                            committed_text.chars().count()
                        );
                        return Ok(Transcription {
                            text: committed_text,
                            detected_language: transcription.detected_language,
                        });
                    }
                }
            }
            _ = &mut commit_timeout, if final_commit_sent => {
                return Err(RealtimeError::CommitTimeout);
            }
        }
    }
}

fn append_committed_segment(transcript: &mut String, segment: &str) {
    let segment = segment.trim();
    if segment.is_empty() {
        return;
    }
    if !transcript.is_empty() && !transcript.chars().last().is_some_and(char::is_whitespace) {
        transcript.push(' ');
    }
    transcript.push_str(segment);
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
    if let Some(error) = server_error(&event) {
        return Err(error);
    }
    match event.message_type.as_str() {
        "session_started" => Ok(Some(true)),
        "warning" | "partial_transcript" | "committed_transcript" => Ok(None),
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
    if let Some(error) = server_error(&event) {
        return Err(error);
    }
    match event.message_type.as_str() {
        "committed_transcript" => Ok(Some(Transcription {
            text: event.text.unwrap_or_default(),
            detected_language: None,
        })),
        _ => Ok(None),
    }
}

fn server_error(event: &ServerEvent) -> Option<RealtimeError> {
    if event.message_type == "session_time_limit_exceeded" {
        return Some(RealtimeError::SessionTimeLimitExceeded);
    }
    matches!(
        event.message_type.as_str(),
        "auth_error"
            | "quota_exceeded"
            | "transcriber_error"
            | "input_error"
            | "invalid_request"
            | "rate_limited"
            | "commit_throttled"
            | "unaccepted_terms"
            | "queue_overflow"
            | "resource_exhausted"
            | "chunk_size_exceeded"
            | "insufficient_audio_activity"
            | "error"
    )
    .then(|| RealtimeError::Server {
        kind: event.message_type.clone(),
        details: event.error.clone().unwrap_or_default(),
    })
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
    #[serde(default)]
    error: Option<String>,
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
    #[error("ElevenLabs realtime session reached its maximum duration")]
    SessionTimeLimitExceeded,
    #[error("ElevenLabs realtime server returned {kind}: {details}")]
    Server { kind: String, details: String },
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    #[tokio::test]
    async fn joins_segments_from_a_long_realtime_recording() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(
                    serde_json::json!({"message_type": "session_started"})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();

            let mut commits = 0;
            while let Some(message) = socket.next().await {
                let Message::Text(message) = message.unwrap() else {
                    continue;
                };
                let chunk: Value = serde_json::from_str(message.as_str()).unwrap();
                if chunk["commit"] == true {
                    commits += 1;
                    let text = if commits == 1 {
                        "first segment"
                    } else {
                        "second segment"
                    };
                    socket
                        .send(Message::Text(
                            serde_json::json!({
                                "message_type": "committed_transcript",
                                "text": text,
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .unwrap();
                    if commits == 2 {
                        break;
                    }
                }
            }
            commits
        });

        let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel();
        let endpoint = format!("ws://{address}/v1/speech-to-text/realtime");
        let transcription = tokio::spawn(async move {
            transcribe_stream_at(&endpoint, "test-key", None, &[], audio_rx).await
        });
        for _ in 0..251 {
            audio_tx.send(vec![0; 3_200]).unwrap();
        }
        drop(audio_tx);

        let result = tokio::time::timeout(Duration::from_secs(10), transcription)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result.text, "first segment second segment");
        assert_eq!(server.await.unwrap(), 2);
    }
}
