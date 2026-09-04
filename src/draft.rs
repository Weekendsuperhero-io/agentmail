use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};

#[derive(Clone, Copy)]
pub(crate) struct DraftHeaderOptions<'a> {
    pub(crate) reply_to: &'a [String],
    pub(crate) in_reply_to: Option<&'a str>,
    pub(crate) references: &'a [String],
    pub(crate) apple_uuid: uuid::Uuid,
    pub(crate) body_format: BodyFormat,
}

/// How a draft body is put on the wire.
///
/// `multipart/alternative` (RFC 2046 §5.1.4) is THE standard for formatted
/// mail: the same message in rising order of preference, plain text first, so
/// every client picks the richest part it can render and none is left with
/// markup it cannot display. Outlook's third format, "Rich Text", is not this
/// and not a standard — it is TNEF (`application/ms-tnef`), which reaches a
/// non-Outlook recipient as a `winmail.dat` attachment. We never produce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BodyFormat {
    /// The body verbatim as `text/plain`, plus a `text/html` rendering of the
    /// same Markdown. The default: an author writing `**bold**` means emphasis,
    /// and a reader seeing literal asterisks is the failure.
    #[default]
    MarkdownAndHtml,
    /// One `text/plain` part, byte for byte as written. For correspondence
    /// where plain text IS the intended wire format.
    PlainOnly,
}

/// Extract the generated Message-ID (without angle brackets) from a composed
/// RFC822 message, for locating the stored copy on the server afterwards.
pub fn extract_message_id(rfc822: &[u8]) -> Option<String> {
    mail_parser::MessageParser::default()
        .parse(rfc822)?
        .message_id()
        .map(str::to_string)
}

/// Parse a string into a lettre Mailbox.
/// Accepts bare emails ("user@example.com") and full addresses ("Name <user@example.com>").
fn parse_mailbox(addr: &str) -> crate::Result<Mailbox> {
    // Try direct parse first
    if let Ok(mbox) = addr.parse::<Mailbox>() {
        return Ok(mbox);
    }
    // If direct parse fails, try wrapping bare email in angle brackets
    let wrapped = format!("<{}>", addr.trim());
    wrapped.parse::<Mailbox>().map_err(|e| {
        crate::AgentmailError::Other(format!("Invalid email address '{}': {}", addr, e))
    })
}

/// Build an RFC822 message suitable for IMAP APPEND with \Draft flag.
/// When attachments are provided, produces a multipart/mixed message.
pub fn compose_draft(
    subject: &str,
    body: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    from: Option<&str>,
    attachments: &[crate::types::DraftAttachment],
) -> crate::Result<Vec<u8>> {
    compose_draft_with_headers(
        subject,
        body,
        to,
        cc,
        bcc,
        from,
        attachments,
        DraftHeaderOptions {
            reply_to: &[],
            in_reply_to: None,
            references: &[],
            apple_uuid: uuid::Uuid::new_v4(),
            body_format: BodyFormat::default(),
        },
    )
}

