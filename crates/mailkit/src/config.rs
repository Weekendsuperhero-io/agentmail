use secret::Secret;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Top-level configuration file.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Explicit default account name. If omitted and only one account exists, that account is the default.
    pub default_account: Option<String>,
    #[serde(default)]
    pub accounts: HashMap<String, AccountConfig>,
}

/// Configuration for a single IMAP account.
#[derive(Debug, Clone, Deserialize)]
pub struct AccountConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    /// Password secret: `password.raw = "..."`, `password.cmd = "..."`,
    /// or `password.keyring = "..."`. Legacy `password = "..."` is also accepted.
    #[serde(default, deserialize_with = "deserialize_password_opt")]
    pub password: Option<Secret>,
    #[serde(default = "default_tls")]
    pub tls: bool,
    /// Trash mailbox name override (default: auto-detect or "Trash").
    pub trash_mailbox: Option<String>,
    /// Drafts mailbox name override (default: auto-detect or "Drafts").
    pub drafts_mailbox: Option<String>,
}

fn default_port() -> u16 {
    993
}
fn default_tls() -> bool {
    true
}

/// Deserialize password from either a Secret table or a plain string (backward compat).
///
/// New format (table):
///   password.raw = "hunter2"
///   password.cmd = "security find-internet-password ..."
///   password.keyring = "you@gmail.com"
///
/// Legacy format (plain string):
///   password = "hunter2"  →  treated as Secret::Raw("hunter2")
fn deserialize_password_opt<'de, D>(deserializer: D) -> Result<Option<Secret>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum PasswordField {
        Secret(Secret),
        Plain(String),
    }

    let opt = Option::<PasswordField>::deserialize(deserializer)?;
    Ok(opt.map(|pf| match pf {
        PasswordField::Secret(s) => s,
        PasswordField::Plain(s) => Secret::new_raw(s),
    }))
}

impl Config {
    /// Load config from the default path or `MAILKIT_CONFIG` env override.
    pub fn load() -> crate::Result<Self> {
        let path = Self::default_path();
        Self::load_from(&path)
    }

    /// Load from a specific path.
    pub fn load_from(path: &std::path::Path) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            crate::MailkitError::Config(format!(
                "Failed to read config file '{}': {}. \
                 Create it with your IMAP account settings. See README for format.",
                path.display(),
                e
            ))
        })?;
        let config: Config = toml::from_str(&content).map_err(|e| {
            crate::MailkitError::Config(format!(
                "Failed to parse config file '{}': {}",
                path.display(),
                e
            ))
        })?;
        if config.accounts.is_empty() {
            return Err(crate::MailkitError::Config(
                "No accounts configured. Add at least one [accounts.<name>] section.".into(),
            ));
        }
        Ok(config)
    }

    /// Returns the default account name: explicit `default_account` if set,
    /// or the sole account name if only one account is configured.
    pub fn default_account(&self) -> Option<&str> {
        if let Some(ref name) = self.default_account
            && self.accounts.contains_key(name)
        {
            return Some(name);
        }
        if self.accounts.len() == 1 {
            return self.accounts.keys().next().map(|s| s.as_str());
        }
        None
    }

    /// Default config path: `$MAILKIT_CONFIG` or `~/.config/mailkit/config.toml`.
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("MAILKIT_CONFIG") {
            return PathBuf::from(p);
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("mailkit")
            .join("config.toml")
    }
}
