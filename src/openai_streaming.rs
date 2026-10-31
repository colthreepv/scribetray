//! Streaming dictation using the existing, read-only Codex session.
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, timeout, timeout_at},
};
use wreq::{
    Client, redirect, retry,
    ws::{WebSocket, message::Message},
};
use wreq_util::Emulation;

use crate::{openai, scribe::Transcription};

const ENDPOINT: &str = "wss://chatgpt.com/backend-api/dictation/stream";
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const FINISH_TIMEOUT: Duration = Duration::from_secs(60);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MESSAGE: usize = 1024 * 1024;
const MAX_TRANSCRIPT: usize = 1024 * 1024;
const MAX_UTTERANCES: usize = 4096;
const MAX_CHUNK: usize = 256 * 1024;
const MAX_QUEUED_CHUNKS: usize = 256;

/// Streams signed 16-bit little-endian mono PCM at 16 kHz. Closing the audio
/// channel finishes normally; a true cancellation value aborts the connection.
pub async fn transcribe_stream(
    language: Option<&str>,
    audio: mpsc::UnboundedReceiver<Vec<u8>>,
    mut cancel: watch::Receiver<bool>,
) -> Result<Transcription, String> {
    tokio::select! {
        biased;
        _ = cancelled(&mut cancel) => Err("OpenAI streaming cancelled".into()),
        result = connect_and_run(language, audio) => result,
    }
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            // A dropped sender is not a cancellation and cannot change again.
            std::future::pending::<()>().await;
        }
    }
}

async fn connect_and_run(
    language: Option<&str>,
    audio: mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<Transcription, String> {
    let started_at = Instant::now();
    let deadline = Instant::now() + READY_TIMEOUT;
    let session = openai::credentials().map_err(|e| e.to_string())?;
    let client = Client::builder()
        .emulation(Emulation::Chrome147)
        .redirect(redirect::Policy::none())
        .retry(retry::Policy::never())
        .build()
        .map_err(|e| {
            tracing::warn!(error = ?e, "OpenAI streaming client build failed");
            "Could not initialize OpenAI streaming client"
        })?;
    let connect = async {
        let response = client
            .websocket(ENDPOINT)
            .bearer_auth(&session.access_token)
            .header("ChatGPT-Account-Id", &session.account_id)
            .header("originator", "codex_desktop")
            .header("User-Agent", openai::CODEX_DESKTOP_USER_AGENT)
            .protocols(["chatgpt-dictation"])
            .max_message_size(MAX_MESSAGE)
            .max_frame_size(MAX_MESSAGE)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(error = ?e, "OpenAI streaming connection failed");
                "OpenAI streaming connection failed"
            })?;
        let status = response.status();
        let ws = response
            .into_websocket()
            .await
            .map_err(|e| {
                tracing::warn!(%status, error = ?e, "OpenAI streaming handshake failed");
                "OpenAI streaming handshake failed"
            })?;
        if ws.protocol().and_then(|p| p.to_str().ok()) != Some("chatgpt-dictation") {
            tracing::warn!(protocol = ?ws.protocol(), "OpenAI streaming subprotocol mismatch");
            return Err("OpenAI streaming subprotocol was not accepted");
        }
        Ok(ws)
    };
    let mut ws = timeout_at(deadline, connect)
        .await
        .map_err(|_| "OpenAI streaming readiness timed out")??;
    let start = start_message(language);
    timeout_at(deadline, send_json(&mut ws, start))
        .await
        .map_err(|_| "OpenAI streaming readiness timed out")??;
    let mut state = State {
        started_at: Some(started_at),
        ..State::default()
    };
    while !state.ready {
        let message = timeout_at(deadline, ws.recv())
            .await
            .map_err(|_| "OpenAI streaming readiness timed out")?;
        handle_message(&mut ws, &mut state, message, false).await?;
    }
    tracing::info!(
        elapsed_ms = started_at.elapsed().as_millis() as u64,
        "OpenAI streaming socket and session ready"
    );
    run(ws, audio, state).await
}