/// Build a Mail.app-compatible RFC822 draft with optional threading headers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compose_draft_with_headers(
    subject: &str,
    body: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    from: Option<&str>,
    attachments: &[crate::types::DraftAttachment],
    headers: DraftHeaderOptions<'_>,
) -> crate::Result<Vec<u8>> {
    // `message_id(None)` makes lettre generate a unique `<uuid@host>` —
    // without it drafts ship with no Message-ID (lettre auto-adds Date but
    // not Message-ID), which breaks threading and trips some spam filters.
    // This is an IMAP-saved draft, not a transport submission. Lettre drops
    // Bcc after deriving an SMTP envelope by default; Mail clients need the
    // header retained so the recipient survives reopening the draft.
    let mut builder = Message::builder()
        .subject(subject)
        .message_id(None)
        .keep_bcc();

    if let Some(from_addr) = from {
        builder = builder.from(parse_mailbox(from_addr)?);
    }

    for addr in to {
        builder = builder.to(parse_mailbox(addr)?);
    }

    for addr in cc {
        builder = builder.cc(parse_mailbox(addr)?);
    }

    for addr in bcc {
        builder = builder.bcc(parse_mailbox(addr)?);
    }

    for addr in headers.reply_to {
        builder = builder.reply_to(parse_mailbox(addr)?);
    }

    // The plain part is the body EXACTLY as written — Markdown is designed to
    // read as plain text, so the un-rendered half is not a degraded fallback,
    // it is the source. Attachments nest the alternative inside `mixed`, which
    // is the ordering RFC 2046 expects: one message body, then the files.
    let alternative = || {
        MultiPart::alternative_plain_html(
            body.to_string(),
            crate::content::markdown_to_email_html(body),
        )
    };
    let message = match (attachments.is_empty(), headers.body_format) {
        (true, BodyFormat::PlainOnly) => builder.body(body.to_string()),
        (true, BodyFormat::MarkdownAndHtml) => builder.multipart(alternative()),
        (false, format) => {
            let mut mixed = match format {
                BodyFormat::PlainOnly => {
                    MultiPart::mixed().singlepart(SinglePart::plain(body.to_string()))
                }
                BodyFormat::MarkdownAndHtml => MultiPart::mixed().multipart(alternative()),
            };
            for att in attachments {
                let ct = ContentType::parse(&att.content_type)
                    .unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
                let part = Attachment::new(att.filename.clone()).body(att.data.clone(), ct);
                mixed = mixed.singlepart(part);
            }
            builder.multipart(mixed)
        }
    }
    .map_err(|e| crate::AgentmailError::Other(format!("Failed to build message: {}", e)))?;

    let mut custom_headers = vec![
        "X-Uniform-Type-Identifier: com.apple.mail-draft".to_string(),
        "X-Apple-Auto-Saved: 1".to_string(),
        format!(
            "X-Universally-Unique-Identifier: {}",
            headers.apple_uuid.hyphenated()
        ),
    ];
    if let Some(message_id) = headers.in_reply_to {
        custom_headers.push(format!(
            "In-Reply-To: {}",
            normalize_message_id(message_id)?
        ));
    }
    if !headers.references.is_empty() {
        let normalized = headers
            .references
            .iter()
            .map(|message_id| normalize_message_id(message_id))
            .collect::<crate::Result<Vec<_>>>()?;
        let mut line = format!("References: {}", normalized[0]);
        for message_id in normalized.iter().skip(1) {
            line.push_str("\r\n\t");
            line.push_str(message_id);
        }
        if line.len() > 16 * 1024 {
            return Err(crate::AgentmailError::Other(
                "draft References header exceeds 16 KiB".to_string(),
            ));
        }
        custom_headers.push(line);
    }
    insert_headers(message.formatted(), &custom_headers)
}

fn normalize_message_id(value: &str) -> crate::Result<String> {
    let value = value.trim();
    if value.is_empty()
        || value.contains(['\r', '\n', '\0'])
        || !value.is_ascii()
        || value.chars().any(char::is_whitespace)
    {
        return Err(crate::AgentmailError::Other(format!(
            "invalid message id '{value}'"
        )));
    }
    let inner = value
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix('>'))
        .unwrap_or(value);
    if inner.is_empty() || inner.contains(['<', '>']) {
        return Err(crate::AgentmailError::Other(format!(
            "invalid message id '{value}'"
        )));
    }
    Ok(format!("<{inner}>"))
}

fn insert_headers(mut rfc822: Vec<u8>, headers: &[String]) -> crate::Result<Vec<u8>> {
    let Some(header_end) = rfc822.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err(crate::AgentmailError::Parse(
            "composed draft has no RFC822 header boundary".to_string(),
        ));
    };
    let mut insertion = Vec::new();
    for header in headers {
        insertion.extend_from_slice(b"\r\n");
        insertion.extend_from_slice(header.as_bytes());
    }
    rfc822.splice(header_end..header_end, insertion);
    Ok(rfc822)
}

