use crate::config::AccountConfig;
use crate::secret::Secret;

/// Supported mail providers with pre-configured IMAP defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailProvider {
    Gmail,
    ICloud,
    Yahoo,
    Fastmail,
    Custom,
}

impl MailProvider {
    /// All known providers and their display names.
    pub fn all() -> &'static [(&'static str, MailProvider)] {
        &[
            ("Gmail", MailProvider::Gmail),
            ("iCloud", MailProvider::ICloud),
            ("Yahoo", MailProvider::Yahoo),
            ("Fastmail", MailProvider::Fastmail),
            ("Custom", MailProvider::Custom),
        ]
    }

    /// Parse a provider name (case-insensitive).
    pub fn from_name(name: &str) -> Option<MailProvider> {
        match name.to_lowercase().as_str() {
            "gmail" | "google" => Some(MailProvider::Gmail),
            "icloud" | "apple" => Some(MailProvider::ICloud),
            "yahoo" => Some(MailProvider::Yahoo),
            "fastmail" => Some(MailProvider::Fastmail),
            "custom" => Some(MailProvider::Custom),
            _ => None,
        }
    }

    /// Display name for this provider.
    pub fn name(&self) -> &'static str {
        match self {
            MailProvider::Gmail => "Gmail",
            MailProvider::ICloud => "iCloud",
            MailProvider::Yahoo => "Yahoo",
            MailProvider::Fastmail => "Fastmail",
            MailProvider::Custom => "Custom",
        }
    }

    /// IMAP host for this provider.
    pub fn host(&self) -> &'static str {
        match self {
            MailProvider::Gmail => "imap.gmail.com",
            MailProvider::ICloud => "imap.mail.me.com",
            MailProvider::Yahoo => "imap.mail.yahoo.com",
            MailProvider::Fastmail => "imap.fastmail.com",
            MailProvider::Custom => "",
        }
    }

    /// Default IMAP port (993 for all known providers).
    pub fn port(&self) -> u16 {
        993
    }

    /// Build an AccountConfig with this provider's defaults.
    /// Password is resolved via keyring using the username at connection time.
    /// Trash and drafts mailboxes are auto-detected via RFC 6154 roles at runtime.
    pub fn default_config(&self, username: &str) -> AccountConfig {
        let password = Some(Secret::new_keyring(format!("mail.{}", username)));

        AccountConfig {
            host: self.host().to_string(),
            port: self.port(),
            username: username.to_string(),
            password,
            tls: true,
            max_connections: None,
        }
    }
}
