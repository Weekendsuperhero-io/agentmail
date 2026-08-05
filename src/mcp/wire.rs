//! Compact, schema-stable MCP output contracts.
//!
//! Core response types remain useful to Rust callers and may contain fields
//! needed by internal workflows. These DTOs are the narrower public MCP
//! contract: they keep stable UID identities and action audit data while
//! omitting credentials, message bodies, raw list-action headers, and values
//! already present in a response wrapper.

use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
    schemars::JsonSchema,
};
use serde::Serialize;

use super::resources::format_email_uri;
use crate::next_offset;

const MAX_BREAKDOWN_ROWS: usize = 50;
const MAX_LIST_SENDER_PREVIEW: usize = 5;

/// Marker for schema-stable MCP outputs that can be serialized on both result
/// channels.
pub(super) trait WireOutput: Serialize {}

/// Construct a structured value plus the same complete JSON in a text block.
///
/// Some MCP hosts still render only `content`, so truncating or summarizing
/// this fallback silently changes a requested page (historically every ranked
/// result looked like a five-row page). Keeping both representations equal
/// makes `limit` mean the same thing in every host.
pub(super) fn compact_result<T>(output: T) -> Result<CallToolResult, McpError>
where
    T: WireOutput,
{
    let structured = serde_json::to_value(output).map_err(|error| {
        McpError::internal_error(format!("failed to serialize tool result: {error}"), None)
    })?;
    let fallback = serde_json::to_string(&structured).map_err(|error| {
        McpError::internal_error(
            format!("failed to serialize tool result text: {error}"),
            None,
        )
    })?;
    let mut result = CallToolResult::success(vec![ContentBlock::text(fallback)]);
    result.structured_content = Some(structured);
    Ok(result)
}

/// Convert an operational failure into an `isError` tool RESULT, not a JSON-RPC
/// protocol error.
///
/// Tool-execution failures — a UID that no longer exists, a missing mailbox, an
/// IMAP rejection, a consent-required stop — are outcomes the LLM should SEE
/// and react to (e.g. re-run a ranking when a sampled UID went stale), so per
/// the MCP spec they belong in the result with `isError: true`. Returning them
/// as protocol errors (`McpError`) instead made the bridge classify a single
/// bad call as `BackendConnectionFailed` — i.e. report the whole AgentMail
/// backend as down. Protocol errors stay reserved for malformed requests, which
/// the tool handlers validate up front before the operation runs.
pub(super) fn tool_error_result(e: &crate::AgentmailError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(e.to_string())])
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

impl WireOutput for ListAccountsOutput {}

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

impl WireOutput for ListMailboxesOutput {}

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

impl WireOutput for CheckConnectionOutput {}

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

impl WireOutput for ListCapabilitiesOutput {}

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

impl WireOutput for GetMessagesOutput {}

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

impl WireOutput for SearchMessagesOutput {}

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
    pub(super) colors: Vec<ColorCountOutput>,
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

impl WireOutput for ListFlagsOutput {}

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

impl WireOutput for FindAttachmentsOutput {}

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

impl WireOutput for TopSendersOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct DomainRankOutput {
    /// Exact canonical Header From domain. Parent domains and subdomains are
    /// intentionally distinct rows.
    pub(super) domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) registrable_domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) subdomain: Option<String>,
    pub(super) count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) oldest_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) newest_date: Option<String>,
    pub(super) sample: MessageSampleOutput,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct TopDomainsOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) total_messages: u32,
    /// Total exact-domain rows in the pagination universe.
    pub(super) total: usize,
    pub(super) offset: usize,
    pub(super) limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_offset: Option<usize>,
    pub(super) domains: Vec<DomainRankOutput>,
}

