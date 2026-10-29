//! Provider selection and the batch transcription boundary.
use crate::{
    config::{Config, Provider},
    openai,
    scribe::{ScribeClient, Transcription},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountState {
    pub ready: bool,
    pub header: Option<String>,
    pub detail: Option<String>,
    pub unavailable_reason: &'static str,
}

pub fn account_state(config: &Config) -> AccountState {
    match config.provider {
        Provider::Elevenlabs => AccountState {
            ready: config.resolved_api_key().is_some(),
            header: None,
            detail: None,
            unavailable_reason: "Set an API key in settings",
        },
        Provider::Openai => openai_account_state(openai::session_status()),
    }
}

pub fn openai_account_state(status: openai::SessionStatus) -> AccountState {
    match status {
        openai::SessionStatus::Ready { email, plan } => AccountState {
            ready: true,
            header: Some(match plan.filter(|p| !p.trim().is_empty()) {
                Some(plan) => format!("OpenAI · {}", display_plan(&plan)),
                None => "OpenAI".to_owned(),
            }),
            detail: email.filter(|email| !email.trim().is_empty()),
            unavailable_reason: "Open Codex to sign in",
        },
        openai::SessionStatus::Missing => AccountState {
            ready: false,
            header: Some("OpenAI".to_owned()),
            detail: Some("Open Codex to sign in".to_owned()),
            unavailable_reason: "Open Codex to sign in",
        },
        openai::SessionStatus::Expired => AccountState {
            ready: false,
            header: Some("OpenAI".to_owned()),
            detail: Some("Session expired · open Codex".to_owned()),
            unavailable_reason: "Session expired · open Codex",
        },
        openai::SessionStatus::Unavailable(_) => AccountState {
            ready: false,
            header: Some("OpenAI".to_owned()),
            detail: Some("Session unavailable · open Codex".to_owned()),
            unavailable_reason: "Session unavailable · open Codex",
        },
    }
}

fn display_plan(plan: &str) -> String {
    let mut chars = plan.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

pub fn transcribe(
    config: &Config,
    pcm: &[u8],
    language: Option<&str>,
) -> Result<Transcription, String> {
    match config.provider {
        Provider::Elevenlabs => {
            let key = config
                .resolved_api_key()
                .ok_or_else(|| "ElevenLabs API key is not configured".to_owned())?;
            ScribeClient::new()
                .and_then(|client| client.transcribe(&key, pcm, &config.model, language))
                .map_err(|error| error.to_string())
        }
        Provider::Openai => openai::transcribe(pcm, language).map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_display_and_readiness_follow_session() {
        let ready = openai_account_state(openai::SessionStatus::Ready {
            email: Some("user@example.com".into()),
            plan: Some("plus".into()),
        });
        assert!(ready.ready);
        assert_eq!(ready.header.as_deref(), Some("OpenAI · Plus"));
        assert_eq!(ready.detail.as_deref(), Some("user@example.com"));
        assert!(!openai_account_state(openai::SessionStatus::Missing).ready);
        assert!(!openai_account_state(openai::SessionStatus::Expired).ready);
        assert!(!openai_account_state(openai::SessionStatus::Unavailable("private".into())).ready);
    }
}
