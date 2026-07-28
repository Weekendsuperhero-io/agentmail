//! MCP prompts: guided multi-step email workflows.

use super::AgentMailServer;
use super::args::*;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{PromptMessage, Role},
    prompt, prompt_router,
};

#[prompt_router(vis = "pub(super)")]
impl AgentMailServer {
    #[prompt(
        name = "inbox-summary",
        description = "Get a comprehensive overview of your inbox: mailbox structure, unread counts, top senders by volume, and recent messages."
    )]
    async fn inbox_summary_prompt(
        &self,
        params: Parameters<InboxSummaryArgs>,
    ) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Give me a comprehensive overview of my email for account \"{}\". \
                 First, call list_mailboxes with this account to see selectable folders, message totals, and unread counts. \
                 Then use top_senders with mailbox and limit omitted so its account-wide default page of 10 ranks the top senders by volume. \
                 Finally, show the 10 most recent unread INBOX messages using search_messages with mailbox=\"INBOX\", read=false, and limit=10. \
                 Search results are metadata-only; read a row's UIDVALIDITY-safe resourceUri only when more content is needed.",
                params.0.account
            ),
        )]
    }

    #[prompt(
        name = "cleanup-sender",
        description = "Find and bulk-delete all emails from a specific sender. Shows a preview before deleting."
    )]
    async fn cleanup_sender_prompt(
        &self,
        params: Parameters<CleanupSenderArgs>,
    ) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Help me clean up all emails from \"{}\" in account \"{}\". \
                 First, use search_messages with mailbox=\"INBOX\", senderContains, and limit=5. Results are metadata-only; \
                 show each returned sender, subject, and date plus the total so I can confirm the exact sender identity. \
                 Wait for confirmation. Then call delete_by_sender using one approved row's uid, the search wrapper's \
                 uidValidity as expectedUidValidity, mailbox=\"INBOX\", and allMailboxes=true. Explain that deletion \
                 matches the sample message's exact address + display name and may differ from the original substring.",
                params.0.sender, params.0.account
            ),
        )]
    }

    #[prompt(
        name = "find-attachments",
        description = "Scan a mailbox for messages with attachments and list them for review or download."
    )]
    async fn find_attachments_prompt(
        &self,
        params: Parameters<FindAttachmentsPromptArgs>,
    ) -> Vec<PromptMessage> {
        let mailbox = params.0.mailbox.as_deref().unwrap_or("INBOX");
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Find all messages with attachments in mailbox \"{}\" for account \"{}\". \
                 Use find_attachments with limit=10. Each hit has mailbox, uidValidity, uid, date, and resourceUri. \
                 Show those safe identities and dates; read resourceUri for any hit whose sender or subject I ask to inspect. \
                 If I approve a download, call download_attachments with the hit's mailbox and uid, mapping uidValidity \
                 to expectedUidValidity so a recycled UID cannot target another message.",
                mailbox, params.0.account
            ),
        )]
    }

    #[prompt(
        name = "compose-email",
        description = "Draft a new email message with guided composition."
    )]
    async fn compose_email_prompt(
        &self,
        params: Parameters<ComposeEmailArgs>,
    ) -> Vec<PromptMessage> {
        let mut instructions = format!(
            "Help me compose a new email from account \"{}\".",
            params.0.account
        );
        if let Some(ref to) = params.0.to {
            instructions.push_str(&format!(" The recipient is \"{}\".", to));
        }
        if let Some(ref subject) = params.0.subject {
            instructions.push_str(&format!(" The subject is \"{}\".", subject));
        }
        instructions.push_str(
            " Ask me what I want to say, help me write the body, then use create_draft \
             (with optional attachments) to save it. Show me a preview before saving; create_draft resolves \
             the proper Drafts mailbox and applies the Draft flag itself.",
        );
        vec![PromptMessage::new_text(Role::User, instructions)]
    }

    #[prompt(
        name = "unsubscribe-cleanup",
        description = "Identify high-volume subscriptions and perform approved filing or consented, verified unsubscribe actions."
    )]
    async fn unsubscribe_cleanup_prompt(
        &self,
        params: Parameters<UnsubscribeCleanupArgs>,
    ) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Help me clean up mailing list clutter in account \"{}\". \
                 Step 1: Use top_subscriptions with mailbox and limit omitted to get the default account-wide page of 10 \
                 of bulk-mail sender addresses. Results are grouped by normalized sender email and sorted by advertised one-click \
                 syntax first, then count; execution still requires live DKIM verification. \
                 Step 2: Present each row's address, count, advertisedOneClick, and nested \
                 sample {{mailbox, uidValidity, uid, resourceUri}}. Offer either filing with move_subscription \
                 or an unsubscribe POST; ask me to approve each action and destination. For filing, map the sample \
                 to mailbox, expectedUidValidity, and uid; move_subscription derives the live exact sender and \
                 optional List-Id scope and moves matching bulk mail account-wide. \
                 Step 3: For each filing I approve, call move_subscription with sample.uid as uid, \
                 sample.uidValidity as expectedUidValidity, sample.mailbox as mailbox, and my approved destination. \
                 For each unsubscribe POST I approve, call unsubscribe_message with the same sample mapping and \
                 confirmOneClick=true. Filing approval is not unsubscribe consent, and unsubscribe approval is not \
                 filing approval. Omit cleanup \
                 unless I separately approve deleting matching mail. If I approve cleanup, use \
                 cleanup {{when: \"afterSuccess\", identity: \"listIdOrSender\", deletion: \"trash\"}}. This prefers a \
                 DKIM-authenticated List-Id; its sender fallback requires exact sender email + List-Unsubscribe-Post + \
                 the sample's List-Id when it has one. Never use when=\"always\", deletion=\"trashThenPermanent\", or \
                 deletion=\"permanent\" unless I separately and explicitly authorize that higher-risk policy.",
                params.0.account
            ),
        )]
    }

    #[prompt(
        name = "list-id-cleanup",
        description = "Identify mailing lists by List-Id and bulk-delete entire lists."
    )]
    async fn list_id_cleanup_prompt(
        &self,
        params: Parameters<ListIdCleanupArgs>,
    ) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Help me clean up mailing lists in account \"{}\". \
                 Step 1: Use top_mailing_lists with mailbox and limit omitted to get the default account-wide page of 10 \
                 of mailing lists grouped by their List-Id header. This groups all messages from the same \
                 mailing list regardless of sender — useful for lists like GitHub notifications where \
                 multiple senders share one List-Id. \
                 Step 2: Present me with the ranked list so I can see which lists have the most messages. \
                 Show listId, display name, message count, senderCount, the bounded sender preview, and safe sample resourceUri. \
                 Step 3: For each list I approve, call delete_list_id with its listId value to remove \
                 all messages from that mailing list across all mailboxes.",
                params.0.account
            ),
        )]
    }
}
