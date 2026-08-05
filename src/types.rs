use hashbrown::HashMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A configured IMAP account.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct AccountInfo {
    pub name: String,
    pub host: String,
    pub username: String,
    pub is_default: bool,
}

/// A mailbox on the server with message counts.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
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
    #[serde(default)]
    pub no_select: bool,
    /// `true` when no child mailboxes exist or can be created.
    #[serde(default)]
    pub no_inferiors: bool,
    /// First recognized special-use role, retained for API compatibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// All recognized registered IMAP special-use roles. The current set is
    /// "all", "archive", "drafts", "flagged", "important", "junk",
    /// "memos", "scheduled", "sent", "snoozed", and "trash".
    #[serde(default)]
    pub roles: Vec<String>,
}

/// Metadata for a MIME attachment part (no binary content).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct AttachmentInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub content_type: String,
    pub size: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
}

/// Stable identity for an IMAP message within one mailbox UID epoch.
///
/// A numeric UID is never reusable without its mailbox and UIDVALIDITY.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct MailboxMessageIdentity {
    pub mailbox: String,
    pub uid_validity: u32,
    pub uid: u32,
}

/// A parsed email message.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
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
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,

    // MIME structure
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentInfo>,

    // All headers (raw original values)
    #[serde(default)]
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
    /// Internal-date lower bound (inclusive) — IMAP `SINCE`.
    pub since: Option<chrono::NaiveDate>,
    /// Internal-date upper bound (exclusive) — IMAP `BEFORE`.
    pub before: Option<chrono::NaiveDate>,
    /// Minimum RFC822 size in octets — IMAP `LARGER`.
    pub larger_than: Option<u32>,
    /// Maximum RFC822 size in octets — IMAP `SMALLER`.
    pub smaller_than: Option<u32>,
}

/// Summary of messages from a single sender address.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct SenderSummary {
    /// Combined `"Display Name <email>"` for direct use in search.
    pub sender: String,
    /// Normalized email address (lowercase).
    pub address: String,
    /// Display name from the most recent message.
    pub display_name: String,
    /// Newest message that can be safely inspected or used by a later action.
    pub sample: MailboxMessageIdentity,
    /// Number of messages from this sender.
    pub count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_date: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_date: Option<DateTime<Utc>>,
}

/// Summary of mailing-list messages grouped by normalized sender email.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct ListSummary {
    /// Normalized sender email address and sole grouping key.
    pub address: String,
    /// Whether the newest message advertises syntactically valid RFC 8058
    /// one-click headers. Execution still re-fetches the message and requires
    /// a passing DKIM signature over both headers.
    pub advertised_one_click: bool,
    /// Newest message that can be safely passed to unsubscribe_message.
    pub sample: MailboxMessageIdentity,
    /// Decoded Subject of the sample message, fetched live at page time so a
    /// caller can see WHAT the subscription is before acting. Never persisted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Number of messages from this sender with list-action headers.
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
    /// UIDVALIDITY epoch shared by every message UID in this response.
    pub uid_validity: u32,
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
    /// UIDVALIDITY epoch checked before fetching the requested UIDs.
    pub uid_validity: u32,
    pub messages: Vec<MessageInfo>,
}

/// Response for search_messages.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchMessagesResponse {
    pub mailbox: String,
    pub account: String,
    /// UIDVALIDITY epoch shared by every message UID in this response.
    pub uid_validity: u32,
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
    pub colors: Vec<ColorCount>,
    pub per_mailbox: Vec<MailboxFlagBreakdown>,
}

/// A flag name with its count.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct FlagCount {
    pub flag: String,
    pub count: u32,
}

/// A resolved Apple Mail color with its count.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct ColorCount {
    pub color: String,
    pub count: u32,
}

/// Per-mailbox flag breakdown.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
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
    pub messages: Vec<AttachmentMessage>,
    pub per_mailbox: Vec<MailboxAttachmentCount>,
}

/// A mailbox-safe attachment search hit.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct AttachmentMessage {
    pub mailbox: String,
    pub uid_validity: u32,
    pub uid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<DateTime<Utc>>,
}

/// Per-mailbox attachment count.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct MailboxAttachmentCount {
    pub mailbox: String,
    pub count: usize,
}

/// Response for top_senders.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TopSendersResponse {
    pub mailbox: String,
    pub account: String,
    pub total_messages: u32,
    pub unique_senders: usize,
    pub offset: usize,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    pub senders: Vec<SenderSummary>,
}

/// Summary of messages grouped by the exact normalized Header From domain.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct DomainSummary {
    pub domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registrable_domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdomain: Option<String>,
    pub count: u32,
    pub sample: MailboxMessageIdentity,
    /// Decoded subject of the live sample. Subjects are never persisted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_date: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_date: Option<DateTime<Utc>>,
}