impl From<crate::TopDomainsResponse> for TopDomainsOutput {
    fn from(value: crate::TopDomainsResponse) -> Self {
        let account = value.account;
        Self {
            domains: value
                .domains
                .into_iter()
                .map(|domain| DomainRankOutput {
                    domain: domain.domain,
                    registrable_domain: domain.registrable_domain,
                    subdomain: domain.subdomain,
                    count: domain.count,
                    subject: domain.subject,
                    oldest_date: domain.oldest_date.map(|date| date.to_rfc3339()),
                    newest_date: domain.newest_date.map(|date| date.to_rfc3339()),
                    sample: MessageSampleOutput::new(&account, domain.sample),
                })
                .collect(),
            account,
            mailbox: value.mailbox,
            total_messages: value.total_messages,
            total: value.unique_domains,
            offset: value.offset,
            limit: value.limit,
            next_offset: value.next_offset,
        }
    }
}

impl WireOutput for TopDomainsOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct SubscriptionRankOutput {
    pub(super) address: String,
    pub(super) advertised_one_click: bool,
    pub(super) count: u32,
    /// Decoded Subject of the newest (sample) message — what this
    /// subscription's mail actually looks like. Absent when the sample could
    /// not be fetched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) subject: Option<String>,
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
    /// Total ranked rows (unique sender addresses) — the pagination universe.
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
                    advertised_one_click: list.advertised_one_click,
                    count: list.count,
                    subject: list.subject,
                    oldest_date: list.oldest_date.map(|date| date.to_rfc3339()),
                    newest_date: list.newest_date.map(|date| date.to_rfc3339()),
                    sample: MessageSampleOutput::new(&account, list.sample),
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

impl WireOutput for TopSubscriptionsOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct MailingListRankOutput {
    pub(super) list_id: String,
    pub(super) display_name: String,
    pub(super) senders: Vec<String>,
    pub(super) sender_count: usize,
    pub(super) count: u32,
    /// Decoded Subject of the newest (sample) message — what this list's mail
    /// actually looks like. Absent when the sample could not be fetched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) subject: Option<String>,
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
                    subject: list.subject,
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

impl WireOutput for TopMailingListsOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct PerMailboxDeleteOutput {
    pub(super) mailbox: String,
    pub(super) found: usize,
    pub(super) deleted: usize,
    pub(super) failed: usize,
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) operation_ids: Vec<String>,
}

impl From<crate::PerMailboxDeleteResult> for PerMailboxDeleteOutput {
    fn from(value: crate::PerMailboxDeleteResult) -> Self {
        Self {
            mailbox: value.mailbox,
            found: value.found,
            deleted: value.deleted,
            failed: value.failed,
            pending: value.pending,
            needs_attention: value.needs_attention,
            operation_ids: value.operation_ids,
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
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) operation_ids: Vec<String>,
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
            pending: value.pending,
            needs_attention: value.needs_attention,
            operation_ids: value.operation_ids,
            trash_fallback: value.trash_fallback,
            permanent: value.permanent,
        }
    }
}

impl WireOutput for DeleteMessagesOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeleteBySenderOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) sender: String,
    pub(super) found: usize,
    pub(super) deleted: usize,
    pub(super) failed: usize,
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) operation_ids: Vec<String>,
    pub(super) mailboxes: Vec<PerMailboxDeleteOutput>,
    pub(super) mailboxes_total: usize,
    pub(super) mailboxes_truncated: bool,
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
            pending: value.pending,
            needs_attention: value.needs_attention,
            operation_ids: value.operation_ids,
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

impl WireOutput for DeleteBySenderOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeleteByDomainOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    /// One exact canonical domain; no subdomains are included implicitly.
    pub(super) domain: String,
    pub(super) found: usize,
    pub(super) deleted: usize,
    pub(super) failed: usize,
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) operation_ids: Vec<String>,
    pub(super) mailboxes: Vec<PerMailboxDeleteOutput>,
    pub(super) mailboxes_total: usize,
    pub(super) mailboxes_truncated: bool,
    pub(super) skipped: Vec<String>,
    pub(super) skipped_total: usize,
    pub(super) skipped_truncated: bool,
    pub(super) permanent: bool,
}

