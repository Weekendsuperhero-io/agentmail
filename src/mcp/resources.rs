//! MCP resources: addressable single-message reads via `email://` URIs.
//!
//! Two URI templates are exposed:
//! - `email://{account}/{mailbox}/{uid}` — the message rendered as markdown
//! - `email://{account}/{mailbox}/{uid}/source` — the raw RFC822 source
//!
//! Account and mailbox are percent-encoded URI segments; a `/` inside a
//! mailbox name (hierarchy delimiter) must be encoded as `%2F` so it cannot
//! be confused with the URI segment separator.

use super::{AgentMailServer, to_mcp_error};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use rmcp::ErrorData as McpError;
use rmcp::model::{
    AnnotateAble, CompleteRequestParams, CompleteResult, CompletionContext, CompletionInfo,
    RawResourceTemplate, ReadResourceResult, ResourceContents, ResourceTemplate,
};

pub(super) const EMAIL_BODY_TEMPLATE: &str = "email://{account}/{mailbox}/{uid}";
pub(super) const EMAIL_SOURCE_TEMPLATE: &str = "email://{account}/{mailbox}/{uid}/source";

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

#[derive(Debug, PartialEq)]
pub(super) struct EmailResourceUri {
    pub(super) account: String,
    pub(super) mailbox: String,
    pub(super) uid: u32,
    pub(super) raw_source: bool,
}

/// Build an `email://` URI from raw parts. The server only ever decodes —
/// clients construct URIs — so this is exercised by the round-trip tests.
#[cfg(test)]
fn format_email_uri(account: &str, mailbox: &str, uid: u32, raw_source: bool) -> String {
    let base = format!(
        "email://{}/{}/{uid}",
        encode_segment(account),
        encode_segment(mailbox)
    );
    if raw_source { base + "/source" } else { base }
}

pub(super) fn parse_email_uri(uri: &str) -> Result<EmailResourceUri, String> {
    let Some(rest) = uri.strip_prefix("email://") else {
        return Err(format!(
            "unsupported resource URI (expected email:// scheme): {uri}"
        ));
    };
    let segments: Vec<&str> = rest.split('/').collect();
    let (account, mailbox, uid, raw_source) = match segments.as_slice() {
        [a, m, u] => (a, m, u, false),
        [a, m, u, "source"] => (a, m, u, true),
        _ => {
            return Err(format!(
                "expected email://{{account}}/{{mailbox}}/{{uid}}[/source], got: {uri}"
            ));
        }
    };
    let account =
        decode_segment(account).ok_or_else(|| format!("invalid account segment in {uri}"))?;
    let mailbox =
        decode_segment(mailbox).ok_or_else(|| format!("invalid mailbox segment in {uri}"))?;
    let uid: u32 = uid
        .parse()
        .map_err(|_| format!("invalid uid segment (expected u32) in {uri}"))?;
    Ok(EmailResourceUri {
        account,
        mailbox,
        uid,
        raw_source,
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
                 and UIDs from get_messages/search_messages/rank_* tools.",
            )
            .with_mime_type("text/markdown")
            .no_annotation(),
        RawResourceTemplate::new(EMAIL_SOURCE_TEMPLATE, "email-message-source")
            .with_title("Email message (raw RFC822 source)")
            .with_description(
                "The raw RFC822 source of a single email, including all headers \
                 and MIME structure.",
            )
            .with_mime_type("message/rfc822")
            .no_annotation(),
    ]
}

/// Render a message as a markdown document: subject heading, metadata list,
/// then the (already markdown-normalized, length-capped) body.
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
    out
}

