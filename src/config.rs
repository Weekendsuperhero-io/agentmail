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
    /// Primary mailbox address when the IMAP login is not itself an email
    /// address. Used for own-address filtering, never for authentication.
    #[serde(default)]
    pub email: Option<String>,
    /// Additional mailbox identities used for own-address filtering.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Password secret: `password.raw = "..."`, `password.cmd = "..."`,
    /// or `password.keyring = "..."`. Legacy `password = "..."` is also
    /// accepted. Under `auth = "xoauth2"` this yields the OAuth access token.
    #[serde(default, deserialize_with = "deserialize_password_opt")]
    pub password: Option<Secret>,
    #[serde(default = "default_tls")]
    pub tls: bool,
    /// Max concurrent IMAP connections for this account. When unset, defaults
    /// per host: 1 for login-rate-limited providers (AOL/Yahoo, so concurrent
    /// work queues on one held connection instead of opening a second,
    /// rate-limited LOGIN), 3 otherwise.
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
        let username = username.into().trim().to_string();
        let password = Some(Secret::new_keyring(format!("mail.{}", username)));
        Self {
            host: normalize_host(&host.into()),
            port: 993,
            username,
            email: None,
            aliases: Vec::new(),
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

    /// Set the primary mailbox address independently of the login username.
    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    /// Set additional mailbox identities used for own-address filtering.
    pub fn with_aliases<I, S>(mut self, aliases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.aliases = aliases.into_iter().map(Into::into).collect();
        self
    }

    /// Return the configured primary address, falling back to an email-shaped
    /// login username.
    ///
    /// IMAP usernames are not required to be email addresses (iCloud is a
    /// common counterexample), so callers must handle `None`.
    pub fn canonical_email(&self) -> Option<String> {
        self.email
            .as_deref()
            .and_then(canonicalize_email)
            .or_else(|| canonicalize_email(&self.username))
    }

    /// Return every configured mailbox identity in canonical, deduplicated
    /// form. The primary identity is first, followed by an email-shaped login
    /// (when distinct) and then aliases.
    pub fn canonical_addresses(&self) -> Vec<String> {
        let mut addresses = Vec::with_capacity(self.aliases.len().saturating_add(2));
        if let Some(primary) = self.canonical_email() {
            addresses.push(primary);
        }
        if let Some(login) = canonicalize_email(&self.username)
            && !addresses.contains(&login)
        {
            addresses.push(login);
        }
        for alias in &self.aliases {
            if let Some(alias) = canonicalize_email(alias)
                && !addresses.contains(&alias)
            {
                addresses.push(alias);
            }
        }
        addresses
    }

    fn normalize(&mut self) {
        self.host = normalize_host(&self.host);
        self.username = self.username.trim().to_string();
        if let Some(email) = &mut self.email {
            let trimmed = email.trim();
            *email = canonicalize_email(trimmed).unwrap_or_else(|| trimmed.to_string());
        }
        for alias in &mut self.aliases {
            let trimmed = alias.trim();
            *alias = canonicalize_email(trimmed).unwrap_or_else(|| trimmed.to_string());
        }
        let primary = self.canonical_email();
        let login = canonicalize_email(&self.username);
        let mut seen = hashbrown::HashSet::with_capacity(self.aliases.len());
        self.aliases.retain(|alias| {
            primary.as_ref() != Some(alias)
                && login.as_ref() != Some(alias)
                && seen.insert(alias.clone())
        });
    }

    fn validate(&self, account_name: &str) -> crate::Result<()> {
        if self.host.is_empty() {
            return Err(config_error(account_name, "host must not be empty"));
        }
        if self.host.chars().any(char::is_whitespace)
            || self.host.contains(['\r', '\n', '\0', '/', '\\'])
        {
            return Err(config_error(
                account_name,
                "host must be a hostname or IP address without whitespace or path separators",
            ));
        }
        if self.username.is_empty() {
            return Err(config_error(account_name, "username must not be empty"));
        }
        if self.username.chars().any(char::is_control) {
            return Err(config_error(
                account_name,
                "username must not contain control characters",
            ));
        }
        if let Some(email) = self.email.as_deref()
            && canonicalize_email(email).is_none()
        {
            return Err(config_error(
                account_name,
                "email must be a valid bare email address",
            ));
        }
        if self
            .aliases
            .iter()
            .any(|alias| canonicalize_email(alias).is_none())
        {
            return Err(config_error(
                account_name,
                "aliases must contain only valid bare email addresses",
            ));
        }
        if self.port == 0 {
            return Err(config_error(
                account_name,
                "port must be between 1 and 65535",
            ));
        }
        if !self.tls {
            return Err(config_error(
                account_name,
                "tls=false is not supported; AgentMail refuses plaintext IMAP credentials",
            ));
        }
        if let Some(max_connections) = self.max_connections
            && !(1..=32).contains(&max_connections)
        {
            return Err(config_error(
                account_name,
                "max_connections must be between 1 and 32",
            ));
        }
        Ok(())
    }
}

