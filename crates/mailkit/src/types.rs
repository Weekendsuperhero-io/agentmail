use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A configured IMAP account.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    pub name: String,
    pub host: String,
    pub username: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_default: bool,
}

/// A mailbox on the server with message counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxInfo {
    pub name: String,
    pub account: String,
    pub total_messages: u32,
    pub unseen_messages: u32,
    pub recent_messages: u32,
    /// Delimiter character (e.g., "/" or ".")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delimiter: Option<String>,
    /// Full IMAP path including hierarchy
    pub path: String,
}

/// Metadata for a MIME attachment part (no binary content).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub content_type: String,
    pub size: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
}

/// A parsed email message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageInfo {
    /// IMAP UID (unique within mailbox + UIDVALIDITY epoch)
    pub uid: u32,
    pub subject: String,
    pub sender: String,
    pub reply_to: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub mailbox: String,
    pub account: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<DateTime<Utc>>,
    /// IMAP flags, e.g., ["\\Seen", "\\Flagged"]
    pub flags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_unsubscribe: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_unsubscribe_post: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_help: Option<String>,

    // Envelope / threading
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub references: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub bcc: Vec<String>,

    // MIME structure
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub attachments: Vec<AttachmentInfo>,

    // All headers (raw original values)
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub headers: HashMap<String, Vec<String>>,
}

/// Connection status for an account.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionStatus {
    pub account: String,
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_greeting: Option<String>,
}

/// Search criteria for IMAP SEARCH.
#[derive(Debug, Default, Clone)]
pub struct SearchCriteria {
    pub text: Option<String>,
    pub from: Option<String>,
    pub subject: Option<String>,
    pub to: Option<String>,
    pub seen: Option<bool>,
    pub flagged: Option<bool>,
    pub deleted: Option<bool>,
    pub header: Option<(String, String)>,
}

/// Summary of messages from a single sender address.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SenderSummary {
    /// Combined "Display Name <email>" for direct use in search.
    pub sender: String,
    /// Normalized email address (lowercase).
    pub address: String,
    /// Display name from the most recent message.
    pub display_name: String,
    /// Number of messages from this sender.
    pub count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_date: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_date: Option<DateTime<Utc>>,
}

/// Summary of mailing list messages grouped by sender.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSummary {
    /// Sender display string ("Display Name <email>" or just "email").
    pub sender: String,
    /// Normalized sender email address (lowercase).
    pub address: String,
    /// Sender display name.
    pub display_name: String,
    /// Raw List-Unsubscribe header value from the newest message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_unsubscribe: Option<String>,
    /// Extracted HTTPS unsubscribe URL, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsubscribe_url: Option<String>,
    /// Raw List-Unsubscribe-Post header value from the newest message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_unsubscribe_post: Option<String>,
    /// Whether RFC 8058 one-click unsubscribe is supported.
    pub one_click: bool,
    /// List-Id header value, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_id: Option<String>,
    /// UID of the newest message — can be passed to unsubscribe_message.
    pub sample_uid: u32,
    /// Mailbox containing sample_uid (relevant for account-wide scans).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_mailbox: Option<String>,
    /// Number of messages from this sender with List-Unsubscribe.
    pub count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_date: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_date: Option<DateTime<Utc>>,
}

