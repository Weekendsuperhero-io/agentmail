use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};

use crate::AgentmailError;
use crate::config::{AccountConfig, canonicalize_email, display_name_problem};
use crate::types::MessageInfo;

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
/// Accepts bare emails ("user@example.com") and full addresses ("Name <user@example.com>"),
/// including a name left unquoted around a special ("Blake, Mark <m@x>").
fn parse_mailbox(addr: &str) -> crate::Result<Mailbox> {
    // Try direct parse first
    if let Ok(mbox) = addr.parse::<Mailbox>() {
        return Ok(mbox);
    }
    // If direct parse fails, try wrapping bare email in angle brackets
    let wrapped = format!("<{}>", addr.trim());
    let error = match wrapped.parse::<Mailbox>() {
        Ok(mbox) => return Ok(mbox),
        Err(error) => error,
    };
    parse_unquoted_name_addr(addr).ok_or_else(|| {
        AgentmailError::Other(format!("Invalid email address '{}': {}", addr, error))
    })
}

/// `Blake, Mark <m@x>`: a display name holding an RFC 5322 special, left
/// unquoted — how people, and agents, write a "Last, First" name. Strictly ONE
/// mailbox: a single trailing `<addr>` and a name with no `<`, `>` or `@`, so
/// `Bob <b@y>, Alice <a@x>` is still refused instead of becoming one mailbox
/// named `Bob <b@y>, Alice` that silently drops Bob.
fn parse_unquoted_name_addr(value: &str) -> Option<Mailbox> {
    let inner = value.trim().strip_suffix('>')?;
    let open = inner.rfind('<')?;
    let name = inner[..open].trim();
    if name.is_empty() || name.contains(['<', '>', '@']) || name.chars().any(char::is_control) {
        return None;
    }
    let email = inner[open + 1..].trim().parse::<lettre::Address>().ok()?;
    Some(Mailbox::new(Some(name.to_string()), email))
}

/// The canonical address in `Name <addr>` or a bare `addr`, for comparing a
/// header's mailbox with the account's own addresses.
pub(crate) fn canonical_recipient_address(value: &str) -> Option<String> {
    let value = value.trim();
    let candidate = value
        .rfind('<')
        .and_then(|start| value.get(start + 1..))
        .and_then(|tail| tail.strip_suffix('>'))
        .unwrap_or(value)
        .trim();
    canonicalize_email(candidate)
}

/// A mailbox as `Name <addr>` — the same quoting [`crate::parser`] gives a
/// header it read — for reporting a draft's sender. Never lettre's `Display`,
/// which fails on a CR or LF in the name.
pub(crate) fn format_mailbox(mailbox: &Mailbox) -> String {
    crate::parser::format_name_addr(
        mailbox.name.as_deref().unwrap_or(""),
        mailbox.email.as_ref(),
    )
}

/// The `From` of a stored message, for keeping a draft's sender across
/// `update_draft`. `None` when it is absent or not a usable address. Control
/// characters in the decoded name are dropped: an RFC 2047 word can decode to
/// anything, and a name is going back into a header.
pub(crate) fn extract_from_mailbox(rfc822: &[u8]) -> Option<Mailbox> {
    let parsed = mail_parser::MessageParser::default().parse(rfc822)?;
    let from = parsed.from()?.first()?;
    let email = from
        .address
        .as_deref()?
        .trim()
        .parse::<lettre::Address>()
        .ok()?;
    let name = from
        .name
        .as_deref()
        .map(|name| name.chars().filter(|c| !c.is_control()).collect::<String>())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    Some(Mailbox::new(name, email))
}

