//! Secret type for password/credential storage.
//!
//! Replaces the `secret::Secret` type from the `secret-lib` crate.
//! Supports three storage backends:
//! - `Raw` — plaintext value (for testing/config)
//! - `Keyring` — OS keyring entry (macOS Keychain, etc.)
//! - `Command` — shell command whose stdout is the password

use std::sync::OnceLock;

use serde::Deserialize;
use thiserror::Error;

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

/// Typed error returned by [`Secret`] operations.
///
/// Callers (e.g. the CLI in `main.rs`) print the `Display` form, which carries
/// remediation hints for the common macOS launch-context failures.
#[derive(Debug, Error)]
pub enum SecretError {
    #[error(
        "no default keychain configured for this user (errSecNoDefaultKeychain / -25307). \
         Fix: run `security default-keychain -s login.keychain-db`, \
         or set AGENTMAIL_PASSWORD_<ACCOUNT> as a fallback."
    )]
    NoDefaultKeychain,

    #[error(
        "keychain not accessible in this context (errSecInteractionNotAllowed / -25308). \
         You're likely running headless (launchd, SSH, CI) where the keychain can't prompt. \
         Set AGENTMAIL_PASSWORD_<ACCOUNT> as a fallback."
    )]
    InteractionNotAllowed,

    #[error(
        "keychain entry needs an entitlement this binary doesn't have (errSecMissingEntitlement / -34018). \
         The data-protection keychain requires a signed binary with a stable team identifier. \
         Use a file-based keyring entry or set AGENTMAIL_PASSWORD_<ACCOUNT>."
    )]
    MissingEntitlement,

    #[error(
        "no default keyring store has been installed in this process \
         (neither data-protection nor file-based backend could be opened)"
    )]
    NoDefaultStore,

    #[error("keyring backend error: {0}")]
    Backend(String),

    #[error("setting a command-based secret is not supported")]
    CommandNotWritable,

    #[error("command failed ({status}): {stderr}")]
    CommandFailed { status: String, stderr: String },

    #[error("command I/O error: {0}")]
    CommandIo(String),

    #[error("internal task error: {0}")]
    Internal(String),
}

/// Translate a `keyring_core::Error` into our typed `SecretError`.
///
/// `keyring-core` surfaces platform error codes via `PlatformFailure`/`NoStorageAccess`
/// with an opaque `Box<dyn Error>`. We grep the `Display` for known macOS codes
/// (and their textual messages, since the user's locale affects which surface).
pub(crate) fn map_keyring_error(err: keyring_core::error::Error) -> SecretError {
    use keyring_core::error::Error as KErr;

    match err {
        KErr::NoDefaultStore => SecretError::NoDefaultStore,
        KErr::PlatformFailure(ref inner) | KErr::NoStorageAccess(ref inner) => {
            classify_platform_message(&inner.to_string())
                .unwrap_or_else(|| SecretError::Backend(err.to_string()))
        }
        other => SecretError::Backend(other.to_string()),
    }
}

