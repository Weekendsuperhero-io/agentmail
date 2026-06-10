//! MCP prompts: guided multi-step email workflows.

use super::AgentMailServer;
use super::args::*;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{PromptMessage, PromptMessageRole},
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
            PromptMessageRole::User,
            format!(
                "Give me a comprehensive overview of my email for account \"{}\". \
                 First, list all mailboxes to see the folder structure, message totals, and unread counts. \
                 Then use rank_senders with limit 20 (omit mailbox to scan the entire account) to show me the top senders by volume. \
                 Finally, show me the 10 most recent unread messages using search_messages with read=false.",
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
            PromptMessageRole::User,
            format!(
                "Help me clean up all emails from \"{}\" in account \"{}\". \
                 First, search for messages from this sender in INBOX to see how many there are. \
                 Show me the 5 most recent ones with include_content=false so I can confirm. \
                 Then wait for my confirmation before bulk-deleting them all.",
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
            PromptMessageRole::User,
            format!(
                "Find all messages with attachments in mailbox \"{}\" for account \"{}\". \
                 Use find_attachments to get the UIDs. \
                 Show me the first 10 so I can see who sent them and the subjects. \
                 I may ask you to download specific attachments afterward.",
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
             (with optional attachments) to save it. Show me a preview of the draft before saving.",
        );
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            instructions,
        )]
    }

    #[prompt(
        name = "unsubscribe-cleanup",
        description = "Identify high-volume mailing lists and unsubscribe + bulk-delete them."
    )]
    async fn unsubscribe_cleanup_prompt(
        &self,
        params: Parameters<UnsubscribeCleanupArgs>,
    ) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            PromptMessageRole::User,
            format!(
                "Help me clean up mailing list clutter in account \"{}\". \
                 Step 1: Use rank_unsubscribe (omit mailbox to scan the entire account) to get a ranked list \
                 of bulk-mail senders. Messages with either List-Unsubscribe or List-Unsubscribe-Post are \
                 included. Results are grouped by sender and sorted by one-click support first, then count. \
                 The unsubscribe URL comes from the newest message per sender. \
                 Step 2: Present me with the ranked list so I can pick which ones to clean up. \
                 Step 3: For each one I approve, call unsubscribe_message with the sample UID and mailbox, \
                 and delete_matching=true. Deletion matches by exact sender + either List-Unsubscribe \
                 or List-Unsubscribe-Post header to ensure only bulk mail is removed. The unsubscribe \
                 POST is best-effort — if it fails, the messages are still deleted across all mailboxes.",
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
            PromptMessageRole::User,
            format!(
                "Help me clean up mailing lists in account \"{}\". \
                 Step 1: Use rank_list_id (omit mailbox to scan the entire account) to get a ranked list \
                 of mailing lists grouped by their List-Id header. This groups all messages from the same \
                 mailing list regardless of sender — useful for lists like GitHub notifications where \
                 multiple senders share one List-Id. \
                 Step 2: Present me with the ranked list so I can see which lists have the most messages. \
                 Show the list name, message count, and the unique senders for each. \
                 Step 3: For each list I approve, call delete_list_id with the list_id value to remove \
                 all messages from that mailing list across all mailboxes.",
                params.0.account
            ),
        )]
    }
}
