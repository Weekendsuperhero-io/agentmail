use hashbrown::HashMap;

use mail_parser::{MessageParser, MimeHeaders};

use crate::content;
use crate::types::{AttachmentInfo, MessageInfo};

/// Decode the Subject from a raw header block (RFC 2047 encoded-words
/// included). `None` when the block has no non-empty subject.
pub(crate) fn parse_subject(header: &[u8]) -> Option<String> {
    let parsed = MessageParser::default().parse(header)?;
    let subject = parsed.subject()?.trim();
    (!subject.is_empty()).then(|| subject.to_string())
}

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
        crate::AgentmailError::Parse(format!("Failed to parse RFC822 message UID {}", uid))
    })?;

    let subject = parsed.subject().unwrap_or("").to_string();
    let sender = format_address(parsed.from());
    let reply_to = format_address_list(parsed.reply_to());
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
/// Parsed partial-header fields: `(email, display_name, date, message_id)`.
pub type SenderHeaders = (
    String,
    String,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
);

pub fn parse_sender_date(raw: &[u8]) -> crate::Result<SenderHeaders> {
    let parsed = MessageParser::default().parse(raw).ok_or_else(|| {
        crate::AgentmailError::Parse("Failed to parse partial headers".to_string())
    })?;

    let (email, name) = extract_from_parts(parsed.from());

    let date = parsed
        .date()
        .and_then(|d| chrono::DateTime::from_timestamp(d.to_timestamp(), 0));

    // Message-ID identifies the logical message across folders — used to
    // deduplicate account-wide scans (the same message appears under each
    // Gmail label / in All Mail with a different per-mailbox UID).
    let message_id = parsed.message_id().map(|s| s.to_string());

    Ok((email, name, date, message_id))
}

/// Extract (email, display_name) from a From address. Email is lowercased for grouping.
fn extract_from_parts(addr: Option<&mail_parser::Address<'_>>) -> (String, String) {
    match addr {
        Some(mail_parser::Address::List(list)) if !list.is_empty() => {
            let a = &list[0];
            let email = canonical_sender_address(a.address.as_deref().unwrap_or(""));
            let name = a.name.as_deref().unwrap_or("").to_string();
            (email, name)
        }
        Some(mail_parser::Address::Group(groups)) if !groups.is_empty() => {
            if let Some(a) = groups[0].addresses.first() {
                let email = canonical_sender_address(a.address.as_deref().unwrap_or(""));
                let name = a.name.as_deref().unwrap_or("").to_string();
                (email, name)
            } else {
                (String::new(), String::new())
            }
        }
        _ => (String::new(), String::new()),
    }
}

