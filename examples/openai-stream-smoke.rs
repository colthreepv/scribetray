//! Explicit live replay using the production OpenAI clients.
//! Usage: openai-stream-smoke stream|batch|trimmed|cancel recording.pcm [output.json]
//! Sends the supplied audio to OpenAI; credentials are read locally, never printed.
#![allow(dead_code)]

#[path = "../src/openai.rs"]
mod openai;
#[path = "../src/openai_streaming.rs"]
mod openai_streaming;
#[path = "../src/scribe.rs"]
mod scribe;
#[path = "../src/silence.rs"]
mod silence;

use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or("expected stream|batch|trimmed|cancel")?;
    let pcm = std::fs::read(args.next().ok_or("expected PCM file")?)?;
    let output = args.next();
    if pcm.is_empty() || pcm.len() % 2 != 0 {
        return Err("expected nonempty 16 kHz mono PCM16".into());
    }
    let original_seconds = pcm.len() as f64 / 32_000.0;
    let started = Instant::now();
    let (result, after_stop, uploaded_seconds) = match mode.as_str() {
        "batch" | "trimmed" => {
            let trimmed = (mode == "trimmed")
                .then(|| silence::trim_silence(&pcm))
                .flatten();
            let bytes = trimmed.as_ref().map_or(pcm.as_slice(), |(audio, _)| audio);
            let result = openai::transcribe(bytes, None).map_err(|e| e.to_string());
            (result, started.elapsed(), bytes.len() as f64 / 32_000.0)
        }
        "stream" | "cancel" => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
                let (cancel, cancellation) = tokio::sync::watch::channel(false);
                let feed = async {
                    let origin = tokio::time::Instant::now();
                    let mut sent_bytes = 0;
                    for (index, chunk) in pcm.chunks(3_200).enumerate() {
                        if mode == "cancel" && index == 10 {
                            let _ = cancel.send(true);
                            break;
                        }
                        if sender.send(chunk.to_vec()).is_err() {
                            break;
                        }
                        sent_bytes += chunk.len();
                        let end = ((index + 1) * 3_200).min(pcm.len());
                        tokio::time::sleep_until(
                            origin + Duration::from_secs_f64(end as f64 / 32_000.0),
                        )
                        .await;
                    }
                    drop(sender);
                    (Instant::now(), sent_bytes)
                };
                let stream = async {
                    let result =
                        openai_streaming::transcribe_stream(None, receiver, cancellation).await;
                    (result, Instant::now())
                };
                let ((stopped, sent_bytes), (result, completed)) = tokio::join!(feed, stream);
                (
                    result,
                    completed.saturating_duration_since(stopped),
                    sent_bytes as f64 / 32_000.0,
                )
            })
        }
        _ => return Err("expected stream|batch|trimmed|cancel".into()),
    };
    let (success, text, error) = match result {
        Ok(result) => (!result.text.trim().is_empty(), Some(result.text), None),
        Err(error) => (false, None, Some(error)),
    };
    let expected_cancel = error.as_deref() == Some("OpenAI streaming cancelled");
    let report = serde_json::json!({
        "mode": mode, "success": success, "original_seconds": original_seconds,
        "uploaded_seconds": uploaded_seconds, "after_stop_seconds": after_stop.as_secs_f64(),
        "total_seconds": started.elapsed().as_secs_f64(),
        "transcript_chars": text.as_ref().map_or(0, |text| text.len()), "error": error,
    });
    println!("{report}");
    if let Some(path) = output {
        let mut saved = report;
        saved["text"] = text.into();
        std::fs::write(path, serde_json::to_vec_pretty(&saved)?)?;
    }
    if mode == "cancel" {
        if success || !expected_cancel {
            return Err("cancel did not return the expected cancellation result".into());
        }
    } else if !success {
        return Err("transcription failed".into());
    }
    Ok(())
}
