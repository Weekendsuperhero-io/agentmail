//! MCP resources: navigable account/mailbox catalogs and addressable messages.
//!
//! Six URI templates are exposed. The mailbox template is a paged discovery
//! view; every message URI carries the complete IMAP identity so a delayed
//! read cannot silently use a recycled UID:
//! - `email://{account}/{mailbox}{?offset,limit}` — newest message metadata
//! - `email://{account}/{mailbox}/{uidValidity}/{uid}` — markdown body
//! - `email://{account}/{mailbox}/{uidValidity}/{uid}/headers` — exact headers
//! - `email://{account}/{mailbox}/{uidValidity}/{uid}/source` — raw RFC822
//! - `email://{account}/{mailbox}/{uidValidity}/{uid}/info` — JSON metadata
//!   (subject, sender, date, flags, attachment inventory, sibling URIs)
//! - `email://{account}/{mailbox}/{uidValidity}/{uid}/attachments/{index}` —
//!   one MIME attachment as a blob, addressed by zero-based part index
//!
//! Attachment parts follow one naming nomenclature everywhere: the canonical
//! filename is `{uid}_{index}_{sanitized-original-name}` ("unnamed" when the
//! part has no name), identical to what `download_attachments` writes to disk.
//!
//! Account and mailbox are percent-encoded URI segments; a `/` inside a
//! mailbox name (hierarchy delimiter) must be encoded as `%2F` so it cannot
//! be confused with the URI segment separator.

use super::{AgentMailServer, to_mcp_error};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use rmcp::ErrorData as McpError;
use rmcp::model::{
    Annotations, CompleteRequestParams, CompleteResult, CompletionContext, CompletionInfo,
    ReadResourceResult, Resource, ResourceContents, ResourceTemplate, Role,
};

pub(super) const EMAIL_MAILBOX_TEMPLATE: &str = "email://{account}/{mailbox}{?offset,limit}";

pub(super) const EMAIL_BODY_TEMPLATE: &str = "email://{account}/{mailbox}/{uidValidity}/{uid}";
pub(super) const EMAIL_HEADERS_TEMPLATE: &str =
    "email://{account}/{mailbox}/{uidValidity}/{uid}/headers";
pub(super) const EMAIL_SOURCE_TEMPLATE: &str =
    "email://{account}/{mailbox}/{uidValidity}/{uid}/source";
pub(super) const EMAIL_INFO_TEMPLATE: &str = "email://{account}/{mailbox}/{uidValidity}/{uid}/info";
pub(super) const EMAIL_ATTACHMENT_TEMPLATE: &str =
    "email://{account}/{mailbox}/{uidValidity}/{uid}/attachments/{index}";

const DEFAULT_MAILBOX_PAGE_LIMIT: usize = 25;
const MAX_MAILBOX_PAGE_LIMIT: usize = 50;

// Identity of the two representations tool results LINK to (`wire.rs`
// `message_resource_links`). Shared with `email_resource_templates` below so a
// link and the template it instantiates cannot disagree about what a URI is —
// agreement is structural, and the LITERAL values are pinned on both sides by
// `the_linked_representations_advertise_their_published_identity` here and
// `a_row_with_a_resource_uri_emits_body_and_info_links` in `wire.rs`.
pub(super) const EMAIL_BODY_NAME: &str = "email-message";
pub(super) const EMAIL_BODY_TITLE: &str = "Email message (markdown)";
pub(super) const EMAIL_BODY_MIME: &str = "text/markdown";
pub(super) const EMAIL_INFO_NAME: &str = "email-message-info";
pub(super) const EMAIL_INFO_TITLE: &str = "Email message info (JSON metadata)";
pub(super) const EMAIL_INFO_MIME: &str = "application/json";

const MAX_BODY_CHARS: usize = 100_000;
const MAX_HEADERS_BYTES: usize = 64 * 1024;
const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_ATTACHMENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TRANSIENT_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Characters percent-encoded inside a single URI segment. `/` is the
/// critical one (mailbox hierarchy vs. URI segment separator) and `%` must
/// be escaped for round-tripping; the rest are URI delimiters or unsafe
/// ASCII. Non-ASCII bytes (UTF-8 mailbox names) are always encoded.
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'%')
    .add(b'/')
    .add(b'?')
    .add(b'#')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'|');

pub(super) fn encode_segment(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT).to_string()
}

pub(super) fn assistant_annotations(priority: f32) -> Annotations {
    Annotations::default()
        .with_audience(vec![Role::Assistant])
        .with_priority(priority)
}

pub(super) fn user_and_assistant_annotations(priority: f32) -> Annotations {
    Annotations::default()
        .with_audience(vec![Role::User, Role::Assistant])
        .with_priority(priority)
}

pub(super) fn account_resource_uri(account: &str) -> String {
    format!("email://{}", encode_segment(account))
}

fn mailbox_resource_uri(account: &str, mailbox: &str) -> String {
    format!(
        "email://{}/{}",
        encode_segment(account),
        encode_segment(mailbox)
    )
}

