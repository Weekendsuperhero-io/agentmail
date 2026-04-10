use hashbrown::HashMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A configured IMAP account.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    pub name: String,
    pub host: String,
    pub username: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_default: bool,
}

/// A mailbox on the server with message counts.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
    /// `true` when the mailbox cannot be SELECTed (virtual container only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_select: bool,
    /// `true` when no child mailboxes exist or can be created.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_inferiors: bool,
    /// RFC 6154 special-use role: "all", "archive", "drafts", "flagged",
    /// "junk", "sent", or "trash". `None` for ordinary user mailboxes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// Metadata for a MIME attachment part (no binary content).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
    #[schemars(with = "std::collections::HashMap<String, Vec<String>>")]
    pub headers: HashMap<String, Vec<String>>,
}

/// Connection status for an account.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SenderSummary {
    /// Combined `"Display Name <email>"` for direct use in search.
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListSummary {
    /// Sender display string (`"Display Name <email>"` or just `"email"`).
    pub sender: String,
    /// Normalized sender email address (lowercase).
    pub address: String,
    /// Sender display name (used internally; not serialized — already in `sender`).
    #[serde(skip_serializing)]
    pub display_name: String,
    /// Raw List-Unsubscribe header value (used internally; not serialized — `unsubscribe_url` has the extracted URL).
    #[serde(skip_serializing)]
    pub list_unsubscribe: Option<String>,
    /// Extracted HTTPS unsubscribe URL, if present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsubscribe_url: Option<String>,
    /// Raw List-Unsubscribe-Post header value from the newest message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_unsubscribe_post: Option<String>,
    /// Whether RFC 8058 one-click unsubscribe is supported.
    pub one_click: bool,
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

// ============================================================================
// Response wrappers for MCP structured content
// ============================================================================

/// Response for list_accounts.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListAccountsResponse {
    pub accounts: Vec<AccountInfo>,
}

/// Response for list_mailboxes.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListMailboxesResponse {
    pub mailboxes: Vec<MailboxInfo>,
}

/// Response for list_capabilities.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListCapabilitiesResponse {
    pub account: String,
    pub capabilities: Vec<String>,
}

// ============================================================================
// Read tool responses
// ============================================================================

/// Response for get_messages.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetMessagesResponse {
    pub mailbox: String,
    pub account: String,
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
    pub messages: Vec<MessageInfo>,
}

/// Response for get_messages_by_uid.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetMessagesByUidResponse {
    pub mailbox: String,
    pub account: String,
    pub messages: Vec<MessageInfo>,
}

/// Response for search_messages.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchMessagesResponse {
    pub mailbox: String,
    pub account: String,
    pub offset: usize,
    pub limit: usize,
    pub total_matches: usize,
    pub messages: Vec<MessageInfo>,
}

/// Response for list_flags.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListFlagsResponse {
    pub mailbox: String,
    pub account: String,
    pub total_flags: usize,
    pub flags: Vec<FlagCount>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub colors: Vec<ColorCount>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub per_mailbox: Vec<MailboxFlagBreakdown>,
}

/// A flag name with its count.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlagCount {
    pub flag: String,
    pub count: u32,
}

/// A resolved Apple Mail color with its count.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ColorCount {
    pub color: String,
    pub count: u32,
}

/// Per-mailbox flag breakdown.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailboxFlagBreakdown {
    pub mailbox: String,
    pub total_flags: usize,
    pub flags: Vec<FlagCount>,
}

/// Response for find_attachments.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FindAttachmentsResponse {
    pub mailbox: String,
    pub account: String,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub uids: Vec<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub per_mailbox: Vec<MailboxAttachmentCount>,
}

/// Per-mailbox attachment count.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailboxAttachmentCount {
    pub mailbox: String,
    pub count: usize,
}

/// Response for rank_senders.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RankSendersResponse {
    pub mailbox: String,
    pub account: String,
    pub total_messages: u32,
    pub unique_senders: usize,
    pub senders: Vec<SenderSummary>,
}

/// Response for rank_unsubscribe.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RankUnsubscribeResponse {
    pub mailbox: String,
    pub account: String,
    pub total_messages: u32,
    pub unique_lists: usize,
    pub lists: Vec<ListSummary>,
}

