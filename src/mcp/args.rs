//! Tool and prompt argument structs for the MCP server.

use schemars::JsonSchema;
use serde::Deserialize;

pub(super) fn default_true() -> bool {
    true
}

pub(super) fn default_false() -> bool {
    false
}

// ---------------------------------------------------------------------------
// Tool argument structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "No arguments.")]
pub(super) struct ListAccountsArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for listing mailboxes.")]
pub(super) struct ListMailboxesArgs {
    #[schemars(
        description = "Optional account name. If omitted, list mailboxes across all accounts."
    )]
    pub(super) account: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for checking IMAP connection status.")]
pub(super) struct CheckConnectionArgs {
    #[schemars(description = "Account name to check connectivity for.")]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for listing IMAP server capabilities.")]
pub(super) struct ListCapabilitiesArgs {
    #[schemars(description = "Account name to query capabilities for.")]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for fetching a paginated chunk of messages.")]
pub(super) struct GetMessagesArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(description = "Zero-based row offset. Defaults to 0.")]
    pub(super) offset: Option<u64>,
    #[schemars(description = "Page size. Defaults to 25 and is clamped to 1..50.")]
    pub(super) limit: Option<u64>,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, include normalized markdown content (trimmed for context window safety)."
    )]
    pub(super) include_content: bool,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, include the full raw headers map. Off by default — structured fields (subject, sender, to, cc, date, message_id, etc.) are always returned."
    )]
    pub(super) include_headers: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for mailbox message search with optional filters.")]
pub(super) struct SearchMessagesArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(description = "General full-text search across message fields (IMAP TEXT search).")]
    pub(super) query: Option<String>,
    #[schemars(description = "Filter by sender (IMAP FROM search).")]
    pub(super) sender_contains: Option<String>,
    #[schemars(description = "Filter by subject (IMAP SUBJECT search).")]
    pub(super) subject_contains: Option<String>,
    #[schemars(description = "Filter by recipient (IMAP TO search).")]
    pub(super) to_contains: Option<String>,
    #[schemars(description = "Header key for header-based search.")]
    pub(super) header_key: Option<String>,
    #[schemars(description = "Header value filter (used with header_key).")]
    pub(super) header_value_contains: Option<String>,
    #[schemars(description = "Filter by flagged status.")]
    pub(super) flagged: Option<bool>,
    #[schemars(description = "Filter by read/seen status.")]
    pub(super) read: Option<bool>,
    #[serde(default = "default_false")]
    #[schemars(description = "Include deleted messages. Defaults to false.")]
    pub(super) deleted: bool,
    #[schemars(description = "Zero-based row offset. Defaults to 0.")]
    pub(super) offset: Option<u64>,
    #[schemars(description = "Page size. Defaults to 25 and is clamped to 1..50.")]
    pub(super) limit: Option<u64>,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, include normalized markdown content (trimmed for context window safety)."
    )]
    pub(super) include_content: bool,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, include the full raw headers map. Off by default — structured fields (subject, sender, to, cc, date, message_id, etc.) are always returned."
    )]
    pub(super) include_headers: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for listing flags in use.")]
pub(super) struct ListFlagsArgs {
    #[schemars(description = "Mailbox to scan. Omit to scan all mailboxes in the account.")]
    pub(super) mailbox: Option<String>,
    #[schemars(description = "Account name (required).")]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for deleting one or more messages.")]
pub(super) struct DeleteMessagesArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(description = "Array of IMAP UIDs to delete. One or more UIDs, up to 500.")]
    pub(super) uids: Vec<u32>,
    #[serde(default = "default_false")]
    #[schemars(
        description = "When true, permanently delete (flag \\Deleted + UID EXPUNGE), bypassing Trash. Irreversible. Defaults to false (move to Trash when available)."
    )]
    pub(super) permanent: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "Arguments for deleting all messages from a specific sender. The sender string is matched as a substring against the full From header (covers both display name and email address)."
)]
pub(super) struct DeleteBySenderArgs {
    #[schemars(description = "Mailbox containing the target UID. Defaults to INBOX when omitted.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        description = "UID of a message from the sender to delete. The exact sender (email + display name) is extracted from this message and used to find all other messages from the same sender."
    )]
    pub(super) uid: u32,
    #[serde(default = "default_false")]
    #[schemars(
        description = "When true, search and delete across ALL mailboxes in the account (not just the source mailbox). Defaults to false."
    )]
    pub(super) all_mailboxes: bool,
    #[serde(default = "default_false")]
    #[schemars(
        description = "When true, permanently delete (flag \\Deleted + UID EXPUNGE), bypassing Trash. Irreversible. Defaults to false (move to Trash when available)."
    )]
    pub(super) permanent: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for finding messages with attachments.")]
