//! Cached ElevenLabs subscription usage for the tray menu.

use std::time::Duration;

use serde::Deserialize;
use windows::{
    Win32::{
        Foundation::{FILETIME, SYSTEMTIME},
        Globalization::{DATE_MONTHDAY, GetDateFormatEx},
        System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
    },
    core::PCWSTR,
};

const ENDPOINT: &str = "https://api.elevenlabs.io/v1/user/subscription";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const WINDOWS_EPOCH_OFFSET_100NS: u64 = 116_444_736_000_000_000;

#[derive(Debug)]
pub enum UsageError {
    PermissionDenied,
    HttpStatus(u16),
    RequestFailed,
    InvalidResponse,
    ClientInitialization,
}

/// Raw usage information needed to render the tray's usage header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub used: u64,
    pub limit: u64,
    pub reset_unix: Option<i64>,
    pub overage: Option<String>,
}

#[derive(Deserialize)]
struct SubscriptionResponse {
    character_count: u64,
    character_limit: u64,
    #[serde(default)]
    next_character_count_reset_unix: Option<i64>,
    #[serde(default)]
    current_overage: Option<CurrentOverage>,
}

#[derive(Deserialize)]
struct CurrentOverage {
    amount: String,
}

/// Retrieves usage without exposing API-key material in errors.
pub fn fetch_usage(api_key: &str) -> Result<UsageSnapshot, UsageError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| UsageError::ClientInitialization)?;
    let response = client
        .get(ENDPOINT)
        .header("xi-api-key", api_key)
        .send()
        .map_err(|_| UsageError::RequestFailed)?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(UsageError::PermissionDenied);
    }
    if !status.is_success() {
        return Err(UsageError::HttpStatus(status.as_u16()));
    }
    let subscription = response
        .json::<SubscriptionResponse>()
        .map_err(|_| UsageError::InvalidResponse)?;

    let overage = subscription.current_overage.and_then(|overage| {
        let amount = overage.amount.trim();
        amount
            .parse::<f64>()
            .is_ok_and(|amount| amount.is_finite() && amount != 0.0)
            .then(|| amount.to_owned())
    });
    Ok(UsageSnapshot {
        used: subscription.character_count,
        limit: subscription.character_limit,
        reset_unix: subscription.next_character_count_reset_unix,
        overage,
    })
}

pub fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let first_group = match digits.len() % 3 {
        0 => 3,
        length => length,
    };
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    grouped.push_str(&digits[..first_group]);
    for chunk in digits[first_group..].as_bytes().chunks(3) {
        grouped.push(',');
        grouped.push_str(std::str::from_utf8(chunk).expect("ASCII digits"));
    }
    grouped
}

pub fn format_reset_date(unix_seconds: i64) -> Option<String> {
    let ticks = u64::try_from(unix_seconds)
        .ok()?
        .checked_mul(10_000_000)?
        .checked_add(WINDOWS_EPOCH_OFFSET_100NS)?;
    let file_time = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    unsafe { FileTimeToSystemTime(&file_time, &mut utc) }.ok()?;
    let mut local = SYSTEMTIME::default();
    unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.ok()?;

    let mut buffer = [0_u16; 128];
    let length = unsafe {
        GetDateFormatEx(
            PCWSTR::null(),
            DATE_MONTHDAY,
            Some(&local),
            PCWSTR::null(),
            Some(&mut buffer),
            PCWSTR::null(),
        )
    };
    if length <= 1 || length as usize > buffer.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..length as usize - 1]))
}
