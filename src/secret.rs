//! Secret type for password/credential storage.
//!
//! Replaces the `secret::Secret` type from the `secret-lib` crate.
//! Supports three storage backends:
//! - `Raw` — plaintext value (for testing/config)
//! - `Keyring` — OS keyring entry (macOS Keychain, etc.)
//! - `Command` — shell command whose stdout is the password

use std::sync::OnceLock;

use serde::Deserialize;

/// Keyring service name. Set at app startup. Falls back to `"agentmail"` for standalone use.
static SERVICE_NAME: OnceLock<String> = OnceLock::new();

/// Initialize the keyring service name.
///
/// When embedded in the Agent app, this is set to the app's bundle identifier.
/// When running standalone, this defaults to `"agentmail"`.
pub fn init_service_name(name: &str) {
    SERVICE_NAME.set(name.to_string()).ok();
}

/// Returns the current keyring service name.
pub fn service_name() -> &'static str {
    SERVICE_NAME
        .get()
        .map(|s| s.as_str())
        .unwrap_or("agentmail")
}

/// A secret value that can be stored in different backends.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Secret {
    /// Plaintext value.
    Raw(String),
    /// OS keyring entry key (the service name is implicit from [`service_name()`]).
    Keyring(String),
    /// Shell command whose stdout is the secret.
    #[serde(alias = "cmd")]
    Command(String),
}

impl Secret {
    /// Create a raw (plaintext) secret.
    pub fn new_raw(value: impl Into<String>) -> Self {
        Self::Raw(value.into())
    }

    /// Create a keyring-backed secret.
    pub fn new_keyring(key: impl Into<String>) -> Self {
        Self::Keyring(key.into())
    }

    /// Retrieve the secret value.
    pub async fn get(&self) -> Result<String, String> {
        match self {
            Secret::Raw(v) => Ok(v.clone()),
            Secret::Keyring(key) => {
                let service = service_name().to_string();
                let key = key.clone();
                tokio::task::spawn_blocking(move || {
                    let entry = keyring_core::Entry::new(&service, &key)
                        .map_err(|e| format!("keyring entry error: {e}"))?;
                    entry
                        .get_password()
                        .map_err(|e| format!("keyring get_password error: {e}"))
                })
                .await
                .map_err(|e| format!("spawn_blocking error: {e}"))?
            }
            Secret::Command(cmd) => {
                let output = tokio::process::Command::new("sh")
                    .args(["-c", cmd])
                    .output()
                    .await
                    .map_err(|e| format!("command error: {e}"))?;
                if !output.status.success() {
                    return Err(format!(
                        "command failed ({}): {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            }
        }
    }

    /// Store a value into this secret's backend.
    pub async fn set(&mut self, value: &str) -> Result<(), String> {
        match self {
            Secret::Raw(v) => {
                *v = value.to_string();
                Ok(())
            }
            Secret::Keyring(key) => {
                let service = service_name().to_string();
                let key = key.clone();
                let value = value.to_string();
                tokio::task::spawn_blocking(move || {
                    let entry = keyring_core::Entry::new(&service, &key)
                        .map_err(|e| format!("keyring entry error: {e}"))?;
                    entry
                        .set_password(&value)
                        .map_err(|e| format!("keyring set_password error: {e}"))
                })
                .await
                .map_err(|e| format!("spawn_blocking error: {e}"))?
            }
            Secret::Command(_) => Err("Cannot set a command-based secret".to_string()),
        }
    }

    /// Delete this secret from its backend.
    pub async fn delete(&mut self) -> Result<(), String> {
        match self {
            Secret::Raw(v) => {
                v.clear();
                Ok(())
            }
            Secret::Keyring(key) => {
                let service = service_name().to_string();
                let key = key.clone();
                tokio::task::spawn_blocking(move || {
                    if let Ok(entry) = keyring_core::Entry::new(&service, &key) {
                        let _ = entry.delete_credential();
                    }
                    Ok(())
                })
                .await
                .map_err(|e| format!("spawn_blocking error: {e}"))?
            }
            Secret::Command(_) => Ok(()),
        }
    }
}
