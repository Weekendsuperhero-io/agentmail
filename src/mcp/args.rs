//! Tool and prompt argument structs for the MCP server.

use schemars::JsonSchema;
use serde::Deserialize;

pub(super) fn default_false() -> bool {
    false
}

pub(super) fn default_true() -> bool {
    true
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
    #[schemars(description = "Account name (required).")]
    pub(super) account: String,
    #[schemars(
        range(max = 1_000_000),
        description = "Zero-based selectable-mailbox offset. Defaults to 0; maximum 1,000,000."
    )]
    pub(super) offset: Option<u64>,
    #[schemars(
        range(min = 1, max = 500),
        description = "Page size. Defaults to 100; maximum 500."
    )]
    pub(super) limit: Option<u64>,
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
    #[schemars(description = "Mailbox name (required). Get names from list_mailboxes.")]
    pub(super) mailbox: String,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        range(max = 1_000_000),
        description = "Zero-based row offset. Defaults to 0; maximum 1,000,000."
    )]
    pub(super) offset: Option<u64>,
    #[schemars(
        range(min = 1, max = 50),
        description = "Page size. Defaults to 25; valid range 1..50."
    )]
    pub(super) limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for mailbox message search with optional filters.")]
pub(super) struct SearchMessagesArgs {
    #[schemars(description = "Mailbox name (required). Get names from list_mailboxes.")]
    pub(super) mailbox: String,
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
    #[schemars(
        description = "Only messages received on or after this date (inclusive), as YYYY-MM-DD (IMAP SINCE, by server internal date)."
    )]
    pub(super) since: Option<String>,
    #[schemars(
        description = "Only messages received before this date (exclusive), as YYYY-MM-DD (IMAP BEFORE, by server internal date)."
    )]
    pub(super) before: Option<String>,
    #[schemars(description = "Only messages larger than this many bytes (IMAP LARGER).")]
    pub(super) larger_than: Option<u32>,
    #[schemars(description = "Only messages smaller than this many bytes (IMAP SMALLER).")]
    pub(super) smaller_than: Option<u32>,
    #[serde(default = "default_false")]
    #[schemars(description = "Include deleted messages. Defaults to false.")]
    pub(super) deleted: bool,
    #[schemars(
        range(max = 1_000_000),
        description = "Zero-based row offset. Defaults to 0; maximum 1,000,000."
    )]
    pub(super) offset: Option<u64>,
    #[schemars(
        range(min = 1, max = 50),
        description = "Page size. Defaults to 25; valid range 1..50."
    )]
    pub(super) limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for listing flags in use.")]
pub(super) struct ListFlagsArgs {
    #[schemars(description = "Mailbox to scan. Omit to use the account-wide discovery plan.")]
    pub(super) mailbox: Option<String>,
    #[schemars(description = "Account name (required).")]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for deleting one or more messages.")]
pub(super) struct DeleteMessagesArgs {
    #[schemars(
        description = "Mailbox containing the UIDs (required) — the same mailbox that produced expectedUidValidity."
    )]
    pub(super) mailbox: String,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        length(min = 1, max = 500),
        inner(range(min = 1)),
        description = "Array of non-zero IMAP UIDs to delete. One or more UIDs, up to 500."
    )]
    pub(super) uids: Vec<u32>,
    #[schemars(
        range(min = 1),
        description = "UIDVALIDITY returned by the discovery response. The action fails if the mailbox UID epoch changed."
    )]
    pub(super) expected_uid_validity: u32,
    #[serde(default = "default_false")]
    #[schemars(
        description = "When true, permanently delete (flag \\Deleted + UID EXPUNGE), bypassing Trash. Irreversible. Defaults to false (move to Trash when available)."
    )]
    pub(super) permanent: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "Arguments for deleting all messages from the exact sender extracted from a specific message UID."
)]
pub(super) struct DeleteBySenderArgs {
    #[schemars(
        description = "Mailbox containing the sample UID (required), e.g. sample.mailbox from top_senders."
    )]
    pub(super) mailbox: String,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        range(min = 1),
        description = "UID of a message from the sender to delete. The exact sender (email + display name) is extracted from this message and used to find all other messages from the same sender."
    )]
    pub(super) uid: u32,
    #[schemars(
        range(min = 1),
        description = "UIDVALIDITY paired with the sample UID. The action fails if the mailbox UID epoch changed."
    )]
    pub(super) expected_uid_validity: u32,
    #[serde(default = "default_false")]
    #[schemars(
        description = "When true, use the account-wide mutation plan to enumerate selectable storage mailboxes (not just the source mailbox). Defaults to false."
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
    #[schemars(description = "Mailbox name. Omit to use the account-wide discovery plan.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        range(max = 1_000_000),
        description = "Number of attachment hits to skip. Defaults to 0; maximum 1,000,000."
    )]
    pub(super) offset: Option<u64>,
    #[schemars(
        range(min = 1, max = 100),
        description = "Maximum attachment hits to return. Defaults to 25; maximum 100."
    )]
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
    #[schemars(
        range(min = 1),
        description = "Non-zero IMAP UID of the message to move."
    )]
    pub(super) uid: u32,
    #[schemars(
        range(min = 1),
        description = "UIDVALIDITY paired with the UID. The action fails if the source mailbox UID epoch changed."
    )]
    pub(super) expected_uid_validity: u32,
    #[schemars(description = "Destination mailbox name.")]
    pub(super) destination: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for unsubscribe + optional list cleanup.")]
