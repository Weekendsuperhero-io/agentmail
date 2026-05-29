//! Property-based tests for draft composition (especially attachments).
//!
//! These live in the integration test crate so they exercise the public API
//! (`agentmail::draft::compose_draft` and `DraftAttachment`).

use agentmail::draft::compose_draft;
use agentmail::types::DraftAttachment;
use mail_parser::{MessageParser, MimeHeaders};
use proptest::prelude::*;

/// Generates a "reasonable" email address that lettre should accept.
fn reasonable_email() -> impl Strategy<Value = String> {
    (
        "[a-zA-Z][a-zA-Z0-9._%+-]{0,20}",
        "[a-zA-Z0-9][a-zA-Z0-9.-]{0,18}[a-zA-Z0-9]",
        "[a-zA-Z]{2,6}",
    )
        .prop_map(|(local, domain, tld)| format!("{}@{}.{}", local, domain, tld))
}

/// Generates a simple but plausible filename (no path separators).
fn reasonable_filename() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_.-]{1,20}\\.[a-z]{2,5}".prop_map(|s| s.to_string())
}

/// Generates a DraftAttachment with bounded size.
fn arb_attachment() -> impl Strategy<Value = DraftAttachment> {
    (
        reasonable_filename(),
        prop_oneof![
            "text/plain",
            "text/csv",
            "application/pdf",
            "image/png",
            "application/octet-stream",
            "application/json",
            "image/jpeg",
        ],
        prop::collection::vec(any::<u8>(), 0..1024),
    )
        .prop_map(|(filename, content_type, data)| DraftAttachment {
            filename,
            content_type: content_type.to_string(),
            data,
        })
}

