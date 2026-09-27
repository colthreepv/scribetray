//! Per-user Windows startup registration.

use std::mem::size_of;

use thiserror::Error;
use windows::{
    Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
    },
    core::{Error as WindowsError, PCWSTR, w},
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("Scribetray");

/// Changes the current user's Scribetray startup entry.
pub fn set_enabled(enabled: bool) -> Result<(), AutostartError> {
    if enabled {
        set_command(current_command()?)
    } else {
        remove_command()
    }
}

fn current_command() -> Result<Vec<u16>, AutostartError> {
    let executable = std::env::current_exe().map_err(AutostartError::CurrentExecutable)?;
    let quoted = format!("\"{}\"", executable.display());
    Ok(quoted.encode_utf16().chain(std::iter::once(0)).collect())
}

fn set_command(command: Vec<u16>) -> Result<(), AutostartError> {
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    let _key = RegistryKey(key);

    let bytes = unsafe {
        std::slice::from_raw_parts(
            command.as_ptr().cast::<u8>(),
            command.len() * size_of::<u16>(),
        )
    };
    unsafe { RegSetValueExW(key, VALUE_NAME, None, REG_SZ, Some(bytes)).ok()? };
    Ok(())
}

fn remove_command() -> Result<(), AutostartError> {
    let mut key = HKEY::default();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            None,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut key,
        )
    };
    if result.0 == 2 {
        return Ok(());
    }
    result.ok()?;
    let _key = RegistryKey(key);

    let result = unsafe { RegDeleteValueW(key, VALUE_NAME) };
    if result.0 == 2 {
        return Ok(());
    }
    result.ok()?;
    Ok(())
}

struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

#[derive(Debug, Error)]
pub enum AutostartError {
    #[error("could not locate the Scribetray executable: {0}")]
    CurrentExecutable(#[source] std::io::Error),
    #[error("Windows could not update Scribetray startup settings: {0}")]
    Windows(#[from] WindowsError),
}
