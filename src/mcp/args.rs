//! Tool and prompt argument structs for the MCP server.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub(super) fn default_false() -> bool {
    false
}

// ---------------------------------------------------------------------------
// Tool argument structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "No arguments.")]
pub(super) struct ListAccountsArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Arguments for checking IMAP connection status.")]
pub(super) struct CheckConnectionArgs {
    #[schemars(description = "Account name to check connectivity for.")]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Arguments for listing IMAP server capabilities.")]
pub(super) struct ListCapabilitiesArgs {
    #[schemars(description = "Account name to query capabilities for.")]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Arguments for listing flags in use.")]
pub(super) struct ListFlagsArgs {
    #[schemars(description = "Mailbox to scan. Omit to use the account-wide discovery plan.")]
    pub(super) mailbox: Option<String>,
    #[schemars(description = "Account name (required).")]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(
    description = "Arguments for deleting all messages from an exact sender identity (email + display name)."
)]
pub(super) struct DeleteBySenderArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        description = "The sender's exact email address to match (from a top_senders row's address)."
    )]
    pub(super) email: String,
    #[serde(default)]
    #[schemars(
        description = "The sender's exact display name to match (from the row's displayName). Omit or pass \"\" for senders without a display name; matching is exact on both fields."
    )]
    pub(super) name: Option<String>,
    #[schemars(description = "Mailbox to search. Omit to use the account-wide mutation plan.")]
    pub(super) mailbox: Option<String>,
    #[serde(default = "default_false")]
    #[schemars(
        description = "When true, permanently delete (flag \\Deleted + UID EXPUNGE), bypassing Trash. Irreversible. Defaults to false (move to Trash when available)."
    )]
    pub(super) permanent: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    #[schemars(description = "Reply-To header addresses for responses to this draft.")]
    pub(super) reply_to: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Message-ID this draft replies to. Angle brackets are optional.")]
    pub(super) in_reply_to: Option<String>,
    #[serde(default)]
    #[schemars(description = "Ordered ancestor Message-IDs for the References header.")]
    pub(super) references: Vec<String>,
    #[serde(default)]
    #[schemars(
        description = "Reply to this live message instead of composing fresh. When present, `to`, `cc`, `inReplyTo` and `references` are DERIVED from it and must be omitted; `subject` becomes an optional override of the Re: form."
    )]
    pub(super) reply_to_message: Option<ReplyToMessageArg>,
    #[serde(default)]
    #[schemars(
        length(max = 20),
        description = "Attachments to include (maximum 20 files, 25 MiB each, 40 MiB aggregate). Each entry requires a local filesystem 'path'. 'filename' and 'contentType' are optional and will be inferred when omitted."
    )]
    pub(super) attachments: Vec<DraftAttachmentArg>,
    #[serde(default)]
    #[schemars(
        description = "Send the body as plain text ONLY, with no rendered alternative. Default false: the body is read as Markdown and sent as multipart/alternative — the text exactly as written, plus an HTML rendering of it, so **bold**, lists, links and tables arrive formatted instead of as literal syntax. Set true when literal plain text is the point (a plain-text-only recipient, a mailing list, or a body that must not be reinterpreted)."
    )]
    pub(super) plain_text_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) enum ReplyModeArg {
    Reply,
    ReplyAll,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(inline)]
#[schemars(
    description = "The live message this draft replies to. Supplying it derives the recipients, the Re: subject and the RFC threading headers from that message."
)]
pub(super) struct ReplyToMessageArg {
    #[schemars(
        description = "Mailbox holding the message being replied to — the same mailbox that produced expectedUidValidity."
    )]
    pub(super) mailbox: String,
    #[schemars(range(min = 1), description = "Non-zero IMAP UID of that message.")]
    pub(super) uid: u32,
    #[schemars(
        range(min = 1),
        description = "UIDVALIDITY paired with the UID. The draft is refused if the mailbox UID epoch changed."
    )]
    pub(super) expected_uid_validity: u32,
    #[schemars(
        description = "reply (the sender only) or replyAll (sender plus the original To and Cc, minus this account's own addresses)."
    )]
    pub(super) mode: ReplyModeArg,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Complete replacement specification for one live IMAP draft.")]
