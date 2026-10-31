//! Protocol probe for the OpenAI dictation stream.
//! Usage: openai-stream-probe <session_ttl_ms> <recording.pcm> [repeat] [pause_at_s] [pause_s]
//! Streams 16 kHz mono PCM16 in real time and prints every server event with its timestamp.
#![allow(dead_code)]

#[path = "../src/openai.rs"]
mod openai;
#[path = "../src/scribe.rs"]
mod scribe;

use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use wreq::{Client, ws::message::Message};
use wreq_util::Emulation;

fn summarize(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(mut event) => {
            if let Some(t) = event.get("text").and_then(Value::as_str) {
                let short: String = t.chars().take(60).collect();
                event["text"] = json!(format!("{short}... ({} chars)", t.len()));
            }
            if event["type"] == "session.updated" {
                let s = &event["session"];
                return format!("session.updated status={} buffered={} utterance={} ttl_left={}ms",
                    s["status"], s["buffered_audio_bytes"], s["current_utterance_bytes"],
                    s["expires_at_ms"].as_i64().unwrap_or(0) - s["last_activity_ms"].as_i64().unwrap_or(0));
            }
            event.to_string()
        }
        Err(_) => text.chars().take(300).collect(),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let ttl: u64 = args.next().ok_or("expected session_ttl_ms")?.parse()?;
    let pcm = std::fs::read(args.next().ok_or("expected PCM file")?)?;
    let repeat: usize = args.next().map_or(Ok(1), |s| s.parse())?;
    let pause_at: f64 = args.next().map_or(Ok(f64::INFINITY), |s| s.parse())?;
    let pause_len: f64 = args.next().map_or(Ok(0.0), |s| s.parse())?;
    let audio: Vec<u8> = pcm.iter().copied().cycle().take(pcm.len() * repeat).collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        let session = openai::credentials()?;
        let client = Client::builder().emulation(Emulation::Chrome147).build()?;
        let t0 = Instant::now();
        let stamp = |t0: Instant| format!("{:7.2}s", t0.elapsed().as_secs_f64());
        let response = client
            .websocket("wss://chatgpt.com/backend-api/dictation/stream")
            .bearer_auth(&session.access_token)
            .header("ChatGPT-Account-Id", &session.account_id)
            .header("originator", "codex_desktop")
            .header("User-Agent", openai::CODEX_DESKTOP_USER_AGENT)
            .protocols(["chatgpt-dictation"])
            .send()
            .await?;
        let mut ws = response.into_websocket().await?;
        let start = json!({"type": "session.start",
            "dictation_session_id": uuid::Uuid::new_v4().to_string(),
            "attempt_id": uuid::Uuid::new_v4().to_string(),
            "config": {"input_audio_format": "pcm16", "sample_rate_hz": 16000, "num_channels": 1,
                "max_buffer_size_bytes": 4194304, "max_utterance_duration_ms": 30000,
                "session_ttl_ms": ttl, "provider_mode": "streaming_sse",
                "transcript_delivery_mode": "final_only", "session_asset_mode": "none",
                "vad": {"type": "server_vad"}},
            "client_capabilities": ["server_stop_requested"]});
        ws.send(Message::text(start.to_string())).await?;
        println!("{} sent session.start ttl={ttl} audio={:.1}s", stamp(t0), audio.len() as f64 / 32_000.0);
        let origin = tokio::time::Instant::now();
        let mut chunks = audio.chunks(3_200);
        let mut finishing = false;
        let mut sent = 0usize;
        loop {
            let at = sent as f64 / 32_000.0;
            let next_send = origin + Duration::from_secs_f64(if at >= pause_at { at + pause_len } else { at });
            tokio::select! {
                message = ws.recv() => match message {
                    Some(Ok(Message::Text(text))) => {
                        let line = summarize(text.as_ref());
                        let routine = line.starts_with("session.updated status=\"active\"");
                        if !routine || (sent / 3_200) % 50 == 0 { println!("{} <- {}", stamp(t0), line); }
                    }
                    Some(Ok(Message::Close(frame))) => { println!("{} <- CLOSE {:?}", stamp(t0), frame); break; }
                    Some(Ok(other)) => println!("{} <- {:?}", stamp(t0), other),
                    Some(Err(e)) => { println!("{} <- ERROR {e:?}", stamp(t0)); break; }
                    None => { println!("{} <- socket ended", stamp(t0)); break; }
                },
                _ = tokio::time::sleep_until(next_send), if !finishing => match chunks.next() {
                    Some(chunk) => {
                        let msg = json!({"type": "audio.append", "audio": STANDARD.encode(chunk)});
                        if let Err(e) = ws.send(Message::text(msg.to_string())).await {
                            println!("{} send failed after {:.1}s audio: {e:?}", stamp(t0), sent as f64 / 32_000.0);
                            break;
                        }
                        sent += chunk.len();
                    }
                    None => {
                        finishing = true;
                        println!("{} audio done ({:.1}s), flush + close", stamp(t0), sent as f64 / 32_000.0);
                        ws.send(Message::text(json!({"type": "audio.flush"}).to_string())).await?;
                        ws.send(Message::text(json!({"type": "session.close"}).to_string())).await?;
                    }
                },
            }
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}