pub(super) struct FindAttachmentsArgs {
    #[schemars(description = "Mailbox name. Omit to scan all mailboxes in the account.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(description = "Number of UIDs to skip. Defaults to 0.")]
    pub(super) offset: Option<u64>,
    #[schemars(description = "Max UIDs to return. Defaults to 25, max 100.")]
    pub(super) limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for creating a draft message.")]
pub(super) struct CreateDraftArgs {
    #[schemars(
        description = "Account name (required). Draft is saved to this account's Drafts folder."
    )]
    pub(super) account: String,
    #[serde(default)]
    #[schemars(description = "Draft subject line.")]
    pub(super) subject: String,
    #[serde(default)]
    #[schemars(description = "Draft body content.")]
    pub(super) body: String,
    #[serde(default)]
    #[schemars(description = "To recipient email addresses.")]
    pub(super) to: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Cc recipient email addresses.")]
    pub(super) cc: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Bcc recipient email addresses.")]
    pub(super) bcc: Vec<String>,
    #[serde(default)]
    #[schemars(
        description = "Attachments to include. Each entry requires a local filesystem 'path'. 'filename' and 'contentType' are optional and will be inferred when omitted."
    )]
    pub(super) attachments: Vec<DraftAttachmentArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline, description = "Attachment to attach to a draft.")]
pub(super) struct DraftAttachmentArg {
    #[schemars(description = "Local filesystem path to the file to attach (required).")]
    pub(super) path: String,
    #[serde(default)]
    #[schemars(
        description = "Override filename to show in the email. Defaults to the file's basename."
    )]
    pub(super) filename: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "MIME content type (e.g. 'application/pdf'). Inferred from extension when omitted."
    )]
    pub(super) content_type: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for moving a message between mailboxes.")]
pub(super) struct MoveMessageArgs {
    #[schemars(description = "Source mailbox name.")]
    pub(super) mailbox: String,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(description = "IMAP UID of the message to move.")]
    pub(super) uid: u32,
    #[schemars(description = "Destination mailbox name.")]
    pub(super) destination: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for unsubscribe + optional list cleanup.")]
pub(super) struct UnsubscribeMessageArgs {
    #[schemars(
        description = "Mailbox containing the target message. Defaults to INBOX. When deleting matching messages, all mailboxes are searched regardless of this value."
    )]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(description = "IMAP UID of the message.")]
    pub(super) uid: u32,
    #[serde(default = "default_true")]
    #[schemars(
        description = "If true, bulk-delete matching messages. For List-Unsubscribe messages: deletes all from the exact sender with a List-Unsubscribe header. For List-Id-only messages: deletes all with the same List-Id."
    )]
    pub(super) delete_matching: bool,
    #[serde(default = "default_false")]
    #[schemars(
        description = "When true, permanently delete matching messages (flag \\Deleted + UID EXPUNGE), bypassing Trash. Irreversible. Defaults to false (move to Trash when available)."
    )]
    pub(super) permanent: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for listing top senders by message count.")]
pub(super) struct TopSendersArgs {
    #[schemars(description = "Mailbox name. When omitted, scans ALL mailboxes in the account.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        description = "Maximum number of senders to return. Defaults to 100; set higher to return more."
    )]
    pub(super) limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "Arguments for listing top subscriptions (bulk senders) by message count."
)]
pub(super) struct TopSubscriptionsArgs {
    #[schemars(description = "Mailbox name. When omitted, scans ALL mailboxes in the account.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        description = "Maximum number of lists to return. Defaults to 100; set higher to return more."
    )]
    pub(super) limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for listing top mailing lists by List-Id header.")]