pub(crate) fn canonical_sender_address(address: &str) -> String {
    let lower = address.to_lowercase();
    crate::config::canonicalize_email(&lower).unwrap_or(lower)
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
            // entry_ref: `name` is a borrowed `&str` — allocate the owned key
            // only when a header first appears. See PERF-entry-ref.md.
            map.entry_ref(name).or_default().push(trimmed);
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
pub fn extract_attachment_data(
    raw: &[u8],
    uid: u32,
) -> crate::Result<Vec<(String, String, Vec<u8>)>> {
    let parsed = MessageParser::default().parse(raw).ok_or_else(|| {
        crate::AgentmailError::Parse(format!("Failed to parse RFC822 message UID {}", uid))
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
        Some(mail_parser::Address::List(list)) if !list.is_empty() => format_single_addr(&list[0]),
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
        Some(mail_parser::Address::List(list)) => list.iter().map(format_single_addr).collect(),
        Some(mail_parser::Address::Group(groups)) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .map(format_single_addr)
            .collect(),
        _ => Vec::new(),
    }
}

fn format_single_addr(a: &mail_parser::Addr<'_>) -> String {
    format_name_addr(
        a.name.as_deref().unwrap_or(""),
        a.address.as_deref().unwrap_or(""),
    )
}

/// `Name <email>`, or the bare `email` when there is no name — written so it
/// parses back as exactly ONE mailbox.
///
/// A display name holding an RFC 5322 special (`,` `.` `@` `"` `<` …) is
/// emitted as a quoted-string with `\` and `"` escaped. Unquoted, the sender
/// `"Blake, Mark" <m@x>` came out as `Blake, Mark <m@x>` — two mailboxes to
/// any parser, and a refused address when a reply fed it back into a draft.
/// Control characters, CR and LF included, cannot appear in a header phrase
/// and are dropped. Our own quoting rather than lettre's `Mailbox` `Display`,
/// which fails outright on a CR or LF in the name.
pub(crate) fn format_name_addr(name: &str, email: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    let name = name.trim();
    if name.is_empty() {
        return email.to_string();
    }
    if name.chars().all(|c| c == ' ' || is_atext(c)) {
        return format!("{name} <{email}>");
    }
    let mut quoted = String::with_capacity(name.len() + 2);
    quoted.push('"');
    for c in name.chars() {
        if matches!(c, '\\' | '"') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    format!("{quoted} <{email}>")
}

/// RFC 5322 `atext`, plus the non-ASCII letters RFC 6532 admits: everything
/// that may stand in an unquoted display name between spaces.
fn is_atext(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~".contains(c) || !c.is_ascii()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `build_headers_map` accumulates REPEATED headers (same name appearing
    /// multiple times — e.g. the `Received:` trace) into one key's `Vec`. This
    /// is exactly the get-or-insert path the `entry_ref` conversion changed: the
    /// owned `String` key is created once (first occurrence), and every later
    /// occurrence is a borrowed-key hit that just pushes onto the existing `Vec`.
    /// See PERF-entry-ref.md.
    #[test]
    fn build_headers_map_accumulates_repeated_headers() {
        let raw = b"Received: from a.example\r\n\
                    Received: from b.example\r\n\
                    Subject: Hi there\r\n\
                    \r\n\
                    body";
        let parsed = MessageParser::default()
            .parse(raw.as_slice())
            .expect("parse");
        let map = build_headers_map(&parsed);

        let received = map.get("Received").expect("Received header present");
        assert_eq!(
            received.len(),
            2,
            "both Received values accumulate under one key"
        );
        assert!(received.iter().any(|v| v.contains("a.example")));
        assert!(received.iter().any(|v| v.contains("b.example")));

        // A single-occurrence header is a one-element Vec, trimmed.
        assert_eq!(
            map.get("Subject").map(Vec::as_slice),
            Some(["Hi there".to_string()].as_slice())
        );
    }

    /// Every formatted address parses back as exactly ONE mailbox with the
    /// same name — the property a reply depends on when it feeds a sender
    /// back into a draft.
    #[test]
    fn format_name_addr_quotes_names_that_hold_specials() {
        for (name, expected) in [
            ("", "m@x.example"),
            ("Mark Blake", "Mark Blake <m@x.example>"),
            ("José Ñúñez", "José Ñúñez <m@x.example>"),
            ("Blake, Mark", "\"Blake, Mark\" <m@x.example>"),
            ("m@x.example", "\"m@x.example\" <m@x.example>"),
            ("J. Smith", "\"J. Smith\" <m@x.example>"),
            (
                "Mark \"Shark\" Blake",
                "\"Mark \\\"Shark\\\" Blake\" <m@x.example>",
            ),
            ("back\\slash", "\"back\\\\slash\" <m@x.example>"),
            (
                "Mark\r\nBcc: evil@x.example",
                "\"MarkBcc: evil@x.example\" <m@x.example>",
            ),
        ] {
            let formatted = format_name_addr(name, "m@x.example");
            assert_eq!(formatted, expected, "name {name:?}");
            let mailbox: lettre::message::Mailbox = formatted
                .parse()
                .unwrap_or_else(|error| panic!("{formatted:?} must parse: {error}"));
            let expected_name: String = name.chars().filter(|c| !c.is_control()).collect();
            assert_eq!(
                mailbox.name.as_deref().unwrap_or(""),
                expected_name,
                "name {name:?}"
            );
        }
    }

    #[test]
    fn every_reply_to_address_is_kept() {
        let raw = b"From: \"Blake, Mark\" <mark@example.com>\r\n\
                    Reply-To: Desk <desk@example.com>, \"Blake, Mark\" <mark@example.com>\r\n\
                    Subject: s\r\n\r\nbody";
        let message =
            parse_rfc822(raw, 1, Vec::new(), None, "INBOX", "work", false, false).expect("parse");

        assert_eq!(message.sender, "\"Blake, Mark\" <mark@example.com>");
        assert_eq!(
            message.reply_to,
            [
                "Desk <desk@example.com>",
                "\"Blake, Mark\" <mark@example.com>"
            ]
        );
        let without = parse_rfc822(
            b"From: a@b.example\r\n\r\nx",
            2,
            Vec::new(),
            None,
            "INBOX",
            "work",
            false,
            false,
        )
        .expect("parse");
        assert!(without.reply_to.is_empty());
    }

    /// A message shaped the way mail-parser 1.0 reads differently from 0.11:
    /// a photo shown inline between two text parts (iPhone Mail), a latin-1
    /// text attachment, a `cid:` image in `multipart/related`, a forwarded
    /// message, a group in To, a blank Subject and single-id In-Reply-To.
    const MIXED: &str = concat!(
        "From: Ann <ann@example.com>\r\n",
        "To: Team: Bob <bob@example.com>, carol@example.com;, Dan <dan@example.com>\r\n",
        "Subject:\r\n",
        "In-Reply-To: <parent@example.com>\r\n",
        "References: <root@example.com> <parent@example.com>\r\n",
        "X-mailer: Probe 1.0\r\n",
        "List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=\"outer\"\r\n",
        "\r\n",
        "--outer\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "Look at this:\r\n",
        "--outer\r\n",
        "Content-Type: image/png; name=\"photo.png\"\r\n",
        "Content-Disposition: inline; filename=\"photo.png\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "iVBORw0KGgo=\r\n",
        "--outer\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "Nice, right?\r\n",
        "--outer\r\n",
        "Content-Type: text/plain; charset=iso-8859-1; name=\"notes.txt\"\r\n",
        "Content-Disposition: attachment; filename=\"notes.txt\"\r\n",
        "Content-Transfer-Encoding: quoted-printable\r\n",
        "\r\n",
        "caf=E9\r\n",
        "--outer\r\n",
        "Content-Type: multipart/related; boundary=\"rel\"\r\n",
        "\r\n",
        "--rel\r\n",
        "Content-Type: text/html; charset=utf-8\r\n",
        "\r\n",
        "<p>Logo: <img src=\"cid:logo@x\"></p>\r\n",
        "--rel\r\n",
        "Content-Type: image/gif\r\n",
        "Content-ID: <logo@x>\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "R0lGODlhAQABAAAAACw=\r\n",
        "--rel--\r\n",
        "--outer\r\n",
        "Content-Type: message/rfc822\r\n",
        "\r\n",
        "From: inner@example.com\r\n",
        "Subject: inner\r\n",
        "\r\n",
        "inner body\r\n",
        "--outer--\r\n",
    );

    /// Every attachment is listed, in document order, with its bytes and
    /// size. The inline photo is the one RFC 8621 — and so mail-parser 1.0's
    /// own `attachments()` — leaves out; without it a photo sent from an
    /// iPhone could not be listed or saved. A text attachment is UTF-8,
    /// converted from its charset.
    #[test]
    fn attachments_keep_inline_media_and_text_as_utf8() {
        let message = parse_rfc822(
            MIXED.as_bytes(),
            1,
            Vec::new(),
            None,
            "INBOX",
            "work",
            true,
            false,
        )
        .expect("parse");
        let listed: Vec<_> = message
            .attachments
            .iter()
            .map(|a| {
                (
                    a.name.as_deref(),
                    a.content_type.as_str(),
                    a.size,
                    a.content_id.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                (Some("photo.png"), "image/png", 8, None),
                (Some("notes.txt"), "text/plain", 5, None),
                (None, "image/gif", 14, Some("logo@x")),
                (None, "message/rfc822", 53, None),
            ]
        );

        let data = extract_attachment_data(MIXED.as_bytes(), 1).expect("attachments");
        let names: Vec<_> = data.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(names, ["photo.png", "notes.txt", "unnamed", "unnamed"]);
        assert_eq!(data[0].2, b"\x89PNG\r\n\x1a\n", "base64 decoded");
        assert_eq!(
            data[1].2,
            "café".as_bytes(),
            "a text attachment arrives as UTF-8, converted from latin-1"
        );
        assert!(data[2].2.starts_with(b"GIF89a"));
        assert!(
            data[3]
                .2
                .starts_with(b"From: inner@example.com\r\nSubject: inner\r\n"),
            "a forwarded message is its source"
        );
        for ((_, _, bytes), info) in data.iter().zip(&message.attachments) {
            assert_eq!(bytes.len(), info.size, "{:?}", info.name);
        }
    }

    #[test]
    fn header_fields_flatten_groups_and_keep_blank_subjects() {
        let message = parse_rfc822(
            MIXED.as_bytes(),
            1,
            Vec::new(),
            None,
            "INBOX",
            "work",
            true,
            true,
        )
        .expect("parse");
        assert_eq!(message.subject, "", "a blank Subject is empty");
        assert_eq!(parse_subject(MIXED.as_bytes()), None);
        assert_eq!(message.sender, "Ann <ann@example.com>");
        assert_eq!(
            message.to,
            [
                "Bob <bob@example.com>",
                "carol@example.com",
                "Dan <dan@example.com>"
            ],
            "a group's members and the mailboxes after it, flattened"
        );
        assert_eq!(message.in_reply_to.as_deref(), Some("parent@example.com"));
        assert_eq!(
            message.references,
            ["root@example.com", "parent@example.com"]
        );
        assert_eq!(
            message.list_unsubscribe_post.as_deref(),
            Some("List-Unsubscribe=One-Click")
        );
        assert_eq!(message.mime_type.as_deref(), Some("multipart/mixed"));
        assert_eq!(
            message.content_format.as_deref(),
            Some("markdown"),
            "the HTML body is preferred"
        );
        assert_eq!(
            message.headers.get("To").map(Vec::as_slice),
            Some(
                [
                    "Team: Bob <bob@example.com>, carol@example.com;, Dan <dan@example.com>"
                        .to_string()
                ]
                .as_slice()
            ),
            "raw, not re-rendered"
        );
        // Keyed as written: mail-parser 1.0 re-keys a header it knows to
        // its standard spelling, so the 1.0 port flips this knowingly.
        assert_eq!(
            message.headers.get("X-mailer").map(Vec::as_slice),
            Some(["Probe 1.0".to_string()].as_slice())
        );

        let (email, name, _, _) =
            parse_sender_date(b"From: Team: \"Blake, Mark\" <Mark@Example.com>;\r\n\r\n")
                .expect("headers");
        assert_eq!(
            (email.as_str(), name.as_str()),
            ("mark@example.com", "Blake, Mark"),
            "a From inside a group is still the sender"
        );
    }

    /// Empty (whitespace-only) header values are skipped — no key is created,
    /// so no owned key is allocated for them.
    #[test]
    fn build_headers_map_skips_empty_values() {
        let raw = b"X-Empty:   \r\nSubject: Hi\r\n\r\nbody";
        let parsed = MessageParser::default()
            .parse(raw.as_slice())
            .expect("parse");
        let map = build_headers_map(&parsed);
        assert!(
            !map.contains_key("X-Empty"),
            "empty header value is skipped"
        );
        assert!(map.contains_key("Subject"));
    }
}
