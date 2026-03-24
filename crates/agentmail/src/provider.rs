use crate::config::AccountConfig;
use secret::Secret;

/// Supported mail providers with pre-configured IMAP defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailProvider {
    Gmail,
    ICloud,
    Outlook,
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
            ("Outlook", MailProvider::Outlook),
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
            "outlook" | "microsoft" | "office365" | "hotmail" | "live" => {
                Some(MailProvider::Outlook)
            }
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
            MailProvider::Outlook => "Outlook",
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
            MailProvider::Outlook => "outlook.office365.com",
            MailProvider::Yahoo => "imap.mail.yahoo.com",
            MailProvider::Fastmail => "imap.fastmail.com",
            MailProvider::Custom => "",
        }
    }

    /// Default IMAP port (993 for all known providers).
    pub fn port(&self) -> u16 {
        993
    }

    /// Trash mailbox name for this provider.
    pub fn trash_mailbox(&self) -> &'static str {
        match self {
            MailProvider::Gmail => "[Gmail]/Trash",
            MailProvider::ICloud => "Deleted Messages",
            MailProvider::Outlook => "Deleted",
            MailProvider::Yahoo => "Trash",
            MailProvider::Fastmail => "Trash",
            MailProvider::Custom => "Trash",
        }
    }

    /// Drafts mailbox name for this provider.
    pub fn drafts_mailbox(&self) -> &'static str {
        match self {
            MailProvider::Gmail => "[Gmail]/Drafts",
            MailProvider::Yahoo => "Draft",
            _ => "Drafts",
        }
    }

    /// Build an AccountConfig with this provider's defaults.
    /// Password is resolved via keyring using the username at connection time.
    pub fn default_config(&self, username: &str) -> AccountConfig {
        let password = secret::keyring::KeyringEntry::try_new(username)
            .ok()
            .map(Secret::new_keyring_entry);

        AccountConfig {
            host: self.host().to_string(),
            port: self.port(),
            username: username.to_string(),
            password,
            tls: true,
            trash_mailbox: Some(self.trash_mailbox().to_string()),
            drafts_mailbox: Some(self.drafts_mailbox().to_string()),
            max_connections: None,
        }
    }
}