/// Response for top_domains.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TopDomainsResponse {
    pub mailbox: String,
    pub account: String,
    pub total_messages: u32,
    pub unique_domains: usize,
    pub offset: usize,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    pub domains: Vec<DomainSummary>,
}

/// Response for top_subscriptions.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TopSubscriptionsResponse {
    pub mailbox: String,
    pub account: String,
    pub total_messages: u32,
    pub unique_senders: usize,
    pub offset: usize,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    pub lists: Vec<ListSummary>,
}

/// Summary of messages grouped by List-Id (RFC 2919).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct ListIdSummary {
    /// The List-Id header value (grouping key).
    pub list_id: String,
    /// Display name extracted from the List-Id (text before angle brackets).
    pub display_name: String,
    /// Unique sender addresses seen for this list (for context).
    pub senders: Vec<String>,
    /// Total unique senders, including senders omitted from the preview.
    pub sender_count: usize,
    /// Number of messages with this List-Id.
    pub count: u32,
    /// Newest message that can be safely inspected.
    pub sample: MailboxMessageIdentity,
    /// Decoded Subject of the sample message, fetched live at page time so a
    /// caller can see WHAT the list is before acting on it. Never persisted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_date: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_date: Option<DateTime<Utc>>,
}

/// Response for top_mailing_lists.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TopMailingListsResponse {
    pub mailbox: String,
    pub account: String,
    pub total_messages: u32,
    pub unique_lists: usize,
    pub offset: usize,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
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
    pub pending: usize,
    pub needs_attention: usize,
    pub operation_ids: Vec<String>,
    pub mailboxes: Vec<PerMailboxDeleteResult>,
    pub skipped: Vec<String>,
    /// True when the caller requested a permanent delete (Trash bypassed on
    /// standard IMAP; Gmail safely routes the request through Trash).
    pub permanent: bool,
}

// ============================================================================
// Write tool responses
// ============================================================================

/// How a delete should dispose of messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeleteMode {
    /// Move to the account's Trash mailbox when one exists, else permanently
    /// delete (flag `\Deleted` + UID EXPUNGE).
    #[default]
    TrashFirst,
    /// Always permanently delete, bypassing Trash. Irreversible.
    Permanent,
}

/// When matching-message cleanup may run relative to the unsubscribe attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CleanupWhen {
    /// Only after the one-click POST verifiably succeeded.
    #[default]
    AfterSuccess,
    /// Even when DKIM, URL validation, DNS, or the HTTPS POST failed. Cleanup
    /// identity requirements (authenticated List-Id or the sender fallback)
    /// still apply unchanged.
    Always,
}

/// Which identity cleanup may match messages by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CleanupIdentityMode {
    /// Only a DKIM-authenticated List-Id; skip cleanup when there is none.
    ListIdOnly,
    /// Prefer the authenticated List-Id; when there is none, fall back to
    /// messages from the exact sender email that carry List-Unsubscribe-Post.
    /// When the target has one usable List-Id, require that same List-Id too.
    #[default]
    ListIdOrSender,
}

/// How cleanup disposes of matched messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CleanupDeletion {
    /// Move to Trash; skip cleanup when no Trash mailbox is resolvable.
    #[default]
    Trash,
    /// Move to Trash, but permit an irreversible UID EXPUNGE when Trash is
    /// unavailable or the MOVE fails. Never used on Gmail, where in-place
    /// EXPUNGE only removes the current label.
    TrashThenPermanent,
    /// Permanently delete (flag `\Deleted` + UID EXPUNGE), bypassing Trash.
    /// On Gmail this safely routes through Trash instead.
    Permanent,
}

/// Matching-message cleanup policy. The three axes are orthogonal; every
/// combination is meaningful, so no cross-field validation exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CleanupPolicy {
    pub when: CleanupWhen,
    pub identity: CleanupIdentityMode,
    pub deletion: CleanupDeletion,
}

impl CleanupPolicy {
    /// The `DeleteMode` this deletion policy requests.
    pub fn mode(self) -> DeleteMode {
        match self.deletion {
            CleanupDeletion::Permanent => DeleteMode::Permanent,
            CleanupDeletion::Trash | CleanupDeletion::TrashThenPermanent => DeleteMode::TrashFirst,
        }
    }

    /// Whether a failed Trash resolution or MOVE may escalate to EXPUNGE.
    pub fn allow_permanent_fallback(self) -> bool {
        self.deletion == CleanupDeletion::TrashThenPermanent
    }
}