pub(super) struct UpdateDraftArgs {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    #[schemars(range(min = 1))]
    pub(super) expected_uid_validity: u32,
    #[serde(default)]
    pub(super) subject: String,
    #[serde(default)]
    pub(super) body: String,
    #[serde(default)]
    pub(super) to: Vec<String>,
    #[serde(default)]
    pub(super) cc: Vec<String>,
    #[serde(default)]
    pub(super) bcc: Vec<String>,
    #[serde(default)]
    pub(super) reply_to: Vec<String>,
    #[serde(default)]
    pub(super) in_reply_to: Option<String>,
    #[serde(default)]
    pub(super) references: Vec<String>,
    #[serde(default)]
    #[schemars(
        length(max = 20),
        description = "Complete replacement attachment list. Omitted means no attachments; partial preservation is intentionally unsupported."
    )]
    pub(super) attachments: Vec<DraftAttachmentArg>,
    #[serde(default)]
    #[schemars(
        description = "Send the body as plain text ONLY, with no rendered alternative. Default false: the body is read as Markdown and sent as multipart/alternative — the text exactly as written, plus an HTML rendering of it, so **bold**, lists, links and tables arrive formatted instead of as literal syntax. Set true when literal plain text is the point (a plain-text-only recipient, a mailing list, or a body that must not be reinterpreted)."
    )]
    pub(super) plain_text_only: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    #[schemars(
        description = "Optional matching-message cleanup. Omit to only unsubscribe. When present, messages matching the verified cleanup identity are deleted account-wide under the nested when/identity/deletion policy."
    )]
    pub(super) cleanup: Option<UnsubscribeCleanupSpec>,
}

/// When matching-message cleanup may run relative to the unsubscribe attempt.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) enum CleanupWhenArg {
    /// Only after the one-click POST verifiably succeeded.
    #[default]
    AfterSuccess,
    /// Even when DKIM, URL validation, DNS, or the HTTPS POST failed.
    Always,
}

/// Which identity cleanup may match messages by.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) enum CleanupIdentityArg {
    /// Only a DKIM-authenticated List-Id; skip cleanup when there is none.
    ListIdOnly,
    /// Prefer the authenticated List-Id; otherwise require the exact sender
    /// email plus List-Unsubscribe-Post, and the target's List-Id when present.
    #[default]
    ListIdOrSender,
}

/// How cleanup disposes of matched messages.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) enum CleanupDeletionArg {
    /// Move to Trash; skip cleanup when no Trash mailbox is resolvable.
    #[default]
    Trash,
    /// Move to Trash, but permit an irreversible UID EXPUNGE when Trash is
    /// unavailable or the MOVE fails (never on Gmail).
    TrashThenPermanent,
    /// Permanently delete, bypassing Trash. Irreversible on standard IMAP; on
    /// Gmail this safely moves to Trash instead.
    Permanent,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(
    inline,
    description = "Matching-message cleanup policy. The three axes are independent; every combination is valid."
)]
pub(super) struct UnsubscribeCleanupSpec {
    #[serde(default)]
    #[schemars(
        description = "When cleanup may run: \"afterSuccess\" (default) only after a verified successful POST, or \"always\" even when the unsubscribe attempt failed."
    )]
    pub(super) when: CleanupWhenArg,
    #[serde(default)]
    #[schemars(
        description = "Cleanup identity: \"listIdOrSender\" (default) prefers the DKIM-authenticated List-Id and otherwise requires the exact sender email plus List-Unsubscribe-Post, also requiring the target's List-Id when one exists; \"listIdOnly\" requires the authenticated List-Id strictly."
    )]
    pub(super) identity: CleanupIdentityArg,
    #[serde(default)]
    #[schemars(
        description = "Disposal: \"trash\" (default) moves to Trash and skips cleanup when Trash is unavailable; \"trashThenPermanent\" permits an irreversible fallback EXPUNGE; \"permanent\" bypasses Trash outright (on Gmail it still moves to Trash)."
    )]
    pub(super) deletion: CleanupDeletionArg,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Arguments for listing exact Header From domains by message count.")]