/// Summary of messages grouped by List-Id (RFC 2919).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListIdSummary {
    /// The List-Id header value (grouping key).
    pub list_id: String,
    /// Display name extracted from the List-Id (text before angle brackets).
    pub display_name: String,
    /// Unique sender addresses seen for this list (for context).
    pub senders: Vec<String>,
    /// Number of messages with this List-Id.
    pub count: u32,
    /// UID of the newest message.
    pub sample_uid: u32,
    /// Mailbox containing sample_uid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_mailbox: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_date: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_date: Option<DateTime<Utc>>,
}

/// Response for rank_list_id.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RankListIdResponse {
    pub mailbox: String,
    pub account: String,
    pub total_messages: u32,
    pub unique_lists: usize,
    pub lists: Vec<ListIdSummary>,
}

/// Response for delete_list_id.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteListIdResponse {
    pub mailbox: String,
    pub account: String,
    pub list_id: String,
    pub found: usize,
    pub deleted: usize,
    pub failed: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mailboxes: Vec<PerMailboxDeleteResult>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

// ============================================================================
// Write tool responses
// ============================================================================

/// Response for delete_messages.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteMessagesResponse {
    pub mailbox: String,
    pub account: String,
    pub deleted: usize,
    pub failed: usize,
    /// True when configured trash mailbox was unavailable and deletion
    /// fell back to flag+expunge (permanent delete).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub trash_fallback: bool,
}

/// Per-mailbox deletion result (shared by delete_by_sender and unsubscribe_message).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PerMailboxDeleteResult {
    pub mailbox: String,
    pub found: usize,
    pub deleted: usize,
    pub failed: usize,
}

/// Response for delete_by_sender.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteBySenderResponse {
    pub mailbox: String,
    pub account: String,
    pub sender: String,
    pub found: usize,
    pub deleted: usize,
    pub failed: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mailboxes: Vec<PerMailboxDeleteResult>,
    /// Mailboxes that could not be selected or searched (skipped during scan).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

/// Response for move_message.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoveMessageResponse {
    pub mailbox: String,
    pub account: String,
    pub uid: u32,
    pub destination: String,
    pub moved: bool,
}

/// Response for create_mailbox.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateMailboxResponse {
    pub account: String,
    pub mailbox: String,
    pub created: bool,
    /// True when mailbox already existed (CREATE was skipped).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub already_exists: bool,
}

/// Recipients for a draft email.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DraftRecipients {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

/// Response for create_draft.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateDraftResponse {
    pub created: bool,
    pub account: String,
    pub drafts_mailbox: String,
    pub subject: String,
    pub recipients: DraftRecipients,
}

/// A downloaded attachment file.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DownloadedFile {
    pub index: usize,
    pub filename: String,
    pub path: String,
    pub content_type: String,
    pub size: usize,
}

/// Response for download_attachments.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DownloadAttachmentsResponse {
    pub mailbox: String,
    pub account: String,
    pub uid: u32,
    pub downloaded: Vec<DownloadedFile>,
}

/// Response for get_message_source.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetMessageSourceResponse {
    pub mailbox: String,
    pub account: String,
    pub uid: u32,
    pub source: String,
}

/// Result of a one-click unsubscribe attempt.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnsubscribeResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Bulk deletion results from unsubscribe_message.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MatchingMessagesResult {
    pub matched_by: String,
    pub sender: String,
    pub found: usize,
    pub deleted: usize,
    pub failed: usize,
    pub mailboxes: Vec<PerMailboxDeleteResult>,
    /// Mailboxes that could not be selected or searched (skipped during scan).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

/// Response for unsubscribe_message.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnsubscribeResponse {
    pub mailbox: String,
    pub account: String,
    pub uid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_unsubscribe: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_unsubscribe_post: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pathway: Option<String>,
    pub unsubscribed: UnsubscribeResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matching_messages: Option<MatchingMessagesResult>,
}

// ============================================================================
// Flag tool responses
// ============================================================================

/// Response for add_flags / remove_flags.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateFlagsResponse {
    pub mailbox: String,
    pub account: String,
    pub uid: u32,
    pub flags: Vec<String>,
    /// Resolved Apple Mail color (red/orange/yellow/green/blue/purple/gray) or null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}
