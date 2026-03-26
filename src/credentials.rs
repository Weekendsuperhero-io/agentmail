use crate::config::AccountConfig;
use secret::Secret;

/// Initialize the keyring backend with the default "agentmail" service name.
/// Call once at startup before any password operations.
pub fn init_keyring() {
    init_keyring_with_service("agentmail");
}

/// Initialize the keyring backend with a custom service name.
/// Use this when embedding agentmail in another app (e.g. agent) so credentials
/// are stored under the host app's keyring service, not "agentmail".
pub fn init_keyring_with_service(name: &'static str) {
    secret::keyring::set_global_service_name(name);
}

/// Retrieve the password for an IMAP account.
///
/// Lookup order:
/// 1. Environment variable `AGENTMAIL_PASSWORD_{ACCOUNT_NAME}` (override for CI/Docker)
/// 2. `password` field in config (Secret: raw, command, or keyring)
/// 3. Default keyring entry under "agentmail" service with username as key (backward compat)
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
    let default_entry = secret::keyring::KeyringEntry::try_new(&config.username).map_err(|e| {
        crate::AgentmailError::Credential(format!(
            "Failed to create default keyring entry for '{}': {}",
            config.username, e
        ))
    })?;
    let default_secret = Secret::new_keyring_entry(default_entry);
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
/// Otherwise, stores under the default "agentmail" service with the username as key.
pub async fn set_password(
    account_name: &str,
    config: &AccountConfig,
    password: &str,
) -> crate::Result<()> {
    let mut secret = match config.password {
        Some(ref s @ Secret::Keyring(_)) => s.clone(),
        _ => {
            let entry = secret::keyring::KeyringEntry::try_new(&config.username).map_err(|e| {
                crate::AgentmailError::Credential(format!(
                    "Failed to create keyring entry for '{}': {}",
                    config.username, e
                ))
            })?;
            Secret::new_keyring_entry(entry)
        }
    };

    secret.set(password).await.map_err(|e| {
        crate::AgentmailError::Credential(format!(
            "Failed to store password for account '{}': {}",
            account_name, e
        ))
    })?;
    Ok(())
}