pub(super) struct TopDomainsArgs {
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
        description = "Page size. Defaults to 20; maximum 100."
    )]
    pub(super) limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Arguments for deleting mail from one exact sender domain.")]
pub(super) struct DeleteByDomainArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        description = "Exact canonical domain from a top_domains row. A parent such as example.com never includes mail.example.com."
    )]
    pub(super) domain: String,
    #[schemars(description = "Mailbox to search. Omit to use the account-wide mutation plan.")]
    pub(super) mailbox: Option<String>,
    #[serde(default = "default_false")]
    #[schemars(
        description = "When true, permanently delete (flag \\Deleted + UID EXPUNGE), bypassing Trash. Irreversible. Defaults to false (move to Trash when available)."
    )]
    pub(super) permanent: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(
    description = "Arguments for moving all messages with a specific List-Id to another mailbox."
)]
pub(super) struct MoveListIdArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(description = "The List-Id header value to match (from top_mailing_lists).")]
    pub(super) list_id: String,
    #[schemars(
        description = "Destination mailbox (required). Must already exist; use create_mailbox first if needed."
    )]
    pub(super) destination: String,
    #[schemars(
        description = "Mailbox to search. Omit to use the account-wide mutation plan (the destination itself is excluded)."
    )]
    pub(super) mailbox: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(
    description = "Arguments for moving all messages from an exact sender identity to another mailbox."
)]
pub(super) struct MoveBySenderArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        description = "The sender's exact email address to match (from a top_senders row's address)."
    )]
    pub(super) email: String,
    #[serde(default)]
    #[schemars(
        description = "The sender's exact display name to match (from the row's displayName). Omit or pass \"\" for senders without a display name; matching is exact on both fields."
    )]
    pub(super) name: Option<String>,
    #[schemars(
        description = "Destination mailbox (required). Must already exist; use create_mailbox first if needed."
    )]
    pub(super) destination: String,
    #[schemars(
        description = "Mailbox to search. Omit to use the account-wide mutation plan (the destination itself is excluded)."
    )]
    pub(super) mailbox: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Arguments for moving mail from one exact sender domain.")]
pub(super) struct MoveByDomainArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        description = "Exact canonical domain from a top_domains row. A parent such as example.com never includes mail.example.com."
    )]
    pub(super) domain: String,
    #[schemars(
        description = "Destination mailbox (required). Must already exist; use create_mailbox first if needed."
    )]
    pub(super) destination: String,
    #[schemars(
        description = "Mailbox to search. Omit to use the account-wide mutation plan (the destination itself is excluded)."
    )]
    pub(super) mailbox: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(
    description = "Arguments for moving the exact bulk-mail subscription represented by a top_subscriptions sample."
)]
pub(super) struct MoveSubscriptionArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        description = "Mailbox containing the ranking sample. Map top_subscriptions sample.mailbox here; matching messages are swept account-wide and the destination is excluded."
    )]
    pub(super) mailbox: String,
    #[schemars(
        range(min = 1),
        description = "UID from top_subscriptions sample.uid. The live sample supplies the exact sender and optional List-Id scope."
    )]
    pub(super) uid: u32,
    #[schemars(
        range(min = 1),
        description = "UIDVALIDITY from top_subscriptions sample.uidValidity. The action fails if the sample mailbox UID epoch changed."
    )]
    pub(super) expected_uid_validity: u32,
    #[schemars(
        description = "Destination mailbox (required). Must already exist; use create_mailbox first if needed."
    )]
    pub(super) destination: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Arguments for listing durable MOVE operations awaiting reconciliation.")]
pub(super) struct ListPendingMovesArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Arguments for safely reconciling durable non-native MOVE operations.")]
pub(super) struct ReconcileMovesArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        description = "Optional operationId from list_pending_moves. Omit to reconcile every pending operation for the account."
    )]
    pub(super) operation_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Arguments for previewing or confirming a guarded mailbox rename.")]