pub(super) fn account_resources(accounts: impl IntoIterator<Item = String>) -> Vec<Resource> {
    accounts
        .into_iter()
        .map(|account| {
            Resource::new(account_resource_uri(&account), format!("{account} mail"))
                .with_title(format!("AgentMail account: {account}"))
                .with_description(
                    "Selectable mailbox catalog. Read this resource to discover mailbox resource URIs.",
                )
                .with_mime_type("application/json")
                .with_annotations(assistant_annotations(0.8))
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct MailboxIndexUri {
    account: String,
    mailbox: String,
    offset: usize,
    limit: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum CatalogResourceUri {
    Account(String),
    Mailbox(MailboxIndexUri),
}

fn parse_catalog_uri(uri: &str) -> Result<Option<CatalogResourceUri>, String> {
    let Some(rest) = uri.strip_prefix("email://") else {
        return Ok(None);
    };
    let (path, query) = rest
        .split_once('?')
        .map_or((rest, None), |(path, query)| (path, Some(query)));
    let segments: Vec<&str> = path.split('/').collect();
    match segments.as_slice() {
        [account] if query.is_none() => {
            let account = decode_segment(account)
                .ok_or_else(|| format!("invalid account segment in {uri}"))?;
            Ok(Some(CatalogResourceUri::Account(account)))
        }
        [account, mailbox] => {
            let account = decode_segment(account)
                .ok_or_else(|| format!("invalid account segment in {uri}"))?;
            let mailbox = decode_segment(mailbox)
                .ok_or_else(|| format!("invalid mailbox segment in {uri}"))?;
            let mut offset = 0usize;
            let mut limit = DEFAULT_MAILBOX_PAGE_LIMIT;
            let mut saw_offset = false;
            let mut saw_limit = false;
            if let Some(query) = query.filter(|query| !query.is_empty()) {
                for pair in query.split('&') {
                    let (key, value) = pair
                        .split_once('=')
                        .ok_or_else(|| format!("invalid mailbox resource query in {uri}"))?;
                    match key {
                        "offset" if !saw_offset => {
                            offset = value.parse().map_err(|_| {
                                format!("offset must be an unsigned integer in {uri}")
                            })?;
                            saw_offset = true;
                        }
                        "limit" if !saw_limit => {
                            limit = value.parse().map_err(|_| {
                                format!("limit must be an unsigned integer in {uri}")
                            })?;
                            saw_limit = true;
                        }
                        "offset" | "limit" => {
                            return Err(format!("duplicate query parameter in {uri}"));
                        }
                        _ => return Err(format!("unsupported query parameter '{key}' in {uri}")),
                    }
                }
            }
            if offset > 1_000_000 {
                return Err(format!("offset exceeds 1000000 in {uri}"));
            }
            if !(1..=MAX_MAILBOX_PAGE_LIMIT).contains(&limit) {
                return Err(format!(
                    "limit must be between 1 and {MAX_MAILBOX_PAGE_LIMIT} in {uri}"
                ));
            }
            Ok(Some(CatalogResourceUri::Mailbox(MailboxIndexUri {
                account,
                mailbox,
                offset,
                limit,
            })))
        }
        _ => Ok(None),
    }
}

fn decode_segment(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    percent_decode_str(s)
        .decode_utf8()
        .ok()
        .map(std::borrow::Cow::into_owned)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EmailResourceKind {
    Body,
    Headers,
    Source,
    Info,
    /// One MIME attachment part, addressed by zero-based index.
    Attachment(usize),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct EmailResourceUri {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) uid_validity: u32,
    pub(super) uid: u32,
    pub(super) kind: EmailResourceKind,
}

/// Build the canonical markdown resource URI for a live message identity.
///
/// Account and mailbox are encoded as individual URI segments. Callers must
/// only pass non-zero UIDVALIDITY and UID values obtained from a live read.
pub(super) fn format_email_uri(
    account: &str,
    mailbox: &str,
    uid_validity: u32,
    uid: u32,
) -> String {
    format_email_uri_for_kind(account, mailbox, uid_validity, uid, EmailResourceKind::Body)
}

fn format_email_uri_for_kind(
    account: &str,
    mailbox: &str,
    uid_validity: u32,
    uid: u32,
    kind: EmailResourceKind,
) -> String {
    let mut uri = format!(
        "email://{}/{}/{uid_validity}/{uid}",
        encode_segment(account),
        encode_segment(mailbox)
    );
    match kind {
        EmailResourceKind::Body => {}
        EmailResourceKind::Headers => uri.push_str("/headers"),
        EmailResourceKind::Source => uri.push_str("/source"),
        EmailResourceKind::Info => uri.push_str("/info"),
        EmailResourceKind::Attachment(index) => {
            uri.push_str("/attachments/");
            uri.push_str(&index.to_string());
        }
    }
    uri
}

pub(super) fn parse_email_uri(uri: &str) -> Result<EmailResourceUri, String> {
    let Some(rest) = uri.strip_prefix("email://") else {
        return Err(format!(
            "unsupported resource URI (expected email:// scheme): {uri}"
        ));
    };
    let segments: Vec<&str> = rest.split('/').collect();
    let (account, mailbox, uid_validity, uid, kind) = match segments.as_slice() {
        [a, m, v, u] => (a, m, v, u, EmailResourceKind::Body),
        [a, m, v, u, "headers"] => (a, m, v, u, EmailResourceKind::Headers),
        [a, m, v, u, "source"] => (a, m, v, u, EmailResourceKind::Source),
        [a, m, v, u, "info"] => (a, m, v, u, EmailResourceKind::Info),
        [a, m, v, u, "attachments", index] => {
            let index: usize = index.parse().map_err(|_| {
                format!("invalid attachment index segment (expected unsigned integer) in {uri}")
            })?;
            (a, m, v, u, EmailResourceKind::Attachment(index))
        }
        _ => {
            return Err(format!(
                "expected email://{{account}}/{{mailbox}}/{{uidValidity}}/{{uid}}[/headers|/source|/info|/attachments/{{index}}], got: {uri}"
            ));
        }
    };
    let account =
        decode_segment(account).ok_or_else(|| format!("invalid account segment in {uri}"))?;
    let mailbox =
        decode_segment(mailbox).ok_or_else(|| format!("invalid mailbox segment in {uri}"))?;
    let uid_validity: u32 = uid_validity
        .parse()
        .map_err(|_| format!("invalid UIDVALIDITY segment (expected u32) in {uri}"))?;
    if uid_validity == 0 {
        return Err(format!("UIDVALIDITY must be non-zero in {uri}"));
    }
    let uid: u32 = uid
        .parse()
        .map_err(|_| format!("invalid uid segment (expected u32) in {uri}"))?;
    if uid == 0 {
        return Err(format!("UID must be non-zero in {uri}"));
    }
    Ok(EmailResourceUri {
        account,
        mailbox,
        uid_validity,
        uid,
        kind,
    })
}

pub(super) fn email_resource_templates() -> Vec<ResourceTemplate> {
    vec![
        ResourceTemplate::new(EMAIL_MAILBOX_TEMPLATE, "email-mailbox")
            .with_title("Email mailbox (newest messages)")
            .with_description(
                "A selectable mailbox index ordered newest-first. Read an account root first to discover exact mailbox URIs. Optional offset and limit paginate metadata; limit defaults to 25 and is capped at 50.",
            )
            .with_mime_type("application/json")
            .with_annotations(assistant_annotations(0.8)),
        ResourceTemplate::new(EMAIL_BODY_TEMPLATE, EMAIL_BODY_NAME)
            .with_title(EMAIL_BODY_TITLE)
            .with_description(
                "A single email rendered as markdown. Percent-encode the account and \
                 mailbox segments; a '/' inside a mailbox name must be encoded as %2F. \
                 Get account names from list_accounts, mailbox names from list_mailboxes, \
                 and the UIDVALIDITY + UID identity from a current discovery result. \
                 Markdown output is limited to 100,000 characters.",
            )
            .with_mime_type(EMAIL_BODY_MIME)
            .with_annotations(assistant_annotations(0.8)),
        ResourceTemplate::new(EMAIL_HEADERS_TEMPLATE, "email-message-headers")
            .with_title("Email message headers (exact RFC822 syntax)")
            .with_description(
                "The exact RFC822 header block for a live message identity, preserving \
                 field names, order, folding, and line endings. Output is limited to 64 KiB.",
            )
            .with_mime_type("text/rfc822-headers")
            .with_annotations(assistant_annotations(0.5)),
        ResourceTemplate::new(EMAIL_SOURCE_TEMPLATE, "email-message-source")
            .with_title("Email message (raw RFC822 source)")
            .with_description(
                "The raw RFC822 source of a single email, including all headers \
                 and MIME structure. Output is limited to 256 KiB; use the markdown, \
                 headers, or attachment APIs for larger messages.",
            )
            .with_mime_type("message/rfc822")
            .with_annotations(assistant_annotations(0.2)),
        ResourceTemplate::new(EMAIL_INFO_TEMPLATE, EMAIL_INFO_NAME)
            .with_title(EMAIL_INFO_TITLE)
            .with_description(
                "Compact JSON metadata for a live message: subject, sender, \
                 recipients, date, flags, size, and the attachment inventory — \
                 each attachment's index, original name, canonical filename \
                 ({uid}_{index}_{name}), content type, size, and its \
                 /attachments/{index} resource URI — plus sibling body, headers, \
                 and source resource URIs. Read this before fetching attachments.",
            )
            .with_mime_type(EMAIL_INFO_MIME)
            .with_annotations(assistant_annotations(0.5)),
        ResourceTemplate::new(EMAIL_ATTACHMENT_TEMPLATE, "email-message-attachment")
            .with_title("Email attachment (binary)")
            .with_description(
                "One MIME attachment of a live message, addressed by zero-based \
                 part index and returned as a base64 blob with the part's own \
                 content type. Discover indices, names, and sizes via the /info \
                 resource. Attachments above 4 MiB are refused — use the \
                 download_attachments tool to save large files to disk.",
            )
            .with_annotations(user_and_assistant_annotations(0.8)),
    ]
}

fn json_contents(uri: &str, value: &serde_json::Value) -> Result<ReadResourceResult, McpError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    Ok(ReadResourceResult::new(vec![
        ResourceContents::text(text, uri).with_mime_type("application/json"),
    ]))
}

/// Render a message as a markdown document: subject heading, metadata list,
/// then the already markdown-normalized body.
fn render_message_markdown(msg: &crate::MessageInfo) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", msg.subject));
    out.push_str(&format!("- **From:** {}\n", msg.sender));
    if !msg.to.is_empty() {
        out.push_str(&format!("- **To:** {}\n", msg.to.join(", ")));
    }
    if !msg.cc.is_empty() {
        out.push_str(&format!("- **Cc:** {}\n", msg.cc.join(", ")));
    }
    if let Some(date) = &msg.date {
        out.push_str(&format!("- **Date:** {}\n", date.to_rfc3339()));
    }
    if !msg.flags.is_empty() {
        out.push_str(&format!("- **Flags:** {}\n", msg.flags.join(", ")));
    }
    out.push_str(&format!(
        "- **UID:** {} ({} / {})\n",
        msg.uid, msg.account, msg.mailbox
    ));
    if !msg.attachments.is_empty() {
        let names: Vec<String> = msg
            .attachments
            .iter()
            .map(|a| a.name.clone().unwrap_or_else(|| a.content_type.clone()))
            .collect();
        out.push_str(&format!("- **Attachments:** {}\n", names.join(", ")));
    }
    out.push('\n');
    match &msg.content {
        Some(content) => out.push_str(content),
        None => out.push_str("*(no message body)*"),
    }
    if msg.content_truncated == Some(true) {
        out.push_str("\n\n*(body truncated)*");
    }
    cap_chars(&out, MAX_BODY_CHARS)
}

/// Canonical exposed filename for an attachment part — the same nomenclature
/// `download_attachments` uses on disk: `{uid}_{index}_{sanitized-name}`.
fn attachment_filename(uid: u32, index: usize, name: Option<&str>) -> String {
    format!(
        "{uid}_{index}_{}",
        crate::sanitize_filename(name.unwrap_or("unnamed"))
    )
}

/// Drop `null` members recursively so the info document stays compact,
/// matching the wire policy of omitting absent optional fields.
fn strip_nulls(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(members) => serde_json::Value::Object(
            members
                .into_iter()
                .filter(|(_, member)| !member.is_null())
                .map(|(key, member)| (key, strip_nulls(member)))
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(strip_nulls).collect())
        }
        other => other,
    }
}

/// Render the JSON info document for a message: identity, headline metadata,
/// the attachment inventory (with canonical filenames and resource URIs), and
/// sibling resource URIs.
fn render_message_info(parsed: &EmailResourceUri, msg: &crate::MessageInfo) -> String {
    let uri_for = |kind| {
        format_email_uri_for_kind(
            &parsed.account,
            &parsed.mailbox,
            parsed.uid_validity,
            parsed.uid,
            kind,
        )
    };
    let attachments: Vec<serde_json::Value> = msg
        .attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| {
            serde_json::json!({
                "index": index,
                "name": attachment.name,
                "filename": attachment_filename(parsed.uid, index, attachment.name.as_deref()),
                "contentType": attachment.content_type,
                "size": attachment.size,
                "contentId": attachment.content_id,
                "resourceUri": uri_for(EmailResourceKind::Attachment(index)),
            })
        })
        .collect();
    let info = serde_json::json!({
        "account": parsed.account,
        "mailbox": parsed.mailbox,
        "uidValidity": parsed.uid_validity,
        "uid": parsed.uid,
        "subject": msg.subject,
        "from": msg.sender,
        "to": msg.to,
        "cc": msg.cc,
        "date": msg.date.as_ref().map(|date| date.to_rfc3339()),
        "flags": msg.flags,
        "size": msg.size,
        "messageId": msg.message_id,
        "listId": msg.list_id,
        "mimeType": msg.mime_type,
        "attachmentCount": msg.attachments.len(),
        "attachments": attachments,
        "resources": {
            "body": uri_for(EmailResourceKind::Body),
            "headers": uri_for(EmailResourceKind::Headers),
            "source": uri_for(EmailResourceKind::Source),
        },
    });
    serde_json::to_string_pretty(&strip_nulls(info)).unwrap_or_else(|_| "{}".to_string())
}

