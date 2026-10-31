//! Batch transcription through the ChatGPT backend used by a local Codex
//! session.
//!
//! The auth file is treated as read-only input. Only the access token, account
//! ID, and cached identity claims are read; refresh-token material is ignored.

use std::{
    ffi::OsString,
    fmt,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use futures_util::StreamExt;
use serde::Deserialize;
use thiserror::Error;
use wreq::{
    Client, StatusCode,
    header::HeaderValue,
    multipart::{Form, Part},
    redirect, retry,
};
use wreq_util::Emulation;

use crate::scribe::Transcription;

const ENDPOINT: &str = "https://chatgpt.com/backend-api/transcribe";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_AUTH_BYTES: u64 = 1024 * 1024;
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const AUDIO_SAMPLE_RATE: u32 = 16_000;
const AUDIO_CHANNELS: u16 = 1;
const AUDIO_BITS_PER_SAMPLE: u16 = 16;
const AUDIO_BYTES_PER_SAMPLE: usize = 2;
const WAV_HEADER_BYTES: u32 = 44;
const OPENAI_AUTH_CLAIM: &str = "https://api.openai.com/auth";
const OPENAI_PROFILE_CLAIM: &str = "https://api.openai.com/profile";
pub(crate) const CODEX_DESKTOP_USER_AGENT: &str =
    "Codex Desktop/26.1023.6621 (Windows NT 10.0; x64)";

/// The state of the local Codex ChatGPT session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Ready {
        email: Option<String>,
        plan: Option<String>,
    },
    Missing,
    Expired,
    Unavailable(String),
}

/// A safe user-facing failure from OpenAI batch transcription.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum OpenAiError {
    #[error("The recording is empty.")]
    EmptyAudio,
    #[error("The recording contains an incomplete 16-bit sample.")]
    OddAudioLength,
    #[error("The recording is too large for a WAV file.")]
    AudioTooLarge,
    #[error("Codex sign-in was not found. Open Codex and sign in.")]
    SessionMissing,
    #[error("The Codex session expired. Open Codex and sign in again.")]
    SessionExpired,
    #[error("The Codex session is unavailable: {0}")]
    SessionUnavailable(String),
    #[error("The OpenAI transcription request could not be created.")]
    RequestSetup,
    #[error("The OpenAI transcription runtime could not be started.")]
    RuntimeUnavailable,
    #[error("The OpenAI transcription client could not be initialized.")]
    ClientInitialization,
    #[error("OpenAI transcription timed out. Try again.")]
    NetworkTimeout,
    #[error("The OpenAI transcription service could not be reached.")]
    NetworkFailure,
    #[error("Cloudflare challenged the OpenAI transcription request. Open Codex and retry.")]
    CloudflareChallenge,
    #[error("Codex rejected the session (HTTP 401). Open Codex and sign in again.")]
    Unauthorized,
    #[error("Codex denied cloud transcription (HTTP 403). Check the account or plan in Codex.")]
    Forbidden,
    #[error("OpenAI transcription is rate limited (HTTP 429). Wait before trying again.")]
    RateLimited,
    #[error("The OpenAI transcription endpoint returned an unexpected HTTP {0} response.")]
    HttpStatus(u16),
    #[error("The OpenAI transcription response was unexpectedly large.")]
    ResponseTooLarge,
    #[error("The OpenAI transcription response was invalid.")]
    InvalidResponse,
}

#[derive(Deserialize)]
struct AuthFile {
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(default)]
    tokens: Option<AuthTokens>,
}

#[derive(Deserialize)]
struct AuthTokens {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

pub(crate) struct Session {
    pub(crate) access_token: String,
    pub(crate) account_id: String,
}

pub(crate) fn credentials() -> Result<Session, OpenAiError> {
    load_session_at(unix_now_secs())
        .map(Into::into)
        .map_err(SessionFailure::into_error)
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Session")
            .field("access_token", &"<redacted>")
            .field("account_id", &"<redacted>")
            .finish()
    }
}

#[derive(Debug)]
enum SessionFailure {
    Missing,
    Expired,
    Unavailable(&'static str),
}

impl SessionFailure {
    fn into_status(self) -> SessionStatus {
        match self {
            Self::Missing => SessionStatus::Missing,
            Self::Expired => SessionStatus::Expired,
            Self::Unavailable(reason) => SessionStatus::Unavailable(reason.to_owned()),
        }
    }

