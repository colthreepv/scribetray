//! Explicit live check for the production provider dispatcher.
//! Input must be synthetic 16 kHz mono signed-16 little-endian PCM.
#![allow(dead_code)]

#[path = "../src/config.rs"]
mod config;
#[path = "../src/openai.rs"]
mod openai;
#[path = "../src/scribe.rs"]
mod scribe;
#[path = "../src/transcription.rs"]
mod transcription;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let provider = args
        .next()
        .ok_or("usage: transcribe-smoke openai|elevenlabs synthetic.pcm")?;
    let path = args.next().ok_or("missing synthetic PCM path")?;
    let config = config::Config {
        provider: match provider.to_str() {
            Some("openai") => config::Provider::Openai,
            Some("elevenlabs") => config::Provider::Elevenlabs,
            _ => return Err("provider must be openai or elevenlabs".into()),
        },
        ..config::Config::default()
    };
    let pcm = std::fs::read(path)?;
    let started = std::time::Instant::now();
    let result = transcription::transcribe(&config, &pcm, Some("en"))?;
    if result.text.trim().is_empty() {
        return Err("provider returned an empty transcript".into());
    }
    println!(
        "{} ({:.2}s): {}",
        provider.to_string_lossy(),
        started.elapsed().as_secs_f64(),
        result.text
    );
    Ok(())
}
