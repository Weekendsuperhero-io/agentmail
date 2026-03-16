use std::collections::HashMap;

use mail_parser::{MessageParser, MimeHeaders};

use crate::content;
use crate::types::{AttachmentInfo, MessageInfo};

/// Parse raw RFC822 bytes into a MessageInfo.
pub fn parse_rfc822(
    raw: &[u8],
    uid: u32,
    flags: Vec<String>,
    size: Option<u32>,
    mailbox: &str,
    account: &str,
    include_content: bool,
    include_headers: bool,
) -> crate::Result<MessageInfo> {
    let parsed = MessageParser::default().parse(raw).ok_or_else(|| {
        crate::MailkitError::Parse(format!("Failed to parse RFC822 message UID {}", uid))
    })?;

    let subject = parsed.subject().unwrap_or("").to_string();
    let sender = format_address(parsed.from());
    let reply_to = format_address(parsed.reply_to());
    let to = format_address_list(parsed.to());
    let cc = format_address_list(parsed.cc());
    let bcc = format_address_list(parsed.bcc());

    let date = parsed
        .date()
        .and_then(|d| chrono::DateTime::from_timestamp(d.to_timestamp(), 0));

    let list_unsubscribe = parsed
        .header("List-Unsubscribe")
        .and_then(header_to_string)
        .filter(|s| !s.is_empty());
    let list_unsubscribe_post = parsed
        .header("List-Unsubscribe-Post")
        .and_then(header_to_string)
        .filter(|s| !s.is_empty());
    let list_id = parsed
        .header("List-Id")
        .and_then(header_to_string)
        .filter(|s| !s.is_empty());
    let list_help = parsed
        .header("List-Help")
        .and_then(header_to_string)
        .filter(|s| !s.is_empty());

    // Envelope / threading
    let message_id = parsed.message_id().map(|s| s.to_string());
    let in_reply_to = extract_header_value_text(parsed.in_reply_to());
    let references = extract_header_value_text_list(parsed.references());

    // MIME structure
    let mime_type = extract_mime_type(&parsed);
    let attachments = if include_content {
        extract_attachments(&parsed)
    } else {
        Vec::new()
    };

    // Raw headers are never included by default — the important ones
    // (subject, sender, to, cc, date, list-unsubscribe, list-id, message-id,
    // in-reply-to, references) are already extracted into dedicated fields.
    // Use include_headers to explicitly request the full raw header dump.
    let headers = if include_headers {
        build_headers_map(&parsed)
    } else {
        HashMap::new()
    };

    let (body_content, content_format, content_truncated) = if include_content {
        extract_content(&parsed)
    } else {
        (None, None, None)
    };

    Ok(MessageInfo {
        uid,
        subject,
        sender,
        reply_to,
        to,
        cc,
        mailbox: mailbox.to_string(),
        account: account.to_string(),
        date,
        flags,
        size,
        content: body_content,
        content_format,
        content_truncated,
        list_unsubscribe,
        list_unsubscribe_post,
        list_id,
        list_help,
        message_id,
        in_reply_to,
        references,
        bcc,
        mime_type,
        attachments,
        headers,
    })
}

/// Lightweight parse of just FROM and DATE from partial header bytes.
/// Returns (email_address, display_name, date).
pub fn parse_sender_date(
    raw: &[u8],
) -> crate::Result<(String, String, Option<chrono::DateTime<chrono::Utc>>)> {
    let parsed = MessageParser::default().parse(raw).ok_or_else(|| {
        crate::MailkitError::Parse("Failed to parse partial headers".to_string())
    })?;

    let (email, name) = extract_from_parts(parsed.from());

    let date = parsed
        .date()
        .and_then(|d| chrono::DateTime::from_timestamp(d.to_timestamp(), 0));

    Ok((email, name, date))
}

/// Extract (email, display_name) from a From address. Email is lowercased for grouping.
fn extract_from_parts(addr: Option<&mail_parser::Address<'_>>) -> (String, String) {
    match addr {
        Some(mail_parser::Address::List(list)) if !list.is_empty() => {
            let a = &list[0];
            let email = a.address.as_deref().unwrap_or("").to_lowercase();
            let name = a.name.as_deref().unwrap_or("").to_string();
            (email, name)
        }
        Some(mail_parser::Address::Group(groups)) if !groups.is_empty() => {
            if let Some(a) = groups[0].addresses.first() {
                let email = a.address.as_deref().unwrap_or("").to_lowercase();
                let name = a.name.as_deref().unwrap_or("").to_string();
                (email, name)
            } else {
                (String::new(), String::new())
            }
        }
        _ => (String::new(), String::new()),
    }
}

// ---------------------------------------------------------------------------
// Content extraction
// ---------------------------------------------------------------------------

/// Extract and normalize email content (HTML → markdown, or plain text).
fn extract_content(
    msg: &mail_parser::Message<'_>,
) -> (Option<String>, Option<String>, Option<bool>) {
    if let Some(html) = msg.body_html(0) {
        let md = content::html_to_markdown(&html);
        let (truncated, was_truncated) =
            content::truncate_for_context(&md, content::DEFAULT_CONTENT_MAX_CHARS);
        (
            Some(truncated),
            Some("markdown".to_string()),
            Some(was_truncated),
        )
    } else if let Some(text) = msg.body_text(0) {
        let clean = content::plain_to_markdown(&text);
        let (truncated, was_truncated) =
            content::truncate_for_context(&clean, content::DEFAULT_CONTENT_MAX_CHARS);
        (
            Some(truncated),
            Some("plain".to_string()),
            Some(was_truncated),
        )
    } else {
        (None, None, None)
    }
}