/// Classify a stringified platform error from `keyring-core`/`security-framework`.
///
/// Public to the crate so unit tests can exercise it without constructing real
/// `keyring-core` errors (their `PlatformError` field is `non_exhaustive`).
pub(crate) fn classify_platform_message(msg: &str) -> Option<SecretError> {
    let lower = msg.to_lowercase();
    if msg.contains("-25307") || lower.contains("no default keychain") {
        Some(SecretError::NoDefaultKeychain)
    } else if msg.contains("-25308") || lower.contains("interaction is not allowed") {
        Some(SecretError::InteractionNotAllowed)
    } else if msg.contains("-34018") || lower.contains("missing entitlement") {
        Some(SecretError::MissingEntitlement)
    } else {
        None
    }
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
    pub async fn get(&self) -> Result<String, SecretError> {
        match self {
            Secret::Raw(v) => Ok(v.clone()),
            Secret::Keyring(key) => {
                let service = service_name().to_string();
                let key = key.clone();
                tokio::task::spawn_blocking(move || {
                    let entry =
                        keyring_core::Entry::new(&service, &key).map_err(map_keyring_error)?;
                    entry.get_password().map_err(map_keyring_error)
                })
                .await
                .map_err(|e| SecretError::Internal(e.to_string()))?
            }
            Secret::Command(cmd) => {
                let output = tokio::process::Command::new("sh")
                    .args(["-c", cmd])
                    .output()
                    .await
                    .map_err(|e| SecretError::CommandIo(e.to_string()))?;
                if !output.status.success() {
                    return Err(SecretError::CommandFailed {
                        status: output.status.to_string(),
                        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                    });
                }
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            }
        }
    }

    /// Store a value into this secret's backend.
    pub async fn set(&mut self, value: &str) -> Result<(), SecretError> {
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
                    let entry =
                        keyring_core::Entry::new(&service, &key).map_err(map_keyring_error)?;
                    entry.set_password(&value).map_err(map_keyring_error)
                })
                .await
                .map_err(|e| SecretError::Internal(e.to_string()))?
            }
            Secret::Command(_) => Err(SecretError::CommandNotWritable),
        }
    }

    /// Delete this secret from its backend.
    pub async fn delete(&mut self) -> Result<(), SecretError> {
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
                .map_err(|e| SecretError::Internal(e.to_string()))?
            }
            Secret::Command(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- Raw backend -----

    #[tokio::test]
    async fn raw_get_returns_value() {
        let s = Secret::new_raw("hunter2");
        assert_eq!(s.get().await.unwrap(), "hunter2");
    }

    #[tokio::test]
    async fn raw_set_updates_value() {
        let mut s = Secret::new_raw("old");
        s.set("new").await.unwrap();
        assert_eq!(s.get().await.unwrap(), "new");
    }

    #[tokio::test]
    async fn raw_delete_clears() {
        let mut s = Secret::new_raw("hunter2");
        s.delete().await.unwrap();
        assert_eq!(s.get().await.unwrap(), "");
    }

    // ----- Command backend -----

    #[tokio::test]
    async fn command_get_runs_shell() {
        let s = Secret::Command("printf hunter2".to_string());
        assert_eq!(s.get().await.unwrap(), "hunter2");
    }

    #[tokio::test]
    async fn command_set_errors() {
        let mut s = Secret::Command("echo".to_string());
        let err = s.set("anything").await.unwrap_err();
        assert!(matches!(err, SecretError::CommandNotWritable));
    }

    #[tokio::test]
    async fn command_failure_surfaces_stderr() {
        let s = Secret::Command("echo boom 1>&2; exit 1".to_string());
        let err = s.get().await.unwrap_err();
        match err {
            SecretError::CommandFailed { stderr, .. } => assert!(stderr.contains("boom")),
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    // ----- Error mapping (pure function, no store needed) -----

    #[test]
    fn classify_no_default_keychain_by_code() {
        let mapped = classify_platform_message("OSStatus error -25307");
        assert!(matches!(mapped, Some(SecretError::NoDefaultKeychain)));
    }

    #[test]
    fn classify_no_default_keychain_by_message() {
        let mapped = classify_platform_message("No default keychain could be found.");
        assert!(matches!(mapped, Some(SecretError::NoDefaultKeychain)));
    }

    #[test]
    fn classify_interaction_not_allowed_by_code() {
        let mapped = classify_platform_message("error code -25308");
        assert!(matches!(mapped, Some(SecretError::InteractionNotAllowed)));
    }

    #[test]
    fn classify_missing_entitlement_by_code() {
        let mapped = classify_platform_message("OSStatus error -34018");
        assert!(matches!(mapped, Some(SecretError::MissingEntitlement)));
    }

    #[test]
    fn classify_unknown_returns_none() {
        assert!(classify_platform_message("some unrelated error").is_none());
    }

    // ----- Keyring backend roundtrip via mock store -----
    //
    // `keyring_core::set_default_store` is process-global. Nextest runs each
    // test in its own process by default, so this won't leak into the other
    // tests. Under `cargo test` (fallback), this test still works in isolation
    // because no other test in this module installs a default store.

    #[tokio::test]
    async fn keyring_roundtrip_with_mock_store() {
        keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());

        let mut s = Secret::new_keyring("agentmail.test.roundtrip");
        s.set("hunter2").await.unwrap();
        assert_eq!(s.get().await.unwrap(), "hunter2");
        s.delete().await.unwrap();
        // After delete, get should fail with a backend error (NoEntry).
        assert!(s.get().await.is_err());
    }
}
