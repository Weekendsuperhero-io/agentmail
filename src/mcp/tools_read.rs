//! Read-only MCP tools: discovery, message reading, search, and top-N summaries.

use super::AgentMailServer;
use super::args::*;
use super::wire::{
    CheckConnectionOutput, FindAttachmentsOutput, GetMessagesOutput, ListAccountsOutput,
    ListCapabilitiesOutput, ListFlagsOutput, ListMailboxesOutput, ListPendingMovesOutput,
    SearchMessagesOutput, TopDomainsOutput, TopMailingListsOutput, TopSendersOutput,
    TopSubscriptionsOutput, compact_result, tool_error_result,
};
use super::{Pagination, make_cancel_fn, make_progress_fn};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Meta},
    tool, tool_router,
};
use tokio_util::sync::CancellationToken;

/// Parse an optional `YYYY-MM-DD` search-date argument into a `NaiveDate`,
/// surfacing a bad format as an actionable invalid-params error.
fn parse_search_date(s: Option<&str>) -> Result<Option<chrono::NaiveDate>, McpError> {
    match s {
        None => Ok(None),
        Some(d) => chrono::NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d")
            .map(Some)
            .map_err(|_| {
                McpError::invalid_params(format!("invalid date '{d}', expected YYYY-MM-DD"), None)
            }),
    }
}