pub(super) struct UnsubscribeMessageArgs {
    #[schemars(
        description = "Mailbox containing the target message (required), e.g. sample.mailbox from top_subscriptions. Matching-message cleanup uses the account-wide mutation plan regardless of this value."
    )]
    pub(super) mailbox: String,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(range(min = 1), description = "Non-zero IMAP UID of the message.")]
    pub(super) uid: u32,
    #[schemars(
        range(min = 1),
        description = "UIDVALIDITY from top_subscriptions sample.uidValidity. The action fails if the mailbox UID epoch has changed."
    )]
    pub(super) expected_uid_validity: u32,
    #[schemars(
        description = "Required explicit user consent for the RFC 8058 HTTPS POST. Must be true; tool annotations are not authorization."
    )]
    pub(super) confirm_one_click: bool,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, bulk-delete messages with the target's exact normalized List-Id account-wide, but only when the same passing DKIM signature also covers List-Id. Defaults to false."
    )]
    pub(super) delete_matching: bool,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, allow cleanup policy evaluation even when DKIM, URL validation, DNS, or the HTTPS POST fails. Cleanup still requires a DKIM-authenticated List-Id or the separate sender fallback. Defaults to false."
    )]
    pub(super) delete_on_unsubscribe_failure: bool,
    #[serde(default = "default_true")]
    #[schemars(
        description = "When no usable DKIM-authenticated List-Id exists, allow the narrower exact-sender plus list-header cleanup instead. Defaults to true — it only activates when deleteMatching was requested and the unsubscribe identity was already verified. Set false to require the authenticated List-Id path strictly."
    )]
    pub(super) allow_sender_fallback: bool,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, allow irreversible UID EXPUNGE when Trash is unavailable or moving to Trash fails. Defaults to false. Never used as a Gmail fallback because in-place EXPUNGE only removes a label."
    )]
    pub(super) allow_permanent_fallback: bool,
    #[serde(default = "default_false")]
    #[schemars(
        description = "When true, request permanent matching-message deletion (flag \\Deleted + UID EXPUNGE), bypassing Trash. Irreversible on standard IMAP. On Gmail this safely moves to Trash because in-place EXPUNGE only removes a label. Defaults to false."
    )]
    pub(super) permanent: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for listing top senders by message count.")]
pub(super) struct TopSendersArgs {
    #[schemars(description = "Mailbox name. Omit to use the account-wide discovery plan.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        range(max = 1_000_000),
        description = "Zero-based ranked-row offset. Defaults to 0; maximum 1,000,000."
    )]
    pub(super) offset: Option<u64>,
    #[schemars(
        range(min = 1, max = 100),
        description = "Page size. Defaults to 10; maximum 100."
    )]
    pub(super) limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "Arguments for listing top subscriptions (bulk senders) by message count."
)]
pub(super) struct TopSubscriptionsArgs {
    #[schemars(description = "Mailbox name. Omit to use the account-wide discovery plan.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        range(max = 1_000_000),
        description = "Zero-based ranked-row offset. Defaults to 0; maximum 1,000,000."
    )]
    pub(super) offset: Option<u64>,
    #[schemars(
        range(min = 1, max = 100),
        description = "Page size. Defaults to 10; maximum 100."
    )]
    pub(super) limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for listing top mailing lists by List-Id header.")]
pub(super) struct TopMailingListsArgs {
    #[schemars(description = "Mailbox name. Omit to use the account-wide discovery plan.")]
    pub(super) mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        range(max = 1_000_000),
        description = "Zero-based ranked-row offset. Defaults to 0; maximum 1,000,000."
    )]
    pub(super) offset: Option<u64>,
    #[schemars(
        range(min = 1, max = 100),
        description = "Page size. Defaults to 10; maximum 100."
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
    #[schemars(description = "Mailbox to search. Omit to use the account-wide mutation plan.")]
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
    pub(super) mailbox: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for downloading message attachments to disk.")]
pub(super) struct DownloadAttachmentsArgs {
    #[schemars(
        description = "Mailbox containing the message (required) — the same mailbox that produced expectedUidValidity."
    )]
    pub(super) mailbox: String,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(range(min = 1), description = "Non-zero IMAP UID of the message.")]
    pub(super) uid: u32,
    #[schemars(
        range(min = 1),
        description = "UIDVALIDITY paired with the UID. The download fails if the mailbox UID epoch changed."
    )]
    pub(super) expected_uid_validity: u32,
    #[schemars(description = "Directory to save attachments to. Defaults to current directory.")]
    pub(super) output_dir: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "Arguments for adding flags and/or setting an Apple Mail color on a message."
)]
pub(super) struct AddFlagsArgs {
    #[schemars(
        description = "Mailbox containing the message (required) — the same mailbox that produced expectedUidValidity."
    )]
    pub(super) mailbox: String,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(range(min = 1), description = "Non-zero IMAP UID of the message.")]
    pub(super) uid: u32,
    #[schemars(
        range(min = 1),
        description = "UIDVALIDITY paired with the UID. The update fails if the mailbox UID epoch changed."
    )]
    pub(super) expected_uid_validity: u32,
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
    #[schemars(
        description = "Mailbox containing the message (required) — the same mailbox that produced expectedUidValidity."
    )]
    pub(super) mailbox: String,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(range(min = 1), description = "Non-zero IMAP UID of the message.")]
    pub(super) uid: u32,
    #[schemars(
        range(min = 1),
        description = "UIDVALIDITY paired with the UID. The update fails if the mailbox UID epoch changed."
    )]
    pub(super) expected_uid_validity: u32,
    #[serde(default)]
    #[schemars(
        description = "Flags to remove. System flags use backslash prefix (e.g. \"\\\\Seen\"). Cannot include \\\\Deleted or \\\\Recent."
    )]
    pub(super) flags: Vec<String>,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, removes the Apple Mail color flag (\\\\Flagged + all $MailFlagBit keywords). Defaults to false."
    )]
    pub(super) clear_color: bool,
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