/// Decide a draft's `From`. Every address this can return is one of the
/// account's [`AccountConfig::identities`], and the name defaults to its
/// `display_name`. In order:
///
/// 1. An `explicit` sender — `Name <addr>` or `addr` — must be one of the
///    identities, or the draft is refused with the list of them. Its name,
///    when it has one, replaces the display name.
/// 2. A reply (`reply_source`) is From the identity the original involved: its
///    `From` when that is one of ours (a follow-up to our own mail), otherwise
///    the first of our identities among its `To`, then its `Cc` — the address
///    the sender actually wrote to.
/// 3. A replaced draft (`existing`) keeps its current `From` when that is one
///    of ours — name included, the display name filling a blank one — so an
///    alias picked in another mail client survives an edit here.
/// 4. Otherwise the primary identity.
///
/// An account with no identity at all is refused with what to configure,
/// rather than composing a `From` out of a login name.
pub(crate) fn resolve_from(
    account: &str,
    config: &AccountConfig,
    explicit: Option<&str>,
    reply_source: Option<&MessageInfo>,
    existing: Option<&Mailbox>,
) -> crate::Result<Mailbox> {
    let identities = config.identities();
    let Some(primary) = identities.first() else {
        return Err(AgentmailError::Config(format!(
            "account '{account}' has no email address to send from — set `email` in its \
             configuration, or add one in Mail Accounts settings"
        )));
    };
    // `Config::from_accounts` normalizes without validating, so an embedder's
    // name is checked here too: refused loudly rather than dropped.
    let display_name = config.display_name.as_deref();
    if let Some(problem) = display_name.and_then(display_name_problem) {
        return Err(AgentmailError::Config(format!(
            "account '{account}': display_name {problem}"
        )));
    }
    let ours = |address: &str| canonicalize_email(address).filter(|a| identities.contains(a));

    if let Some(explicit) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        let requested = parse_mailbox(explicit)?;
        let Some(identity) = ours(requested.email.as_ref()) else {
            return Err(AgentmailError::Other(format!(
                "`from` '{explicit}' is not an address of account '{account}' ({}); use one \
                 of those, or add it to the account as an alias first",
                identities.join(", ")
            )));
        };
        let name = non_blank(requested.name.as_deref());
        if let Some(problem) = name.and_then(display_name_problem) {
            return Err(AgentmailError::Other(format!(
                "`from` '{explicit}': the name {problem}"
            )));
        }
        return sender(&identity, name.or(display_name));
    }

    if let Some(source) = reply_source {
        let addressed = canonical_recipient_address(&source.sender)
            .filter(|address| identities.contains(address))
            .or_else(|| {
                source.to.iter().chain(&source.cc).find_map(|recipient| {
                    canonical_recipient_address(recipient)
                        .filter(|address| identities.contains(address))
                })
            });
        if let Some(identity) = addressed {
            return sender(&identity, display_name);
        }
    }

    if let Some(existing) = existing
        && ours(existing.email.as_ref()).is_some()
    {
        let name = non_blank(existing.name.as_deref())
            .filter(|name| display_name_problem(name).is_none())
            .or(display_name);
        return Ok(Mailbox::new(
            name.map(str::to_string),
            existing.email.clone(),
        ));
    }

    sender(primary, display_name)
}

fn non_blank(name: Option<&str>) -> Option<&str> {
    name.map(str::trim).filter(|name| !name.is_empty())
}