fn cap_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    const NOTICE: &str = "\n\n*(resource truncated)*";
    let notice_chars = NOTICE.chars().count();
    if max_chars <= notice_chars {
        return NOTICE.chars().take(max_chars).collect();
    }
    let prefix_chars = max_chars - notice_chars;
    let byte_end = value
        .char_indices()
        .nth(prefix_chars)
        .map_or(value.len(), |(index, _)| index);
    let mut capped = String::with_capacity(byte_end + NOTICE.len());
    capped.push_str(&value[..byte_end]);
    capped.push_str(NOTICE);
    capped
}

/// Return the exact header field block without normalizing field names,
/// folding, whitespace, or line endings.
fn exact_header_block(source: &str) -> &str {
    let crlf_end = source.find("\r\n\r\n");
    let lf_end = source.find("\n\n");
    let end = match (crlf_end, lf_end) {
        (Some(crlf), Some(lf)) => crlf.min(lf),
        (Some(crlf), None) => crlf,
        (None, Some(lf)) => lf,
        (None, None) => source.len(),
    };
    &source[..end]
}

fn resource_not_found(parsed: &EmailResourceUri, detail: &str) -> McpError {
    McpError::resource_not_found(
        format!(
            "email resource for mailbox '{}' UIDVALIDITY {} UID {} is unavailable: {detail}. Refresh get_messages, search_messages, or a top_* discovery result and use its current resourceUri",
            parsed.mailbox, parsed.uid_validity, parsed.uid
        ),
        None,
    )
}