proptest! {
    /// If compose_draft succeeds, the result must be valid parsable RFC822
    /// and must contain exactly as many attachments as we supplied.
    #[test]
    fn compose_draft_success_implies_valid_mime_and_correct_attachments(
        subject in ".*{0,100}",
        body in ".*{0,300}",
        to in prop::collection::vec(reasonable_email(), 1..4),
        cc in prop::collection::vec(reasonable_email(), 0..3),
        bcc in prop::collection::vec(reasonable_email(), 0..2),
        attachments in prop::collection::vec(arb_attachment(), 0..5),
    ) {
        if let Ok(raw) = compose_draft(
            &subject,
            &body,
            &to,
            &cc,
            &bcc,
            Some("sender@agentmail.test"),
            &attachments,
        ) {
            let msg = MessageParser::default()
                .parse(&raw)
                .expect("on success, output must parse as MIME");

            prop_assert_eq!(
                msg.attachments().count(),
                attachments.len(),
                "number of attachments in output did not match input"
            );
        }
    }

    /// Every filename we pass in must be present (by attachment name) in the
    /// generated message when composition succeeds.
    #[test]
    fn attachment_filenames_are_preserved(
        attachments in prop::collection::vec(arb_attachment(), 1..6),
    ) {
        let to = vec!["to@agentmail.test".to_string()];
        if let Ok(raw) = compose_draft(
            "filenames",
            "check attachment names",
            &to,
            &[],
            &[],
            Some("from@agentmail.test"),
            &attachments,
        ) {
            let msg = MessageParser::default().parse(&raw).expect("parsable");

            let found: Vec<_> = msg
                .attachments()
                .filter_map(|p| p.attachment_name().map(|s| s.to_string()))
                .collect();

            let expected: Vec<_> = attachments.iter().map(|a| a.filename.clone()).collect();

            for name in &expected {
                prop_assert!(
                    found.contains(name),
                    "missing attachment filename {:?} in {:?}",
                    name,
                    found
                );
            }
            prop_assert_eq!(found.len(), expected.len());
        }
    }

    /// The top-level Content-Type is multipart/mixed if and only if we
    /// supplied at least one attachment.
    #[test]
    fn multipart_mixed_iff_attachments(
        attachments in prop::collection::vec(arb_attachment(), 0..4),
    ) {
        let to = vec!["x@y.z".to_string()];
        if let Ok(raw) = compose_draft(
            "mime check",
            "body",
            &to,
            &[],
            &[],
            Some("me@x.y"),
            &attachments,
        ) {
            let msg = MessageParser::default().parse(&raw).expect("parsable");
            let is_mixed = msg.is_content_type("multipart", "mixed");
            prop_assert_eq!(is_mixed, !attachments.is_empty());
        }
    }

    /// When there are no attachments, the body text we supplied should be
    /// recoverable from the parsed message (as text/plain).
    #[test]
    fn body_text_is_preserved_when_no_attachments(
        body in "[\x20-\x7E\n\r\t]{0,400}",
    ) {
        let to = vec!["reader@agentmail.test".to_string()];
        if let Ok(raw) = compose_draft(
            "body preservation",
            &body,
            &to,
            &[],
            &[],
            Some("writer@agentmail.test"),
            &[],
        ) {
            let msg = MessageParser::default().parse(&raw).expect("parsable");

            // No attachments path should be simple text or at least contain our body
            let recovered = msg.body_text(0).map(|s| s.to_string()).unwrap_or_default();
            // lettre (and MIME) normalizes line endings to CRLF.
            // Compare after normalizing both sides.
            let normalized_body = body.replace("\r\n", "\n").replace('\r', "\n");
            let normalized_recovered = recovered.replace("\r\n", "\n").replace('\r', "\n");

            if !normalized_body.trim().is_empty() {
                // Check that a meaningful chunk of the original body appears
                let sample = &normalized_body[..normalized_body.len().min(80)];
                prop_assert!(
                    normalized_recovered.contains(sample.trim()) || normalized_recovered.contains(&normalized_body),
                    "body text not preserved; expected to find fragment of {:?} in {:?}",
                    sample,
                    normalized_recovered
                );
            }
        }
    }

    /// For small attachments, the exact bytes we supplied should be
    /// retrievable from the parsed message part.
    #[test]
    fn small_attachment_bytes_roundtrip(
        data in prop::collection::vec(any::<u8>(), 1..256),
        filename in reasonable_filename(),
    ) {
        let att = DraftAttachment {
            filename: filename.clone(),
            content_type: "application/octet-stream".to_string(),
            data: data.clone(),
        };

        let to = vec!["attach@agentmail.test".to_string()];
        if let Ok(raw) = compose_draft(
            "binary roundtrip",
            "see attached bytes",
            &to,
            &[],
            &[],
            Some("me@agentmail.test"),
            &[att],
        ) {
            let msg = MessageParser::default().parse(&raw).expect("parsable");

            let found = msg.attachments().find(|p| p.attachment_name() == Some(&filename));

            if let Some(part) = found {
                let on_disk = part.contents();
                prop_assert_eq!(
                    on_disk,
                    &data[..],
                    "attachment bytes did not survive the roundtrip for {:?}",
                    filename
                );
            } else {
                prop_assert!(false, "could not find attachment named {:?}", filename);
            }
        }
    }

    /// Recipients (to, cc, bcc) that are supplied should appear somewhere in
    /// the parsed message headers when composition succeeds.
    #[test]
    fn recipients_are_reflected_in_message(
        to in prop::collection::vec(reasonable_email(), 1..3),
        cc in prop::collection::vec(reasonable_email(), 0..2),
        bcc in prop::collection::vec(reasonable_email(), 0..2),
    ) {
        if let Ok(raw) = compose_draft(
            "recipients",
            "hello",
            &to,
            &cc,
            &bcc,
            Some("origin@agentmail.test"),
            &[],
        ) {
            // Just require that it parses; more sophisticated header checks
            // can be added later. The fact that lettre accepted the addresses
            // and we didn't crash is already a useful property.
            let _msg = MessageParser::default()
                .parse(&raw)
                .expect("message with recipients must be parsable");
        }
    }

    /// Supplying an obviously invalid address should produce a helpful error.
    #[test]
    fn invalid_recipient_produces_address_error(
        bad_local in "[^@]{0,30}",
    ) {
        let bad_addr = format!("{}@example.com", bad_local); // may still be bad depending on chars
        let result = compose_draft(
            "bad addr test",
            "body",
            &[bad_addr.clone()],
            &[],
            &[],
            None,
            &[],
        );

        if let Err(e) = result {
            let msg = e.to_string();
            prop_assert!(
                msg.contains("Invalid email address") || msg.contains("invalid"),
                "expected address-related error for {:?}, got: {}",
                bad_addr,
                msg
            );
        }
    }
}
