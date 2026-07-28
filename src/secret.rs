//! Secret type for password/credential storage.
//!
//! Replaces the `secret::Secret` type from the `secret-lib` crate.
//! Supports three storage backends:
//! - `Raw` — plaintext value (for testing/config)
//! - `Keyring` — OS keyring entry (macOS Keychain, etc.)
//! - `Command` — shell command whose stdout is the password

use std::fmt;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Keyring service name. Set at app startup. Falls back to `"agentmail"` for standalone use.
static SERVICE_NAME: OnceLock<String> = OnceLock::new();

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;

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

    #[error(
        "credential command failed ({status}); stderr was withheld because credential helpers may emit secret material"
    )]
    CommandFailed { status: String },

    #[error("credential command timed out after {seconds}s")]
    CommandTimedOut { seconds: u64 },

    #[error("credential command output exceeded the {limit_bytes}-byte safety limit")]
    CommandOutputTooLarge { limit_bytes: usize },

    #[error("credential command output was not valid UTF-8")]
    CommandOutputNotUtf8,

    #[error("credential command returned an empty secret")]
    CommandOutputEmpty,

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
#[derive(Clone, Deserialize)]
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

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Raw(_) => formatter.write_str("Raw([REDACTED])"),
            Self::Keyring(_) => formatter.write_str("Keyring([REDACTED])"),
            Self::Command(_) => formatter.write_str("Command([REDACTED])"),
        }
    }
}

#[derive(Debug)]
enum CommandRunError {
    Io(std::io::Error),
    OutputTooLarge,
}

impl From<std::io::Error> for CommandRunError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

async fn read_bounded<R>(reader: R, limit: usize) -> Result<Vec<u8>, CommandRunError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    reader
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        return Err(CommandRunError::OutputTooLarge);
    }
    Ok(bytes)
}

async fn command_secret_with_limits(
    command: &str,
    timeout: Duration,
    output_limit: usize,
) -> Result<String, SecretError> {
    let mut child = tokio::process::Command::new("sh")
        .args(["-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| SecretError::CommandIo(error.to_string()))?;
    let stdout = child.stdout.take().ok_or_else(|| {
        SecretError::Internal("credential command stdout pipe was unavailable".to_string())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        SecretError::Internal("credential command stderr pipe was unavailable".to_string())
    })?;

    let execution = async {
        tokio::try_join!(
            async { child.wait().await.map_err(CommandRunError::from) },
            read_bounded(stdout, output_limit),
            read_bounded(stderr, output_limit),
        )
    };

    let outcome = tokio::time::timeout(timeout, execution).await;
    let (status, stdout, _stderr) = match outcome {
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(SecretError::CommandTimedOut {
                seconds: timeout.as_secs(),
            });
        }
        Ok(Err(CommandRunError::OutputTooLarge)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(SecretError::CommandOutputTooLarge {
                limit_bytes: output_limit,
            });
        }
        Ok(Err(CommandRunError::Io(error))) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(SecretError::CommandIo(error.to_string()));
        }
        Ok(Ok(output)) => output,
    };

    if !status.success() {
        return Err(SecretError::CommandFailed {
            status: status.to_string(),
        });
    }
    let secret = String::from_utf8(stdout).map_err(|_| SecretError::CommandOutputNotUtf8)?;
    let secret = secret.trim().to_string();
    if secret.is_empty() {
        return Err(SecretError::CommandOutputEmpty);
    }
    Ok(secret)
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
                command_secret_with_limits(cmd, COMMAND_TIMEOUT, MAX_COMMAND_OUTPUT_BYTES).await
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
    async fn command_failure_withholds_stderr() {
        let s = Secret::Command("echo boom 1>&2; exit 1".to_string());
        let err = s.get().await.unwrap_err();
        assert!(matches!(err, SecretError::CommandFailed { .. }));
        assert!(!err.to_string().contains("boom"));
    }

    #[test]
    fn secret_debug_never_exposes_secret_material() {
        let values = [
            Secret::new_raw("hunter2"),
            Secret::new_keyring("private@example.com"),
            Secret::Command("printf super-secret-token".to_string()),
        ];

        let rendered = format!("{values:?}");

        assert_eq!(rendered.matches("[REDACTED]").count(), 3);
        assert!(!rendered.contains("hunter2"));
        assert!(!rendered.contains("private@example.com"));
        assert!(!rendered.contains("super-secret-token"));
    }

    #[tokio::test]
    async fn command_timeout_terminates_the_credential_process() {
        let error = command_secret_with_limits(
            "sleep 5",
            Duration::from_millis(25),
            MAX_COMMAND_OUTPUT_BYTES,
        )
        .await
        .expect_err("slow command must time out");

        assert!(matches!(error, SecretError::CommandTimedOut { .. }));
    }

    #[tokio::test]
    async fn command_output_is_bounded() {
        let error = command_secret_with_limits("printf 123456789", Duration::from_secs(1), 4)
            .await
            .expect_err("oversized output must fail");

        assert!(matches!(
            error,
            SecretError::CommandOutputTooLarge { limit_bytes: 4 }
        ));
    }

    #[tokio::test]
    async fn command_empty_output_is_rejected() {
        let error = command_secret_with_limits("true", Duration::from_secs(1), 4)
            .await
            .expect_err("empty output must fail");

        assert!(matches!(error, SecretError::CommandOutputEmpty));
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
