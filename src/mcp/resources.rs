//! MCP resources: addressable single-message reads via `email://` URIs.
//!
//! Three URI templates are exposed. Every URI carries the complete IMAP
//! message identity so a delayed read cannot silently use a recycled UID:
//! - `email://{account}/{mailbox}/{uidValidity}/{uid}` — markdown body
//! - `email://{account}/{mailbox}/{uidValidity}/{uid}/headers` — exact headers
//! - `email://{account}/{mailbox}/{uidValidity}/{uid}/source` — raw RFC822
//!
//! Account and mailbox are percent-encoded URI segments; a `/` inside a
//! mailbox name (hierarchy delimiter) must be encoded as `%2F` so it cannot
//! be confused with the URI segment separator.

use super::{AgentMailServer, to_mcp_error};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use rmcp::ErrorData as McpError;
use rmcp::model::{
    AnnotateAble, CompleteRequestParams, CompleteResult, CompletionContext, CompletionInfo,
    RawResourceTemplate, ReadResourceResult, ResourceContents, ResourceTemplate,
};

pub(super) const EMAIL_BODY_TEMPLATE: &str = "email://{account}/{mailbox}/{uidValidity}/{uid}";
pub(super) const EMAIL_HEADERS_TEMPLATE: &str =
    "email://{account}/{mailbox}/{uidValidity}/{uid}/headers";
pub(super) const EMAIL_SOURCE_TEMPLATE: &str =
    "email://{account}/{mailbox}/{uidValidity}/{uid}/source";

const MAX_BODY_CHARS: usize = 100_000;
const MAX_HEADERS_BYTES: usize = 64 * 1024;
const MAX_SOURCE_BYTES: usize = 256 * 1024;
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
        _ => {
            return Err(format!(
                "expected email://{{account}}/{{mailbox}}/{{uidValidity}}/{{uid}}[/headers|/source], got: {uri}"
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
        RawResourceTemplate::new(EMAIL_BODY_TEMPLATE, "email-message")
            .with_title("Email message (markdown)")
            .with_description(
                "A single email rendered as markdown. Percent-encode the account and \
                 mailbox segments; a '/' inside a mailbox name must be encoded as %2F. \
                 Get account names from list_accounts, mailbox names from list_mailboxes, \
                 and the UIDVALIDITY + UID identity from a current discovery result. \
                 Markdown output is limited to 100,000 characters.",
            )
            .with_mime_type("text/markdown")
            .no_annotation(),
        RawResourceTemplate::new(EMAIL_HEADERS_TEMPLATE, "email-message-headers")
            .with_title("Email message headers (exact RFC822 syntax)")
            .with_description(
                "The exact RFC822 header block for a live message identity, preserving \
                 field names, order, folding, and line endings. Output is limited to 64 KiB.",
            )
            .with_mime_type("text/rfc822-headers")
            .no_annotation(),
        RawResourceTemplate::new(EMAIL_SOURCE_TEMPLATE, "email-message-source")
            .with_title("Email message (raw RFC822 source)")
            .with_description(
                "The raw RFC822 source of a single email, including all headers \
                 and MIME structure. Output is limited to 256 KiB; use the markdown, \
                 headers, or attachment APIs for larger messages.",
            )
            .with_mime_type("message/rfc822")
            .no_annotation(),
    ]
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
            u == EMAIL_BODY_TEMPLATE || u == EMAIL_HEADERS_TEMPLATE || u == EMAIL_SOURCE_TEMPLATE
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
                // uidValidity, uid, sender, to, subject: not enumerable.
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

    async fn complete_mailbox(&self, prefix: &str, ctx: Option<&CompletionContext>) -> Vec<String> {
        let account = ctx
            .and_then(|c| c.get_argument("account").cloned())
            .or_else(|| {
                self.agentmail
                    .config()
                    .default_account()
                    .map(str::to_string)
            });
        let Some(account) = account else {
            return Vec::new();
        };
        match self.agentmail.cached_mailbox_layout(&account).await {
            Ok(entries) => mailbox_completions(&entries, prefix),
            Err(_) => Vec::new(),
        }
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
        ];
        for uri in bad {
            assert!(parse_email_uri(uri).is_err(), "should reject: {uri}");
        }
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

    #[test]
    fn templates_include_body_headers_and_source() {
        let templates = email_resource_templates();

        assert_eq!(templates.len(), 3);
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
