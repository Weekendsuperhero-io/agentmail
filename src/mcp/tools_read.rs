//! Read-only MCP tools: discovery, message reading, search, and ranking.

use super::AgentMailServer;
use super::args::*;
use super::{make_cancel_fn, make_progress_fn, to_mcp_error};
use crate::{
    ConnectionStatus, FindAttachmentsResponse, GetMessagesResponse, ListAccountsResponse,
    ListCapabilitiesResponse, ListFlagsResponse, ListMailboxesResponse, RankListIdResponse,
    RankSendersResponse, RankUnsubscribeResponse, SearchMessagesResponse,
};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer,
    handler::server::wrapper::{Json, Parameters},
    model::Meta,
    tool, tool_router,
};
use tokio_util::sync::CancellationToken;

#[tool_router(router = read_tools_router, vis = "pub(super)")]
impl AgentMailServer {
    #[tool(
        name = "list_accounts",
        description = "Return configured IMAP account names. Use this first to discover valid account selectors.",
        annotations(
            title = "List Accounts",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_accounts_tool(
        &self,
        Parameters(_args): Parameters<ListAccountsArgs>,
    ) -> Result<Json<ListAccountsResponse>, McpError> {
        match self.agentmail.list_accounts().await {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "list_mailboxes",
        description = "List all mailboxes (folders) with message counts: total, unseen, and recent. Shows the full folder tree. Optionally filter to a single account.",
        annotations(title = "List Mailboxes", read_only_hint = true)
    )]
    async fn list_mailboxes_tool(
        &self,
        Parameters(args): Parameters<ListMailboxesArgs>,
    ) -> Result<Json<ListMailboxesResponse>, McpError> {
        let account = args.account.filter(|s| !s.trim().is_empty());
        match self.agentmail.list_mailboxes(account.as_deref()).await {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "check_connection",
        description = "Test IMAP connectivity for an account. Connects, authenticates, and reports status.",
        annotations(title = "Check Connection", read_only_hint = true)
    )]
    async fn check_connection_tool(
        &self,
        Parameters(args): Parameters<CheckConnectionArgs>,
    ) -> Result<Json<ConnectionStatus>, McpError> {
        match self.agentmail.check_connection(&args.account).await {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "list_capabilities",
        description = "List IMAP server capabilities for an account. Shows supported extensions like IDLE, MOVE, CONDSTORE, etc.",
        annotations(title = "List IMAP Capabilities", read_only_hint = true)
    )]
    async fn list_capabilities_tool(
        &self,
        Parameters(args): Parameters<ListCapabilitiesArgs>,
    ) -> Result<Json<ListCapabilitiesResponse>, McpError> {
        match self.agentmail.list_capabilities(&args.account).await {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "get_messages",
        description = "Fetch a paginated list of messages from a mailbox, newest-first. Returns metadata (subject, from, date, flags, UID) by default. Set include_content=true to also get the message body as markdown. Set include_headers=true for the full raw headers map. Defaults: mailbox=INBOX, offset=0, limit=25 (max 50).",
        annotations(title = "Get Messages", read_only_hint = true)
    )]
    async fn get_messages_tool(
        &self,
        Parameters(args): Parameters<GetMessagesArgs>,
    ) -> Result<Json<GetMessagesResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        let offset = crate::clamp_usize(args.offset, 0, 0, 1_000_000);
        let limit = crate::clamp_usize(args.limit, 25, 1, 50);

        match self
            .agentmail
            .get_messages(
                &mailbox,
                &args.account,
                offset,
                limit,
                args.include_content,
                args.include_headers,
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "search_messages",
        description = "Search messages with filters: sender_contains, subject_contains, to_contains, query (full-text), read, flagged, and header key/value. Returns paginated results newest-first. Content excluded by default — set include_content=true to get message bodies. Set include_headers=true for the full raw headers map.",
        annotations(title = "Search Messages", read_only_hint = true)
    )]
    async fn search_messages_tool(
        &self,
        Parameters(args): Parameters<SearchMessagesArgs>,
    ) -> Result<Json<SearchMessagesResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        let offset = crate::clamp_usize(args.offset, 0, 0, 1_000_000);
        let limit = crate::clamp_usize(args.limit, 25, 1, 50);