pub(super) struct RenameMailboxArgs {
    #[schemars(description = "Account name (required).")]
    pub(super) account: String,
    #[schemars(description = "Existing mailbox name.")]
    pub(super) mailbox: String,
    #[schemars(description = "New mailbox name. The destination must not already exist.")]
    pub(super) new_mailbox: String,
    #[serde(default = "default_false")]
    #[schemars(
        description = "False returns a live preflight only. Set true to perform the rename."
    )]
    pub(super) confirm_rename: bool,
    #[schemars(
        description = "Exact messageCount from the latest preflight; required when confirmRename=true."
    )]
    pub(super) expected_message_count: Option<u32>,
    #[serde(default = "default_false")]
    #[schemars(description = "Acknowledge renaming a special-use mailbox.")]
    pub(super) confirm_special_use: bool,
    #[serde(default = "default_false")]
    #[schemars(
        description = "Acknowledge that the mailbox has descendants whose paths may change."
    )]
    pub(super) confirm_descendants: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Arguments for previewing or confirming a guarded mailbox delete.")]
pub(super) struct DeleteMailboxArgs {
    #[schemars(description = "Account name (required).")]
    pub(super) account: String,
    #[schemars(description = "Mailbox name to delete.")]
    pub(super) mailbox: String,
    #[serde(default = "default_false")]
    #[schemars(
        description = "False returns a live preflight only. Set true to perform the delete."
    )]
    pub(super) confirm_delete: bool,
    #[schemars(
        description = "Exact messageCount from the latest preflight; required when confirmDelete=true."
    )]
    pub(super) expected_message_count: Option<u32>,
    #[serde(default = "default_false")]
    #[schemars(description = "Acknowledge deleting a non-empty mailbox.")]
    pub(super) confirm_non_empty: bool,
    #[serde(default = "default_false")]
    #[schemars(description = "Acknowledge deleting a special-use mailbox.")]
    pub(super) confirm_special_use: bool,
    #[serde(default = "default_false")]
    #[schemars(description = "Acknowledge that the mailbox has descendants.")]
    pub(super) confirm_descendants: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    #[schemars(
        description = "Where to write, resolved against the active session workspace (standalone server: AGENTMAIL_FILE_ROOT). OMIT IT to write to the workspace root — that is the default and is normally what you want. An absolute path is accepted only if it already lies inside the workspace; do not invent one."
    )]
    pub(super) output_dir: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Arguments for saving one exact RFC822 message source to disk.")]