fn map_resource_error(parsed: &EmailResourceUri, error: &crate::AgentmailError) -> McpError {
    match error {
        crate::AgentmailError::MessageNotFound(_)
        | crate::AgentmailError::UidValidityUnavailable { .. }
        | crate::AgentmailError::UidValidityChanged { .. } => {
            resource_not_found(parsed, &error.to_string())
        }
        _ => to_mcp_error(error),
    }
}

fn oversize_error(
    parsed: &EmailResourceUri,
    representation: &str,
    actual: usize,
    maximum: usize,
    alternative: &str,
) -> McpError {
    McpError::invalid_request(
        format!(
            "{representation} for mailbox '{}' UID {} is {actual} bytes, above the {maximum}-byte resource limit; {alternative}",
            parsed.mailbox, parsed.uid
        ),
        None,
    )
}

fn raw_source_contents(source: &[u8], uri: &str) -> ResourceContents {
    ResourceContents::blob(STANDARD.encode(source), uri).with_mime_type("message/rfc822")
}

impl AgentMailServer {
    pub(super) async fn read_email_resource(
        &self,
        uri: &str,
    ) -> Result<ReadResourceResult, McpError> {
        if let Some(catalog) =
            parse_catalog_uri(uri).map_err(|error| McpError::invalid_params(error, None))?
        {
            return match catalog {
                CatalogResourceUri::Account(account) => {
                    let entries = self
                        .agentmail
                        .cached_mailbox_layout(&account)
                        .await
                        .map_err(|error| to_mcp_error(&error))?;
                    let mailboxes: Vec<_> = entries
                        .iter()
                        .filter(|entry| entry.is_selectable())
                        .map(|entry| {
                            serde_json::json!({
                                "name": entry.path,
                                "delimiter": entry.delimiter,
                                "roles": entry.roles,
                                "noInferiors": entry.no_inferiors,
                                "resourceUri": mailbox_resource_uri(&account, &entry.path),
                            })
                        })
                        .collect();
                    json_contents(
                        uri,
                        &serde_json::json!({
                            "account": account,
                            "mailboxCount": mailboxes.len(),
                            "mailboxes": mailboxes,
                        }),
                    )
                }
                CatalogResourceUri::Mailbox(parsed) => {
                    let response = self
                        .agentmail
                        .get_messages(
                            &parsed.mailbox,
                            &parsed.account,
                            parsed.offset,
                            parsed.limit,
                            false,
                            false,
                        )
                        .await
                        .map_err(|error| to_mcp_error(&error))?;
                    let next_offset = (response.offset + response.messages.len() < response.total)
                        .then_some(response.offset + response.messages.len());
                    let messages: Vec<_> = response
                        .messages
                        .into_iter()
                        .map(|message| {
                            serde_json::json!({
                                "uid": message.uid,
                                "subject": message.subject,
                                "sender": message.sender,
                                "date": message.date.map(|date| date.to_rfc3339()),
                                "flags": message.flags,
                                "size": message.size,
                                "resourceUri": format_email_uri(
                                    &parsed.account,
                                    &parsed.mailbox,
                                    response.uid_validity,
                                    message.uid,
                                ),
                            })
                        })
                        .collect();
                    json_contents(
                        uri,
                        &strip_nulls(serde_json::json!({
                            "account": parsed.account,
                            "mailbox": parsed.mailbox,
                            "uidValidity": response.uid_validity,
                            "offset": response.offset,
                            "limit": response.limit,
                            "total": response.total,
                            "nextOffset": next_offset,
                            "messages": messages,
                        })),
                    )
                }
            };
        }
        let parsed = parse_email_uri(uri).map_err(|e| McpError::invalid_params(e, None))?;

        match parsed.kind {
            EmailResourceKind::Body => {
                let response = self
                    .agentmail
                    .get_messages_by_uid(
                        &parsed.mailbox,
                        &parsed.account,
                        &[parsed.uid],
                        parsed.uid_validity,
                        true,
                        false,
                    )
                    .await
                    .map_err(|error| map_resource_error(&parsed, &error))?;
                let Some(message) = response.messages.into_iter().next() else {
                    return Err(resource_not_found(&parsed, "the UID no longer exists"));
                };
                if let Some(size) = message.size
                    && size as usize > MAX_TRANSIENT_MESSAGE_BYTES
                {
                    return Err(oversize_error(
                        &parsed,
                        "message",
                        size as usize,
                        MAX_TRANSIENT_MESSAGE_BYTES,
                        "use metadata, headers, or attachment-specific tools instead",
                    ));
                }
                Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(render_message_markdown(&message), uri)
                        .with_mime_type("text/markdown"),
                ]))
            }
            EmailResourceKind::Headers => {
                let headers = self
                    .agentmail
                    .get_message_headers(
                        &parsed.mailbox,
                        &parsed.account,
                        parsed.uid,
                        parsed.uid_validity,
                        MAX_HEADERS_BYTES as u32,
                    )
                    .await
                    .map_err(|error| map_resource_error(&parsed, &error))?;
                let headers = exact_header_block(&headers);
                if headers.len() > MAX_HEADERS_BYTES {
                    return Err(oversize_error(
                        &parsed,
                        "header block",
                        headers.len(),
                        MAX_HEADERS_BYTES,
                        "use the markdown body resource or targeted discovery fields instead",
                    ));
                }
                Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(headers, uri).with_mime_type("text/rfc822-headers"),
                ]))
            }
            EmailResourceKind::Source => {
                let source = self
                    .agentmail
                    .get_message_source_bytes_with_limit(
                        &parsed.mailbox,
                        &parsed.account,
                        parsed.uid,
                        parsed.uid_validity,
                        MAX_SOURCE_BYTES as u32,
                    )
                    .await
                    .map_err(|error| map_resource_error(&parsed, &error))?;
                if source.len() > MAX_SOURCE_BYTES {
                    return Err(oversize_error(
                        &parsed,
                        "raw source",
                        source.len(),
                        MAX_SOURCE_BYTES,
                        "use the markdown body, /headers, or attachment-specific tools instead",
                    ));
                }
                Ok(ReadResourceResult::new(vec![raw_source_contents(
                    &source, uri,
                )]))
            }
            EmailResourceKind::Info => {
                let response = self
                    .agentmail
                    .get_messages_by_uid(
                        &parsed.mailbox,
                        &parsed.account,
                        &[parsed.uid],
                        parsed.uid_validity,
                        true,
                        false,
                    )
                    .await
                    .map_err(|error| map_resource_error(&parsed, &error))?;
                let Some(message) = response.messages.into_iter().next() else {
                    return Err(resource_not_found(&parsed, "the UID no longer exists"));
                };
                // No oversize guard: the rendered info document is bounded by
                // the attachment inventory, not the message body size.
                Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(render_message_info(&parsed, &message), uri)
                        .with_mime_type("application/json"),
                ]))
            }
            EmailResourceKind::Attachment(index) => {
                let attachments = self
                    .agentmail
                    .get_attachment_data(
                        &parsed.mailbox,
                        &parsed.account,
                        parsed.uid,
                        parsed.uid_validity,
                    )
                    .await
                    .map_err(|error| map_resource_error(&parsed, &error))?;
                let count = attachments.len();
                let Some((name, content_type, bytes)) = attachments.into_iter().nth(index) else {
                    return Err(resource_not_found(
                        &parsed,
                        &format!(
                            "no attachment at index {index}; the message has {count} attachment parts (see the /info resource)"
                        ),
                    ));
                };
                if bytes.len() > MAX_ATTACHMENT_BYTES {
                    return Err(oversize_error(
                        &parsed,
                        &format!("attachment {index} ('{name}')"),
                        bytes.len(),
                        MAX_ATTACHMENT_BYTES,
                        "use the download_attachments tool to save it to disk instead",
                    ));
                }
                Ok(ReadResourceResult::new(vec![
                    ResourceContents::blob(STANDARD.encode(&bytes), uri)
                        .with_mime_type(content_type),
                ]))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Completions
// ---------------------------------------------------------------------------

impl AgentMailServer {
    /// Argument autocompletion for prompts and the email:// resource
    /// templates. `account` completes instantly from config; `mailbox`
    /// uses the account's short-lived layout catalog. A cold or expired
    /// catalog refresh performs one IMAP LIST. Errors yield an empty list —
    /// completion must never surface an error for a keystroke.
    pub(super) async fn handle_complete(
        &self,
        request: CompleteRequestParams,
    ) -> Result<CompleteResult, McpError> {
        let is_prompt = request.r#ref.as_prompt_name().is_some();
        let is_email_template = request.r#ref.as_resource_uri().is_some_and(|u| {
            u == EMAIL_BODY_TEMPLATE
                || u == EMAIL_HEADERS_TEMPLATE
                || u == EMAIL_SOURCE_TEMPLATE
                || u == EMAIL_INFO_TEMPLATE
                || u == EMAIL_ATTACHMENT_TEMPLATE
        });

        let values = if is_prompt || is_email_template {
            match request.argument.name.as_str() {
                "account" => {
                    let names = self.complete_account(&request.argument.value);
                    if is_email_template {
                        names.iter().map(|name| encode_segment(name)).collect()
                    } else {
                        names
                    }
                }
                "mailbox" => {
                    let names = self
                        .complete_mailbox(&request.argument.value, request.context.as_ref())
                        .await;
                    if is_email_template {
                        // Template variables are substituted into the URI by
                        // the client, so offer percent-encoded segments.
                        names.iter().map(|n| encode_segment(n)).collect()
                    } else {
                        names
                    }
                }
                // One value, not a list — but a value we can only name once the
                // earlier blanks are known, which is exactly what `context`
                // carries. Template-only: a prompt argument named uidValidity
                // has no mailbox to resolve against.
                "uidValidity" if is_email_template => {
                    self.complete_uid_validity(&request.argument.value, request.context.as_ref())
                        .await
                }
                // uid, sender, to, subject: not enumerable.
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };

        let completion =
            CompletionInfo::new(values).map_err(|e| McpError::internal_error(e, None))?;
        Ok(CompleteResult::new(completion))
    }

    fn complete_account(&self, prefix: &str) -> Vec<String> {
        let prefix = prefix.to_lowercase();
        self.agentmail
            .account_names()
            .into_iter()
            .filter(|n| n.to_lowercase().starts_with(&prefix))
            .take(CompletionInfo::MAX_VALUES)
            .collect()
    }

    /// The mailbox's current UIDVALIDITY, as a one-element list.
    ///
    /// Enumerable ONLY because the earlier blanks arrive as `context.arguments`:
    /// a client that fills `{account}` and `{mailbox}` first tells us exactly
    /// which mailbox to STATUS. Without a mailbox there is nothing to resolve
    /// and the list is empty — the same contract `complete_mailbox` follows for
    /// a missing account.
    ///
    /// This used to return nothing ("not enumerable"), which was true before
    /// completion context existed: there was no way to know which mailbox the
    /// caller meant.
    async fn complete_uid_validity(
        &self,
        prefix: &str,
        ctx: Option<&CompletionContext>,
    ) -> Vec<String> {
        let Some(mailbox) = ctx.and_then(|c| c.get_argument("mailbox").cloned()) else {
            return Vec::new();
        };
        let Some(account) = self.completion_account(ctx) else {
            return Vec::new();
        };
        // Both values came back from THIS server percent-encoded (the template
        // arms above encode what the client substitutes into the URI), so decode
        // before handing them to IMAP. A value that isn't encoded decodes to
        // itself, so this is safe for a hand-typed one too.
        let account = decode_segment(&account).unwrap_or(account);
        let mailbox = decode_segment(&mailbox).unwrap_or(mailbox);
        match self
            .agentmail
            .mailbox_uid_validity(&account, &mailbox)
            .await
        {
            Ok(uid_validity) => uid_validity_completions(uid_validity, prefix),
            // Unreachable server, unknown mailbox, a server that withholds
            // UIDVALIDITY: offer nothing rather than a guess. The user can
            // still type the epoch by hand.
            Err(_) => Vec::new(),
        }
    }

    /// The account a completion is scoped to: the one already filled in, else
    /// the configured default.
    fn completion_account(&self, ctx: Option<&CompletionContext>) -> Option<String> {
        ctx.and_then(|c| c.get_argument("account").cloned())
            .or_else(|| {
                self.agentmail
                    .config()
                    .default_account()
                    .map(str::to_string)
            })
    }

    async fn complete_mailbox(&self, prefix: &str, ctx: Option<&CompletionContext>) -> Vec<String> {
        let Some(account) = self.completion_account(ctx) else {
            return Vec::new();
        };
        // A context value echoed back from OUR template suggestions is
        // percent-encoded; an unencoded one decodes to itself. Ordinary
        // `user@host` account names need no escaping (`@` isn't in `SEGMENT`),
        // which is why this went unnoticed — but one containing a space or `%`
        // would look up as a nonexistent account and silently offer nothing.
        let account = decode_segment(&account).unwrap_or(account);
        match self.agentmail.cached_mailbox_layout(&account).await {
            Ok(entries) => mailbox_completions(&entries, prefix),
            Err(_) => Vec::new(),
        }
    }
}