fn config_error(account_name: &str, message: &str) -> crate::AgentmailError {
    crate::AgentmailError::Config(format!("account '{account_name}': {message}"))
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Canonicalize an email-shaped account identity for equality comparisons.
///
/// AgentMail's parsed sender addresses are lowercased, so account identities
/// use the same representation. Login usernames themselves are left
/// case-preserving because some IMAP servers treat them as opaque strings.
pub fn canonicalize_email(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.contains(['<', '>', '\r', '\n', '\0'])
        || value.chars().any(char::is_whitespace)
    {
        return None;
    }
    let (local, domain) = value.rsplit_once('@')?;
    if local.is_empty() || domain.is_empty() || local.contains('@') {
        return None;
    }
    let domain = crate::domain::canonicalize_domain(domain)?;
    Some(format!("{}@{}", local.to_lowercase(), domain))
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
        let mut config: Config = toml::from_str(&content).map_err(|e| {
            crate::AgentmailError::Config(format!(
                "Failed to parse config file '{}': {}",
                path.display(),
                e.message()
            ))
        })?;
        config.normalize_and_validate()?;
        Ok(config)
    }

    /// Validate configuration invariants without changing the configuration.
    pub fn validate(&self) -> crate::Result<()> {
        if self.accounts.is_empty() {
            return Err(crate::AgentmailError::Config(
                "No accounts configured. Add at least one [accounts.<name>] section.".into(),
            ));
        }
        for (name, account) in &self.accounts {
            if name.trim().is_empty() {
                return Err(crate::AgentmailError::Config(
                    "account names must not be empty".into(),
                ));
            }
            if name.trim() != name {
                return Err(crate::AgentmailError::Config(format!(
                    "account name '{name}' must not have leading or trailing whitespace"
                )));
            }
            if name.chars().any(char::is_control) {
                return Err(crate::AgentmailError::Config(format!(
                    "account name '{name}' must not contain control characters"
                )));
            }
            account.validate(name)?;
        }
        if let Some(default_account) = self.default_account.as_deref()
            && !self.accounts.contains_key(default_account)
        {
            return Err(crate::AgentmailError::Config(format!(
                "default_account '{default_account}' does not name a configured account"
            )));
        }
        Ok(())
    }

    /// Normalize safe textual forms, then validate the complete configuration.
    pub fn normalize_and_validate(&mut self) -> crate::Result<()> {
        if let Some(default_account) = &mut self.default_account {
            *default_account = default_account.trim().to_string();
        }
        for account in self.accounts.values_mut() {
            account.normalize();
        }
        self.validate()
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
    /// Used by in-process MCP when accounts come from the host app. This
    /// compatibility constructor is infallible; new callers should prefer
    /// [`Self::try_from_accounts`] so invalid input is rejected immediately.
    pub fn from_accounts(accounts: Vec<(String, AccountConfig)>) -> Self {
        let accounts = accounts
            .into_iter()
            .map(|(name, mut account)| {
                account.normalize();
                (name, account)
            })
            .collect();
        Self {
            default_account: None,
            accounts,
        }
    }

    /// Build and validate programmatically supplied account configuration.
    pub fn try_from_accounts(accounts: Vec<(String, AccountConfig)>) -> crate::Result<Self> {
        let mut normalized = HashMap::with_capacity(accounts.len());
        for (name, mut account) in accounts {
            account.normalize();
            if normalized.insert(name.clone(), account).is_some() {
                return Err(crate::AgentmailError::Config(format!(
                    "duplicate account name '{name}'"
                )));
            }
        }
        let config = Self {
            default_account: None,
            accounts: normalized,
        };
        config.validate()?;
        Ok(config)
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

    #[test]
    fn normalization_trims_identity_and_canonicalizes_host() {
        let mut config: Config = ::toml::from_str(
            r#"
                [accounts.work]
                host = " IMAP.Example.COM. "
                username = " Me@Example.COM "
            "#,
        )
        .expect("valid TOML");

        config.normalize_and_validate().expect("valid config");

        assert_eq!(config.accounts["work"].host, "imap.example.com");
        assert_eq!(
            config.accounts["work"].canonical_email().as_deref(),
            Some("me@example.com")
        );
    }

    #[test]
    fn validation_rejects_insecure_or_invalid_transport_settings() {
        for (field, expected) in [
            ("port = 0", "port must be between"),
            ("tls = false", "refuses plaintext IMAP"),
            ("max_connections = 0", "max_connections must be"),
            ("max_connections = 33", "max_connections must be"),
        ] {
            let input = format!(
                "[accounts.bad]\nhost = \"imap.example.com\"\nusername = \"me@example.com\"\n{field}\n"
            );
            let mut config: Config = ::toml::from_str(&input).expect("valid TOML shape");
            let error = config
                .normalize_and_validate()
                .expect_err("invalid account must be rejected")
                .to_string();
            assert!(error.contains(expected), "unexpected error: {error}");
        }
    }

    #[test]
    fn validation_rejects_unknown_explicit_default_account() {
        let mut config: Config = ::toml::from_str(
            r#"
                default_account = "missing"
                [accounts.work]
                host = "imap.example.com"
                username = "me@example.com"
            "#,
        )
        .expect("valid TOML");

        let error = config
            .normalize_and_validate()
            .expect_err("unknown default must fail")
            .to_string();

        assert!(error.contains("default_account 'missing'"));
    }

    #[test]
    fn canonicalize_email_rejects_non_email_login_names() {
        assert_eq!(canonicalize_email("johnappleseed"), None);
        assert_eq!(canonicalize_email("person@."), None);
        assert_eq!(
            canonicalize_email("Reader@BÜCHER.DE"),
            Some("reader@xn--bcher-kva.de".to_string())
        );
    }

    #[test]
    fn primary_email_and_aliases_are_canonicalized_and_deduplicated() {
        let mut config: Config = ::toml::from_str(
            r#"
                [accounts.icloud]
                host = "imap.mail.me.com"
                username = "johnappleseed"
                email = " Primary@Example.COM. "
                aliases = [
                    "Alias@Example.COM",
                    "alias@example.com",
                    "primary@example.com",
                ]
            "#,
        )
        .expect("valid TOML");

        config.normalize_and_validate().expect("valid config");
        let account = &config.accounts["icloud"];

        assert_eq!(account.email.as_deref(), Some("primary@example.com"));
        assert_eq!(account.aliases, ["alias@example.com"]);
        assert_eq!(
            account.canonical_addresses(),
            ["primary@example.com", "alias@example.com"]
        );
    }

    #[test]
    fn invalid_primary_email_or_alias_is_rejected() {
        for (field, expected) in [
            ("email = \"not-an-email\"", "email must be"),
            ("aliases = [\"Display <me@example.com>\"]", "aliases must"),
        ] {
            let input = format!(
                "[accounts.bad]\nhost = \"imap.example.com\"\nusername = \"login\"\n{field}\n"
            );
            let mut config: Config = ::toml::from_str(&input).expect("valid TOML shape");
            let error = config
                .normalize_and_validate()
                .expect_err("invalid address must be rejected")
                .to_string();
            assert!(error.contains(expected), "unexpected error: {error}");
        }
    }

    #[test]
    fn validated_programmatic_config_rejects_duplicate_names() {
        let error = Config::try_from_accounts(vec![
            (
                "work".to_string(),
                AccountConfig::new("imap.example.com", "one@example.com"),
            ),
            (
                "work".to_string(),
                AccountConfig::new("imap.example.com", "two@example.com"),
            ),
        ])
        .expect_err("duplicate names must not overwrite silently")
        .to_string();

        assert!(error.contains("duplicate account name 'work'"));
    }

    #[test]
    fn config_parse_errors_do_not_echo_file_contents() {
        let path = std::env::temp_dir().join(format!(
            "agentmail-invalid-config-{}.toml",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            "[accounts.work]\nhost = \"imap.example.com\"\nusername = \"me@example.com\"\npassword.raw = \"do-not-leak",
        )
        .expect("write malformed config");

        let error = Config::load_from(&path)
            .expect_err("malformed config must fail")
            .to_string();
        let _ = std::fs::remove_file(path);

        assert!(error.contains("Failed to parse config file"));
        assert!(!error.contains("do-not-leak"));
    }
}