    fn into_error(self) -> OpenAiError {
        match self {
            Self::Missing => OpenAiError::SessionMissing,
            Self::Expired => OpenAiError::SessionExpired,
            Self::Unavailable(reason) => OpenAiError::SessionUnavailable(reason.to_owned()),
        }
    }
}

/// Reads the current Codex session and reports whether it can be used for a
/// transcription immediately.
pub fn session_status() -> SessionStatus {
    match load_session_at(unix_now_secs()) {
        Ok(session) => SessionStatus::Ready {
            email: session.email,
            plan: session.plan,
        },
        Err(failure) => failure.into_status(),
    }
}

/// Transcribes 16 kHz mono signed-16 little-endian PCM. The Codex auth file is
/// re-read immediately before the request so a renewed local session is picked
/// up without restarting Scribetray.
pub fn transcribe(pcm_s16le: &[u8], language: Option<&str>) -> Result<Transcription, OpenAiError> {
    let wav = wrap_pcm_as_wav(pcm_s16le)?;
    let session = load_session_at(unix_now_secs()).map_err(SessionFailure::into_error)?;

    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(OpenAiError::RuntimeUnavailable);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| OpenAiError::RuntimeUnavailable)?;
    runtime.block_on(transcribe_request(session.into(), wav, language))
}

struct ReadySession {
    access_token: String,
    account_id: String,
    email: Option<String>,
    plan: Option<String>,
}

impl From<ReadySession> for Session {
    fn from(session: ReadySession) -> Self {
        Self {
            access_token: session.access_token,
            account_id: session.account_id,
        }
    }
}

fn session_from_auth(auth: AuthFile, now: u64) -> Result<ReadySession, SessionFailure> {
    let mode = auth
        .auth_mode
        .as_deref()
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .ok_or(SessionFailure::Unavailable(
            "the Codex session has no auth mode",
        ))?;

    if mode.eq_ignore_ascii_case("apikey") {
        return Err(SessionFailure::Unavailable(
            "Codex is signed in with an API key; sign in with ChatGPT",
        ));
    }
    if !mode.eq_ignore_ascii_case("chatgpt") {
        return Err(SessionFailure::Unavailable(
            "the Codex session has an unsupported auth mode",
        ));
    }

    let tokens = auth.tokens.ok_or(SessionFailure::Unavailable(
        "the Codex session has no ChatGPT tokens",
    ))?;
    let access_token = tokens
        .access_token
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
        .ok_or(SessionFailure::Unavailable(
            "the Codex access token is missing",
        ))?;
    let access_claims = jwt_claims(&access_token).ok_or(SessionFailure::Unavailable(
        "the Codex access token is malformed",
    ))?;
    let expires_at = access_claims
        .get("exp")
        .and_then(serde_json::Value::as_u64)
        .ok_or(SessionFailure::Unavailable(
            "the Codex access token has no valid expiry",
        ))?;
    if expires_at <= now {
        return Err(SessionFailure::Expired);
    }

    let id_claims = tokens.id_token.as_deref().and_then(jwt_claims);
    let account_id = tokens
        .account_id
        .as_deref()
        .and_then(nonempty_trimmed)
        .or_else(|| nested_string(&access_claims, OPENAI_AUTH_CLAIM, "chatgpt_account_id"))
        .or_else(|| {
            id_claims
                .as_ref()
                .and_then(|claims| nested_string(claims, OPENAI_AUTH_CLAIM, "chatgpt_account_id"))
        })
        .ok_or(SessionFailure::Unavailable(
            "the Codex session has no account ID",
        ))?;

    let email = metadata_string(
        &access_claims,
        id_claims.as_ref(),
        OPENAI_PROFILE_CLAIM,
        "email",
    )
    .and_then(|email| sanitize_display(&email, 254));
    let plan = metadata_string(
        &access_claims,
        id_claims.as_ref(),
        OPENAI_AUTH_CLAIM,
        "chatgpt_plan_type",
    )
    .and_then(|plan| sanitize_display(&plan, 32));

    Ok(ReadySession {
        access_token,
        account_id,
        email,
        plan,
    })
}

fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let mut segments = token.split('.');
    let _header = segments.next()?;
    let payload = segments.next().filter(|payload| !payload.is_empty())?;
    let _signature = segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims.is_object().then_some(claims)
}

fn metadata_string(
    access_claims: &serde_json::Value,
    id_claims: Option<&serde_json::Value>,
    namespace: &str,
    field: &str,
) -> Option<String> {
    nested_string(access_claims, namespace, field)
        .or_else(|| top_level_string(access_claims, field))
        .or_else(|| id_claims.and_then(|claims| nested_string(claims, namespace, field)))
        .or_else(|| id_claims.and_then(|claims| top_level_string(claims, field)))
}

fn nested_string(claims: &serde_json::Value, namespace: &str, field: &str) -> Option<String> {
    claims
        .get(namespace)?
        .get(field)?
        .as_str()
        .and_then(nonempty_trimmed)
}

fn top_level_string(claims: &serde_json::Value, field: &str) -> Option<String> {
    claims.get(field)?.as_str().and_then(nonempty_trimmed)
}

fn nonempty_trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn sanitize_display(value: &str, max_characters: usize) -> Option<String> {
    let mut sanitized = String::new();
    let mut previous_was_space = false;
    for character in value.trim().chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if character.is_whitespace() {
            if !previous_was_space && !sanitized.is_empty() {
                sanitized.push(' ');
            }
            previous_was_space = true;
            continue;
        }
        previous_was_space = false;
        if sanitized.chars().count() >= max_characters {
            break;
        }
        sanitized.push(character);
    }
    while sanitized.ends_with(' ') {
        sanitized.pop();
    }
    (!sanitized.is_empty()).then_some(sanitized)
}