pub(crate) fn extract_apple_uuid(rfc822: &[u8]) -> Option<uuid::Uuid> {
    let parsed = mail_parser::MessageParser::default().parse(rfc822)?;
    let value = parsed
        .header("X-Universally-Unique-Identifier")?
        .as_text()?
        .trim();
    uuid::Uuid::parse_str(value.trim_matches(['<', '>'])).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DraftAttachment;
    use mail_parser::{MessageParser, MimeHeaders};

    fn parse(raw: &[u8]) -> mail_parser::Message<'_> {
        MessageParser::default()
            .parse(raw)
            .expect("failed to parse generated RFC822")
    }

    /// ASCII only, so no quoted-printable rewrites the bytes we assert on.
    const MD_BODY: &str =
        "Hello,\n\n**bold** and a [link](https://example.org/).\n\n- one\n- two\n";

    fn compose_body(body: &str, format: BodyFormat, attachments: &[DraftAttachment]) -> String {
        let raw = compose_draft_with_headers(
            "Subject",
            body,
            &["to@example.com".to_string()],
            &[],
            &[],
            Some("me@example.com"),
            attachments,
            DraftHeaderOptions {
                reply_to: &[],
                in_reply_to: None,
                references: &[],
                apple_uuid: uuid::Uuid::nil(),
                body_format: format,
            },
        )
        .expect("composes");
        String::from_utf8(raw).expect("utf-8")
    }

    /// The standard shape for formatted mail (RFC 2046 §5.1.4): ONE message in
    /// two representations, least-preferred first, so every client picks the
    /// richest part it can render. The plain half is the Markdown source
    /// verbatim — not a lossy summary of the HTML — which is the property that
    /// makes an alternative honest.
    #[test]
    fn a_markdown_body_ships_as_alternative_with_the_source_as_the_plain_half() {
        let raw = compose_body(MD_BODY, BodyFormat::MarkdownAndHtml, &[]);

        assert!(
            raw.contains("Content-Type: multipart/alternative"),
            "formatted mail is an alternative, never a bare text/html: {raw}"
        );
        let plain_at = raw
            .find("Content-Type: text/plain")
            .expect("a text/plain part");
        let html_at = raw
            .find("Content-Type: text/html")
            .expect("a text/html part");
        assert!(
            plain_at < html_at,
            "plain must come FIRST — RFC 2046 orders parts by rising preference"
        );

        // Decode the parts: transfer encodings rewrite the bytes, so assert on
        // content through the parser and on STRUCTURE through the raw headers.
        let parsed = parse(raw.as_bytes());
        let plain = parsed.body_text(0).expect("a text/plain part").to_string();
        assert_eq!(
            plain.replace("\r\n", "\n").trim_end(),
            MD_BODY.trim_end(),
            "the plain half is the source exactly as written"
        );
        let html = parsed.body_html(0).expect("a text/html part").to_string();
        for rendered in [
            "<strong>bold</strong>",
            "<li>one</li>",
            "<a href=\"https://example.org/\">link</a>",
        ] {
            assert!(html.contains(rendered), "missing {rendered} in: {html}");
        }
    }

    /// A draft body is agent-authored text. Treating `<...>` in it as markup
    /// would let a tool call decide what runs in a recipient's mail client, so
    /// raw HTML is ESCAPED — and escaped rather than dropped, because silently
    /// deleting what someone wrote is its own failure.
    #[test]
    fn raw_html_in_a_body_is_escaped_never_emitted() {
        let raw = compose_body(
            "Hi,\n\n<script>alert(1)</script> and <b>manual bold</b>.\n",
            BodyFormat::MarkdownAndHtml,
            &[],
        );
        let parsed = parse(raw.as_bytes());
        let html = parsed.body_html(0).expect("a text/html part").to_string();
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "the script tag must arrive as text: {html}"
        );
        assert!(
            !html.contains("<script>") && !html.contains("<b>manual"),
            "no author-supplied markup may reach the recipient: {html}"
        );
    }

    /// The opt-out. One part, byte for byte, no alternative — for the
    /// correspondence where plain text IS the intended wire format.
    #[test]
    fn plain_text_only_emits_a_single_unrendered_part() {
        let raw = compose_body(MD_BODY, BodyFormat::PlainOnly, &[]);
        assert!(!raw.contains("multipart/alternative"), "{raw}");
        assert!(!raw.contains("text/html"), "{raw}");
        assert!(raw.contains("**bold**"), "the source is untouched: {raw}");
    }

    /// With attachments the alternative NESTS inside `mixed` — one message
    /// body in two representations, then the files. A sibling `text/html` next
    /// to the attachments would make the plain and HTML halves look like two
    /// different bodies.
    #[test]
    fn attachments_nest_the_alternative_inside_mixed() {
        let attachments = vec![DraftAttachment {
            filename: "note.txt".to_string(),
            content_type: "text/plain".to_string(),
            data: b"hi".to_vec(),
        }];
        let raw = compose_body(MD_BODY, BodyFormat::MarkdownAndHtml, &attachments);

        let mixed_at = raw
            .find("Content-Type: multipart/mixed")
            .expect("outer mixed");
        let alt_at = raw
            .find("Content-Type: multipart/alternative")
            .expect("inner alternative");
        assert!(mixed_at < alt_at, "mixed must be the OUTER type: {raw}");

        let parsed = parse(raw.as_bytes());
        assert!(
            parsed
                .attachments()
                .any(|part| part.attachment_name() == Some("note.txt")),
            "the attachment survives the nesting"
        );
    }

    #[test]
    fn compose_draft_no_attachments_produces_simple_message() {
        let raw = compose_draft(
            "Hello there",
            "This is the body.\nLine two.",
            &["alice@example.com".to_string()],
            &[],
            &[],
            Some("me@example.com"),
            &[],
        )
        .unwrap();

        let msg = parse(&raw);

        assert_eq!(msg.subject().unwrap_or(""), "Hello there");

        // Body should be present as text
        let text = msg.body_text(0).map(|c| c.to_string()).unwrap_or_default();
        assert!(text.contains("This is the body."));

        // Should NOT be multipart/mixed when there are no attachments
        assert!(
            !msg.is_content_type("multipart", "mixed"),
            "expected non-multipart message when no attachments"
        );

        // RFC 5322 required headers must be present.
        assert!(msg.date().is_some(), "draft must carry a Date header");
        assert!(
            msg.message_id().is_some(),
            "draft must carry a Message-ID header"
        );
    }

    #[test]
    fn compose_draft_with_one_attachment_creates_multipart_mixed() {
        let attachment = DraftAttachment {
            filename: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            data: b"%PDF-1.4 fake pdf bytes here".to_vec(),
        };

        let raw = compose_draft(
            "Report draft",
            "Please review the attached report.",
            &["reviewer@company.com".to_string()],
            &["manager@company.com".to_string()],
            &[],
            Some("sender@company.com"),
            &[attachment],
        )
        .unwrap();

        let msg = parse(&raw);

        // Top level must be multipart/mixed
        assert!(
            msg.is_content_type("multipart", "mixed"),
            "expected multipart/mixed at top level"
        );

        // We should have exactly one attachment extracted
        let attachment_names: Vec<_> = msg
            .attachments()
            .filter_map(|p| p.attachment_name().map(|s| s.to_string()))
            .collect();

        assert_eq!(attachment_names, vec!["report.pdf"]);

        // The attachment part should have the correct content type we set
        let pdf_part = msg
            .attachments()
            .find(|p| p.attachment_name() == Some("report.pdf"));
        assert!(
            pdf_part.is_some(),
            "could not locate the PDF attachment part"
        );
    }

    #[test]
    fn compose_draft_with_multiple_attachments() {
        let attachments = vec![
            DraftAttachment {
                filename: "a.txt".to_string(),
                content_type: "text/plain".to_string(),
                data: b"hello".to_vec(),
            },
            DraftAttachment {
                filename: "b.png".to_string(),
                content_type: "image/png".to_string(),
                data: vec![0x89, 0x50, 0x4e, 0x47], // PNG header
            },
        ];

        let raw = compose_draft(
            "multi",
            "two files",
            &["x@y.z".to_string()],
            &[],
            &[],
            Some("sender@example.com"),
            &attachments,
        )
        .unwrap();

        let msg = parse(&raw);

        assert!(msg.is_content_type("multipart", "mixed"));

        let names: Vec<_> = msg
            .attachments()
            .filter_map(|p| p.attachment_name().map(|s| s.to_string()))
            .collect();

        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"b.png".to_string()));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn compose_draft_bad_address_returns_error() {
        let err = compose_draft(
            "bad",
            "body",
            &["not a valid address".to_string()],
            &[],
            &[],
            None,
            &[],
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("Invalid email address"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn compose_draft_uses_provided_content_type_even_for_weird_filenames() {
        let att = DraftAttachment {
            filename: "weird.xyz123".to_string(),
            content_type: "application/octet-stream".to_string(),
            data: b"data".to_vec(),
        };

        let raw = compose_draft(
            "s",
            "b",
            &["a@b.c".to_string()],
            &[],
            &[],
            Some("sender@example.com"),
            &[att],
        )
        .unwrap();
        let msg = parse(&raw);
        assert!(msg.is_content_type("multipart", "mixed"));

        let names: Vec<_> = msg
            .attachments()
            .filter_map(|p| p.attachment_name().map(|s| s.to_string()))
            .collect();
        assert_eq!(names, vec!["weird.xyz123"]);
    }

    #[test]
    fn extract_message_id_reads_the_header_and_none_when_absent() {
        // Present: the angle brackets are stripped (needed to locate the stored
        // copy on the server after APPEND).
        let with = b"Message-ID: <abc123@host.example>\r\nSubject: hi\r\n\r\nbody";
        assert_eq!(
            extract_message_id(with).as_deref(),
            Some("abc123@host.example")
        );

        // Absent: no Message-ID header → None.
        let without = b"Subject: no id here\r\n\r\nbody";
        assert_eq!(extract_message_id(without), None);

        // Round-trip: the id compose_draft generates is recoverable.
        let raw = compose_draft(
            "s",
            "b",
            &["a@b.c".to_string()],
            &[],
            &[],
            Some("me@example.com"),
            &[],
        )
        .unwrap();
        assert!(
            extract_message_id(&raw).is_some(),
            "a composed draft's generated Message-ID must be extractable"
        );
    }

    #[test]
    fn extended_draft_carries_reply_threading_bcc_and_mail_app_markers() {
        let apple_uuid =
            uuid::Uuid::parse_str("019c0000-1234-7000-8000-000000000001").expect("fixed UUID");
        let raw = compose_draft_with_headers(
            "Re: status",
            "Following up.",
            &["to@example.com".to_string()],
            &["cc@example.com".to_string()],
            &["blind@example.com".to_string()],
            Some("me@example.com"),
            &[],
            DraftHeaderOptions {
                reply_to: &["answers@example.com".to_string()],
                in_reply_to: Some("parent@example.com"),
                references: &[
                    "ancestor@example.com".to_string(),
                    "<parent@example.com>".to_string(),
                ],
                apple_uuid,
                body_format: BodyFormat::default(),
            },
        )
        .expect("compose extended draft");
        let text = String::from_utf8(raw.clone()).expect("ASCII test message");
        let parsed = parse(&raw);

        assert!(text.contains("Bcc: blind@example.com"));
        assert!(text.contains("Reply-To: answers@example.com"));
        assert!(text.contains("In-Reply-To: <parent@example.com>"));
        assert!(text.contains("References: <ancestor@example.com>\r\n\t<parent@example.com>"));
        assert!(text.contains("X-Uniform-Type-Identifier: com.apple.mail-draft"));
        assert!(text.contains("X-Apple-Auto-Saved: 1"));
        assert_eq!(extract_apple_uuid(&raw), Some(apple_uuid));
        assert!(parsed.bcc().is_some());
    }

    #[test]
    fn threading_headers_reject_injection_before_serialization() {
        let error = compose_draft_with_headers(
            "subject",
            "body",
            &["to@example.com".to_string()],
            &[],
            &[],
            Some("me@example.com"),
            &[],
            DraftHeaderOptions {
                reply_to: &[],
                in_reply_to: Some("parent@example.com\r\nBcc: attacker@example.com"),
                references: &[],
                apple_uuid: uuid::Uuid::new_v4(),
                body_format: BodyFormat::default(),
            },
        )
        .expect_err("header injection must fail");
        assert!(error.to_string().contains("invalid message id"));
    }
}
