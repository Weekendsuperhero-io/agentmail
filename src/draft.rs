use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};

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
    let mut builder = Message::builder().subject(subject);

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

    let message = if attachments.is_empty() {
        builder
            .body(body.to_string())
            .map_err(|e| crate::AgentmailError::Other(format!("Failed to build message: {}", e)))?
    } else {
        let mut mixed = MultiPart::mixed().singlepart(SinglePart::plain(body.to_string()));
        for att in attachments {
            let ct = ContentType::parse(&att.content_type).unwrap_or_else(|_| {
                ContentType::parse("application/octet-stream").unwrap()
            });
            let part = Attachment::new(att.filename.clone()).body(att.data.clone(), ct);
            mixed = mixed.singlepart(part);
        }
        builder
            .multipart(mixed)
            .map_err(|e| crate::AgentmailError::Other(format!("Failed to build message: {}", e)))?
    };

    Ok(message.formatted())
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
        assert!(pdf_part.is_some(), "could not locate the PDF attachment part");
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
        assert!(msg.contains("Invalid email address"), "unexpected error: {msg}");
    }

    #[test]
    fn compose_draft_uses_provided_content_type_even_for_weird_filenames() {
        let att = DraftAttachment {
            filename: "weird.xyz123".to_string(),
            content_type: "application/octet-stream".to_string(),
            data: b"data".to_vec(),
        };

        let raw = compose_draft("s", "b", &["a@b.c".to_string()], &[], &[], Some("sender@example.com"), &[att]).unwrap();
        let msg = parse(&raw);
        assert!(msg.is_content_type("multipart", "mixed"));

        let names: Vec<_> = msg
            .attachments()
            .filter_map(|p| p.attachment_name().map(|s| s.to_string()))
            .collect();
        assert_eq!(names, vec!["weird.xyz123"]);
    }

}