fn load_session_at(now: u64) -> Result<ReadySession, SessionFailure> {
    let path = resolve_auth_path(
        std::env::var_os("CODEX_HOME"),
        std::env::var_os("USERPROFILE"),
    )?;
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SessionFailure::Missing);
        }
        Err(_) => {
            return Err(SessionFailure::Unavailable(
                "the Codex session file cannot be read",
            ));
        }
    };
    if !metadata.is_file() {
        return Err(SessionFailure::Unavailable(
            "the Codex session path is not a file",
        ));
    }
    if metadata.len() > MAX_AUTH_BYTES {
        return Err(SessionFailure::Unavailable(
            "the Codex session file is unexpectedly large",
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|_| SessionFailure::Unavailable("the Codex session file cannot be read"))?;
    parse_auth_bytes(&bytes, now)
}

fn resolve_auth_path(
    codex_home: Option<OsString>,
    user_profile: Option<OsString>,
) -> Result<PathBuf, SessionFailure> {
    if let Some(codex_home) = codex_home.filter(|value| !value.is_empty()) {
        let codex_home = PathBuf::from(codex_home);
        if !codex_home.is_dir() {
            return Err(SessionFailure::Unavailable(
                "the configured CODEX_HOME folder is invalid",
            ));
        }
        return Ok(codex_home.join("auth.json"));
    }

    let user_profile =
        user_profile
            .filter(|value| !value.is_empty())
            .ok_or(SessionFailure::Unavailable(
                "USERPROFILE is unavailable for Codex session discovery",
            ))?;
    Ok(PathBuf::from(user_profile).join(".codex").join("auth.json"))
}

fn parse_auth_bytes(bytes: &[u8], now: u64) -> Result<ReadySession, SessionFailure> {
    let auth: AuthFile = serde_json::from_slice(bytes)
        .map_err(|_| SessionFailure::Unavailable("the Codex session file is malformed"))?;
    session_from_auth(auth, now)
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn validate_pcm_len(length: usize) -> Result<u32, OpenAiError> {
    if length == 0 {
        return Err(OpenAiError::EmptyAudio);
    }
    if !length.is_multiple_of(AUDIO_BYTES_PER_SAMPLE) {
        return Err(OpenAiError::OddAudioLength);
    }
    let data_length = u32::try_from(length).map_err(|_| OpenAiError::AudioTooLarge)?;
    if data_length > u32::MAX - (WAV_HEADER_BYTES - 8) {
        return Err(OpenAiError::AudioTooLarge);
    }
    Ok(data_length)
}

fn wrap_pcm_as_wav(pcm_s16le: &[u8]) -> Result<Vec<u8>, OpenAiError> {
    let data_length = validate_pcm_len(pcm_s16le.len())?;
    let riff_length = 36_u32
        .checked_add(data_length)
        .ok_or(OpenAiError::AudioTooLarge)?;
    let byte_rate = AUDIO_SAMPLE_RATE
        .checked_mul(u32::from(AUDIO_CHANNELS))
        .and_then(|rate| rate.checked_mul(u32::from(AUDIO_BITS_PER_SAMPLE / 8)))
        .ok_or(OpenAiError::AudioTooLarge)?;
    let block_align = AUDIO_CHANNELS
        .checked_mul(AUDIO_BITS_PER_SAMPLE / 8)
        .ok_or(OpenAiError::AudioTooLarge)?;

    let capacity = WAV_HEADER_BYTES as usize + pcm_s16le.len();
    let mut wav = Vec::with_capacity(capacity);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_length.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&AUDIO_CHANNELS.to_le_bytes());
    wav.extend_from_slice(&AUDIO_SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&AUDIO_BITS_PER_SAMPLE.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_length.to_le_bytes());
    wav.extend_from_slice(pcm_s16le);
    debug_assert_eq!(wav.len(), capacity);
    Ok(wav)
}
async fn transcribe_request(
    session: Session,
    wav: Vec<u8>,
    language: Option<&str>,
) -> Result<Transcription, OpenAiError> {
    HeaderValue::from_str(&session.access_token).map_err(|_| OpenAiError::RequestSetup)?;
    HeaderValue::from_str(&session.account_id).map_err(|_| OpenAiError::RequestSetup)?;

    let mut form = Form::new();
    if let Some(language) = language
        .map(str::trim)
        .filter(|language| !language.is_empty() && !language.eq_ignore_ascii_case("auto"))
    {
        form = form.text("language", language.to_owned());
    }
    let audio = Part::bytes(wav)
        .file_name("scribetray.wav")
        .mime_str("audio/wav")
        .map_err(|_| OpenAiError::RequestSetup)?;
    form = form.part("file", audio);

    let client = Client::builder()
        .emulation(Emulation::Chrome147)
        .timeout(REQUEST_TIMEOUT)
        .redirect(redirect::Policy::none())
        .retry(retry::Policy::never())
        .build()
        .map_err(|_| OpenAiError::ClientInitialization)?;
    let response = client
        .post(ENDPOINT)
        .bearer_auth(&session.access_token)
        .header("ChatGPT-Account-Id", &session.account_id)
        .header("originator", "codex_desktop")
        .header("User-Agent", CODEX_DESKTOP_USER_AGENT)
        .multipart(form)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                OpenAiError::NetworkTimeout
            } else {
                OpenAiError::NetworkFailure
            }
        })?;

    let status = response.status();
    let cloudflare_challenge = response
        .headers()
        .get("cf-mitigated")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let value = value.trim();
            !value.is_empty() && !value.eq_ignore_ascii_case("none")
        });
    if cloudflare_challenge {
        return Err(OpenAiError::CloudflareChallenge);
    }
    if !status.is_success() {
        return Err(http_status_error(status));
    }

    let bytes = read_bounded_response(response).await?;
    parse_transcription_body(&bytes)
}

async fn read_bounded_response(response: wreq::Response) -> Result<Vec<u8>, OpenAiError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err(OpenAiError::ResponseTooLarge);
    }

    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            if error.is_timeout() {
                OpenAiError::NetworkTimeout
            } else {
                OpenAiError::NetworkFailure
            }
        })?;
        checked_response_extend(&mut body, &chunk)?;
    }
    Ok(body)
}

