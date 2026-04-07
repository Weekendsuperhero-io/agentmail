use crate::config::AccountConfig;
use crate::secret::Secret;

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
    if let Ok(pw) = default_secret.get().await {
        return Ok(pw);
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
