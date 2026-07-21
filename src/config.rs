use hashbrown::HashMap;
use serde::Deserialize;

use crate::secret::Secret;
use std::path::PathBuf;

/// Top-level configuration file.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Explicit default account name. If omitted and only one account exists, that account is the default.
    pub default_account: Option<String>,
    #[serde(default)]
    pub accounts: HashMap<String, AccountConfig>,
}

/// How the account authenticates to the IMAP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// Plain `LOGIN` with the resolved password (app password on providers
    /// that require one). The default.
    #[default]
    Password,
    /// SASL `AUTHENTICATE XOAUTH2` with an OAuth 2.0 access token. The
    /// `password` secret then yields the ACCESS TOKEN, not a password — use
    /// `password.cmd` pointing at a token helper (or the embedding app's
    /// credential injection) so refresh happens outside agentmail. Tokens
    /// expire (~1h); a stale token fails auth exactly like a bad password
    /// and the caller re-resolves the secret on the next connect.
    Xoauth2,
}

/// Configuration for a single IMAP account.
#[derive(Debug, Clone, Deserialize)]
pub struct AccountConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    /// Password secret: `password.raw = "..."`, `password.cmd = "..."`,
    /// or `password.keyring = "..."`. Legacy `password = "..."` is also
    /// accepted. Under `auth = "xoauth2"` this yields the OAuth access token.
    #[serde(default, deserialize_with = "deserialize_password_opt")]
    pub password: Option<Secret>,
    #[serde(default = "default_tls")]
    pub tls: bool,
    /// Max concurrent IMAP connections for this account (default: 3).
    #[serde(default)]
    pub max_connections: Option<usize>,
    /// Authentication method: `"password"` (default) or `"xoauth2"`.
    #[serde(default)]
    pub auth: AuthMethod,
}

impl AccountConfig {
    /// Create an account config programmatically (for in-process use).
    /// Password is resolved via keyring using the username.
    pub fn new(host: impl Into<String>, username: impl Into<String>) -> Self {
        let username = username.into();
        let password = Some(Secret::new_keyring(format!("mail.{}", username)));
        Self {
            host: host.into(),
            port: 993,
            username,
            password,
            tls: true,
            max_connections: None,
            auth: AuthMethod::Password,
        }
    }

    /// Set the IMAP port.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set max concurrent IMAP connections.
    pub fn with_max_connections(mut self, n: usize) -> Self {
        self.max_connections = Some(n);
        self
    }

    /// Set the authentication method (default: password `LOGIN`).
    pub fn with_auth(mut self, auth: AuthMethod) -> Self {
        self.auth = auth;
        self
    }
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
    /// Load config from the default path or `AGENTMAIL_CONFIG` env override.
    pub fn load() -> crate::Result<Self> {
        let path = Self::default_path();
        Self::load_from(&path)
    }

    /// Load from a specific path.
    pub fn load_from(path: &std::path::Path) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            crate::AgentmailError::Config(format!(
                "Failed to read config file '{}': {}. \
                 Create it with your IMAP account settings. See README for format.",
                path.display(),
                e
            ))
        })?;
        let config: Config = toml::from_str(&content).map_err(|e| {
            crate::AgentmailError::Config(format!(
                "Failed to parse config file '{}': {}",
                path.display(),
                e
            ))
        })?;
        if config.accounts.is_empty() {
            return Err(crate::AgentmailError::Config(
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

    /// Build config from a list of account configs (no file).
    /// Used by in-process MCP when accounts come from the host app.
    pub fn from_accounts(accounts: Vec<(String, AccountConfig)>) -> Self {
        Self {
            default_account: None,
            accounts: accounts.into_iter().collect(),
        }
    }

    /// Build an empty config with no accounts.
    pub fn empty() -> Self {
        Self {
            default_account: None,
            accounts: HashMap::new(),
        }
    }

    /// Default config path: `$AGENTMAIL_CONFIG` or `~/.config/agentmail/config.toml`.
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("AGENTMAIL_CONFIG") {
            return PathBuf::from(p);
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("agentmail")
            .join("config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `auth` parses from TOML, defaults to password, and rejects unknown
    /// methods — the config contract for XOAUTH2 accounts.
    #[test]
    fn auth_method_parses_defaults_and_rejects_unknowns() {
        let toml = r#"
            [accounts.gmail]
            host = "imap.gmail.com"
            username = "you@gmail.com"
            auth = "xoauth2"
            password.cmd = "oauth-helper --email you@gmail.com"

            [accounts.legacy]
            host = "imap.example.com"
            username = "you@example.com"
            password.raw = "app-password"
        "#;
        let config: Config = ::toml::from_str(toml).expect("valid config");
        assert_eq!(config.accounts["gmail"].auth, AuthMethod::Xoauth2);
        assert_eq!(
            config.accounts["legacy"].auth,
            AuthMethod::Password,
            "auth defaults to password when omitted"
        );

        let bad = r#"
            [accounts.bad]
            host = "imap.example.com"
            username = "you@example.com"
            auth = "oauth1"
        "#;
        assert!(
            ::toml::from_str::<Config>(bad).is_err(),
            "unknown auth methods must be rejected, not silently ignored"
        );
    }
}
