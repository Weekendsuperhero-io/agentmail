//! Compact, schema-stable MCP output contracts.
//!
//! Core response types remain useful to Rust callers and may contain fields
//! needed by internal workflows. These DTOs are the narrower public MCP
//! contract: they keep stable UID identities and action audit data while
//! omitting credentials, message bodies, raw list-action headers, and values
//! already present in a response wrapper.

use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
    schemars::JsonSchema,
};
use serde::Serialize;

use super::resources::format_email_uri;
use crate::next_offset;

const MAX_FALLBACK_CHARS: usize = 8_000;
const MAX_BREAKDOWN_ROWS: usize = 50;
const MAX_LIST_SENDER_PREVIEW: usize = 5;

/// An MCP output that can provide a short fallback for clients that do not
/// consume `structuredContent`.
pub(super) trait WireOutput: Serialize {
    fn text_summary(&self) -> String;
}

/// Construct one compact text block plus one structured value.
///
/// This deliberately does not call `CallToolResult::structured`, because that
/// constructor also serializes the complete value into a JSON text block.
pub(super) fn compact_result<T>(output: T) -> Result<CallToolResult, McpError>
where
    T: WireOutput,
{
    let summary = truncate_fallback(output.text_summary());
    let structured = serde_json::to_value(output).map_err(|error| {
        McpError::internal_error(format!("failed to serialize tool result: {error}"), None)
    })?;
    let mut result = CallToolResult::success(vec![Content::text(summary)]);
    result.structured_content = Some(structured);
    Ok(result)
}

/// Build the canonical body-resource URI for a UID-valid message identity.
pub(super) fn message_resource_uri(
    account: &str,
    mailbox: &str,
    uid_validity: u32,
    uid: u32,
) -> String {
    format_email_uri(account, mailbox, uid_validity, uid)
}

fn truncate_fallback(value: String) -> String {
    if value.chars().count() <= MAX_FALLBACK_CHARS {
        return value;
    }

    let mut truncated: String = value.chars().take(MAX_FALLBACK_CHARS - 1).collect();
    truncated.push('…');
    truncated
}

fn truncate_rows<T>(mut rows: Vec<T>) -> (Vec<T>, usize, bool) {
    let total = rows.len();
    rows.truncate(MAX_BREAKDOWN_ROWS);
    let truncated = rows.len() < total;
    (rows, total, truncated)
}