fn sender(address: &str, name: Option<&str>) -> crate::Result<Mailbox> {
    let email = address.parse::<lettre::Address>().map_err(|error| {
        AgentmailError::Other(format!("Invalid email address '{address}': {error}"))
    })?;
    Ok(Mailbox::new(name.map(str::to_string), email))
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
    let from = from.map(parse_mailbox).transpose()?;
    compose_draft_with_headers(
        subject,
        body,
        to,
        cc,
        bcc,
        from.as_ref(),
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
/// `from` is already resolved — see [`resolve_from`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn compose_draft_with_headers(
    subject: &str,
    body: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    from: Option<&Mailbox>,
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

    if let Some(from) = from {
        builder = builder.from(from.clone());
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

    fn me() -> Mailbox {
        "me@example.com".parse().expect("valid mailbox")
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
            Some(&me()),
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
            Some(&me()),
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

    fn account(display_name: Option<&str>) -> AccountConfig {
        let mut config = AccountConfig::new("imap.example.com", "login-name")
            .with_email("Mark@Example.com")
            .with_aliases(["sales@example.com", "Mark@Example.org"]);
        if let Some(name) = display_name {
            config = config.with_display_name(name);
        }
        crate::Config::from_accounts(vec![("work".to_string(), config)]).accounts["work"].clone()
    }

    fn message(headers: &str) -> MessageInfo {
        let raw = format!("{headers}\r\nSubject: s\r\n\r\nbody");
        crate::parser::parse_rfc822(
            raw.as_bytes(),
            1,
            Vec::new(),
            None,
            "INBOX",
            "work",
            false,
            false,
        )
        .expect("parse source")
    }

    fn resolved(
        config: &AccountConfig,
        explicit: Option<&str>,
        reply_source: Option<&MessageInfo>,
        existing: Option<&Mailbox>,
    ) -> String {
        format_mailbox(
            &resolve_from("work", config, explicit, reply_source, existing).expect("resolves"),
        )
    }

    #[test]
    fn the_default_sender_is_the_display_name_on_the_primary_address() {
        assert_eq!(
            resolved(&account(Some("Mark Blake")), None, None, None),
            "Mark Blake <mark@example.com>"
        );
        assert_eq!(
            resolved(&account(None), None, None, None),
            "mark@example.com"
        );
    }

    #[test]
    fn an_explicit_sender_must_be_one_of_the_accounts_identities() {
        let config = account(Some("Mark Blake"));
        assert_eq!(
            resolved(&config, Some("SALES@example.com"), None, None),
            "Mark Blake <sales@example.com>",
            "a bare address takes the display name"
        );
        assert_eq!(
            resolved(&config, Some("Sales Team <sales@example.com>"), None, None),
            "Sales Team <sales@example.com>",
            "a name given with the address wins"
        );
        assert_eq!(
            resolved(&config, Some("  "), None, None),
            "Mark Blake <mark@example.com>",
            "a blank sender is no sender"
        );

        let error = resolve_from(
            "work",
            &config,
            Some("Mark <mark@elsewhere.example>"),
            None,
            None,
        )
        .expect_err("a foreign address is refused")
        .to_string();
        assert!(
            error.contains("mark@example.com, sales@example.com, mark@example.org")
                && error.contains("alias"),
            "the refusal lists the usable addresses and the fix: {error}"
        );
        let error = resolve_from("work", &config, Some("login-name"), None, None)
            .expect_err("a login is not an address")
            .to_string();
        assert!(error.contains("Invalid email address"), "{error}");
    }

    #[test]
    fn a_reply_is_from_the_identity_the_original_was_sent_to() {
        let config = account(Some("Mark Blake"));
        let via_to =
            message("From: client@example.net\r\nTo: Sales <SALES@example.com>, other@example.net");
        assert_eq!(
            resolved(&config, None, Some(&via_to), None),
            "Mark Blake <sales@example.com>"
        );
        let via_cc =
            message("From: client@example.net\r\nTo: other@example.net\r\nCc: mark@example.org");
        assert_eq!(
            resolved(&config, None, Some(&via_cc), None),
            "Mark Blake <mark@example.org>"
        );
        let follow_up =
            message("From: \"Blake, Mark\" <sales@example.com>\r\nTo: mark@example.com");
        assert_eq!(
            resolved(&config, None, Some(&follow_up), None),
            "Mark Blake <sales@example.com>",
            "a follow-up to our own mail stays on the address it was sent from"
        );
        let bcc_only = message("From: client@example.net\r\nTo: list@example.net");
        assert_eq!(
            resolved(&config, None, Some(&bcc_only), None),
            "Mark Blake <mark@example.com>",
            "no identity in sight falls back to the primary"
        );
        assert_eq!(
            resolved(&config, Some("mark@example.com"), Some(&via_to), None),
            "Mark Blake <mark@example.com>",
            "an explicit sender still wins"
        );
    }

    #[test]
    fn an_updated_draft_keeps_a_sender_of_ours_and_replaces_a_foreign_one() {
        let config = account(Some("Mark Blake"));
        let chosen: Mailbox = "\"Mark B.\" <Sales@Example.com>".parse().expect("mailbox");
        assert_eq!(
            resolved(&config, None, None, Some(&chosen)),
            "\"Mark B.\" <Sales@Example.com>",
            "an alias picked in another client survives, name and spelling included"
        );
        let bare: Mailbox = "sales@example.com".parse().expect("mailbox");
        assert_eq!(
            resolved(&config, None, None, Some(&bare)),
            "Mark Blake <sales@example.com>",
            "a bare sender of ours gains the display name"
        );
        let foreign: Mailbox = "Someone <someone@else.example>".parse().expect("mailbox");
        assert_eq!(
            resolved(&config, None, None, Some(&foreign)),
            "Mark Blake <mark@example.com>"
        );
    }

    #[test]
    fn an_account_with_no_address_is_refused_with_what_to_configure() {
        let config = crate::Config::from_accounts(vec![(
            "icloud".to_string(),
            AccountConfig::new("imap.mail.me.com", "johnappleseed"),
        )])
        .accounts["icloud"]
            .clone();
        let error = resolve_from("icloud", &config, None, None, None)
            .expect_err("no identity, no sender")
            .to_string();
        assert!(
            error.contains("account 'icloud' has no email address")
                && error.contains("Mail Accounts settings"),
            "{error}"
        );
    }

    #[test]
    fn a_sender_name_with_a_line_break_is_refused() {
        let error = resolve_from(
            "work",
            &account(None),
            Some("\"Mark\r\nBcc: x@evil.example\" <mark@example.com>"),
            None,
            None,
        )
        .expect_err("a CR/LF name never reaches a header");
        assert!(
            error.to_string().contains("Invalid email address"),
            "{error}"
        );

        // `from_accounts` normalizes but does not validate, so a bad configured
        // name must be caught at resolution.
        let config = account(Some("Mark\r\nBcc: x@evil.example"));
        let error = resolve_from("work", &config, None, None, None)
            .expect_err("a configured CR/LF name is refused")
            .to_string();
        assert!(error.contains("control characters"), "{error}");
    }

    /// The whole point: a name with a comma, quotes and an accent reaches the
    /// header intact and reads back as ONE sender.
    #[test]
    fn a_display_name_with_specials_round_trips_through_the_header() {
        let name = "Blake, Mark \"Shark\" José";
        let from = resolve_from("work", &account(Some(name)), None, None, None).expect("resolves");
        let raw = compose_draft_with_headers(
            "s",
            "b",
            &["to@example.com".to_string()],
            &[],
            &[],
            Some(&from),
            &[],
            DraftHeaderOptions {
                reply_to: &[],
                in_reply_to: None,
                references: &[],
                apple_uuid: uuid::Uuid::nil(),
                body_format: BodyFormat::PlainOnly,
            },
        )
        .expect("composes");

        let parsed = parse(&raw);
        let senders: Vec<_> = parsed.from().expect("a From header").iter().collect();
        assert_eq!(senders.len(), 1, "one sender, not split at the comma");
        assert_eq!(senders[0].name.as_deref(), Some(name));
        assert_eq!(senders[0].address.as_deref(), Some("mark@example.com"));
        assert_eq!(
            extract_from_mailbox(&raw).map(|mailbox| format_mailbox(&mailbox)),
            Some("\"Blake, Mark \\\"Shark\\\" José\" <mark@example.com>".to_string()),
            "and reads back for update_draft unchanged"
        );
    }

    #[test]
    fn an_unquoted_last_first_name_parses_as_one_mailbox_and_two_mailboxes_do_not() {
        let mailbox = parse_mailbox("Blake, Mark <mark@example.com>").expect("tolerated");
        assert_eq!(mailbox.name.as_deref(), Some("Blake, Mark"));
        assert_eq!(mailbox.email.to_string(), "mark@example.com");

        for two in [
            "Bob <bob@example.com>, Alice <alice@example.com>",
            "bob@example.com, Alice <alice@example.com>",
        ] {
            let error = parse_mailbox(two).expect_err("two mailboxes are not one");
            assert!(
                error.to_string().contains("Invalid email address"),
                "{error}"
            );
        }
    }

    #[test]
    fn threading_headers_reject_injection_before_serialization() {
        let error = compose_draft_with_headers(
            "subject",
            "body",
            &["to@example.com".to_string()],
            &[],
            &[],
            Some(&me()),
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
