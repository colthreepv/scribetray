//! Best-effort NVIDIA Broadcast state via its local MCP gateway.

use std::{
    fs,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Active source and microphone effect settings.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct BroadcastSnapshot {
    pub source_microphone: Option<String>,
    pub effects: Value,
}

/// Reads Broadcast state within a total 300 ms timeout.
pub fn snapshot() -> Option<BroadcastSnapshot> {
    match try_snapshot() {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            tracing::debug!(%error, "NVIDIA Broadcast snapshot unavailable");
            None
        }
    }
}

fn try_snapshot() -> Result<BroadcastSnapshot, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_millis(300);
    let path = std::path::PathBuf::from(std::env::var_os("APPDATA").ok_or("APPDATA unavailable")?)
        .join("nvidia-broadcast")
        .join("gateway.json");
    let port = gateway_port(&fs::read(path)?)?;
    let url = format!("http://127.0.0.1:{port}/gateway");
    let client = reqwest::blocking::Client::builder().no_proxy().build()?;
    let init = call(
        &client,
        &url,
        deadline,
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"scribetray", "version":env!("CARGO_PKG_VERSION")}}}),
    )?;
    if init
        .pointer("/result/serverInfo/name")
        .and_then(Value::as_str)
        != Some("nvidia-broadcast")
    {
        return Err("unexpected Broadcast server".into());
    }
    let effects = call(
        &client,
        &url,
        deadline,
        json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"get_broadcast_state", "arguments":{"effectType":"microphone"}}}),
    )?;
    let effects = effects
        .pointer("/result/structuredContent")
        .filter(|value| value.is_object())
        .ok_or("missing effects")?
        .clone();
    let devices = call(
        &client,
        &url,
        deadline,
        json!({"jsonrpc":"2.0", "id":3, "method":"tools/call", "params":{"name":"list_devices", "arguments":{}}}),
    )?;
    let microphones = devices
        .pointer("/result/structuredContent/microphones")
        .and_then(Value::as_array)
        .ok_or("missing microphones")?;
    let source_microphone = microphones
        .iter()
        .find(|device| device.get("active").and_then(Value::as_bool) == Some(true))
        .and_then(|device| device.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(BroadcastSnapshot {
        source_microphone,
        effects,
    })
}

fn gateway_port(contents: &[u8]) -> Result<u16, Box<dyn std::error::Error>> {
    let decoded = STANDARD.decode(std::str::from_utf8(contents)?.trim())?;
    let value: Value = serde_json::from_slice(&decoded)?;
    let port = value
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|&port| port != 0)
        .ok_or("invalid gateway port")?;
    Ok(port)
}

fn call(
    client: &reqwest::blocking::Client,
    url: &str,
    deadline: Instant,
    request: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or("Broadcast timeout")?;
    let response = client
        .post(url)
        .timeout(timeout)
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .json(&request)
        .send()?
        .error_for_status()?;
    let body = response.text()?;
    let value = parse_body(&body)?;
    if value.get("error").is_some()
        || value.pointer("/result/isError").and_then(Value::as_bool) == Some(true)
    {
        return Err("Broadcast RPC error".into());
    }
    Ok(value)
}

fn parse_body(body: &str) -> Result<Value, Box<dyn std::error::Error>> {
    if let Ok(value) = serde_json::from_str(body) {
        return Ok(value);
    }
    let data = body
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .map(|line| line.strip_prefix(' ').unwrap_or(line))
        })
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    Ok(serde_json::from_str(&data)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gateway() {
        assert_eq!(
            gateway_port(STANDARD.encode(r#"{"port":12345}"#).as_bytes()).unwrap(),
            12345
        );
        assert!(gateway_port(b"not base64").is_err());
        assert!(gateway_port(STANDARD.encode(r#"{"port":65536}"#).as_bytes()).is_err());
    }

    #[test]
    fn parses_json_and_sse() {
        let expected = json!({"result":{"ok":true}});
        assert_eq!(parse_body(r#"{"result":{"ok":true}}"#).unwrap(), expected);
        assert_eq!(
            parse_body(
                "event: message
data: {\"result\":
data: {\"ok\":true}}

"
            )
            .unwrap(),
            expected
        );
        assert!(
            parse_body(
                "data: nope
"
            )
            .is_err()
        );
    }
}
