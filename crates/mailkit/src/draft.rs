use lettre::message::{Mailbox, Message};

/// Parse a string into a lettre Mailbox.
/// Accepts bare emails ("user@example.com") and full addresses ("Name <user@example.com>").
fn parse_mailbox(addr: &str) -> crate::Result<Mailbox> {
    // Try direct parse first
    if let Ok(mbox) = addr.parse::<Mailbox>() {
        return Ok(mbox);
    }
    // If direct parse fails, try wrapping bare email in angle brackets
    let wrapped = format!("<{}>", addr.trim());
    wrapped
        .parse::<Mailbox>()
        .map_err(|e| crate::MailkitError::Other(format!("Invalid email address '{}': {}", addr, e)))
}

/// Build an RFC822 message suitable for IMAP APPEND with \Draft flag.
pub fn compose_draft(
    subject: &str,
    body: &str,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    from: Option<&str>,
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

    let message = builder
        .body(body.to_string())
        .map_err(|e| crate::MailkitError::Other(format!("Failed to build message: {}", e)))?;

    Ok(message.formatted())
}