        let criteria = crate::SearchCriteria {
            text: args.query,
            from: args.sender_contains,
            subject: args.subject_contains,
            to: args.to_contains,
            seen: args.read,
            flagged: args.flagged,
            deleted: Some(args.deleted),
            header: match (args.header_key, args.header_value_contains) {
                (Some(k), Some(v)) => Some((k, v)),
                (Some(k), None) => Some((k, String::new())),
                _ => None,
            },
        };

        match self
            .agentmail
            .search_messages(
                &mailbox,
                &args.account,
                &criteria,
                offset,
                limit,
                args.include_content,
                args.include_headers,
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "list_flags",
        description = "List all IMAP flags in use with counts per flag (e.g. \\Seen: 1234, \\Flagged: 56). Omit mailbox to scan the entire account across all mailboxes. Resolves Apple Mail $MailFlagBit color flags to color names (red, orange, yellow, green, blue, purple, gray).",
        annotations(title = "List Flags", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn list_flags_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<ListFlagsArgs>,
    ) -> Result<Json<ListFlagsResponse>, McpError> {
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        match self
            .agentmail
            .list_flags(
                args.mailbox.as_deref(),
                &args.account,
                progress.as_ref(),
                Some(&cancel),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "find_attachments",
        description = "Scan for messages with attachments (multipart/mixed or multipart/related). Returns paginated UIDs (newest-first) and total count. Omit mailbox to scan the entire account with a per-mailbox breakdown. Use download_attachments with a specific UID to save files to disk.",
        annotations(title = "Find Attachments", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn find_attachments_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<FindAttachmentsArgs>,
    ) -> Result<Json<FindAttachmentsResponse>, McpError> {
        let offset = crate::clamp_usize(args.offset, 0, 0, 100_000);
        let limit = crate::clamp_usize(args.limit, 25, 1, 100);
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);

        match self
            .agentmail
            .find_attachments(
                args.mailbox.as_deref(),
                &args.account,
                offset,
                limit,
                progress.as_ref(),
                Some(&cancel),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "rank_senders",
        description = "Rank all senders by message count. Omit mailbox to scan the entire account across all mailboxes. Groups by (email, display name) — 'Find My <noreply@apple.com>' and 'iCloud <noreply@apple.com>' are separate entries. Sorted by message count descending. Efficient: fetches only FROM+DATE headers using BODY.PEEK.",
        annotations(title = "Rank Senders", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn rank_senders_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<RankSendersArgs>,
    ) -> Result<Json<RankSendersResponse>, McpError> {
        let limit = Some(args.limit.map_or(100, |v| v as usize));
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);

        match self
            .agentmail
            .group_by_sender(
                args.mailbox.as_deref(),
                &args.account,
                limit,
                progress.as_ref(),
                Some(&cancel),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "rank_unsubscribe",
        description = "Rank bulk-mail senders by message count. Omit mailbox to scan the entire account. Includes messages with either List-Unsubscribe or List-Unsubscribe-Post. Grouped by sender (From), sorted by one-click support first then by count. To clean up a sender, pass the sampleUid and sampleMailbox to unsubscribe_message (not delete_by_sender). Returns count, unsubscribe URL, one-click flag, sample UID + mailbox.",
        annotations(title = "Rank Bulk-Mail Senders", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn rank_unsubscribe_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<RankUnsubscribeArgs>,
    ) -> Result<Json<RankUnsubscribeResponse>, McpError> {
        let limit = Some(args.limit.map_or(100, |v| v as usize));
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);

        match self
            .agentmail
            .group_by_list(
                args.mailbox.as_deref(),
                &args.account,
                limit,
                progress.as_ref(),
                Some(&cancel),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "rank_list_id",
        description = "Rank mailing lists by List-Id header (RFC 2919). Groups all messages from the same mailing list regardless of sender address — useful for lists like GitHub notifications where multiple senders share one List-Id. Omit mailbox to scan the entire account. Use delete_list_id to remove all messages from a list.",
        annotations(title = "Rank Mailing Lists by List-Id", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn rank_list_id_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<RankListIdArgs>,
    ) -> Result<Json<RankListIdResponse>, McpError> {
        let limit = Some(args.limit.map_or(100, |v| v as usize));
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);

        match self
            .agentmail
            .group_by_list_id(
                args.mailbox.as_deref(),
                &args.account,
                limit,
                progress.as_ref(),
                Some(&cancel),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }
}