impl From<crate::DeleteByDomainResponse> for DeleteByDomainOutput {
    fn from(value: crate::DeleteByDomainResponse) -> Self {
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
            domain: value.domain,
            found: value.found,
            deleted: value.deleted,
            failed: value.failed,
            pending: value.pending,
            needs_attention: value.needs_attention,
            operation_ids: value.operation_ids,
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

impl WireOutput for DeleteByDomainOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeleteListIdOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) list_id: String,
    pub(super) found: usize,
    pub(super) deleted: usize,
    pub(super) failed: usize,
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) operation_ids: Vec<String>,
    pub(super) mailboxes: Vec<PerMailboxDeleteOutput>,
    pub(super) mailboxes_total: usize,
    pub(super) mailboxes_truncated: bool,
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
            pending: value.pending,
            needs_attention: value.needs_attention,
            operation_ids: value.operation_ids,
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

impl WireOutput for DeleteListIdOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct PerMailboxMoveOutput {
    pub(super) mailbox: String,
    pub(super) found: usize,
    pub(super) moved: usize,
    pub(super) failed: usize,
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) operation_ids: Vec<String>,
}

impl From<crate::PerMailboxMoveResult> for PerMailboxMoveOutput {
    fn from(value: crate::PerMailboxMoveResult) -> Self {
        Self {
            mailbox: value.mailbox,
            found: value.found,
            moved: value.moved,
            failed: value.failed,
            pending: value.pending,
            needs_attention: value.needs_attention,
            operation_ids: value.operation_ids,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct MoveListIdOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) list_id: String,
    pub(super) destination: String,
    pub(super) found: usize,
    pub(super) moved: usize,
    pub(super) failed: usize,
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) operation_ids: Vec<String>,
    pub(super) mailboxes: Vec<PerMailboxMoveOutput>,
    pub(super) mailboxes_total: usize,
    pub(super) mailboxes_truncated: bool,
    pub(super) skipped: Vec<String>,
    pub(super) skipped_total: usize,
    pub(super) skipped_truncated: bool,
}

impl From<crate::MoveListIdResponse> for MoveListIdOutput {
    fn from(value: crate::MoveListIdResponse) -> Self {
        let mailboxes = value
            .mailboxes
            .into_iter()
            .map(PerMailboxMoveOutput::from)
            .collect();
        let (mailboxes, mailboxes_total, mailboxes_truncated) = truncate_rows(mailboxes);
        let (skipped, skipped_total, skipped_truncated) = truncate_rows(value.skipped);
        Self {
            account: value.account,
            mailbox: value.mailbox,
            list_id: value.list_id,
            destination: value.destination,
            found: value.found,
            moved: value.moved,
            failed: value.failed,
            pending: value.pending,
            needs_attention: value.needs_attention,
            operation_ids: value.operation_ids,
            mailboxes,
            mailboxes_total,
            mailboxes_truncated,
            skipped,
            skipped_total,
            skipped_truncated,
        }
    }
}

impl WireOutput for MoveListIdOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct MoveBySenderOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) sender: String,
    pub(super) destination: String,
    pub(super) found: usize,
    pub(super) moved: usize,
    pub(super) failed: usize,
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) operation_ids: Vec<String>,
    pub(super) mailboxes: Vec<PerMailboxMoveOutput>,
    pub(super) mailboxes_total: usize,
    pub(super) mailboxes_truncated: bool,
    pub(super) skipped: Vec<String>,
    pub(super) skipped_total: usize,
    pub(super) skipped_truncated: bool,
}

impl From<crate::MoveBySenderResponse> for MoveBySenderOutput {
    fn from(value: crate::MoveBySenderResponse) -> Self {
        let mailboxes = value
            .mailboxes
            .into_iter()
            .map(PerMailboxMoveOutput::from)
            .collect();
        let (mailboxes, mailboxes_total, mailboxes_truncated) = truncate_rows(mailboxes);
        let (skipped, skipped_total, skipped_truncated) = truncate_rows(value.skipped);
        Self {
            account: value.account,
            mailbox: value.mailbox,
            sender: value.sender,
            destination: value.destination,
            found: value.found,
            moved: value.moved,
            failed: value.failed,
            pending: value.pending,
            needs_attention: value.needs_attention,
            operation_ids: value.operation_ids,
            mailboxes,
            mailboxes_total,
            mailboxes_truncated,
            skipped,
            skipped_total,
            skipped_truncated,
        }
    }
}