/// Safety policy for an authenticated one-click unsubscribe and optional
/// matching-message cleanup. `cleanup: None` means unsubscribe only — the
/// nested policy exists exactly when cleanup was requested, so contradictory
/// flag combinations are unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsubscribeOptions {
    /// UIDVALIDITY observed when the sample UID was discovered.
    pub expected_uid_validity: u32,
    /// Explicit RFC 8058 user consent. Execution refuses `false` rather than
    /// treating a tool annotation as authorization.
    pub confirm_one_click: bool,
    /// Delete matching messages after the POST attempt, under this policy.
    pub cleanup: Option<CleanupPolicy>,
}

/// Response for delete_messages.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteMessagesResponse {
    pub mailbox: String,
    pub account: String,
    pub deleted: usize,
    pub failed: usize,
    /// COPY reached an ambiguity-safe journal state and needs reconciliation.
    pub pending: usize,
    /// Operations whose automatic recovery could not prove a unique safe path.
    pub needs_attention: usize,
    /// Durable operation identifiers for pending/attention items.
    pub operation_ids: Vec<String>,
    /// True when configured trash mailbox was unavailable and deletion
    /// fell back to flag+expunge (permanent delete).
    pub trash_fallback: bool,
    /// True when the caller requested a permanent delete (Trash bypassed on
    /// standard IMAP; Gmail safely routes the request through Trash).
    pub permanent: bool,
}

/// Per-mailbox deletion result (shared by delete_by_sender and unsubscribe_message).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct PerMailboxDeleteResult {
    pub mailbox: String,
    pub found: usize,
    pub deleted: usize,
    pub failed: usize,
    pub pending: usize,
    pub needs_attention: usize,
    pub operation_ids: Vec<String>,
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
    pub pending: usize,
    pub needs_attention: usize,
    pub operation_ids: Vec<String>,
    pub mailboxes: Vec<PerMailboxDeleteResult>,
    /// Mailboxes that could not be selected or searched (skipped during scan).
    pub skipped: Vec<String>,
    /// True when the caller requested a permanent delete (Trash bypassed).
    pub permanent: bool,
}

/// Response for delete_by_domain. The selector is one exact canonical domain;
/// subdomains are separate and are never included implicitly.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeleteByDomainResponse {
    pub mailbox: String,
    pub account: String,
    pub domain: String,
    pub found: usize,
    pub deleted: usize,
    pub failed: usize,
    pub pending: usize,
    pub needs_attention: usize,
    pub operation_ids: Vec<String>,
    pub mailboxes: Vec<PerMailboxDeleteResult>,
    pub skipped: Vec<String>,
    pub permanent: bool,
}

/// Durable outcome of one move request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum MoveStatus {
    Moved,
    Failed,
    ReconciliationPending,
    NeedsAttention,
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
    pub status: MoveStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}

/// Per-mailbox result of a bulk move.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct PerMailboxMoveResult {
    pub mailbox: String,
    pub found: usize,
    pub moved: usize,
    pub failed: usize,
    pub pending: usize,
    pub needs_attention: usize,
    pub operation_ids: Vec<String>,
}

/// Response for move_list_id.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoveListIdResponse {
    /// `*` when the account-wide mutation plan was swept.
    pub mailbox: String,
    pub account: String,
    pub list_id: String,
    pub destination: String,
    pub found: usize,
    pub moved: usize,
    pub failed: usize,
    pub pending: usize,
    pub needs_attention: usize,
    pub operation_ids: Vec<String>,
    pub mailboxes: Vec<PerMailboxMoveResult>,
    /// Mailboxes that could not be selected or searched (skipped during scan).
    pub skipped: Vec<String>,
}

/// Response for move_by_sender.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoveBySenderResponse {
    /// `*` when the account-wide mutation plan was swept.
    pub mailbox: String,
    pub account: String,
    pub sender: String,
    pub destination: String,
    pub found: usize,
    pub moved: usize,
    pub failed: usize,
    pub pending: usize,
    pub needs_attention: usize,
    pub operation_ids: Vec<String>,
    pub mailboxes: Vec<PerMailboxMoveResult>,
    pub skipped: Vec<String>,
}

/// Response for move_by_domain.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoveByDomainResponse {
    pub mailbox: String,
    pub account: String,
    pub domain: String,
    pub destination: String,
    pub found: usize,
    pub moved: usize,
    pub failed: usize,
    pub pending: usize,
    pub needs_attention: usize,
    pub operation_ids: Vec<String>,
    pub mailboxes: Vec<PerMailboxMoveResult>,
    pub skipped: Vec<String>,
}