#[tool_router(router = read_tools_router, vis = "pub(super)")]
impl AgentMailServer {
    #[tool(
        name = "list_accounts",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ListAccountsOutput>().expect("valid list_accounts output schema"),
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
    ) -> Result<CallToolResult, McpError> {
        match self.agentmail.list_accounts().await {
            Ok(data) => compact_result(ListAccountsOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "list_mailboxes",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ListMailboxesOutput>().expect("valid list_mailboxes output schema"),
        description = "List selectable mailboxes for one required account. Returns a page of mailbox names, total and unseen counts, hierarchy delimiters, no-inferiors state, and all recognized special-use roles. Non-selectable containers are omitted. Defaults: offset=0, limit=100 (max 500).",
        annotations(title = "List Mailboxes", read_only_hint = true)
    )]
    async fn list_mailboxes_tool(
        &self,
        Parameters(args): Parameters<ListMailboxesArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.account.trim().is_empty() {
            return Err(McpError::invalid_params("account is required", None));
        }
        let Pagination { offset, limit } = Pagination::new(args.offset, args.limit, 100, 500)?;
        match self
            .agentmail
            .list_mailboxes_page(&args.account, offset, limit)
            .await
        {
            Ok((data, total)) => compact_result(ListMailboxesOutput::new(
                data,
                &args.account,
                offset,
                limit,
                total,
            )),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "check_connection",
        output_schema = rmcp::handler::server::tool::schema_for_output::<CheckConnectionOutput>().expect("valid check_connection output schema"),
        description = "Test IMAP connectivity for an account. Connects, authenticates, and reports the outcome as data: connected=true, or connected=false with the error text — a probe against a configured account never raises a protocol error. An unknown account raises invalid params.",
        annotations(title = "Check Connection", read_only_hint = true)
    )]
    async fn check_connection_tool(
        &self,
        Parameters(args): Parameters<CheckConnectionArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.agentmail.check_connection(&args.account).await {
            Ok(data) => compact_result(CheckConnectionOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "list_capabilities",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ListCapabilitiesOutput>().expect("valid list_capabilities output schema"),
        description = "List IMAP server capabilities for an account. Shows supported extensions like IDLE, MOVE, CONDSTORE, etc.",
        annotations(title = "List IMAP Capabilities", read_only_hint = true)
    )]
    async fn list_capabilities_tool(
        &self,
        Parameters(args): Parameters<ListCapabilitiesArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.agentmail.list_capabilities(&args.account).await {
            Ok(data) => compact_result(ListCapabilitiesOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "get_messages",
        output_schema = rmcp::handler::server::tool::schema_for_output::<GetMessagesOutput>().expect("valid get_messages output schema"),
        description = "Fetch a metadata-only page of messages from one required mailbox, newest-first. Returns account, mailbox, UIDVALIDITY, pagination data (offset, limit, total, nextOffset), and compact rows with UID, subject, sender, date, flags, size, and a UIDVALIDITY-safe resourceUri. Read resourceUri for markdown content, append /headers for exact headers, or append /source for bounded raw RFC822. Get mailbox names from list_mailboxes. Defaults: offset=0, limit=25 (max 50).",
        annotations(title = "Get Messages", read_only_hint = true)
    )]
    async fn get_messages_tool(
        &self,
        Parameters(args): Parameters<GetMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        let Pagination { offset, limit } = Pagination::new(args.offset, args.limit, 25, 50)?;

        match self
            .agentmail
            .get_messages(&args.mailbox, &args.account, offset, limit, false, false)
            .await
        {
            Ok(data) => compact_result(GetMessagesOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "search_messages",
        output_schema = rmcp::handler::server::tool::schema_for_output::<SearchMessagesOutput>().expect("valid search_messages output schema"),
        description = "Search one required mailbox with filters: senderContains, subjectContains, toContains, query (IMAP full-text), read, flagged, headerKey/headerValueContains, since/before (YYYY-MM-DD), and largerThan/smallerThan (bytes). Filters are AND-combined. Returns metadata-only results newest-first with the mailbox UIDVALIDITY, pagination data (offset, limit, total, nextOffset), and one UIDVALIDITY-safe resourceUri per row; read that resource for content or append /headers or /source. Get mailbox names from list_mailboxes. Defaults: offset=0, limit=25 (max 50).",
        annotations(title = "Search Messages", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn search_messages_tool(
        &self,
        Parameters(args): Parameters<SearchMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        let Pagination { offset, limit } = Pagination::new(args.offset, args.limit, 25, 50)?;

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
            since: parse_search_date(args.since.as_deref())?,
            before: parse_search_date(args.before.as_deref())?,
            larger_than: args.larger_than,
            smaller_than: args.smaller_than,
        };

        match self
            .agentmail
            .search_messages(
                &args.mailbox,
                &args.account,
                &criteria,
                offset,
                limit,
                false,
                false,
            )
            .await
        {
            Ok(data) => compact_result(SearchMessagesOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "list_flags",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ListFlagsOutput>().expect("valid list_flags output schema"),
        description = "List all IMAP flags in use with counts per flag (e.g. \\Seen: 1234, \\Flagged: 56). Omit mailbox for account-wide discovery: one selectable \\All mailbox is preferred, otherwise selectable storage mailboxes are enumerated without Trash/Junk/Drafts or virtual aggregate views. Resolves Apple Mail $MailFlagBit color flags to color names (red, orange, yellow, green, blue, purple, gray).",
        annotations(title = "List Flags", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn list_flags_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<ListFlagsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let result = self
            .agentmail
            .list_flags(
                args.mailbox.as_deref(),
                &args.account,
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(ListFlagsOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "find_attachments",
        output_schema = rmcp::handler::server::tool::schema_for_output::<FindAttachmentsOutput>().expect("valid find_attachments output schema"),
        description = "Find messages with attachments (multipart/mixed or multipart/related), newest-first. Each hit includes mailbox, UIDVALIDITY, UID, date, and resourceUri so account-wide UID collisions are unambiguous. Omit mailbox for account-wide discovery: one selectable \\All mailbox is preferred, otherwise selectable storage mailboxes are enumerated without excluded or virtual views. Defaults: offset=0, limit=25 (max 100). To save files, pass a hit's mailbox, uid, and uidValidity as expectedUidValidity to download_attachments.",
        annotations(title = "Find Attachments", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn find_attachments_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<FindAttachmentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let Pagination { offset, limit } = Pagination::new(args.offset, args.limit, 25, 100)?;
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);

        let result = self
            .agentmail
            .find_attachments(
                args.mailbox.as_deref(),
                &args.account,
                offset,
                limit,
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(FindAttachmentsOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "top_senders",
        output_schema = rmcp::handler::server::tool::schema_for_output::<TopSendersOutput>().expect("valid top_senders output schema"),
        description = "List the senders who email you most, by message count. Omit mailbox for account-wide discovery: one selectable \\All mailbox is scanned alone when available; otherwise selectable storage mailboxes are enumerated, virtual/excluded views are skipped, and Message-ID deduplicates across folders. Excludes your own address and groups by exact address + display name. Every row has a safe nested sample {mailbox, uidValidity, uid, resourceUri} for inspection; pass its exact address and displayName to delete_by_sender or move_by_sender. Live offset pagination defaults to 10 rows (max 100).",
        annotations(title = "Top Senders", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn top_senders_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<TopSendersArgs>,
    ) -> Result<CallToolResult, McpError> {
        let Pagination { offset, limit } = Pagination::new(args.offset, args.limit, 10, 100)?;
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);

        let result = self
            .agentmail
            .top_senders(
                args.mailbox.as_deref(),
                &args.account,
                offset,
                limit,
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(TopSendersOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "top_domains",
        output_schema = rmcp::handler::server::tool::schema_for_output::<TopDomainsOutput>().expect("valid top_domains output schema"),
        description = "List exact canonical Header From domains by message count. A parent and each subdomain are separate rows: example.com never includes mail.example.com. Each row supplies registrableDomain and subdomain as Public Suffix List context, plus a UIDVALIDITY-safe sample and its decoded subject when available. Header From is organizational metadata, not proof of DKIM ownership. Omit mailbox for account-wide discovery. Live offset pagination defaults to 20 rows (max 100); use the exact domain value with delete_by_domain or move_by_domain.",
        annotations(title = "Top Domains", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn top_domains_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<TopDomainsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let Pagination { offset, limit } = Pagination::new(args.offset, args.limit, 20, 100)?;
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);

        let result = self
            .agentmail
            .top_domains(
                args.mailbox.as_deref(),
                &args.account,
                offset,
                limit,
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(TopDomainsOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "top_subscriptions",
        output_schema = rmcp::handler::server::tool::schema_for_output::<TopSubscriptionsOutput>().expect("valid top_subscriptions output schema"),
        description = "List bulk/marketing subscriptions by message count. Omit mailbox for account-wide discovery, which prefers one selectable \\All mailbox and otherwise enumerates storage mailboxes. Rows are grouped only by normalized sender email; display names and List-Id values do not split a sender's row. Rows are sorted by advertised one-click syntax, then count. Each has a nested sample {mailbox, uidValidity, uid, resourceUri} and, when available, the sample message's decoded subject — what this subscription's mail actually looks like. Map the sample to move_subscription to file exact bulk-mail matches without an unsubscribe request, or to unsubscribe_message for a consented verified unsubscribe. advertisedOneClick is syntactic only—the unsubscribe action re-fetches the complete message and verifies DKIM. Live offset pagination defaults to 10 rows (max 100).",
        annotations(title = "Top Subscriptions", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn top_subscriptions_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<TopSubscriptionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let Pagination { offset, limit } = Pagination::new(args.offset, args.limit, 10, 100)?;
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);

        let result = self
            .agentmail
            .top_subscriptions(
                args.mailbox.as_deref(),
                &args.account,
                offset,
                limit,
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(TopSubscriptionsOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "top_mailing_lists",
        output_schema = rmcp::handler::server::tool::schema_for_output::<TopMailingListsOutput>().expect("valid top_mailing_lists output schema"),
        description = "List mailing lists by normalized List-Id (RFC 2919), highest volume first, including List-Id-only messages and grouping across senders. Each row includes a bounded sender preview, senderCount, a UIDVALIDITY-safe nested sample for inspection, and, when available, the sample message's decoded subject — what this list's mail actually looks like. Omit mailbox for account-wide discovery, which prefers one selectable \\All mailbox and otherwise enumerates storage mailboxes. Live offset pagination defaults to 10 rows (max 100). Use delete_list_id or move_list_id with an approved exact listId.",
        annotations(title = "Top Mailing Lists", read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn top_mailing_lists_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<TopMailingListsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let Pagination { offset, limit } = Pagination::new(args.offset, args.limit, 10, 100)?;
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);

        let result = self
            .agentmail
            .top_mailing_lists(
                args.mailbox.as_deref(),
                &args.account,
                offset,
                limit,
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(TopMailingListsOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "list_pending_moves",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ListPendingMovesOutput>().expect("valid list_pending_moves output schema"),
        description = "List durable non-native IMAP MOVE operations that still need automatic reconciliation or human attention. Each row includes operationId, the source UID identity, destination, status, detail, and timestamps. Use reconcile_moves with one operationId, or omit it to reconcile all pending operations for the account.",
        annotations(
            title = "List Pending Moves",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_pending_moves_tool(
        &self,
        Parameters(args): Parameters<ListPendingMovesArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.agentmail.list_pending_moves(&args.account).await {
            Ok(data) => compact_result(ListPendingMovesOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }
}