impl WireOutput for MoveBySenderOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct MoveByDomainOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    /// One exact canonical domain; no subdomains are included implicitly.
    pub(super) domain: String,
    pub(super) destination: String,
    pub(super) found: usize,
    pub(super) moved: usize,
    pub(super) failed: usize,
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) operation_ids: Vec<String>,
    pub(super) mailboxes: Vec<PerMailboxMoveOutput>,
    pub(super) mailboxes_total: usize,
    pub(super) mailboxes_truncated: bool,
    pub(super) skipped: Vec<String>,
    pub(super) skipped_total: usize,
    pub(super) skipped_truncated: bool,
}

impl From<crate::MoveByDomainResponse> for MoveByDomainOutput {
    fn from(value: crate::MoveByDomainResponse) -> Self {
        let mailboxes = value
            .mailboxes
            .into_iter()
            .map(PerMailboxMoveOutput::from)
            .collect();
        let (mailboxes, mailboxes_total, mailboxes_truncated) = truncate_rows(mailboxes);
        let (skipped, skipped_total, skipped_truncated) = truncate_rows(value.skipped);
        Self {
            account: value.account,
            mailbox: value.mailbox,
            domain: value.domain,
            destination: value.destination,
            found: value.found,
            moved: value.moved,
            failed: value.failed,
            pending: value.pending,
            needs_attention: value.needs_attention,
            operation_ids: value.operation_ids,
            mailboxes,
            mailboxes_total,
            mailboxes_truncated,
            skipped,
            skipped_total,
            skipped_truncated,
        }
    }
}

impl WireOutput for MoveByDomainOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct MoveSubscriptionOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) sample_mailbox: String,
    pub(super) sample_uid_validity: u32,
    pub(super) sample_uid: u32,
    pub(super) sender: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) list_id: Option<String>,
    pub(super) matched_by: String,
    pub(super) destination: String,
    pub(super) found: usize,
    pub(super) moved: usize,
    pub(super) failed: usize,
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) operation_ids: Vec<String>,
    pub(super) mailboxes: Vec<PerMailboxMoveOutput>,
    pub(super) mailboxes_total: usize,
    pub(super) mailboxes_truncated: bool,
    pub(super) skipped: Vec<String>,
    pub(super) skipped_total: usize,
    pub(super) skipped_truncated: bool,
}

impl From<crate::MoveSubscriptionResponse> for MoveSubscriptionOutput {
    fn from(value: crate::MoveSubscriptionResponse) -> Self {
        let mailboxes = value
            .mailboxes
            .into_iter()
            .map(PerMailboxMoveOutput::from)
            .collect();
        let (mailboxes, mailboxes_total, mailboxes_truncated) = truncate_rows(mailboxes);
        let (skipped, skipped_total, skipped_truncated) = truncate_rows(value.skipped);
        Self {
            account: value.account,
            mailbox: value.mailbox,
            sample_mailbox: value.sample_mailbox,
            sample_uid_validity: value.sample_uid_validity,
            sample_uid: value.sample_uid,
            sender: value.sender,
            list_id: value.list_id,
            matched_by: value.matched_by,
            destination: value.destination,
            found: value.found,
            moved: value.moved,
            failed: value.failed,
            pending: value.pending,
            needs_attention: value.needs_attention,
            operation_ids: value.operation_ids,
            mailboxes,
            mailboxes_total,
            mailboxes_truncated,
            skipped,
            skipped_total,
            skipped_truncated,
        }
    }
}

impl WireOutput for MoveSubscriptionOutput {}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) enum MoveStatusOutput {
    Moved,
    Failed,
    ReconciliationPending,
    NeedsAttention,
}

