use crate::config::AccountConfig;
use crate::secret::{Secret, SecretError};

/// Retrieve the password for an IMAP account.
///
/// Lookup order:
/// 1. Environment variable `AGENTMAIL_PASSWORD_{ACCOUNT_NAME}` (override for CI/Docker)
/// 2. `password` field in config (Secret: raw, command, or keyring)
/// 3. Default keyring entry with username as key (backward compat)
pub async fn get_password(account_name: &str, config: &AccountConfig) -> crate::Result<String> {
    // 1. Environment variable override
    let env_key = format!(
        "AGENTMAIL_PASSWORD_{}",
        account_name.to_uppercase().replace(['-', ' '], "_")
    );
    if let Ok(pw) = std::env::var(&env_key) {
        return Ok(pw);
    }

    // 2. Configured secret (raw, command, or keyring)
    if let Some(ref secret) = config.password {
        let pw = secret.get().await.map_err(|e| {
            crate::AgentmailError::Credential(format!(
                "Failed to retrieve password for account '{}': {}",
                account_name, e
            ))
        })?;
        return Ok(pw);
    }

    // 3. Default keyring fallback (backward compat: handles passwords stored via set-password
    //    before the config had a password field)
    let default_secret = Secret::new_keyring(format!("mail.{}", config.username));
    match default_secret.get().await {
        Ok(pw) => return Ok(pw),
        // Nothing stored under that name: explain how to configure one, below.
        Err(SecretError::NoEntry) => {}
        // The keychain failed to answer — locked after sleep, inaccessible in
        // this context, timed out. That is not a missing password, and saying
        // so sends the user to re-enter one that exists.
        Err(error) => {
            return Err(crate::AgentmailError::Credential(format!(
                "Could not read the keychain for account '{account_name}': {error}"
            )));
        }
    }

    Err(crate::AgentmailError::Credential(format!(
        "No password found for account '{}' (user='{}').\n\
         Configure it in config.toml:\n  \
         password.keyring = \"{}\"\n  \
         password.cmd = \"security find-internet-password -s {} -a {} -w\"\n\
         Or store it: agentmail set-password --account {}",
        account_name, config.username, config.username, config.host, config.username, account_name
    )))
}

/// Store a password for an account in the system keyring.
///
/// If the config has a Keyring secret, stores into that entry.
/// Otherwise, stores under the default service with the username as key.
pub async fn set_password(
    account_name: &str,
    config: &AccountConfig,
    password: &str,
) -> crate::Result<()> {
    let mut secret = match config.password {
        Some(ref s @ Secret::Keyring(_)) => s.clone(),
        _ => Secret::new_keyring(format!("mail.{}", config.username)),
    };

    secret.set(password).await.map_err(|e| {
        crate::AgentmailError::Credential(format!(
            "Failed to store password for account '{}': {}",
            account_name, e
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An account with no `password` in its config, so its password comes from
    /// the default keychain entry `mail.<username>`.
    fn keychain_only(username: &str) -> AccountConfig {
        AccountConfig {
            password: None,
            ..AccountConfig::new("imap.example.com", username)
        }
    }

    /// A keychain that fails to answer — locked after sleep, inaccessible — is
    /// not a missing password: saying so sends the user to re-enter a password
    /// that exists.
    #[tokio::test]
    async fn a_failing_keychain_is_not_reported_as_a_missing_password() {
        crate::secret::install_mock_keyring();
        let entry =
            keyring_core::Entry::new(crate::secret::service_name(), "mail.locked@example.com")
                .expect("mock entry");
        entry
            .as_any()
            .downcast_ref::<keyring_core::mock::Cred>()
            .expect("mock credential")
            .set_error(keyring_core::Error::NoStorageAccess(
                "errSecInteractionNotAllowed -25308".into(),
            ));

        let error = get_password("locked", &keychain_only("locked@example.com"))
            .await
            .expect_err("the keychain failed")
            .to_string();

        assert!(!error.contains("No password found"), "{error}");
        assert!(
            error.contains("-25308"),
            "the keychain's own reason reaches the user: {error}"
        );
    }

    #[tokio::test]
    async fn an_absent_keychain_entry_still_explains_how_to_configure_one() {
        crate::secret::install_mock_keyring();

        let error = get_password("nobody", &keychain_only("nobody@example.com"))
            .await
            .expect_err("nothing stored")
            .to_string();

        assert!(error.contains("No password found"), "{error}");
    }
}