fn start_message(language: Option<&str>) -> Value {
    let mut config = json!({
        "input_audio_format": "pcm16", "sample_rate_hz": 16000, "num_channels": 1,
        "max_buffer_size_bytes": 4194304, "max_utterance_duration_ms": 30000,
        "session_ttl_ms": 300000, "provider_mode": "streaming_sse",
        "transcript_delivery_mode": "final_only", "session_asset_mode": "none",
        "vad": {"type": "server_vad"}
    });
    if let Some(language) = language
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("auto"))
    {
        config["language"] = json!(language);
    }
    json!({"type": "session.start", "dictation_session_id": uuid::Uuid::new_v4().to_string(),
        "attempt_id": uuid::Uuid::new_v4().to_string(), "config": config,
        "client_capabilities": ["server_stop_requested"]})
}

async fn send_message(ws: &mut WebSocket, message: Message) -> Result<(), String> {
    timeout(SEND_TIMEOUT, ws.send(message))
        .await
        .map_err(|_| "OpenAI streaming send timed out".to_owned())?
        .map_err(|e| {
            tracing::warn!(error = ?e, "OpenAI streaming send failed");
            "OpenAI streaming send failed".to_owned()
        })
}
async fn send_json(ws: &mut WebSocket, message: Value) -> Result<(), String> {
    send_message(ws, Message::text(message.to_string())).await
}

async fn run(
    mut ws: WebSocket,
    mut audio: mpsc::UnboundedReceiver<Vec<u8>>,
    mut state: State,
) -> Result<Transcription, String> {
    let mut audio_bytes = 0_u64;
    let mut audio_chunks = 0_u64;
    loop {
        if audio.len() > MAX_QUEUED_CHUNKS {
            return Err("OpenAI streaming audio backlog exceeded".into());
        }
        tokio::select! {
            biased;
            message = ws.recv() => { handle_message(&mut ws, &mut state, message, false).await?; }
            chunk = audio.recv() => match chunk {
                Some(chunk) => {
                    if chunk.len() > MAX_CHUNK || chunk.len() % 2 != 0 { return Err("Invalid or oversized streaming audio chunk".into()); }
                    if !chunk.is_empty() {
                        audio_bytes = audio_bytes.saturating_add(chunk.len() as u64);
                        audio_chunks = audio_chunks.saturating_add(1);
                        send_json(&mut ws, json!({"type":"audio.append", "audio":STANDARD.encode(chunk)})).await?;
                    }
                }
                None => break,
            }
        }
    }
    state.finish_at = Some(Instant::now());
    tracing::info!(audio_bytes, audio_chunks, "OpenAI streaming finish started");
    // One deadline covers flush, close, and all remaining server events.
    timeout(FINISH_TIMEOUT, async {
        send_json(&mut ws, json!({"type":"audio.flush"})).await?;
        send_json(&mut ws, json!({"type":"session.close"})).await?;
        loop {
            let message = ws.recv().await;
            if handle_message(&mut ws, &mut state, message, true).await? {
                return state.transcription();
            }
        }
    })
    .await
    .map_err(|_| "OpenAI streaming finish timed out".to_owned())?
}

async fn handle_message(
    ws: &mut WebSocket,
    state: &mut State,
    message: Option<Result<Message, wreq::Error>>,
    finishing: bool,
) -> Result<bool, String> {
    match message {
        Some(Ok(Message::Text(text))) => state.event(text.as_ref(), finishing),
        Some(Ok(Message::Ping(data))) => {
            send_message(ws, Message::Pong(data)).await?;
            Ok(false)
        }
        Some(Ok(Message::Pong(_))) => Ok(false),
        Some(Ok(Message::Close(frame))) => {
            tracing::warn!(?frame, "OpenAI streaming socket closed by server");
            Err("OpenAI streaming socket closed prematurely".into())
        }
        None => {
            tracing::warn!("OpenAI streaming socket ended without a close frame");
            Err("OpenAI streaming socket closed prematurely".into())
        }
        Some(Err(e)) => {
            tracing::warn!(error = ?e, "OpenAI streaming receive failed");
            Err("OpenAI streaming receive failed".into())
        }
        Some(Ok(other)) => {
            tracing::warn!(message = ?other, "OpenAI streaming received an unexpected message");
            Err("OpenAI streaming received an invalid message".into())
        }
    }
}