/// Candidate list for a resolved UIDVALIDITY: the value itself, filtered by
/// what the user has typed so far.
///
/// A mailbox has exactly ONE current epoch, so this is a one-or-zero list — the
/// filter exists because a suggestion contradicting the input is worse than no
/// suggestion (the client would show a value the user is visibly not typing).
/// Pure, so the IMAP round trip stays at the edge and this stays testable.
fn uid_validity_completions(uid_validity: u32, prefix: &str) -> Vec<String> {
    let value = uid_validity.to_string();
    if value.starts_with(prefix) {
        vec![value]
    } else {
        Vec::new()
    }
}

fn mailbox_completions(entries: &[crate::imap_client::MailboxLayout], prefix: &str) -> Vec<String> {
    let prefix = prefix.to_lowercase();
    entries
        .iter()
        .filter(|entry| entry.is_selectable())
        .map(|entry| &entry.path)
        .filter(|name| name.to_lowercase().starts_with(&prefix))
        .take(CompletionInfo::MAX_VALUES)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(path: impl Into<String>, no_select: bool) -> crate::imap_client::MailboxLayout {
        crate::imap_client::MailboxLayout {
            path: path.into(),
            delimiter: Some("/".to_string()),
            no_select,
            no_inferiors: false,
            roles: Vec::new(),
        }
    }

    #[test]
    fn mailbox_completion_matches_prefix_case_insensitively() {
        let entries = [layout("INBOX", false), layout("Invoices/2026", false)];

        let values = mailbox_completions(&entries, "in");

        assert_eq!(values, ["INBOX", "Invoices/2026"]);
    }

    /// `{uidValidity}` used to complete to nothing ("not enumerable"), which
    /// was true before completion context existed — there was no way to know
    /// which mailbox the caller meant. With `{account}`/`{mailbox}` arriving as
    /// `context.arguments` there is exactly one right answer, so we offer it.
    #[test]
    fn uid_validity_completion_offers_the_single_current_epoch() {
        assert_eq!(uid_validity_completions(9001, ""), ["9001"]);
        assert_eq!(uid_validity_completions(9001, "90"), ["9001"]);
        assert_eq!(uid_validity_completions(9001, "9001"), ["9001"]);
    }

    /// A value the user is visibly not typing is worse than no suggestion.
    #[test]
    fn uid_validity_completion_respects_the_typed_prefix() {
        assert!(uid_validity_completions(9001, "7").is_empty());
        assert!(uid_validity_completions(9001, "9002").is_empty());
    }

    /// The mailbox reaching `complete_uid_validity` through `context.arguments`
    /// is whatever the client substituted into the URI — and for these
    /// templates that's the percent-encoded form THIS server suggested. It has
    /// to decode back to the real IMAP name or the STATUS lookup misses.
    #[test]
    fn encoded_context_values_decode_back_to_imap_names() {
        let suggested = encode_segment("[Gmail]/All Mail");
        // `/` and the space escape; brackets aren't in `SEGMENT` and pass through.
        assert_eq!(suggested, "[Gmail]%2FAll%20Mail");
        assert_eq!(decode_segment(&suggested).unwrap(), "[Gmail]/All Mail");
        // An unencoded value decodes to itself, so the same path is safe for a
        // hand-typed mailbox.
        assert_eq!(decode_segment("INBOX").unwrap(), "INBOX");
    }

    #[test]
    fn mailbox_completion_excludes_no_select_containers() {
        let entries = [layout("Archive", true)];

        let values = mailbox_completions(&entries, "arc");

        assert!(values.is_empty());
    }

    #[test]
    fn mailbox_completion_respects_protocol_limit() {
        let entries: Vec<_> = (0..=CompletionInfo::MAX_VALUES)
            .map(|index| layout(format!("Mailbox {index}"), false))
            .collect();

        let values = mailbox_completions(&entries, "mailbox");

        assert_eq!(values.len(), CompletionInfo::MAX_VALUES);
    }

    #[test]
    fn uri_round_trips_slash_mailbox() {
        assert_eq!(encode_segment("Archive/2024"), "Archive%2F2024");
        let uri = format_email_uri("work acct", "Archive/2024", 9001, 42);
        assert_eq!(uri, "email://work%20acct/Archive%2F2024/9001/42");
        let parsed = parse_email_uri(&uri).unwrap();
        assert_eq!(
            parsed,
            EmailResourceUri {
                account: "work acct".to_string(),
                mailbox: "Archive/2024".to_string(),
                uid_validity: 9001,
                uid: 42,
                kind: EmailResourceKind::Body,
            }
        );

        let source_uri = format_email_uri_for_kind(
            "work acct",
            "Archive/2024",
            9001,
            42,
            EmailResourceKind::Source,
        );
        assert_eq!(
            source_uri,
            "email://work%20acct/Archive%2F2024/9001/42/source"
        );
        assert_eq!(
            parse_email_uri(&source_uri).unwrap().kind,
            EmailResourceKind::Source
        );
    }

    #[test]
    fn uri_round_trips_space_and_unicode() {
        for mailbox in ["[Gmail]/All Mail", "Boîte aux lettres/Été 2024"] {
            let uri = format_email_uri("dummy", mailbox, 12, 7);
            assert!(uri.is_ascii(), "encoded URI must be pure ASCII: {uri}");
            let parsed = parse_email_uri(&uri).unwrap();
            assert_eq!(parsed.mailbox, mailbox);
            assert_eq!(parsed.account, "dummy");
        }
    }

    #[test]
    fn parse_rejects_malformed() {
        let bad = [
            "file:///etc/passwd",
            "email://only-account",
            "email://acct/INBOX",
            "email://acct/INBOX/1", // old URI without UIDVALIDITY
            "email://acct/INBOX/1/2/3",
            "email://acct/INBOX/1/2/raw",
            "email://acct/INBOX/nope/1",
            "email://acct/INBOX/1/notanumber",
            "email://acct/INBOX/4294967296/1", // UIDVALIDITY > u32::MAX
            "email://acct/INBOX/1/4294967296", // UID > u32::MAX
            "email:///INBOX/1/1",              // empty account
            "email://acct//1/1",               // empty mailbox
            "email://acct/INBOX/0/1",          // zero UIDVALIDITY
            "email://acct/INBOX/1/0",          // zero UID
            "email://acct/INBOX/1/2/info/extra",
            "email://acct/INBOX/1/2/attachments", // missing index
            "email://acct/INBOX/1/2/attachments/", // empty index
            "email://acct/INBOX/1/2/attachments/x",
            "email://acct/INBOX/1/2/attachments/-1",
            "email://acct/INBOX/1/2/attachments/1/extra",
        ];
        for uri in bad {
            assert!(parse_email_uri(uri).is_err(), "should reject: {uri}");
        }
    }

    #[test]
    fn info_and_attachment_uris_round_trip() {
        let info_uri = format_email_uri_for_kind(
            "work acct",
            "Archive/2024",
            9001,
            42,
            EmailResourceKind::Info,
        );
        assert_eq!(info_uri, "email://work%20acct/Archive%2F2024/9001/42/info");
        assert_eq!(
            parse_email_uri(&info_uri).unwrap().kind,
            EmailResourceKind::Info
        );

        let attachment_uri = format_email_uri_for_kind(
            "work acct",
            "Archive/2024",
            9001,
            42,
            EmailResourceKind::Attachment(3),
        );
        assert_eq!(
            attachment_uri,
            "email://work%20acct/Archive%2F2024/9001/42/attachments/3"
        );
        assert_eq!(
            parse_email_uri(&attachment_uri).unwrap().kind,
            EmailResourceKind::Attachment(3)
        );
    }

    #[test]
    fn attachment_filenames_follow_download_nomenclature() {
        assert_eq!(
            attachment_filename(42, 0, Some("Q3 report.pdf")),
            "42_0_Q3 report.pdf"
        );
        assert_eq!(
            attachment_filename(42, 1, Some("../evil/../../path.bin")),
            "42_1_.._evil_.._.._path.bin",
            "path separators are sanitized like download_attachments does"
        );
        assert_eq!(attachment_filename(42, 2, None), "42_2_unnamed");
    }

    fn message_with_attachments() -> crate::MessageInfo {
        serde_json::from_value(serde_json::json!({
            "uid": 42,
            "subject": "Quarterly report",
            "sender": "Alice <alice@example.com>",
            "replyTo": "",
            "to": ["me@example.com"],
            "cc": [],
            "mailbox": "INBOX",
            "account": "work",
            "flags": ["\\Seen"],
            "size": 2048,
            "attachments": [
                {"name": "Q3 report.pdf", "contentType": "application/pdf", "size": 1024},
                {"contentType": "image/png", "size": 10, "contentId": "cid:logo"},
            ],
        }))
        .expect("valid MessageInfo fixture")
    }

    #[test]
    fn info_document_lists_attachment_inventory_with_canonical_names() {
        let parsed = EmailResourceUri {
            account: "work".to_string(),
            mailbox: "INBOX".to_string(),
            uid_validity: 9001,
            uid: 42,
            kind: EmailResourceKind::Info,
        };

        let rendered = render_message_info(&parsed, &message_with_attachments());
        let info: serde_json::Value =
            serde_json::from_str(&rendered).expect("info document is valid JSON");

        assert_eq!(info["subject"], "Quarterly report");
        assert_eq!(info["attachmentCount"], 2);
        assert_eq!(info["attachments"][0]["filename"], "42_0_Q3 report.pdf");
        assert_eq!(info["attachments"][0]["contentType"], "application/pdf");
        assert_eq!(
            info["attachments"][1]["resourceUri"],
            "email://work/INBOX/9001/42/attachments/1"
        );
        assert_eq!(info["attachments"][1]["filename"], "42_1_unnamed");
        assert_eq!(
            info["resources"]["headers"],
            "email://work/INBOX/9001/42/headers"
        );
        assert!(
            info["attachments"][1].get("name").is_none() && info.get("date").is_none(),
            "absent optional fields are stripped, not serialized as null"
        );
    }

    #[test]
    fn header_uri_round_trips() {
        let uri = format_email_uri_for_kind("dummy", "INBOX", 7, 9, EmailResourceKind::Headers);

        assert_eq!(
            parse_email_uri(&uri).unwrap().kind,
            EmailResourceKind::Headers
        );
    }

    #[test]
    fn exact_headers_preserve_original_syntax() {
        let source = "Subject: folded\r\n\tvalue\r\nX-CUSTOM:  yes\r\n\r\nbody\r\n";

        assert_eq!(
            exact_header_block(source),
            "Subject: folded\r\n\tvalue\r\nX-CUSTOM:  yes"
        );
    }

    #[test]
    fn raw_source_resource_preserves_non_utf8_octets_as_a_blob() {
        let original = b"Subject: test\r\n\r\n\xff\x00\xfe";
        let contents = raw_source_contents(original, "email://dummy/INBOX/7/9/source");

        let ResourceContents::BlobResourceContents {
            blob, mime_type, ..
        } = contents
        else {
            panic!("raw RFC822 source must use MCP blob contents");
        };
        assert_eq!(mime_type.as_deref(), Some("message/rfc822"));
        assert_eq!(STANDARD.decode(blob).unwrap(), original);
    }

    #[test]
    fn markdown_cap_is_a_strict_character_limit() {
        let value = "é".repeat(MAX_BODY_CHARS + 10);

        let capped = cap_chars(&value, MAX_BODY_CHARS);

        assert_eq!(capped.chars().count(), MAX_BODY_CHARS);
    }

    /// The advertised identity of the two representations tool results LINK to.
    ///
    /// Agreement between a link and its template is structural — both read the
    /// same constants — so asserting `constant == constant` would be
    /// tautological. What is worth pinning is the LITERAL wire contract: an
    /// agent that met a message through `templates/list` and one that met it
    /// through a `ResourceLink` must see the same name, title and mimeType,
    /// and those strings are part of what we publish. `wire.rs` asserts the
    /// same literals on the link side.
    #[test]
    fn the_linked_representations_advertise_their_published_identity() {
        let templates = email_resource_templates();
        let body = templates
            .iter()
            .find(|t| t.uri_template == EMAIL_BODY_TEMPLATE)
            .expect("body template");
        let info = templates
            .iter()
            .find(|t| t.uri_template == EMAIL_INFO_TEMPLATE)
            .expect("info template");

        assert_eq!(body.name, "email-message");
        assert_eq!(body.title.as_deref(), Some("Email message (markdown)"));
        assert_eq!(body.mime_type.as_deref(), Some("text/markdown"));
        assert_eq!(info.name, "email-message-info");
        assert_eq!(
            info.title.as_deref(),
            Some("Email message info (JSON metadata)")
        );
        assert_eq!(info.mime_type.as_deref(), Some("application/json"));
    }

    /// `wire.rs` builds the `/info` link by APPENDING to the body URI rather
    /// than re-deriving it from parts. That is only correct while `/info` is
    /// literally the body URI plus one segment — pin it against the canonical
    /// builder so a future URI-shape change breaks here rather than silently
    /// producing dead links in every tool result.
    #[test]
    fn the_info_uri_is_the_body_uri_plus_one_segment() {
        let body = format_email_uri("work account", "Archive/2026", 77, 42);
        let info = format_email_uri_for_kind(
            "work account",
            "Archive/2026",
            77,
            42,
            EmailResourceKind::Info,
        );
        assert_eq!(info, format!("{body}/info"));
    }

    #[test]
    fn templates_include_message_info_and_attachment_forms() {
        let templates = email_resource_templates();

        let uris: Vec<&str> = templates.iter().map(|t| t.uri_template.as_str()).collect();
        assert_eq!(
            uris,
            [
                EMAIL_MAILBOX_TEMPLATE,
                EMAIL_BODY_TEMPLATE,
                EMAIL_HEADERS_TEMPLATE,
                EMAIL_SOURCE_TEMPLATE,
                EMAIL_INFO_TEMPLATE,
                EMAIL_ATTACHMENT_TEMPLATE,
            ]
        );
        assert_eq!(
            templates[0]
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.priority),
            Some(0.8)
        );
    }

    #[test]
    fn stale_uidvalidity_maps_to_resource_not_found() {
        let parsed = EmailResourceUri {
            account: "dummy".to_string(),
            mailbox: "INBOX".to_string(),
            uid_validity: 7,
            uid: 42,
            kind: EmailResourceKind::Body,
        };
        let error = crate::AgentmailError::UidValidityChanged {
            mailbox: "INBOX".to_string(),
            expected: 7,
            actual: Some(8),
        };

        let mapped = map_resource_error(&parsed, &error);

        assert_eq!(mapped.code, rmcp::model::ErrorCode::RESOURCE_NOT_FOUND);
    }

    #[test]
    fn stale_uidvalidity_error_tells_client_to_refresh() {
        let parsed = EmailResourceUri {
            account: "dummy".to_string(),
            mailbox: "INBOX".to_string(),
            uid_validity: 7,
            uid: 42,
            kind: EmailResourceKind::Body,
        };
        let error = crate::AgentmailError::UidValidityUnavailable {
            mailbox: "INBOX".to_string(),
        };

        let mapped = map_resource_error(&parsed, &error);

        assert!(mapped.message.contains("Refresh get_messages"));
    }
}