/// Response for move_subscription.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MoveSubscriptionResponse {
    /// Always `*`: subscription rows are account-wide identities and the
    /// destination mailbox is excluded from the sweep.
    pub mailbox: String,
    pub account: String,
    /// Mailbox from the UIDVALIDITY-guarded ranking sample used to derive the
    /// live sender and optional List-Id scope.
    pub sample_mailbox: String,
    pub sample_uid_validity: u32,
    pub sample_uid: u32,
    pub sender: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_id: Option<String>,
    /// Human-readable statement of the exact live predicate used.
    pub matched_by: String,
    pub destination: String,
    pub found: usize,
    pub moved: usize,
    pub failed: usize,
    pub pending: usize,
    pub needs_attention: usize,
    pub operation_ids: Vec<String>,
    pub mailboxes: Vec<PerMailboxMoveResult>,
    pub skipped: Vec<String>,
}

/// One durable non-native MOVE operation awaiting cleanup or review.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct PendingMove {
    pub operation_id: String,
    pub source_mailbox: String,
    pub source_uid_validity: u32,
    pub source_uid: u32,
    pub destination: String,
    pub status: MoveStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Response for list_pending_moves.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListPendingMovesResponse {
    pub account: String,
    pub operations: Vec<PendingMove>,
}

/// Response for reconcile_moves.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileMovesResponse {
    pub account: String,
    pub examined: usize,
    pub completed: usize,
    pub pending: usize,
    pub needs_attention: usize,
    pub failed: usize,
    pub operations: Vec<PendingMove>,
}

/// Response for create_mailbox.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateMailboxResponse {
    pub account: String,
    pub mailbox: String,
    pub created: bool,
    /// True when mailbox already existed (CREATE was skipped).
    pub already_exists: bool,
}

/// Recipients for a draft email.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct DraftRecipients {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

/// Attachment data for creating drafts (internal; bytes already loaded).
#[derive(Debug, Clone)]
pub struct DraftAttachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
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
    #[serde(default)]
    pub attachments: Vec<String>,
    /// UIDVALIDITY of the drafts mailbox when the new draft's identity could
    /// be recovered (best-effort — async-imap exposes no APPENDUID).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid_validity: Option<u32>,
    /// UID of the created draft, when recoverable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
}

/// A downloaded attachment file.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
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

/// Result of a local DKIM verification against the sender's DNS-published key.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct DkimVerification {
    /// `pass`, `fail`, `neutral`, `permError`, `tempError`, or `none`.
    pub result: String,
    /// Signing domain from the selected signature, when one was parseable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Human-readable detail for non-pass and multi-signature outcomes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// UTC time at which AgentMail performed the DNS-backed verification.
    pub checked_at: DateTime<Utc>,
}

/// One exact RFC822 message saved server-side for archival or evidence use.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub struct DownloadedMessageSource {
    pub account: String,
    pub mailbox: String,
    pub uid_validity: u32,
    pub uid: u32,
    /// Absolute path of the newly created `.eml` file.
    pub path: String,
    pub bytes: usize,
    /// Lowercase hexadecimal SHA-256 of the exact bytes written to `path`.
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<DateTime<Utc>>,
    #[serde(rename = "from", skip_serializing_if = "Option::is_none")]
    pub from_header: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub downloaded_at: DateTime<Utc>,
    pub dkim: DkimVerification,
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
#[schemars(inline)]
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
#[schemars(inline)]
pub struct MatchingMessagesResult {
    pub matched_by: String,
    pub sender: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_id: Option<String>,
    pub found: usize,
    pub deleted: usize,
    pub failed: usize,
    pub pending: usize,
    pub needs_attention: usize,
    pub operation_ids: Vec<String>,
    pub mailboxes: Vec<PerMailboxDeleteResult>,
    /// Mailboxes that could not be selected or searched (skipped during scan).
    pub skipped: Vec<String>,
    /// True when cleanup actually used UID EXPUNGE rather than Trash. Gmail's
    /// safe provider-specific permanent request remains false here because it
    /// moves to Trash.
    pub permanent: bool,
    /// True when a failed or unavailable Trash path used an explicitly
    /// authorized permanent fallback.
    pub trash_fallback: bool,
    /// False when any mailbox was skipped or any matching UID failed.
    pub complete: bool,
}

/// Response for unsubscribe_message.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnsubscribeResponse {
    pub mailbox: String,
    pub account: String,
    pub uid: u32,
    /// Live UIDVALIDITY that was checked before the action.
    pub uid_validity: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_unsubscribe: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_unsubscribe_post: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pathway: Option<String>,
    /// True only when a cryptographic DKIM verification passed and its h= tag
    /// covered both RFC 8058 list headers.
    pub dkim_verified: bool,
    /// True only when the same passing DKIM signature also covered the single
    /// List-Id used as an optional account-wide cleanup identity.
    pub list_id_authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dkim_domain: Option<String>,
    pub unsubscribed: UnsubscribeResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matching_messages: Option<MatchingMessagesResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_skipped_reason: Option<String>,
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