impl From<crate::MoveStatus> for MoveStatusOutput {
    fn from(value: crate::MoveStatus) -> Self {
        match value {
            crate::MoveStatus::Moved => Self::Moved,
            crate::MoveStatus::Failed => Self::Failed,
            crate::MoveStatus::ReconciliationPending => Self::ReconciliationPending,
            crate::MoveStatus::NeedsAttention => Self::NeedsAttention,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct PendingMoveOutput {
    pub(super) operation_id: String,
    pub(super) source_mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) source_uid_validity: u32,
    #[schemars(range(min = 1))]
    pub(super) source_uid: u32,
    pub(super) destination: String,
    pub(super) status: MoveStatusOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) detail: Option<String>,
    pub(super) created_at: String,
    pub(super) updated_at: String,
}

impl From<crate::PendingMove> for PendingMoveOutput {
    fn from(value: crate::PendingMove) -> Self {
        Self {
            operation_id: value.operation_id,
            source_mailbox: value.source_mailbox,
            source_uid_validity: value.source_uid_validity,
            source_uid: value.source_uid,
            destination: value.destination,
            status: value.status.into(),
            detail: value.detail,
            created_at: value.created_at.to_rfc3339(),
            updated_at: value.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListPendingMovesOutput {
    pub(super) account: String,
    pub(super) operations: Vec<PendingMoveOutput>,
}

impl From<crate::ListPendingMovesResponse> for ListPendingMovesOutput {
    fn from(value: crate::ListPendingMovesResponse) -> Self {
        Self {
            account: value.account,
            operations: value
                .operations
                .into_iter()
                .map(PendingMoveOutput::from)
                .collect(),
        }
    }
}

impl WireOutput for ListPendingMovesOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReconcileMovesOutput {
    pub(super) account: String,
    pub(super) examined: usize,
    pub(super) completed: usize,
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) failed: usize,
    pub(super) operations: Vec<PendingMoveOutput>,
}

impl From<crate::ReconcileMovesResponse> for ReconcileMovesOutput {
    fn from(value: crate::ReconcileMovesResponse) -> Self {
        Self {
            account: value.account,
            examined: value.examined,
            completed: value.completed,
            pending: value.pending,
            needs_attention: value.needs_attention,
            failed: value.failed,
            operations: value
                .operations
                .into_iter()
                .map(PendingMoveOutput::from)
                .collect(),
        }
    }
}

impl WireOutput for ReconcileMovesOutput {}

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
    pub(super) moved: bool,
    pub(super) status: MoveStatusOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) operation_id: Option<String>,
}

impl MoveMessageOutput {
    pub(super) fn new(value: crate::MoveMessageResponse, uid_validity: u32) -> Self {
        Self {
            account: value.account,
            mailbox: value.mailbox,
            uid_validity,
            uid: value.uid,
            destination: value.destination,
            moved: value.moved,
            status: value.status.into(),
            operation_id: value.operation_id,
        }
    }
}

impl WireOutput for MoveMessageOutput {}

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

impl WireOutput for CreateMailboxOutput {}

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

impl WireOutput for CreateDraftOutput {}

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

impl WireOutput for DownloadAttachmentsOutput {}

/// SPF cannot be recomputed from a stored RFC822 message alone: the SMTP
/// client IP and envelope sender are inputs. This optional field is reserved
/// for a future trusted delivery-metadata source; AgentMail never promotes an
/// untrusted `Authentication-Results` header into a local verification claim.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct SpfEvidenceOutput {
    pub(super) result: String,
    pub(super) source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct DkimEvidenceOutput {
    pub(super) result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) detail: Option<String>,
    pub(super) checked_at: String,
}

impl From<crate::DkimVerification> for DkimEvidenceOutput {
    fn from(value: crate::DkimVerification) -> Self {
        Self {
            result: value.result,
            domain: value.domain,
            detail: value.detail,
            checked_at: value.checked_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(inline)]
pub(super) struct DownloadMessageSourceOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid_validity: u32,
    #[schemars(range(min = 1))]
    pub(super) uid: u32,
    pub(super) path: String,
    pub(super) bytes: usize,
    pub(super) sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) date: Option<String>,
    #[serde(rename = "from", skip_serializing_if = "Option::is_none")]
    pub(super) from_header: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) subject: Option<String>,
    pub(super) downloaded_at: String,
    pub(super) dkim: DkimEvidenceOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) spf: Option<SpfEvidenceOutput>,
}