pub(super) struct TopMailingListsArgs {
    #[schemars(description = "Mailbox name. When omitted, scans ALL mailboxes in the account.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        description = "Maximum number of lists to return. Defaults to 100; set higher to return more."
    )]
    pub(super) limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for deleting all messages with a specific List-Id.")]
pub(super) struct DeleteListIdArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(description = "The List-Id header value to match (from top_mailing_lists).")]
    pub(super) list_id: String,
    #[schemars(description = "Mailbox to search. Omit to search all mailboxes.")]
    pub(super) mailbox: Option<String>,
    #[serde(default = "default_false")]
    #[schemars(
        description = "When true, permanently delete (flag \\Deleted + UID EXPUNGE), bypassing Trash. Irreversible. Defaults to false (move to Trash when available)."
    )]
    pub(super) permanent: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for creating a new mailbox on the server.")]
pub(super) struct CreateMailboxArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        description = "Mailbox name to create. Use delimiter (usually '/') for nested mailboxes, e.g. 'Archive/2024'."
    )]
    pub(super) mailbox_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for downloading message attachments to disk.")]
pub(super) struct DownloadAttachmentsArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(description = "IMAP UID of the message.")]
    pub(super) uid: u32,
    #[schemars(description = "Directory to save attachments to. Defaults to current directory.")]
    pub(super) output_dir: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "Arguments for adding flags and/or setting an Apple Mail color on a message."
)]
pub(super) struct AddFlagsArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(description = "IMAP UID of the message.")]
    pub(super) uid: u32,
    #[serde(default)]
    #[schemars(
        description = "Flags to add. System flags use backslash prefix (e.g. \"\\\\Seen\", \"\\\\Flagged\"). Custom keywords are plain strings. Cannot include \\\\Deleted or \\\\Recent."
    )]
    pub(super) flags: Vec<String>,
    #[schemars(
        description = "Apple Mail color to set (case-insensitive): red, orange, yellow, green, blue, purple, gray. Sets \\\\Flagged + $MailFlagBit keywords. Replaces any existing color."
    )]
    pub(super) color: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "Arguments for removing flags and/or clearing Apple Mail color from a message."
)]
pub(super) struct RemoveFlagsArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(description = "IMAP UID of the message.")]
    pub(super) uid: u32,
    #[serde(default)]
    #[schemars(
        description = "Flags to remove. System flags use backslash prefix (e.g. \"\\\\Seen\"). Cannot include \\\\Deleted or \\\\Recent."
    )]
    pub(super) flags: Vec<String>,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, removes the Apple Mail color flag (\\\\Flagged + all $MailFlagBit keywords). Defaults to false."
    )]
    pub(super) color: bool,
}

// ---------------------------------------------------------------------------
// Prompt argument structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct InboxSummaryArgs {
    #[schemars(description = "Account name to summarize.")]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct CleanupSenderArgs {
    #[schemars(description = "Account name.")]
    pub(super) account: String,
    #[schemars(description = "Sender email address or name to clean up.")]
    pub(super) sender: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct FindAttachmentsPromptArgs {
    #[schemars(description = "Account name.")]
    pub(super) account: String,
    #[schemars(description = "Mailbox to search. Defaults to INBOX.")]
    pub(super) mailbox: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ComposeEmailArgs {
    #[schemars(description = "Account name to send from.")]
    pub(super) account: String,
    #[schemars(description = "Recipient email address.")]
    pub(super) to: Option<String>,
    #[schemars(description = "Email subject line.")]
    pub(super) subject: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct UnsubscribeCleanupArgs {
    #[schemars(description = "Account name.")]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListIdCleanupArgs {
    #[schemars(description = "Account name.")]
    pub(super) account: String,
}