fn redact_urls(value: Option<String>) -> Option<String> {
    value.map(|value| {
        let mut redacted = String::with_capacity(value.len());
        let mut remaining = value.as_str();
        loop {
            let http = remaining.find("http://");
            let https = remaining.find("https://");
            let start = match (http, https) {
                (Some(left), Some(right)) => left.min(right),
                (Some(index), None) | (None, Some(index)) => index,
                (None, None) => {
                    redacted.push_str(remaining);
                    break;
                }
            };
            redacted.push_str(&remaining[..start]);
            redacted.push_str("[redacted-url]");
            let url = &remaining[start..];
            let end = url.find(char::is_whitespace).unwrap_or(url.len());
            remaining = &url[end..];
        }
        redacted
    })
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct AccountOutput {
    pub(super) name: String,
    pub(super) is_default: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListAccountsOutput {
    pub(super) accounts: Vec<AccountOutput>,
}

impl From<crate::ListAccountsResponse> for ListAccountsOutput {
    fn from(value: crate::ListAccountsResponse) -> Self {
        Self {
            accounts: value
                .accounts
                .into_iter()
                .map(|account| AccountOutput {
                    name: account.name,
                    is_default: account.is_default,
                })
                .collect(),
        }
    }
}

impl WireOutput for ListAccountsOutput {
    fn text_summary(&self) -> String {
        let names = self
            .accounts
            .iter()
            .map(|account| account.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{} configured account(s): {names}", self.accounts.len())
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct MailboxOutput {
    pub(super) name: String,
    pub(super) total_messages: u32,
    pub(super) unseen_messages: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) delimiter: Option<String>,
    pub(super) no_inferiors: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListMailboxesOutput {
    pub(super) account: String,
    pub(super) offset: usize,
    pub(super) limit: usize,
    pub(super) total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_offset: Option<usize>,
    pub(super) mailboxes: Vec<MailboxOutput>,
}

impl ListMailboxesOutput {
    /// Project a page that was already filtered and bounded before STATUS.
    pub(super) fn new(
        value: crate::ListMailboxesResponse,
        account: &str,
        offset: usize,
        limit: usize,
        total: usize,
    ) -> Self {
        let limit = limit.clamp(1, 500);
        let mailboxes = value
            .mailboxes
            .into_iter()
            .filter(|mailbox| !mailbox.no_select)
            .take(limit)
            .map(|mailbox| MailboxOutput {
                name: mailbox.name,
                total_messages: mailbox.total_messages,
                unseen_messages: mailbox.unseen_messages,
                delimiter: mailbox.delimiter,
                no_inferiors: mailbox.no_inferiors,
                roles: mailbox.roles,
            })
            .collect::<Vec<_>>();
        let end = offset.saturating_add(mailboxes.len());
        Self {
            account: account.to_string(),
            offset,
            limit,
            total,
            next_offset: (end < total).then_some(end),
            mailboxes,
        }
    }
}

impl WireOutput for ListMailboxesOutput {
    fn text_summary(&self) -> String {
        let names = self
            .mailboxes
            .iter()
            .map(|mailbox| mailbox.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} selectable mailbox(es) for {} ({} returned at offset {}): {names}",
            self.total,
            self.account,
            self.mailboxes.len(),
            self.offset
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct CheckConnectionOutput {
    pub(super) account: String,
    pub(super) connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

impl From<crate::ConnectionStatus> for CheckConnectionOutput {
    fn from(value: crate::ConnectionStatus) -> Self {
        Self {
            account: value.account,
            connected: value.connected,
            error: value.error,
        }
    }
}

impl WireOutput for CheckConnectionOutput {
    fn text_summary(&self) -> String {
        if self.connected {
            format!("{} connected successfully", self.account)
        } else {
            format!(
                "{} connection failed: {}",
                self.account,
                self.error.as_deref().unwrap_or("unknown error")
            )
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListCapabilitiesOutput {
    pub(super) account: String,
    pub(super) capabilities: Vec<String>,
}

impl From<crate::ListCapabilitiesResponse> for ListCapabilitiesOutput {
    fn from(value: crate::ListCapabilitiesResponse) -> Self {
        Self {
            account: value.account,
            capabilities: value.capabilities,
        }
    }
}

impl WireOutput for ListCapabilitiesOutput {
    fn text_summary(&self) -> String {
        format!(
            "{} advertises {} IMAP capability value(s): {}",
            self.account,
            self.capabilities.len(),
            self.capabilities.join(", ")
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct MessageMetadataOutput {
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    pub(super) subject: String,
    pub(super) sender: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) date: Option<String>,
    pub(super) flags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) size: Option<u32>,
    pub(super) resource_uri: String,
}

impl MessageMetadataOutput {
    fn new(value: crate::MessageInfo, account: &str, mailbox: &str, uid_validity: u32) -> Self {
        let resource_uri = message_resource_uri(account, mailbox, uid_validity, value.uid);
        Self {
            uid: value.uid,
            subject: value.subject,
            sender: value.sender,
            date: value.date.map(|date| date.to_rfc3339()),
            flags: value.flags,
            size: value.size,
            resource_uri,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct GetMessagesOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid_validity: u32,
    pub(super) offset: usize,
    pub(super) limit: usize,
    pub(super) total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_offset: Option<usize>,
    pub(super) messages: Vec<MessageMetadataOutput>,
}

impl From<crate::GetMessagesResponse> for GetMessagesOutput {
    fn from(value: crate::GetMessagesResponse) -> Self {
        let crate::GetMessagesResponse {
            mailbox,
            account,
            uid_validity,
            offset,
            limit,
            total,
            messages,
        } = value;
        let messages: Vec<MessageMetadataOutput> = messages
            .into_iter()
            .map(|message| MessageMetadataOutput::new(message, &account, &mailbox, uid_validity))
            .collect();
        Self {
            account,
            mailbox,
            uid_validity,
            offset,
            limit,
            total,
            next_offset: next_offset(offset, messages.len(), total),
            messages,
        }
    }
}

impl WireOutput for GetMessagesOutput {
    fn text_summary(&self) -> String {
        let resources = self
            .messages
            .iter()
            .take(5)
            .map(|message| message.resource_uri.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} of {} message(s) from {} at offset {}; UIDVALIDITY {}. Resources: {resources}",
            self.messages.len(),
            self.total,
            self.mailbox,
            self.offset,
            self.uid_validity
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct SearchMessagesOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid_validity: u32,
    pub(super) offset: usize,
    pub(super) limit: usize,
    pub(super) total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_offset: Option<usize>,
    pub(super) messages: Vec<MessageMetadataOutput>,
}

impl From<crate::SearchMessagesResponse> for SearchMessagesOutput {
    fn from(value: crate::SearchMessagesResponse) -> Self {
        let crate::SearchMessagesResponse {
            mailbox,
            account,
            uid_validity,
            offset,
            limit,
            total_matches,
            messages,
        } = value;
        let messages: Vec<MessageMetadataOutput> = messages
            .into_iter()
            .map(|message| MessageMetadataOutput::new(message, &account, &mailbox, uid_validity))
            .collect();
        Self {
            account,
            mailbox,
            uid_validity,
            offset,
            limit,
            total: total_matches,
            next_offset: next_offset(offset, messages.len(), total_matches),
            messages,
        }
    }
}

impl WireOutput for SearchMessagesOutput {
    fn text_summary(&self) -> String {
        let resources = self
            .messages
            .iter()
            .take(5)
            .map(|message| message.resource_uri.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} of {} matching message(s) from {} at offset {}; UIDVALIDITY {}. Resources: {resources}",
            self.messages.len(),
            self.total,
            self.mailbox,
            self.offset,
            self.uid_validity
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct FlagCountOutput {
    pub(super) flag: String,
    pub(super) count: u32,
}

impl From<crate::FlagCount> for FlagCountOutput {
    fn from(value: crate::FlagCount) -> Self {
        Self {
            flag: value.flag,
            count: value.count,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct ColorCountOutput {
    pub(super) color: String,
    pub(super) count: u32,
}

impl From<crate::ColorCount> for ColorCountOutput {
    fn from(value: crate::ColorCount) -> Self {
        Self {
            color: value.color,
            count: value.count,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct MailboxFlagBreakdownOutput {
    pub(super) mailbox: String,
    pub(super) total_flags: usize,
    pub(super) flags: Vec<FlagCountOutput>,
}

impl From<crate::MailboxFlagBreakdown> for MailboxFlagBreakdownOutput {
    fn from(value: crate::MailboxFlagBreakdown) -> Self {
        Self {
            mailbox: value.mailbox,
            total_flags: value.total_flags,
            flags: value.flags.into_iter().map(FlagCountOutput::from).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListFlagsOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) total_flags: usize,
    pub(super) flags: Vec<FlagCountOutput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) colors: Vec<ColorCountOutput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) per_mailbox: Vec<MailboxFlagBreakdownOutput>,
    pub(super) per_mailbox_total: usize,
    pub(super) per_mailbox_truncated: bool,
}

impl From<crate::ListFlagsResponse> for ListFlagsOutput {
    fn from(value: crate::ListFlagsResponse) -> Self {
        let per_mailbox = value
            .per_mailbox
            .into_iter()
            .map(MailboxFlagBreakdownOutput::from)
            .collect();
        let (per_mailbox, per_mailbox_total, per_mailbox_truncated) = truncate_rows(per_mailbox);
        Self {
            account: value.account,
            mailbox: value.mailbox,
            total_flags: value.total_flags,
            flags: value.flags.into_iter().map(FlagCountOutput::from).collect(),
            colors: value
                .colors
                .into_iter()
                .map(ColorCountOutput::from)
                .collect(),
            per_mailbox,
            per_mailbox_total,
            per_mailbox_truncated,
        }
    }
}

impl WireOutput for ListFlagsOutput {
    fn text_summary(&self) -> String {
        let flags = self
            .flags
            .iter()
            .map(|flag| format!("{}={}", flag.flag, flag.count))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} flag occurrence(s) in {} across {} flag value(s): {flags}",
            self.total_flags,
            self.mailbox,
            self.flags.len()
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct AttachmentHitOutput {
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid_validity: u32,
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) date: Option<String>,
    pub(super) resource_uri: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct MailboxAttachmentCountOutput {
    pub(super) mailbox: String,
    pub(super) count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct FindAttachmentsOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) total: usize,
    pub(super) offset: usize,
    pub(super) limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_offset: Option<usize>,
    pub(super) messages: Vec<AttachmentHitOutput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) per_mailbox: Vec<MailboxAttachmentCountOutput>,
    pub(super) per_mailbox_total: usize,
    pub(super) per_mailbox_truncated: bool,
}

impl From<crate::FindAttachmentsResponse> for FindAttachmentsOutput {
    fn from(value: crate::FindAttachmentsResponse) -> Self {
        let account = value.account;
        let messages = value
            .messages
            .into_iter()
            .map(|message| AttachmentHitOutput {
                resource_uri: message_resource_uri(
                    &account,
                    &message.mailbox,
                    message.uid_validity,
                    message.uid,
                ),
                mailbox: message.mailbox,
                uid_validity: message.uid_validity,
                uid: message.uid,
                date: message.date.map(|date| date.to_rfc3339()),
            })
            .collect::<Vec<_>>();
        let end = value.offset.saturating_add(messages.len());
        let next_offset = (end < value.total).then_some(end);
        let per_mailbox = value
            .per_mailbox
            .into_iter()
            .map(|row| MailboxAttachmentCountOutput {
                mailbox: row.mailbox,
                count: row.count,
            })
            .collect();
        let (per_mailbox, per_mailbox_total, per_mailbox_truncated) = truncate_rows(per_mailbox);
        Self {
            account,
            mailbox: value.mailbox,
            total: value.total,
            offset: value.offset,
            limit: value.limit,
            next_offset,
            messages,
            per_mailbox,
            per_mailbox_total,
            per_mailbox_truncated,
        }
    }
}

impl WireOutput for FindAttachmentsOutput {
    fn text_summary(&self) -> String {
        let resources = self
            .messages
            .iter()
            .take(5)
            .map(|message| message.resource_uri.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} of {} attachment-bearing message(s) at offset {}. Resources: {resources}",
            self.messages.len(),
            self.total,
            self.offset
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct MessageSampleOutput {
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid_validity: u32,
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    pub(super) resource_uri: String,
}

impl MessageSampleOutput {
    fn new(account: &str, value: crate::MailboxMessageIdentity) -> Self {
        Self {
            resource_uri: message_resource_uri(
                account,
                &value.mailbox,
                value.uid_validity,
                value.uid,
            ),
            mailbox: value.mailbox,
            uid_validity: value.uid_validity,
            uid: value.uid,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct SenderRankOutput {
    pub(super) address: String,
    pub(super) display_name: String,
    pub(super) count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) oldest_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) newest_date: Option<String>,
    pub(super) sample: MessageSampleOutput,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct TopSendersOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) total_messages: u32,
    /// Total ranked rows (unique senders) — the pagination universe.
    pub(super) total: usize,
    pub(super) offset: usize,
    pub(super) limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_offset: Option<usize>,
    pub(super) senders: Vec<SenderRankOutput>,
}

impl From<crate::TopSendersResponse> for TopSendersOutput {
    fn from(value: crate::TopSendersResponse) -> Self {
        let account = value.account;
        Self {
            senders: value
                .senders
                .into_iter()
                .map(|sender| SenderRankOutput {
                    address: sender.address,
                    display_name: sender.display_name,
                    count: sender.count,
                    oldest_date: sender.oldest_date.map(|date| date.to_rfc3339()),
                    newest_date: sender.newest_date.map(|date| date.to_rfc3339()),
                    sample: MessageSampleOutput::new(&account, sender.sample),
                })
                .collect(),
            account,
            mailbox: value.mailbox,
            total_messages: value.total_messages,
            total: value.unique_senders,
            offset: value.offset,
            limit: value.limit,
            next_offset: value.next_offset,
        }
    }
}

impl WireOutput for TopSendersOutput {
    fn text_summary(&self) -> String {
        let rows = self
            .senders
            .iter()
            .take(5)
            .map(|sender| {
                format!(
                    "{}={} ({})",
                    sender.address, sender.count, sender.sample.resource_uri
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} of {} ranked sender(s) at offset {}: {rows}",
            self.senders.len(),
            self.total,
            self.offset
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct SubscriptionRankOutput {
    pub(super) address: String,
    pub(super) display_name: String,
    pub(super) advertised_one_click: bool,
    pub(super) count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) oldest_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) newest_date: Option<String>,
    pub(super) sample: MessageSampleOutput,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct TopSubscriptionsOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) total_messages: u32,
    /// Total ranked rows (unique lists) — the pagination universe.
    pub(super) total: usize,
    pub(super) offset: usize,
    pub(super) limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_offset: Option<usize>,
    pub(super) lists: Vec<SubscriptionRankOutput>,
}

impl From<crate::TopSubscriptionsResponse> for TopSubscriptionsOutput {
    fn from(value: crate::TopSubscriptionsResponse) -> Self {
        let account = value.account;
        Self {
            lists: value
                .lists
                .into_iter()
                .map(|list| SubscriptionRankOutput {
                    address: list.address,
                    display_name: list.display_name,
                    advertised_one_click: list.advertised_one_click,
                    count: list.count,
                    oldest_date: list.oldest_date.map(|date| date.to_rfc3339()),
                    newest_date: list.newest_date.map(|date| date.to_rfc3339()),
                    sample: MessageSampleOutput::new(&account, list.sample),
                })
                .collect(),
            account,
            mailbox: value.mailbox,
            total_messages: value.total_messages,
            total: value.unique_lists,
            offset: value.offset,
            limit: value.limit,
            next_offset: value.next_offset,
        }
    }
}

impl WireOutput for TopSubscriptionsOutput {
    fn text_summary(&self) -> String {
        let rows = self
            .lists
            .iter()
            .take(5)
            .map(|list| {
                format!(
                    "{}={} oneClick={} ({})",
                    list.address, list.count, list.advertised_one_click, list.sample.resource_uri
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} of {} ranked subscription(s) at offset {}: {rows}",
            self.lists.len(),
            self.total,
            self.offset
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct MailingListRankOutput {
    pub(super) list_id: String,
    pub(super) display_name: String,
    pub(super) senders: Vec<String>,
    pub(super) sender_count: usize,
    pub(super) count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) oldest_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) newest_date: Option<String>,
    pub(super) sample: MessageSampleOutput,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct TopMailingListsOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) total_messages: u32,
    /// Total ranked rows (unique lists) — the pagination universe.
    pub(super) total: usize,
    pub(super) offset: usize,
    pub(super) limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_offset: Option<usize>,
    pub(super) lists: Vec<MailingListRankOutput>,
}

impl From<crate::TopMailingListsResponse> for TopMailingListsOutput {
    fn from(value: crate::TopMailingListsResponse) -> Self {
        let account = value.account;
        Self {
            lists: value
                .lists
                .into_iter()
                .map(|list| MailingListRankOutput {
                    list_id: list.list_id,
                    display_name: list.display_name,
                    senders: list
                        .senders
                        .into_iter()
                        .take(MAX_LIST_SENDER_PREVIEW)
                        .collect(),
                    sender_count: list.sender_count,
                    count: list.count,
                    oldest_date: list.oldest_date.map(|date| date.to_rfc3339()),
                    newest_date: list.newest_date.map(|date| date.to_rfc3339()),
                    sample: MessageSampleOutput::new(&account, list.sample),
                })
                .collect(),
            account,
            mailbox: value.mailbox,
            total_messages: value.total_messages,
            total: value.unique_lists,
            offset: value.offset,
            limit: value.limit,
            next_offset: value.next_offset,
        }
    }
}

impl WireOutput for TopMailingListsOutput {
    fn text_summary(&self) -> String {
        let rows = self
            .lists
            .iter()
            .take(5)
            .map(|list| {
                format!(
                    "{}={} ({})",
                    list.list_id, list.count, list.sample.resource_uri
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} of {} ranked mailing list(s) at offset {}: {rows}",
            self.lists.len(),
            self.total,
            self.offset
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct PerMailboxDeleteOutput {
    pub(super) mailbox: String,
    pub(super) found: usize,
    pub(super) deleted: usize,
    pub(super) failed: usize,
}

impl From<crate::PerMailboxDeleteResult> for PerMailboxDeleteOutput {
    fn from(value: crate::PerMailboxDeleteResult) -> Self {
        Self {
            mailbox: value.mailbox,
            found: value.found,
            deleted: value.deleted,
            failed: value.failed,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeleteMessagesOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) deleted: usize,
    pub(super) failed: usize,
    pub(super) trash_fallback: bool,
    pub(super) permanent: bool,
}

impl From<crate::DeleteMessagesResponse> for DeleteMessagesOutput {
    fn from(value: crate::DeleteMessagesResponse) -> Self {
        Self {
            account: value.account,
            mailbox: value.mailbox,
            deleted: value.deleted,
            failed: value.failed,
            trash_fallback: value.trash_fallback,
            permanent: value.permanent,
        }
    }
}

impl WireOutput for DeleteMessagesOutput {
    fn text_summary(&self) -> String {
        format!(
            "Deleted {} message(s) from {}; {} failed; permanent={}, trashFallback={}",
            self.deleted, self.mailbox, self.failed, self.permanent, self.trash_fallback
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeleteBySenderOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) sender: String,
    pub(super) found: usize,
    pub(super) deleted: usize,
    pub(super) failed: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) mailboxes: Vec<PerMailboxDeleteOutput>,
    pub(super) mailboxes_total: usize,
    pub(super) mailboxes_truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) skipped: Vec<String>,
    pub(super) skipped_total: usize,
    pub(super) skipped_truncated: bool,
    pub(super) permanent: bool,
}

impl From<crate::DeleteBySenderResponse> for DeleteBySenderOutput {
    fn from(value: crate::DeleteBySenderResponse) -> Self {
        let mailboxes = value
            .mailboxes
            .into_iter()
            .map(PerMailboxDeleteOutput::from)
            .collect();
        let (mailboxes, mailboxes_total, mailboxes_truncated) = truncate_rows(mailboxes);
        let (skipped, skipped_total, skipped_truncated) = truncate_rows(value.skipped);
        Self {
            account: value.account,
            mailbox: value.mailbox,
            sender: value.sender,
            found: value.found,
            deleted: value.deleted,
            failed: value.failed,
            mailboxes,
            mailboxes_total,
            mailboxes_truncated,
            skipped,
            skipped_total,
            skipped_truncated,
            permanent: value.permanent,
        }
    }
}

impl WireOutput for DeleteBySenderOutput {
    fn text_summary(&self) -> String {
        format!(
            "Found {} message(s) from {}; deleted {}, failed {}, skipped {} mailbox(es); permanent={}",
            self.found, self.sender, self.deleted, self.failed, self.skipped_total, self.permanent
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeleteListIdOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) list_id: String,
    pub(super) found: usize,
    pub(super) deleted: usize,
    pub(super) failed: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) mailboxes: Vec<PerMailboxDeleteOutput>,
    pub(super) mailboxes_total: usize,
    pub(super) mailboxes_truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) skipped: Vec<String>,
    pub(super) skipped_total: usize,
    pub(super) skipped_truncated: bool,
    pub(super) permanent: bool,
}

impl From<crate::DeleteListIdResponse> for DeleteListIdOutput {
    fn from(value: crate::DeleteListIdResponse) -> Self {
        let mailboxes = value
            .mailboxes
            .into_iter()
            .map(PerMailboxDeleteOutput::from)
            .collect();
        let (mailboxes, mailboxes_total, mailboxes_truncated) = truncate_rows(mailboxes);
        let (skipped, skipped_total, skipped_truncated) = truncate_rows(value.skipped);
        Self {
            account: value.account,
            mailbox: value.mailbox,
            list_id: value.list_id,
            found: value.found,
            deleted: value.deleted,
            failed: value.failed,
            mailboxes,
            mailboxes_total,
            mailboxes_truncated,
            skipped,
            skipped_total,
            skipped_truncated,
            permanent: value.permanent,
        }
    }
}

impl WireOutput for DeleteListIdOutput {
    fn text_summary(&self) -> String {
        format!(
            "Found {} message(s) for List-Id {}; deleted {}, failed {}, skipped {} mailbox(es); permanent={}",
            self.found, self.list_id, self.deleted, self.failed, self.skipped_total, self.permanent
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct MoveMessageOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid_validity: u32,
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    pub(super) destination: String,
}

impl MoveMessageOutput {
    pub(super) fn new(value: crate::MoveMessageResponse, uid_validity: u32) -> Self {
        Self {
            account: value.account,
            mailbox: value.mailbox,
            uid_validity,
            uid: value.uid,
            destination: value.destination,
        }
    }
}

impl WireOutput for MoveMessageOutput {
    fn text_summary(&self) -> String {
        format!(
            "Moved UID {} (UIDVALIDITY {}) from {} to {}",
            self.uid, self.uid_validity, self.mailbox, self.destination
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateMailboxOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) created: bool,
    pub(super) already_exists: bool,
}

impl From<crate::CreateMailboxResponse> for CreateMailboxOutput {
    fn from(value: crate::CreateMailboxResponse) -> Self {
        Self {
            account: value.account,
            mailbox: value.mailbox,
            created: value.created,
            already_exists: value.already_exists,
        }
    }
}

impl WireOutput for CreateMailboxOutput {
    fn text_summary(&self) -> String {
        if self.created {
            format!("Created mailbox {} in {}", self.mailbox, self.account)
        } else if self.already_exists {
            format!(
                "Mailbox {} already exists in {}",
                self.mailbox, self.account
            )
        } else {
            format!(
                "Mailbox {} was not created in {}",
                self.mailbox, self.account
            )
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateDraftOutput {
    pub(super) created: bool,
    pub(super) account: String,
    pub(super) drafts_mailbox: String,
    pub(super) attachment_count: usize,
    /// UIDVALIDITY of the drafts mailbox, when the server let the new
    /// draft's identity be recovered after APPEND.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub(super) uid_validity: Option<u32>,
    /// UID of the created draft, when recoverable.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub(super) uid: Option<u32>,
    /// UIDVALIDITY-safe resource URI of the created draft, when recoverable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) resource_uri: Option<String>,
}

impl From<crate::CreateDraftResponse> for CreateDraftOutput {
    fn from(value: crate::CreateDraftResponse) -> Self {
        let resource_uri = match (value.uid_validity, value.uid) {
            (Some(uid_validity), Some(uid)) => Some(message_resource_uri(
                &value.account,
                &value.drafts_mailbox,
                uid_validity,
                uid,
            )),
            _ => None,
        };
        Self {
            created: value.created,
            account: value.account,
            drafts_mailbox: value.drafts_mailbox,
            attachment_count: value.attachments.len(),
            uid_validity: value.uid_validity,
            uid: value.uid,
            resource_uri,
        }
    }
}

impl WireOutput for CreateDraftOutput {
    fn text_summary(&self) -> String {
        match &self.resource_uri {
            Some(resource_uri) => format!(
                "Created draft in {} for {} with {} attachment(s): {resource_uri}",
                self.drafts_mailbox, self.account, self.attachment_count
            ),
            None => format!(
                "Created draft in {} for {} with {} attachment(s)",
                self.drafts_mailbox, self.account, self.attachment_count
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct DownloadedFileOutput {
    pub(super) index: usize,
    pub(super) filename: String,
    pub(super) path: String,
    pub(super) content_type: String,
    pub(super) size: usize,
}

impl From<crate::DownloadedFile> for DownloadedFileOutput {
    fn from(value: crate::DownloadedFile) -> Self {
        Self {
            index: value.index,
            filename: value.filename,
            path: value.path,
            content_type: value.content_type,
            size: value.size,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct DownloadAttachmentsOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid_validity: u32,
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    pub(super) downloaded: Vec<DownloadedFileOutput>,
}

impl DownloadAttachmentsOutput {
    pub(super) fn new(value: crate::DownloadAttachmentsResponse, uid_validity: u32) -> Self {
        Self {
            account: value.account,
            mailbox: value.mailbox,
            uid_validity,
            uid: value.uid,
            downloaded: value
                .downloaded
                .into_iter()
                .map(DownloadedFileOutput::from)
                .collect(),
        }
    }
}

impl WireOutput for DownloadAttachmentsOutput {
    fn text_summary(&self) -> String {
        let paths = self
            .downloaded
            .iter()
            .take(10)
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Downloaded {} attachment(s) from UID {} (UIDVALIDITY {}) to: {paths}",
            self.downloaded.len(),
            self.uid,
            self.uid_validity
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct UnsubscribeAttemptOutput {
    pub(super) success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct MatchingMessagesOutput {
    pub(super) matched_by: String,
    pub(super) sender: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) list_id: Option<String>,
    pub(super) found: usize,
    pub(super) deleted: usize,
    pub(super) failed: usize,
    pub(super) mailboxes: Vec<PerMailboxDeleteOutput>,
    pub(super) mailboxes_total: usize,
    pub(super) mailboxes_truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) skipped: Vec<String>,
    pub(super) skipped_total: usize,
    pub(super) skipped_truncated: bool,
    pub(super) permanent: bool,
    pub(super) trash_fallback: bool,
    pub(super) complete: bool,
}

impl From<crate::MatchingMessagesResult> for MatchingMessagesOutput {
    fn from(value: crate::MatchingMessagesResult) -> Self {
        let mailboxes = value
            .mailboxes
            .into_iter()
            .map(PerMailboxDeleteOutput::from)
            .collect();
        let (mailboxes, mailboxes_total, mailboxes_truncated) = truncate_rows(mailboxes);
        let (skipped, skipped_total, skipped_truncated) = truncate_rows(value.skipped);
        Self {
            matched_by: value.matched_by,
            sender: value.sender,
            list_id: value.list_id,
            found: value.found,
            deleted: value.deleted,
            failed: value.failed,
            mailboxes,
            mailboxes_total,
            mailboxes_truncated,
            skipped,
            skipped_total,
            skipped_truncated,
            permanent: value.permanent,
            trash_fallback: value.trash_fallback,
            complete: value.complete,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct UnsubscribeMessageOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    #[schemars(range(min = 1))]
    pub(super) uid_validity: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) list_id: Option<String>,
    pub(super) dkim_verified: bool,
    pub(super) list_id_authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) dkim_domain: Option<String>,
    pub(super) unsubscribed: UnsubscribeAttemptOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) matching_messages: Option<MatchingMessagesOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cleanup_skipped_reason: Option<String>,
}

impl From<crate::UnsubscribeResponse> for UnsubscribeMessageOutput {
    fn from(value: crate::UnsubscribeResponse) -> Self {
        Self {
            account: value.account,
            mailbox: value.mailbox,
            uid: value.uid,
            uid_validity: value.uid_validity,
            list_id: value.list_id,
            dkim_verified: value.dkim_verified,
            list_id_authenticated: value.list_id_authenticated,
            dkim_domain: value.dkim_domain,
            unsubscribed: UnsubscribeAttemptOutput {
                success: value.unsubscribed.success,
                http_status: value.unsubscribed.http_status,
                reason: redact_urls(value.unsubscribed.reason),
            },
            matching_messages: value.matching_messages.map(MatchingMessagesOutput::from),
            cleanup_skipped_reason: redact_urls(value.cleanup_skipped_reason),
        }
    }
}

impl WireOutput for UnsubscribeMessageOutput {
    fn text_summary(&self) -> String {
        let cleanup = self.matching_messages.as_ref().map_or_else(
            || "no matching-message cleanup".to_string(),
            |matching| {
                format!(
                    "cleanup found {}, deleted {}, failed {}, complete={}",
                    matching.found, matching.deleted, matching.failed, matching.complete
                )
            },
        );
        format!(
            "Unsubscribe success={}, HTTP status={:?}, DKIM verified={}, List-Id authenticated={}; {cleanup}",
            self.unsubscribed.success,
            self.unsubscribed.http_status,
            self.dkim_verified,
            self.list_id_authenticated
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct AddFlagsOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid_validity: u32,
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    pub(super) flags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) color: Option<String>,
}

impl AddFlagsOutput {
    pub(super) fn new(value: crate::UpdateFlagsResponse, uid_validity: u32) -> Self {
        Self {
            account: value.account,
            mailbox: value.mailbox,
            uid_validity,
            uid: value.uid,
            flags: value.flags,
            color: value.color,
        }
    }
}

impl WireOutput for AddFlagsOutput {
    fn text_summary(&self) -> String {
        format!(
            "Updated UID {} (UIDVALIDITY {}) in {}; flags: {}; color: {}",
            self.uid,
            self.uid_validity,
            self.mailbox,
            self.flags.join(", "),
            self.color.as_deref().unwrap_or("none")
        )
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct RemoveFlagsOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid_validity: u32,
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    pub(super) flags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) color: Option<String>,
}

impl RemoveFlagsOutput {
    pub(super) fn new(value: crate::UpdateFlagsResponse, uid_validity: u32) -> Self {
        Self {
            account: value.account,
            mailbox: value.mailbox,
            uid_validity,
            uid: value.uid,
            flags: value.flags,
            color: value.color,
        }
    }
}

impl WireOutput for RemoveFlagsOutput {
    fn text_summary(&self) -> String {
        format!(
            "Updated UID {} (UIDVALIDITY {}) in {}; flags: {}; color: {}",
            self.uid,
            self.uid_validity,
            self.mailbox,
            self.flags.join(", "),
            self.color.as_deref().unwrap_or("none")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_ref_free<T: JsonSchema>() {
        let schema = serde_json::to_string(&rmcp::schemars::schema_for!(T)).unwrap();
        assert!(
            !schema.contains("\"$ref\""),
            "schema contains $ref: {schema}"
        );
        assert!(
            !schema.contains("\"$defs\""),
            "schema contains $defs: {schema}"
        );
    }

    #[test]
    fn message_resource_uri_encodes_identity_segments() {
        assert_eq!(
            message_resource_uri("work account", "Archive/2026", 77, 42),
            "email://work%20account/Archive%2F2026/77/42"
        );
    }

    #[test]
    fn fallback_is_limited_by_characters_without_splitting_unicode() {
        let value = truncate_fallback("📨".repeat(MAX_FALLBACK_CHARS + 10));

        assert_eq!(value.chars().count(), MAX_FALLBACK_CHARS);
        assert!(value.ends_with('…'));
    }

    #[test]
    fn unsubscribe_failure_redacts_http_urls() {
        assert_eq!(
            redact_urls(Some(
                "request to https://example.test/token?id=secret failed".to_string()
            )),
            Some("request to [redacted-url] failed".to_string())
        );
    }

    #[test]
    fn all_tool_output_schemas_are_ref_free_root_objects() {
        assert_ref_free::<ListAccountsOutput>();
        assert_ref_free::<ListMailboxesOutput>();
        assert_ref_free::<CheckConnectionOutput>();
        assert_ref_free::<ListCapabilitiesOutput>();
        assert_ref_free::<GetMessagesOutput>();
        assert_ref_free::<SearchMessagesOutput>();
        assert_ref_free::<ListFlagsOutput>();
        assert_ref_free::<FindAttachmentsOutput>();
        assert_ref_free::<TopSendersOutput>();
        assert_ref_free::<TopSubscriptionsOutput>();
        assert_ref_free::<TopMailingListsOutput>();
        assert_ref_free::<CreateMailboxOutput>();
        assert_ref_free::<DeleteMessagesOutput>();
        assert_ref_free::<DeleteBySenderOutput>();
        assert_ref_free::<DownloadAttachmentsOutput>();
        assert_ref_free::<CreateDraftOutput>();
        assert_ref_free::<MoveMessageOutput>();
        assert_ref_free::<UnsubscribeMessageOutput>();
        assert_ref_free::<DeleteListIdOutput>();
        assert_ref_free::<AddFlagsOutput>();
        assert_ref_free::<RemoveFlagsOutput>();
    }
}