impl From<crate::DownloadedMessageSource> for DownloadMessageSourceOutput {
    fn from(value: crate::DownloadedMessageSource) -> Self {
        Self {
            account: value.account,
            mailbox: value.mailbox,
            uid_validity: value.uid_validity,
            uid: value.uid,
            path: value.path,
            bytes: value.bytes,
            sha256: value.sha256,
            message_id: value.message_id,
            date: value.date.map(|date| date.to_rfc3339()),
            from_header: value.from_header,
            subject: value.subject,
            downloaded_at: value.downloaded_at.to_rfc3339(),
            dkim: value.dkim.into(),
            spf: None,
        }
    }
}

impl WireOutput for DownloadMessageSourceOutput {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct DownloadThreadOutput {
    pub(super) account: String,
    pub(super) mailbox: String,
    #[schemars(range(min = 1))]
    pub(super) uid_validity: u32,
    pub(super) created_at: String,
    pub(super) manifest_path: String,
    pub(super) messages: Vec<DownloadMessageSourceOutput>,
}

impl WireOutput for DownloadThreadOutput {}

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
    pub(super) pending: usize,
    pub(super) needs_attention: usize,
    pub(super) operation_ids: Vec<String>,
    pub(super) mailboxes: Vec<PerMailboxDeleteOutput>,
    pub(super) mailboxes_total: usize,
    pub(super) mailboxes_truncated: bool,
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
            pending: value.pending,
            needs_attention: value.needs_attention,
            operation_ids: value.operation_ids,
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

impl WireOutput for UnsubscribeMessageOutput {}

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

impl WireOutput for AddFlagsOutput {}

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

impl WireOutput for RemoveFlagsOutput {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stale-UID (or any operational) failure is an `isError` tool RESULT, so
    /// the bridge forwards it to the agent instead of classifying it as
    /// `BackendConnectionFailed`, and the actionable message reaches the LLM.
    #[test]
    fn tool_error_result_is_an_iserror_result_carrying_the_message() {
        let result = tool_error_result(&crate::AgentmailError::MessageNotFound(434755));
        assert_eq!(
            result.is_error,
            Some(true),
            "operational failures must be isError results, not protocol errors"
        );
        let json = serde_json::to_string(&result).expect("result serializes");
        assert!(
            json.contains("434755") && json.contains("re-run the ranking"),
            "the actionable message reaches the caller: {json}"
        );
    }

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
    fn fallback_contains_the_complete_structured_result() {
        let result = compact_result(ListAccountsOutput {
            accounts: (0..8)
                .map(|index| AccountOutput {
                    name: format!("account-{index}"),
                    is_default: index == 0,
                })
                .collect(),
        })
        .expect("result should serialize");
        let text = result.content[0]
            .as_text()
            .expect("fallback should be text");
        let fallback: serde_json::Value =
            serde_json::from_str(&text.text).expect("fallback should be JSON");

        assert_eq!(Some(fallback), result.structured_content);
        assert_eq!(
            result.structured_content.as_ref().unwrap()["accounts"]
                .as_array()
                .unwrap()
                .len(),
            8,
            "the text fallback must not silently truncate results to five rows"
        );
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
        assert_ref_free::<TopDomainsOutput>();
        assert_ref_free::<TopSubscriptionsOutput>();
        assert_ref_free::<TopMailingListsOutput>();
        assert_ref_free::<CreateMailboxOutput>();
        assert_ref_free::<DeleteMessagesOutput>();
        assert_ref_free::<DeleteBySenderOutput>();
        assert_ref_free::<DeleteByDomainOutput>();
        assert_ref_free::<DownloadAttachmentsOutput>();
        assert_ref_free::<CreateDraftOutput>();
        assert_ref_free::<MoveMessageOutput>();
        assert_ref_free::<MoveListIdOutput>();
        assert_ref_free::<MoveBySenderOutput>();
        assert_ref_free::<MoveByDomainOutput>();
        assert_ref_free::<MoveSubscriptionOutput>();
        assert_ref_free::<ListPendingMovesOutput>();
        assert_ref_free::<ReconcileMovesOutput>();
        assert_ref_free::<UnsubscribeMessageOutput>();
        assert_ref_free::<DeleteListIdOutput>();
        assert_ref_free::<AddFlagsOutput>();
        assert_ref_free::<RemoveFlagsOutput>();
    }
}