pub(super) struct DownloadMessageSourceArgs {
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
        description = "UIDVALIDITY paired with the UID. The download fails before writing if the mailbox UID epoch changed."
    )]
    pub(super) expected_uid_validity: u32,
    #[schemars(
        description = "Where to write, resolved against the active session workspace (standalone server: AGENTMAIL_FILE_ROOT). OMIT IT to write to the workspace root — that is the default and is normally what you want. An absolute path is accepted only if it already lies inside the workspace; do not invent one."
    )]
    pub(super) output_dir: Option<String>,
    #[schemars(
        description = "Optional portable basename for the saved source. Defaults to {uid}.eml. Path separators, traversal, and reserved filename characters are rejected."
    )]
    pub(super) filename: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(
    description = "Arguments for saving a bounded set of exact RFC822 message sources and one JSON evidence manifest."
)]
pub(super) struct DownloadThreadArgs {
    #[schemars(
        description = "Mailbox containing every UID (required) — the same mailbox that produced expectedUidValidity."
    )]
    pub(super) mailbox: String,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    pub(super) account: String,
    #[schemars(
        length(min = 1, max = 100),
        inner(range(min = 1)),
        description = "One to 100 unique, non-zero IMAP UIDs from the same mailbox UIDVALIDITY epoch. Each source is saved as {uid}.eml."
    )]
    pub(super) uids: Vec<u32>,
    #[schemars(
        range(min = 1),
        description = "UIDVALIDITY paired with every UID. The operation stops if the mailbox UID epoch changed."
    )]
    pub(super) expected_uid_validity: u32,
    #[schemars(
        description = "Where to write, resolved against the active session workspace (standalone server: AGENTMAIL_FILE_ROOT). OMIT IT to write to the workspace root — that is the default and is normally what you want. An absolute path is accepted only if it already lies inside the workspace; do not invent one."
    )]
    pub(super) output_dir: Option<String>,
    #[schemars(
        description = "Optional portable basename for the JSON manifest. Defaults to manifest.json. Existing files are never overwritten."
    )]
    pub(super) manifest_filename: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(
    description = "Seed identity for previewing an exact cross-mailbox RFC Message-ID graph."
)]
pub(super) struct PreviewThreadRecordArgs {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    #[schemars(range(min = 1))]
    pub(super) expected_uid_validity: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(
    description = "Confirmed export of an exact thread preview as PDF, RFC822 sources, and an integrity manifest."
)]
pub(super) struct ExportThreadRecordArgs {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    #[schemars(range(min = 1))]
    pub(super) expected_uid_validity: u32,
    #[schemars(
        length(min = 64, max = 64),
        description = "Exact selectionDigest from the latest preview_thread_record result. The export re-discovers the graph and refuses any drift."
    )]
    pub(super) selection_digest: String,
    #[schemars(
        length(min = 1, max = 4000),
        description = "User-supplied explanation of why the record is being prepared. Printed on the PDF cover and preserved in the manifest."
    )]
    pub(super) purpose: String,
    #[schemars(
        description = "Where to write, resolved against the active session workspace. OMIT IT to write to the workspace root — that is the default and is normally what you want. An absolute path is accepted only if it already lies inside the workspace; do not invent one."
    )]
    pub(super) output_dir: Option<String>,
    #[schemars(
        description = "Optional portable directory name for the new bundle. Existing paths are never overwritten."
    )]
    pub(super) bundle_name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(
    description = "Arguments for adding and/or removing flags and setting or clearing the Apple Mail color on one message."
)]
pub(super) struct UpdateFlagsArgs {
    #[schemars(
        description = "Mailbox containing the message (required) — the same mailbox that produced expectedUidValidity."
    )]
    pub(super) mailbox: String,
    #[schemars(description = "Account name (required).")]
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
        description = "Flags to add. System flags use a backslash prefix (e.g. \"\\\\Seen\", \"\\\\Flagged\"); custom keywords are plain strings. Existing flags are preserved. Cannot include \\\\Deleted or \\\\Recent."
    )]
    pub(super) add: Vec<String>,
    #[serde(default)]
    #[schemars(
        description = "Flags to remove. Only these are removed; every other flag remains. Cannot include \\\\Deleted or \\\\Recent."
    )]
    pub(super) remove: Vec<String>,
    #[schemars(
        description = "Apple Mail color, case-insensitive: red, orange, yellow, green, blue, purple, gray — or \"none\" to clear it. Setting a color sets \\\\Flagged plus the $MailFlagBit keywords and replaces any existing color; omit the field to leave the color untouched."
    )]
    pub(super) color: Option<String>,
}

// ---------------------------------------------------------------------------
// Prompt argument structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct InboxSummaryArgs {
    #[schemars(description = "Account name to summarize.")]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct CleanupSenderArgs {
    #[schemars(description = "Account name.")]
    pub(super) account: String,
    #[schemars(description = "Sender email address or name to clean up.")]
    pub(super) sender: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct FindAttachmentsPromptArgs {
    #[schemars(description = "Account name.")]
    pub(super) account: String,
    #[schemars(description = "Mailbox to search. Defaults to INBOX.")]
    pub(super) mailbox: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ComposeEmailArgs {
    #[schemars(description = "Account name to send from.")]
    pub(super) account: String,
    #[schemars(description = "Recipient email address.")]
    pub(super) to: Option<String>,
    #[schemars(description = "Email subject line.")]
    pub(super) subject: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct UnsubscribeCleanupArgs {
    #[schemars(description = "Account name.")]
    pub(super) account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ListIdCleanupArgs {
    #[schemars(description = "Account name.")]
    pub(super) account: String,
}