// ---------------------------------------------------------------------------
// Header helpers
// ---------------------------------------------------------------------------

/// Build a HashMap of all headers using raw original values from the message.
fn build_headers_map(parsed: &mail_parser::Message<'_>) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (name, value) in parsed.headers_raw() {
        let trimmed = value.trim().to_string();
        if !trimmed.is_empty() {
            map.entry(name.to_string()).or_default().push(trimmed);
        }
    }
    map
}

/// Extract a single string from a HeaderValue (Text or first of TextList).
fn extract_header_value_text(hv: &mail_parser::HeaderValue<'_>) -> Option<String> {
    match hv {
        mail_parser::HeaderValue::Text(s) => Some(s.to_string()),
        mail_parser::HeaderValue::TextList(list) => list.first().map(|s| s.to_string()),
        _ => None,
    }
}

/// Extract a list of strings from a HeaderValue (TextList or single Text).
fn extract_header_value_text_list(hv: &mail_parser::HeaderValue<'_>) -> Vec<String> {
    match hv {
        mail_parser::HeaderValue::Text(s) => vec![s.to_string()],
        mail_parser::HeaderValue::TextList(list) => list.iter().map(|s| s.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Convert a HeaderValue to a plain string.
fn header_to_string(hv: &mail_parser::HeaderValue<'_>) -> Option<String> {
    match hv {
        mail_parser::HeaderValue::Text(s) => Some(s.to_string()),
        mail_parser::HeaderValue::TextList(list) => Some(
            list.iter()
                .map(|s| s.as_ref())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// MIME / attachment helpers
// ---------------------------------------------------------------------------

/// Extract the top-level MIME Content-Type (e.g., "multipart/mixed" or "text/plain").
fn extract_mime_type(parsed: &mail_parser::Message<'_>) -> Option<String> {
    parsed.content_type().map(|ct| {
        let mut s = ct.c_type.to_string();
        if let Some(ref sub) = ct.c_subtype {
            s.push('/');
            s.push_str(sub);
        }
        s
    })
}

/// Extract attachment metadata from the parsed message.
fn extract_attachments(parsed: &mail_parser::Message<'_>) -> Vec<AttachmentInfo> {
    parsed
        .attachments()
        .map(|part| {
            let content_type = part
                .content_type()
                .map(|ct| {
                    let mut s = ct.c_type.to_string();
                    if let Some(ref sub) = ct.c_subtype {
                        s.push('/');
                        s.push_str(sub);
                    }
                    s
                })
                .unwrap_or_else(|| "application/octet-stream".to_string());

            AttachmentInfo {
                name: part.attachment_name().map(|s| s.to_string()),
                content_type,
                size: part.contents().len(),
                content_id: part.content_id().map(|s| s.to_string()),
            }
        })
        .collect()
}

/// Extract attachment binary data from raw RFC822 bytes.
/// Returns Vec of (filename, content_type, bytes).
pub fn extract_attachment_data(raw: &[u8], uid: u32) -> crate::Result<Vec<(String, String, Vec<u8>)>> {
    let parsed = MessageParser::default().parse(raw).ok_or_else(|| {
        crate::MailkitError::Parse(format!("Failed to parse RFC822 message UID {}", uid))
    })?;

    let mut results = Vec::new();
    for part in parsed.attachments() {
        let name = part
            .attachment_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unnamed".to_string());

        let content_type = part
            .content_type()
            .map(|ct| {
                let mut s = ct.c_type.to_string();
                if let Some(ref sub) = ct.c_subtype {
                    s.push('/');
                    s.push_str(sub);
                }
                s
            })
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let bytes = part.contents().to_vec();
        results.push((name, content_type, bytes));
    }

    Ok(results)
}

// ---------------------------------------------------------------------------
// Address helpers
// ---------------------------------------------------------------------------

/// Format a mail-parser Address into a single "Name <email>" string (first address).
fn format_address(addr: Option<&mail_parser::Address<'_>>) -> String {
    match addr {
        Some(mail_parser::Address::List(list)) if !list.is_empty() => {
            format_single_addr(&list[0])
        }
        Some(mail_parser::Address::Group(groups)) if !groups.is_empty() => {
            if let Some(a) = groups[0].addresses.first() {
                format_single_addr(a)
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// Format a mail-parser Address into a list of address strings.
fn format_address_list(addr: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    match addr {
        Some(mail_parser::Address::List(list)) => {
            list.iter().map(format_single_addr).collect()
        }
        Some(mail_parser::Address::Group(groups)) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .map(format_single_addr)
            .collect(),
        _ => Vec::new(),
    }
}

fn format_single_addr(a: &mail_parser::Addr<'_>) -> String {
    let name = a.name.as_deref().unwrap_or("");
    let email = a.address.as_deref().unwrap_or("");
    if name.is_empty() {
        email.to_string()
    } else {
        format!("{} <{}>", name, email)
    }
}