#[derive(Default)]
struct State {
    ready: bool,
    started_at: Option<Instant>,
    finish_at: Option<Instant>,
    // Retain first-seen order and only the newest authoritative final.
    finals: Vec<(String, i64, String)>,
    transcript_bytes: usize,
}
impl State {
    fn event(&mut self, text: &str, finishing: bool) -> Result<bool, String> {
        if text.len() > MAX_MESSAGE {
            return Err("OpenAI streaming message too large".into());
        }
        let event: Value = serde_json::from_str(text).map_err(|e| {
            tracing::warn!(error = %e, len = text.len(), "OpenAI streaming event is not JSON");
            "Invalid OpenAI streaming event"
        })?;
        match event.get("type").and_then(Value::as_str) {
            Some("session.started") => {
                if event["session"]["status"] == "closed" {
                    return Err("OpenAI streaming session closed before audio finished".into());
                }
                let config = &event["session"]["config"];
                if config["input_audio_format"] != "pcm16"
                    || config["sample_rate_hz"] != 16000
                    || config["num_channels"] != 1
                    || config["transcript_delivery_mode"] != "final_only"
                {
                    return Err("OpenAI streaming format or delivery mode mismatch".into());
                }
                self.ready = true;
            }
            Some("transcript.final") => {
                tracing::info!(
                    elapsed_ms = self.started_at.map(|t| t.elapsed().as_millis() as u64),
                    finish_elapsed_ms = self.finish_at.map(|t| t.elapsed().as_millis() as u64),
                    "OpenAI streaming final received"
                );
                let id = event["utterance_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or("Invalid OpenAI final event")?;
                let revision = event["revision"]
                    .as_i64()
                    .ok_or("Invalid OpenAI final event")?;
                let value = event["text"].as_str().ok_or("Invalid OpenAI final event")?;
                if let Some(index) = self.finals.iter().position(|(key, _, _)| key == id) {
                    let (_, old_revision, old_text) = &mut self.finals[index];
                    if revision > *old_revision {
                        let new_size = self.transcript_bytes - old_text.len() + value.len();
                        if new_size > MAX_TRANSCRIPT {
                            return Err("OpenAI streaming transcript too large".into());
                        }
                        self.transcript_bytes = new_size;
                        *old_revision = revision;
                        *old_text = value.to_owned();
                    }
                } else {
                    let new_size = self.transcript_bytes + id.len() + value.len();
                    if self.finals.len() >= MAX_UTTERANCES || new_size > MAX_TRANSCRIPT {
                        return Err("OpenAI streaming transcript too large".into());
                    }
                    self.transcript_bytes = new_size;
                    self.finals
                        .push((id.to_owned(), revision, value.to_owned()));
                }
            }
            Some("session.updated") if event["session"]["status"] == "closed" => {
                if !finishing {
                    return Err("OpenAI streaming session closed before audio finished".into());
                }
                return Ok(true);
            }
            Some("session.stop_requested") => {
                tracing::warn!(event = %event, "OpenAI streaming requested an early stop");
                return Err("OpenAI streaming requested an early stop".into());
            }
            Some("transcript.failed")
            | Some("session.error")
            | Some("error")
            | Some("session.failed") => {
                tracing::warn!(event = %event, "OpenAI streaming service reported an error");
                return Err("OpenAI streaming service reported an error".into());
            }
            Some(_) => {}
            None => return Err("Invalid OpenAI streaming event".into()),
        }
        Ok(false)
    }
    fn transcription(self) -> Result<Transcription, String> {
        let text = self
            .finals
            .into_iter()
            .map(|(_, _, text)| text.trim().to_owned())
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if text.trim().is_empty() {
            return Err("OpenAI streaming returned no final transcript".into());
        }
        Ok(Transcription {
            text,
            detected_language: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message as ServerMessage};

    const READY: &str = r#"{"type":"session.started","session":{"config":{"input_audio_format":"pcm16","sample_rate_hz":16000,"num_channels":1,"transcript_delivery_mode":"final_only"}}}"#;
    const CLOSED: &str = r#"{"type":"session.updated","session":{"status":"closed"}}"#;
    const FINAL: &str =
        r#"{"type":"transcript.final","utterance_id":"one","revision":1,"text":"hello"}"#;

    async fn socket_pair() -> (
        WebSocket,
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            accept_async(listener.accept().await.unwrap().0)
                .await
                .unwrap()
        });
        let ws = Client::builder()
            .build()
            .unwrap()
            .websocket(format!("ws://{addr}"))
            .max_message_size(MAX_MESSAGE)
            .send()
            .await
            .unwrap()
            .into_websocket()
            .await
            .unwrap();
        (ws, server.await.unwrap())
    }
    fn ready() -> State {
        let mut state = State::default();
        state.event(READY, false).unwrap();
        state
    }

    #[test]
    fn validates_config_and_deduplicates_finals() {
        let mut state = ready();
        state.event(FINAL, false).unwrap();
        state
            .event(
                r#"{"type":"transcript.final","utterance_id":"two","revision":1,"text":"world"}"#,
                false,
            )
            .unwrap();
        state
            .event(
                r#"{"type":"transcript.final","utterance_id":"one","revision":2,"text":"hi"}"#,
                false,
            )
            .unwrap();
        state.event(FINAL, false).unwrap();
        state.event(r#"{"type":"transcript.segment","utterance_id":"one","revision":3,"text":"ignored"}"#, false).unwrap();
        assert_eq!(state.transcription().unwrap().text, "hi world");
        assert!(
            State::default()
                .event(&READY.replace("16000", "24000"), false)
                .is_err()
        );
        assert!(
            State::default()
                .event(&READY.replace("final_only", "segment"), false)
                .is_err()
        );
        assert!(ready().event(CLOSED, false).is_err());
        assert!(
            ready()
                .event(r#"{"type":"session.stop_requested"}"#, false)
                .is_err()
        );
        assert!(
            ready()
                .event(r#"{"type":"transcript.failed"}"#, false)
                .is_err()
        );
        for kind in ["error", "session.failed"] {
            assert!(
                ready()
                    .event(&json!({"type": kind}).to_string(), true)
                    .is_err()
            );
        }
        assert!(ready().transcription().is_err());
    }

    #[tokio::test]
    async fn loopback_drains_audio_and_waits_for_closed() {
        let (ws, mut server) = socket_pair().await;
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(vec![0, 1]).unwrap();
        tx.send(vec![2, 3]).unwrap();
        drop(tx);
        let task = tokio::spawn(run(ws, rx, ready()));
        for kind in [
            "audio.append",
            "audio.append",
            "audio.flush",
            "session.close",
        ] {
            let text = server.next().await.unwrap().unwrap().into_text().unwrap();
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(event["type"], kind);
        }
        server
            .send(ServerMessage::Text(FINAL.into()))
            .await
            .unwrap();
        assert!(!task.is_finished());
        server
            .send(ServerMessage::Text(CLOSED.into()))
            .await
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap().text, "hello");
    }

    #[tokio::test]
    async fn loopback_rejects_closed_without_final_and_premature_socket_close() {
        for close_socket in [false, true] {
            let (ws, mut server) = socket_pair().await;
            let (tx, rx) = mpsc::unbounded_channel();
            drop(tx);
            let task = tokio::spawn(run(ws, rx, ready()));
            for _ in 0..2 {
                server.next().await.unwrap().unwrap();
            }
            if close_socket {
                server.close(None).await.unwrap();
            } else {
                server
                    .send(ServerMessage::Text(CLOSED.into()))
                    .await
                    .unwrap();
            }
            assert!(task.await.unwrap().is_err());
        }
    }

    #[tokio::test]
    async fn cancellation_sender_drop_is_not_cancel_and_true_interrupts_pending_work() {
        let (tx, mut rx) = watch::channel(false);
        drop(tx);
        assert!(
            timeout(Duration::from_millis(20), cancelled(&mut rx))
                .await
                .is_err()
        );
        let (tx, mut rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            tokio::select! { _ = cancelled(&mut rx) => true, _ = std::future::pending::<()>() => false }
        });
        tx.send(true).unwrap();
        assert!(
            timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn cancellation_drops_socket_without_finishing() {
        let (ws, mut server) = socket_pair().await;
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = cancelled(&mut cancel_rx) => Err("cancelled".to_owned()),
                result = run(ws, audio_rx, ready()) => result,
            }
        });
        audio_tx.send(vec![0, 1]).unwrap();
        let sent = server.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&sent).unwrap()["type"],
            "audio.append"
        );
        cancel_tx.send(true).unwrap();
        assert!(task.await.unwrap().is_err());
        let next = timeout(Duration::from_secs(1), server.next())
            .await
            .unwrap();
        assert!(next.is_none() || matches!(next, Some(Err(_)) | Some(Ok(ServerMessage::Close(_)))));
    }
}
