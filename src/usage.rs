use crate::config::{Config, Provider};
use crate::subscription;
use crate::windows_ui::{UiCommand, UiRuntime};
use std::{
    hash::{Hash, Hasher},
    sync::mpsc::Sender,
    thread,
    time::{Duration, Instant},
};
use tracing::warn;

const SUBSCRIPTION_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);

pub(crate) struct SubscriptionFinished {
    generation: u64,
    result: Result<subscription::UsageSnapshot, subscription::UsageError>,
}

#[derive(Default)]
pub(crate) struct SubscriptionCache {
    generation: u64,
    api_key_fingerprint: Option<u64>,
    last_success: Option<Instant>,
    last_attempt: Option<Instant>,
    fetching: bool,
    permission_denied: bool,
}

impl SubscriptionCache {
    pub(crate) fn reset_for_config(
        &mut self,
        config: &Config,
        ui: &UiRuntime,
        finished_tx: &Sender<SubscriptionFinished>,
    ) {
        self.generation = self.generation.wrapping_add(1);
        self.last_attempt = None;
        self.fetching = false;
        self.permission_denied = false;
        let fingerprint = (config.provider == Provider::Elevenlabs)
            .then(|| config.resolved_api_key())
            .flatten()
            .as_deref()
            .map(api_key_fingerprint);
        if fingerprint != self.api_key_fingerprint {
            self.last_success = None;
            let _ = ui.send(UiCommand::SetUsage(None));
        }
        self.api_key_fingerprint = fingerprint;
        self.refresh(config, finished_tx, true);
    }

    pub(crate) fn refresh(
        &mut self,
        config: &Config,
        finished_tx: &Sender<SubscriptionFinished>,
        force: bool,
    ) {
        if config.provider != Provider::Elevenlabs || self.fetching || self.permission_denied {
            return;
        }
        let Some(api_key) = config.resolved_api_key() else {
            return;
        };
        if !force {
            let stale = self
                .last_success
                .is_none_or(|last_success| last_success.elapsed() >= SUBSCRIPTION_REFRESH_INTERVAL);
            let recently_attempted = self
                .last_attempt
                .is_some_and(|last_attempt| last_attempt.elapsed() < SUBSCRIPTION_REFRESH_INTERVAL);
            if !stale || recently_attempted {
                return;
            }
        }

        self.fetching = true;
        self.last_attempt = Some(Instant::now());
        let generation = self.generation;
        let finished_tx = finished_tx.clone();
        thread::spawn(move || {
            let result = subscription::fetch_usage(&api_key);
            let _ = finished_tx.send(SubscriptionFinished { generation, result });
        });
    }

    pub(crate) fn apply_finished(&mut self, finished: SubscriptionFinished, ui: &UiRuntime) {
        if finished.generation != self.generation {
            return;
        }
        self.fetching = false;
        match finished.result {
            Ok(usage) => {
                self.last_success = Some(Instant::now());
                let _ = ui.send(UiCommand::SetUsage(Some(usage)));
            }
            Err(subscription::UsageError::PermissionDenied) => {
                self.permission_denied = true;
                let _ = ui.send(UiCommand::SetUsage(None));
                warn!("ElevenLabs usage line hidden: API key is missing the user_read permission");
            }
            Err(subscription::UsageError::HttpStatus(status)) => {
                warn!("could not refresh ElevenLabs usage: HTTP {status}");
            }
            Err(subscription::UsageError::RequestFailed) => {
                warn!("could not refresh ElevenLabs usage: request failed");
            }
            Err(subscription::UsageError::InvalidResponse) => {
                warn!("could not refresh ElevenLabs usage: invalid subscription response");
            }
            Err(subscription::UsageError::ClientInitialization) => {
                warn!("could not initialize the ElevenLabs usage HTTP client");
            }
        }
    }
}

fn api_key_fingerprint(api_key: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    api_key.hash(&mut hasher);
    hasher.finish()
}