fn checked_response_extend(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), OpenAiError> {
    let combined_length = body
        .len()
        .checked_add(chunk.len())
        .ok_or(OpenAiError::ResponseTooLarge)?;
    if combined_length as u64 > MAX_RESPONSE_BYTES {
        return Err(OpenAiError::ResponseTooLarge);
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn http_status_error(status: StatusCode) -> OpenAiError {
    match status.as_u16() {
        401 => OpenAiError::Unauthorized,
        403 => OpenAiError::Forbidden,
        429 => OpenAiError::RateLimited,
        status => OpenAiError::HttpStatus(status),
    }
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    text: Option<String>,
    #[serde(default, alias = "language")]
    detected_language: Option<String>,
}

fn parse_transcription_body(bytes: &[u8]) -> Result<Transcription, OpenAiError> {
    let response: TranscriptionResponse =
        serde_json::from_slice(bytes).map_err(|_| OpenAiError::InvalidResponse)?;
    let text = response.text.ok_or(OpenAiError::InvalidResponse)?;
    let detected_language = response
        .detected_language
        .and_then(|language| nonempty_trimmed(&language));
    Ok(Transcription {
        text,
        detected_language,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;

    use super::*;

    fn session_status_from_auth(auth: AuthFile, now: u64) -> SessionStatus {
        match session_from_auth(auth, now) {
            Ok(session) => SessionStatus::Ready {
                email: session.email,
                plan: session.plan,
            },
            Err(failure) => failure.into_status(),
        }
    }

    fn token(claims: serde_json::Value) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("e30.{payload}.signature")
    }

    fn auth_with(
        access_claims: serde_json::Value,
        id_claims: Option<serde_json::Value>,
    ) -> AuthFile {
        AuthFile {
            auth_mode: Some("chatgpt".to_owned()),
            tokens: Some(AuthTokens {
                access_token: Some(token(access_claims)),
                id_token: id_claims.map(token),
                account_id: None,
            }),
        }
    }

    #[test]
    fn parses_chatgpt_metadata_and_account_claim_fallback() {
        let auth = auth_with(
            json!({
                "exp": 2_000,
                OPENAI_AUTH_CLAIM: {
                    "chatgpt_account_id": "account-from-claim",
                    "chatgpt_plan_type": "plus"
                },
                OPENAI_PROFILE_CLAIM: {"email": "user@example.com"}
            }),
            None,
        );

        let status = session_status_from_auth(auth, 1_000);
        assert_eq!(
            status,
            SessionStatus::Ready {
                email: Some("user@example.com".to_owned()),
                plan: Some("plus".to_owned()),
            }
        );
    }

    #[test]
    fn explicit_account_id_wins_and_id_token_expiry_is_ignored() {
        let mut auth = auth_with(
            json!({
                "exp": 2_000,
                OPENAI_AUTH_CLAIM: {"chatgpt_account_id": "claim-account"}
            }),
            Some(json!({
                "exp": 1,
                OPENAI_PROFILE_CLAIM: {"email": "cached@example.com"},
                OPENAI_AUTH_CLAIM: {"chatgpt_plan_type": "pro"}
            })),
        );
        auth.tokens.as_mut().unwrap().account_id = Some("explicit-account".to_owned());

        let session = session_from_auth(auth, 1_000).expect("session should be ready");
        assert_eq!(session.account_id, "explicit-account");
        assert_eq!(session.email.as_deref(), Some("cached@example.com"));
        assert_eq!(session.plan.as_deref(), Some("pro"));
    }

    #[test]
    fn top_level_id_token_email_is_used_as_cached_metadata() {
        let auth = auth_with(
            json!({
                "exp": 2_000,
                OPENAI_AUTH_CLAIM: {"chatgpt_account_id": "account"}
            }),
            Some(json!({"email": "top-level@example.com"})),
        );
        let status = session_status_from_auth(auth, 1_000);
        assert!(matches!(
            status,
            SessionStatus::Ready { email: Some(email), .. } if email == "top-level@example.com"
        ));
    }

    #[test]
    fn access_token_expiry_controls_readiness() {
        let auth = auth_with(
            json!({
                "exp": 999,
                OPENAI_AUTH_CLAIM: {"chatgpt_account_id": "account"}
            }),
            Some(json!({"exp": 10_000})),
        );
        assert_eq!(
            session_status_from_auth(auth, 1_000),
            SessionStatus::Expired
        );
    }

    #[test]
    fn malformed_and_non_chatgpt_auth_are_unavailable() {
        let api_key = AuthFile {
            auth_mode: Some("apikey".to_owned()),
            tokens: None,
        };
        assert!(matches!(
            session_status_from_auth(api_key, 1_000),
            SessionStatus::Unavailable(reason) if reason.contains("sign in with ChatGPT")
        ));
        assert!(matches!(
            parse_auth_bytes(b"not json", 1_000),
            Err(SessionFailure::Unavailable(reason)) if reason.contains("malformed")
        ));
    }

    #[test]
    fn explicit_invalid_codex_home_never_falls_back() {
        let missing = std::env::temp_dir().join(format!(
            "scribetray-missing-codex-home-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let error = resolve_auth_path(
            Some(missing.into_os_string()),
            Some(OsString::from("C:\\valid-profile")),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            SessionFailure::Unavailable(reason) if reason.contains("CODEX_HOME")
        ));
    }

    #[test]
    fn empty_codex_home_uses_user_profile() {
        let path =
            resolve_auth_path(Some(OsString::new()), Some(OsString::from("C:\\profile"))).unwrap();
        assert_eq!(
            path,
            PathBuf::from("C:\\profile")
                .join(".codex")
                .join("auth.json")
        );
    }

    #[test]
    fn wav_header_is_canonical_16khz_mono_pcm() {
        let wav = wrap_pcm_as_wav(&[1, 0, 2, 0]).unwrap();
        assert_eq!(wav.len(), 48);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 40);
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 32_000);
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4);
        assert_eq!(&wav[44..], &[1, 0, 2, 0]);
    }

    #[test]
    fn invalid_or_oversized_pcm_is_rejected() {
        assert_eq!(wrap_pcm_as_wav(&[]).unwrap_err(), OpenAiError::EmptyAudio);
        assert_eq!(
            wrap_pcm_as_wav(&[0]).unwrap_err(),
            OpenAiError::OddAudioLength
        );
        assert_eq!(
            validate_pcm_len(usize::MAX - 1).unwrap_err(),
            OpenAiError::AudioTooLarge
        );
    }

    #[test]
    fn bounded_response_accumulator_rejects_cumulative_overflow() {
        let mut body = vec![0; MAX_RESPONSE_BYTES as usize];
        assert_eq!(
            checked_response_extend(&mut body, &[0]).unwrap_err(),
            OpenAiError::ResponseTooLarge
        );
        assert_eq!(body.len(), MAX_RESPONSE_BYTES as usize);
    }

    #[test]
    fn response_parser_handles_text_and_language() {
        assert_eq!(
            parse_transcription_body(br#"{"text":"hello","language":"en"}"#).unwrap(),
            Transcription {
                text: "hello".to_owned(),
                detected_language: Some("en".to_owned()),
            }
        );
        assert_eq!(
            parse_transcription_body(br#"{"text":"hello"}"#)
                .unwrap()
                .detected_language,
            None
        );
        assert_eq!(
            parse_transcription_body(br#"{"asset_pointer":"x"}"#).unwrap_err(),
            OpenAiError::InvalidResponse
        );
    }

    #[test]
    fn http_and_cloudflare_errors_are_safe_and_distinct() {
        let unauthorized = http_status_error(StatusCode::UNAUTHORIZED).to_string();
        let forbidden = http_status_error(StatusCode::FORBIDDEN).to_string();
        let rate_limited = http_status_error(StatusCode::TOO_MANY_REQUESTS).to_string();
        assert!(unauthorized.contains("sign in"));
        assert!(forbidden.contains("account or plan"));
        assert!(rate_limited.contains("rate limited"));
        assert_ne!(unauthorized, forbidden);
        assert_ne!(forbidden, rate_limited);
        assert!(
            OpenAiError::CloudflareChallenge
                .to_string()
                .contains("Cloudflare")
        );
    }

    #[test]
    fn session_debug_redacts_credentials() {
        let debug = format!(
            "{:?}",
            Session {
                access_token: "secret-access-token".to_owned(),
                account_id: "private-account".to_owned(),
            }
        );
        assert!(!debug.contains("secret-access-token"));
        assert!(!debug.contains("private-account"));
        assert!(debug.contains("<redacted>"));
    }
    #[test]
    #[ignore = "calls the live ChatGPT transcription endpoint; set SCRIBETRAY_TEST_WAV"]
    fn transcribes_synthetic_wav() {
        let path = std::env::var_os("SCRIBETRAY_TEST_WAV")
            .map(PathBuf::from)
            .expect("SCRIBETRAY_TEST_WAV must point to the synthetic WAV");
        let pcm = read_test_wav_as_16khz_pcm(&path);
        let result = transcribe(&pcm, Some("en")).expect("live transcription should succeed");
        let normalized = result
            .text
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            normalized.contains("the quick brown fox checks the transcription proof of concept"),
            "unexpected transcript: {:?}",
            result.text
        );
    }

    fn read_test_wav_as_16khz_pcm(path: &Path) -> Vec<u8> {
        let bytes = std::fs::read(path).expect("read SCRIBETRAY_TEST_WAV");
        assert!(bytes.len() >= 12, "WAV is too short");
        assert_eq!(&bytes[0..4], b"RIFF", "WAV is missing RIFF");
        assert_eq!(&bytes[8..12], b"WAVE", "WAV is not a WAVE file");

        let mut position = 12_usize;
        let mut format = None;
        let mut audio = None;
        while position
            .checked_add(8)
            .is_some_and(|header_end| header_end <= bytes.len())
        {
            let chunk_id = &bytes[position..position + 4];
            let chunk_length = u32::from_le_bytes(
                bytes[position + 4..position + 8]
                    .try_into()
                    .expect("four-byte WAV chunk length"),
            ) as usize;
            let payload_start = position + 8;
            let payload_end = payload_start
                .checked_add(chunk_length)
                .expect("WAV chunk length overflow");
            assert!(payload_end <= bytes.len(), "truncated WAV chunk");
            if chunk_id == b"fmt " {
                assert!(chunk_length >= 16, "WAV fmt chunk is too short");
                let format_tag =
                    u16::from_le_bytes(bytes[payload_start..payload_start + 2].try_into().unwrap());
                let channels = u16::from_le_bytes(
                    bytes[payload_start + 2..payload_start + 4]
                        .try_into()
                        .unwrap(),
                );
                let sample_rate = u32::from_le_bytes(
                    bytes[payload_start + 4..payload_start + 8]
                        .try_into()
                        .unwrap(),
                );
                let bits_per_sample = u16::from_le_bytes(
                    bytes[payload_start + 14..payload_start + 16]
                        .try_into()
                        .unwrap(),
                );
                assert_eq!(format_tag, 1, "test WAV must be PCM");
                assert_eq!(channels, 1, "test WAV must be mono");
                assert_eq!(bits_per_sample, 16, "test WAV must be signed 16-bit");
                format = Some(sample_rate);
            } else if chunk_id == b"data" {
                audio = Some(bytes[payload_start..payload_end].to_vec());
            }
            position = payload_end
                .checked_add(chunk_length & 1)
                .expect("WAV padding overflow");
        }

        let sample_rate = format.expect("test WAV has no fmt chunk");
        let audio = audio.expect("test WAV has no data chunk");
        assert!(!audio.is_empty(), "test WAV contains no samples");
        assert_eq!(audio.len() % 2, 0, "test WAV has a partial sample");
        if sample_rate == AUDIO_SAMPLE_RATE {
            audio
        } else {
            resample_s16le_mono(&audio, sample_rate, AUDIO_SAMPLE_RATE)
        }
    }

    fn resample_s16le_mono(input: &[u8], source_rate: u32, target_rate: u32) -> Vec<u8> {
        assert!(source_rate > 0 && target_rate > 0);
        let source_samples = input.len() / 2;
        let target_samples = ((source_samples as u128 * target_rate as u128
            + source_rate as u128 / 2)
            / source_rate as u128) as usize;
        let mut output = Vec::with_capacity(target_samples * 2);
        for target_index in 0..target_samples {
            let source_position = target_index as u128 * source_rate as u128 / target_rate as u128;
            let source_index = (source_position as usize).min(source_samples - 1);
            let fraction = target_index as u128 * source_rate as u128 % target_rate as u128;
            let first = i16::from_le_bytes(
                input[source_index * 2..source_index * 2 + 2]
                    .try_into()
                    .unwrap(),
            ) as i32;
            let second = if source_index + 1 < source_samples {
                i16::from_le_bytes(
                    input[(source_index + 1) * 2..(source_index + 1) * 2 + 2]
                        .try_into()
                        .unwrap(),
                ) as i32
            } else {
                first
            };
            let sample =
                first + ((second - first) as i128 * fraction as i128 / target_rate as i128) as i32;
            output.extend_from_slice(
                &(sample.clamp(i16::MIN as i32, i16::MAX as i32) as i16).to_le_bytes(),
            );
        }
        output
    }
}