impl AgentMailServer {
    pub(super) async fn read_email_resource(
        &self,
        uri: &str,
    ) -> Result<ReadResourceResult, McpError> {
        let parsed = parse_email_uri(uri).map_err(|e| McpError::invalid_params(e, None))?;

        if parsed.raw_source {
            match self
                .agentmail
                .get_message_source(&parsed.mailbox, &parsed.account, parsed.uid)
                .await
            {
                Ok(resp) => Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(resp.source, uri).with_mime_type("message/rfc822"),
                ])),
                Err(crate::AgentmailError::MessageNotFound(_)) => {
                    Err(McpError::resource_not_found(
                        format!(
                            "no message with UID {} in mailbox '{}'",
                            parsed.uid, parsed.mailbox
                        ),
                        None,
                    ))
                }
                Err(e) => Err(to_mcp_error(&e)),
            }
        } else {
            match self
                .agentmail
                .get_messages_by_uid(&parsed.mailbox, &parsed.account, &[parsed.uid], true, false)
                .await
            {
                Ok(resp) => {
                    // A missing UID is not an error from the lib: it returns
                    // an empty messages vec.
                    let Some(msg) = resp.messages.into_iter().next() else {
                        return Err(McpError::resource_not_found(
                            format!(
                                "no message with UID {} in mailbox '{}'",
                                parsed.uid, parsed.mailbox
                            ),
                            None,
                        ));
                    };
                    Ok(ReadResourceResult::new(vec![
                        ResourceContents::text(render_message_markdown(&msg), uri)
                            .with_mime_type("text/markdown"),
                    ]))
                }
                Err(e) => Err(to_mcp_error(&e)),
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
    /// requires an IMAP LIST scoped to the account from the completion
    /// context (or the default account). Errors yield an empty list —
    /// completion must never surface an error for a keystroke.
    pub(super) async fn handle_complete(
        &self,
        request: CompleteRequestParams,
    ) -> Result<CompleteResult, McpError> {
        let is_prompt = request.r#ref.as_prompt_name().is_some();
        let is_email_template = request
            .r#ref
            .as_resource_uri()
            .is_some_and(|u| u == EMAIL_BODY_TEMPLATE || u == EMAIL_SOURCE_TEMPLATE);

        let values = if is_prompt || is_email_template {
            match request.argument.name.as_str() {
                "account" => self.complete_account(&request.argument.value),
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
        let prefix = prefix.to_lowercase();
        match self.agentmail.list_mailboxes(Some(&account)).await {
            Ok(resp) => resp
                .mailboxes
                .into_iter()
                .map(|m| m.name)
                .filter(|n| n.to_lowercase().starts_with(&prefix))
                .take(CompletionInfo::MAX_VALUES)
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trips_slash_mailbox() {
        assert_eq!(encode_segment("Archive/2024"), "Archive%2F2024");
        let uri = format_email_uri("work acct", "Archive/2024", 42, false);
        assert_eq!(uri, "email://work%20acct/Archive%2F2024/42");
        let parsed = parse_email_uri(&uri).unwrap();
        assert_eq!(
            parsed,
            EmailResourceUri {
                account: "work acct".to_string(),
                mailbox: "Archive/2024".to_string(),
                uid: 42,
                raw_source: false,
            }
        );

        let source_uri = format_email_uri("work acct", "Archive/2024", 42, true);
        assert_eq!(source_uri, "email://work%20acct/Archive%2F2024/42/source");
        assert!(parse_email_uri(&source_uri).unwrap().raw_source);
    }

    #[test]
    fn uri_round_trips_space_and_unicode() {
        for mailbox in ["[Gmail]/All Mail", "Boîte aux lettres/Été 2024"] {
            let uri = format_email_uri("dummy", mailbox, 7, false);
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
            "email://acct/INBOX/1/2/3",
            "email://acct/INBOX/1/raw",
            "email://acct/INBOX/notanumber",
            "email://acct/INBOX/4294967296", // > u32::MAX
            "email:///INBOX/1",              // empty account
            "email://acct//1",               // empty mailbox
        ];
        for uri in bad {
            assert!(parse_email_uri(uri).is_err(), "should reject: {uri}");
        }
    }
}
