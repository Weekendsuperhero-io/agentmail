//! Mutating MCP tools: mailbox/draft creation, flags, moves, deletes, unsubscribe.

use super::AgentMailServer;
use super::args::*;
use super::wire::{
    AddFlagsOutput, CreateDraftOutput, CreateMailboxOutput, DeleteByDomainOutput,
    DeleteBySenderOutput, DeleteListIdOutput, DeleteMessagesOutput, DownloadAttachmentsOutput,
    MoveByDomainOutput, MoveBySenderOutput, MoveListIdOutput, MoveMessageOutput,
    MoveSubscriptionOutput, ReconcileMovesOutput, RemoveFlagsOutput, UnsubscribeMessageOutput,
    compact_result, tool_error_result,
};
use super::{make_cancel_fn, make_progress_fn};
use crate::{
    CleanupDeletion, CleanupIdentityMode, CleanupPolicy, CleanupWhen, DeleteMode, DraftAttachment,
    UnsubscribeOptions,
};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Meta},
    tool, tool_router,
};
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;

const MAX_DRAFT_ATTACHMENTS: usize = 20;
const MAX_DRAFT_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
const MAX_DRAFT_ATTACHMENT_TOTAL_BYTES: u64 = 40 * 1024 * 1024;

/// Map the flat `permanent` tool argument to a `DeleteMode`.
fn delete_mode(permanent: bool) -> DeleteMode {
    if permanent {
        DeleteMode::Permanent
    } else {
        DeleteMode::TrashFirst
    }
}

/// Map the wire cleanup spec to the internal policy.
fn cleanup_policy(spec: UnsubscribeCleanupSpec) -> CleanupPolicy {
    CleanupPolicy {
        when: match spec.when {
            CleanupWhenArg::AfterSuccess => CleanupWhen::AfterSuccess,
            CleanupWhenArg::Always => CleanupWhen::Always,
        },
        identity: match spec.identity {
            CleanupIdentityArg::ListIdOnly => CleanupIdentityMode::ListIdOnly,
            CleanupIdentityArg::ListIdOrSender => CleanupIdentityMode::ListIdOrSender,
        },
        deletion: match spec.deletion {
            CleanupDeletionArg::Trash => CleanupDeletion::Trash,
            CleanupDeletionArg::TrashThenPermanent => CleanupDeletion::TrashThenPermanent,
            CleanupDeletionArg::Permanent => CleanupDeletion::Permanent,
        },
    }
}

#[tool_router(router = write_tools_router, vis = "pub(super)")]
impl AgentMailServer {
    #[tool(
        name = "create_mailbox",
        output_schema = rmcp::handler::server::tool::schema_for_output::<CreateMailboxOutput>().expect("valid create_mailbox output schema"),
        description = "Create a mailbox on the IMAP server. For a nested mailbox, use the hierarchy delimiter reported by list_mailboxes (commonly '/').",
        annotations(
            title = "Create Mailbox",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn create_mailbox_tool(
        &self,
        Parameters(args): Parameters<CreateMailboxArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        match self
            .agentmail
            .create_mailbox(&args.account, &args.mailbox)
            .await
        {
            Ok(data) => compact_result(CreateMailboxOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "delete_messages",
        output_schema = rmcp::handler::server::tool::schema_for_output::<DeleteMessagesOutput>().expect("valid delete_messages output schema"),
        description = "Delete one or more messages by UID. Requires expectedUidValidity from the same mailbox discovery response and fails before mutation if the UID epoch changed. Moves to Trash when available; otherwise permanent fallback follows the requested policy. Supports 1 to 500 non-zero UIDs per call.",
        annotations(
            title = "Delete Messages",
            destructive_hint = true,
            idempotent_hint = true
        ),
        execution(task_support = "optional")
    )]
    async fn delete_messages_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<DeleteMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        if args.uids.is_empty() {
            return Err(McpError::invalid_params(
                "uids must contain at least one UID",
                None,
            ));
        }
        if args.uids.len() > 500 {
            return Err(McpError::invalid_params(
                "uids supports up to 500 UIDs per call",
                None,
            ));
        }

        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let result = self
            .agentmail
            .delete_messages(
                &args.mailbox,
                &args.account,
                &args.uids,
                args.expected_uid_validity,
                delete_mode(args.permanent),
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(DeleteMessagesOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "delete_by_sender",
        output_schema = rmcp::handler::server::tool::schema_for_output::<DeleteBySenderOutput>().expect("valid delete_by_sender output schema"),
        description = "Delete all messages from an exact sender identity. Pass the address and displayName exactly as returned by a top_senders row; matching is exact on both fields, confirmed live before any deletion. Omit mailbox to enumerate selectable storage mailboxes; mutation planning excludes Trash/Junk/Drafts and never writes through \\All, \\Flagged, or \\Important aggregate views. Do not use this for mailing-list cleanup (use delete_list_id or unsubscribe_message cleanup).",
        annotations(title = "Delete by Sender", destructive_hint = true),
        execution(task_support = "optional")
    )]
    async fn delete_by_sender_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<DeleteBySenderArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.email.trim().is_empty() {
            return Err(McpError::invalid_params("email is required", None));
        }

        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let result = self
            .agentmail
            .delete_by_sender(
                args.mailbox.as_deref(),
                &args.account,
                &args.email,
                args.name.as_deref().unwrap_or_default(),
                delete_mode(args.permanent),
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(DeleteBySenderOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "delete_by_domain",
        output_schema = rmcp::handler::server::tool::schema_for_output::<DeleteByDomainOutput>().expect("valid delete_by_domain output schema"),
        description = "Delete every message whose first parsed Header From address has one exact canonical domain. Pass domain exactly as returned by top_domains: example.com never includes mail.example.com or any other subdomain. Omit mailbox to sweep the account-wide mutation plan, which excludes Trash/Junk/Drafts and aggregate views. With permanent=false, Trash is preferred but permanent fallback may be used when Trash is unavailable; permanent=true bypasses Trash. Header From matching is organizational cleanup, not DKIM authentication.",
        annotations(
            title = "Delete by Exact Domain",
            destructive_hint = true,
            idempotent_hint = true
        ),
        execution(task_support = "optional")
    )]
    async fn delete_by_domain_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<DeleteByDomainArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.domain.trim().is_empty() {
            return Err(McpError::invalid_params("domain is required", None));
        }

        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let result = self
            .agentmail
            .delete_by_domain(
                args.mailbox.as_deref(),
                &args.account,
                &args.domain,
                delete_mode(args.permanent),
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(DeleteByDomainOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "download_attachments",
        output_schema = rmcp::handler::server::tool::schema_for_output::<DownloadAttachmentsOutput>().expect("valid download_attachments output schema"),
        description = "Download all attachments from one message to disk. Pass mailbox, uid, and expectedUidValidity from the same find_attachments hit; the download fails before filesystem writes if the mailbox UID epoch changed. Files are saved as {uid}_{originalname}. Returns paths, content types, and sizes.",
        annotations(
            title = "Download Attachments",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        ),
        execution(task_support = "optional")
    )]
    async fn download_attachments_tool(
        &self,
        Parameters(args): Parameters<DownloadAttachmentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        // Confine the LLM-supplied output directory to the file sandbox: a
        // prompt-injection payload must not be able to write attacker bytes
        // into a sensitive directory (e.g. ~/.ssh). Absolute/`..` escapes are
        // rejected; the default lands in the sandbox root.
        let output_dir = match self.file_access.confine_dir(args.output_dir.as_deref()) {
            Ok(dir) => dir,
            Err(reason) => return Err(McpError::invalid_params(reason, None)),
        };

        match self
            .agentmail
            .download_attachments(
                &args.mailbox,
                &args.account,
                args.uid,
                args.expected_uid_validity,
                &output_dir,
            )
            .await
        {
            Ok(data) => compact_result(DownloadAttachmentsOutput::new(
                data,
                args.expected_uid_validity,
            )),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "create_draft",
        output_schema = rmcp::handler::server::tool::schema_for_output::<CreateDraftOutput>().expect("valid create_draft output schema"),
        description = "Create and save a complete RFC822 draft. Resolves the account's selectable \\Drafts special-use mailbox, falls back to Drafts and creates it when needed, then APPENDs the message with the \\Draft flag. Requires at least one to, cc, or bcc recipient. Subject and body are optional; attachments may reference local file paths. Returns the new draft's uid, uidValidity, and resourceUri when the server allows recovering them.",
        annotations(
            title = "Create Draft",
            read_only_hint = false,
            destructive_hint = false
        )
    )]
    async fn create_draft_tool(
        &self,
        Parameters(args): Parameters<CreateDraftArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.to.is_empty() && args.cc.is_empty() && args.bcc.is_empty() {
            return Err(McpError::invalid_params(
                "At least one recipient (to, cc, or bcc) is required",
                None,
            ));
        }
        if args.attachments.len() > MAX_DRAFT_ATTACHMENTS {
            return Err(McpError::invalid_params(
                format!("attachments supports at most {MAX_DRAFT_ATTACHMENTS} files"),
                None,
            ));
        }

        // Resolve and stat every attachment before reading the first byte, so
        // an oversized aggregate cannot leave a large partially loaded draft.
        let mut preflight = Vec::with_capacity(args.attachments.len());
        let mut preflight_total = 0_u64;
        for (index, attachment) in args.attachments.iter().enumerate() {
            let safe_path = self
                .file_access
                .confine_read(&attachment.path)
                .map_err(|reason| {
                    McpError::invalid_params(format!("attachment #{}: {reason}", index + 1), None)
                })?;
            let file = tokio::fs::File::open(&safe_path).await.map_err(|error| {
                McpError::invalid_params(
                    format!(
                        "Failed to open attachment #{} at '{}': {error}",
                        index + 1,
                        attachment.path
                    ),
                    None,
                )
            })?;
            let metadata = file.metadata().await.map_err(|error| {
                McpError::invalid_params(
                    format!(
                        "Failed to inspect attachment #{} at '{}': {error}",
                        index + 1,
                        attachment.path
                    ),
                    None,
                )
            })?;
            if !metadata.is_file() {
                return Err(McpError::invalid_params(
                    format!("attachment #{} is not a regular file", index + 1),
                    None,
                ));
            }
            let size = metadata.len();
            if size > MAX_DRAFT_ATTACHMENT_BYTES {
                return Err(McpError::invalid_params(
                    format!(
                        "attachment #{} is {size} bytes; maximum per file is {MAX_DRAFT_ATTACHMENT_BYTES} bytes",
                        index + 1
                    ),
                    None,
                ));
            }
            preflight_total = preflight_total.checked_add(size).ok_or_else(|| {
                McpError::invalid_params("attachment aggregate size overflow", None)
            })?;
            if preflight_total > MAX_DRAFT_ATTACHMENT_TOTAL_BYTES {
                return Err(McpError::invalid_params(
                    format!(
                        "attachments total {preflight_total} bytes; aggregate maximum is {MAX_DRAFT_ATTACHMENT_TOTAL_BYTES} bytes"
                    ),
                    None,
                ));
            }
            preflight.push((size, file));
        }

        let mut loaded: Vec<DraftAttachment> = Vec::with_capacity(args.attachments.len());
        let mut loaded_total = 0_u64;
        for (i, (a, (preflight_size, file))) in args.attachments.iter().zip(preflight).enumerate() {
            let mut data = Vec::with_capacity(preflight_size as usize);
            file.take(MAX_DRAFT_ATTACHMENT_BYTES + 1)
                .read_to_end(&mut data)
                .await
                .map_err(|error| {
                    McpError::invalid_params(
                        format!(
                            "Failed to read attachment #{} at '{}': {error}",
                            i + 1,
                            a.path
                        ),
                        None,
                    )
                })?;
            if data.len() as u64 > MAX_DRAFT_ATTACHMENT_BYTES {
                return Err(McpError::invalid_params(
                    format!(
                        "attachment #{} grew beyond the {MAX_DRAFT_ATTACHMENT_BYTES}-byte limit while being read",
                        i + 1
                    ),
                    None,
                ));
            }
            loaded_total = loaded_total.checked_add(data.len() as u64).ok_or_else(|| {
                McpError::invalid_params("attachment aggregate size overflow", None)
            })?;
            if loaded_total > MAX_DRAFT_ATTACHMENT_TOTAL_BYTES {
                return Err(McpError::invalid_params(
                    format!(
                        "attachments grew beyond the {MAX_DRAFT_ATTACHMENT_TOTAL_BYTES}-byte aggregate limit while being read"
                    ),
                    None,
                ));
            }
            let filename = a
                .filename
                .clone()
                .or_else(|| {
                    std::path::Path::new(&a.path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| format!("attachment-{}", i + 1));
            let content_type = a
                .content_type
                .clone()
                .unwrap_or_else(|| guess_content_type(&filename));
            loaded.push(DraftAttachment {
                filename,
                content_type,
                data,
            });
        }

        match self
            .agentmail
            .create_draft(
                &args.account,
                args.subject.trim(),
                &args.body,
                &args.to,
                &args.cc,
                &args.bcc,
                &loaded,
            )
            .await
        {
            Ok(data) => compact_result(CreateDraftOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "move_message",
        output_schema = rmcp::handler::server::tool::schema_for_output::<MoveMessageOutput>().expect("valid move_message output schema"),
        description = "Move one message between mailboxes. Requires source mailbox, destination, uid, and expectedUidValidity from the message's discovery response; the action fails before mutation if the source UID epoch changed. Success confirms source-to-destination completion but does not claim a destination UID.",
        annotations(
            title = "Move Message",
            read_only_hint = false,
            destructive_hint = false
        )
    )]
    async fn move_message_tool(
        &self,
        Parameters(args): Parameters<MoveMessageArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        if args.destination.trim().is_empty() {
            return Err(McpError::invalid_params("destination is required", None));
        }

        match self
            .agentmail
            .move_message(
                &args.mailbox,
                &args.account,
                args.uid,
                args.expected_uid_validity,
                &args.destination,
            )
            .await
        {
            Ok(data) => compact_result(MoveMessageOutput::new(data, args.expected_uid_validity)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "unsubscribe_message",
        output_schema = rmcp::handler::server::tool::schema_for_output::<UnsubscribeMessageOutput>().expect("valid unsubscribe_message output schema"),
        description = "Perform a live-validated RFC 8058 one-click unsubscribe and optionally delete matching messages account-wide. Map a top_subscriptions row's nested sample to mailbox, uid, and expectedUidValidity, and require explicit confirmOneClick=true. The POST requires exact headers, local DKIM verification covering both action headers, one public HTTPS destination, no redirects, and a direct 2xx. Omit cleanup to only unsubscribe. When cleanup is present it deletes by the DKIM-authenticated List-Id, or (identity \"listIdOrSender\", the default) falls back to exact sender email + List-Unsubscribe-Post + the target's List-Id when it has one; cleanup.when gates running after a failed attempt and cleanup.deletion controls Trash versus permanent disposal.",
        annotations(
            title = "Unsubscribe from Mailing List",
            destructive_hint = true,
            open_world_hint = true
        ),
        execution(task_support = "optional")
    )]
    async fn unsubscribe_message_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<UnsubscribeMessageArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);

        let result = self
            .agentmail
            .unsubscribe_message(
                &args.mailbox,
                &args.account,
                args.uid,
                UnsubscribeOptions {
                    expected_uid_validity: args.expected_uid_validity,
                    confirm_one_click: args.confirm_one_click,
                    cleanup: args.cleanup.map(cleanup_policy),
                },
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(UnsubscribeMessageOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "delete_list_id",
        output_schema = rmcp::handler::server::tool::schema_for_output::<DeleteListIdOutput>().expect("valid delete_list_id output schema"),
        description = "Delete all messages with a specific List-Id. Identifies the list by its exact List-Id value from top_mailing_lists and deletes matching messages regardless of sender. Omit mailbox to enumerate selectable storage mailboxes; mutation planning excludes Trash/Junk/Drafts and never writes through \\All, \\Flagged, or \\Important aggregate views.",
        annotations(title = "Delete Mailing List by List-Id", destructive_hint = true),
        execution(task_support = "optional")
    )]
    async fn delete_list_id_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<DeleteListIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let result = self
            .agentmail
            .delete_list_id(
                args.mailbox.as_deref(),
                &args.account,
                &args.list_id,
                delete_mode(args.permanent),
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(DeleteListIdOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "move_list_id",
        output_schema = rmcp::handler::server::tool::schema_for_output::<MoveListIdOutput>().expect("valid move_list_id output schema"),
        description = "Move all messages with a specific List-Id to a destination mailbox in one operation — e.g. archive a newsletter or statement list without per-message calls. Identifies the list by its exact List-Id value from top_mailing_lists. The destination must already exist (create_mailbox first if needed). Omit mailbox to sweep selectable storage mailboxes account-wide; the destination itself is always excluded, and mutation planning excludes Trash/Junk/Drafts and aggregate views.",
        annotations(
            title = "Move Mailing List by List-Id",
            read_only_hint = false,
            destructive_hint = false
        ),
        execution(task_support = "optional")
    )]
    async fn move_list_id_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<MoveListIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.destination.trim().is_empty() {
            return Err(McpError::invalid_params("destination is required", None));
        }
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let result = self
            .agentmail
            .move_list_id(
                args.mailbox.as_deref(),
                &args.account,
                &args.list_id,
                &args.destination,
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(MoveListIdOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "move_by_sender",
        output_schema = rmcp::handler::server::tool::schema_for_output::<MoveBySenderOutput>().expect("valid move_by_sender output schema"),
        description = "Move all messages from an exact sender identity to a destination mailbox in one operation — e.g. file monthly statements from a bank into a folder. Pass the address and displayName exactly as returned by a top_senders row; matching is exact on both fields, confirmed live before any move. The destination must already exist. Omit mailbox to sweep selectable storage mailboxes account-wide; the destination itself is always excluded.",
        annotations(
            title = "Move by Sender",
            read_only_hint = false,
            destructive_hint = false
        ),
        execution(task_support = "optional")
    )]
    async fn move_by_sender_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<MoveBySenderArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.email.trim().is_empty() {
            return Err(McpError::invalid_params("email is required", None));
        }
        if args.destination.trim().is_empty() {
            return Err(McpError::invalid_params("destination is required", None));
        }
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let result = self
            .agentmail
            .move_by_sender(
                args.mailbox.as_deref(),
                &args.account,
                &args.email,
                args.name.as_deref().unwrap_or_default(),
                &args.destination,
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(MoveBySenderOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "move_by_domain",
        output_schema = rmcp::handler::server::tool::schema_for_output::<MoveByDomainOutput>().expect("valid move_by_domain output schema"),
        description = "Move every message whose first parsed Header From address has one exact canonical domain to an existing destination mailbox. Pass domain exactly as returned by top_domains: example.com never includes mail.example.com or any other subdomain. Omit mailbox to sweep selectable storage mailboxes account-wide; the destination itself is excluded. Header From matching is organizational cleanup, not DKIM authentication.",
        annotations(
            title = "Move by Exact Domain",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        ),
        execution(task_support = "optional")
    )]
    async fn move_by_domain_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<MoveByDomainArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.domain.trim().is_empty() {
            return Err(McpError::invalid_params("domain is required", None));
        }
        if args.destination.trim().is_empty() {
            return Err(McpError::invalid_params("destination is required", None));
        }
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let result = self
            .agentmail
            .move_by_domain(
                args.mailbox.as_deref(),
                &args.account,
                &args.domain,
                &args.destination,
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(MoveByDomainOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "move_subscription",
        output_schema = rmcp::handler::server::tool::schema_for_output::<MoveSubscriptionOutput>().expect("valid move_subscription output schema"),
        description = "Move the exact bulk-mail subscription represented by one top_subscriptions row to an existing destination mailbox. Map the row's nested sample to mailbox, expectedUidValidity, and uid; the action re-fetches that sample live, derives its canonical sender email and single List-Id when present, then sweeps the account-wide mutation plan. Every moved message must have the exact sender plus List-Unsubscribe or List-Unsubscribe-Post, and must also have the same List-Id when the sample has one. The destination is excluded. This files mail only; it does not send an unsubscribe request.",
        annotations(
            title = "Move Top Subscription",
            read_only_hint = false,
            destructive_hint = false
        ),
        execution(task_support = "optional")
    )]
    async fn move_subscription_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<MoveSubscriptionArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        if args.destination.trim().is_empty() {
            return Err(McpError::invalid_params("destination is required", None));
        }
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let result = self
            .agentmail
            .move_subscription(
                &args.mailbox,
                &args.account,
                args.uid,
                args.expected_uid_validity,
                &args.destination,
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(MoveSubscriptionOutput::from(data)),
            Err(error) => Ok(tool_error_result(&error)),
        }
    }

    #[tool(
        name = "reconcile_moves",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ReconcileMovesOutput>().expect("valid reconcile_moves output schema"),
        description = "Safely reconcile durable non-native IMAP MOVE operations after a connection loss or ambiguous COPY/delete boundary. Pass one operationId from list_pending_moves, or omit it to process all pending operations for the account. The reconciler only removes a source message after it can prove the destination copy; ambiguous cases remain needsAttention instead of risking loss or duplicate deletion.",
        annotations(
            title = "Reconcile Pending Moves",
            destructive_hint = true,
            idempotent_hint = true
        ),
        execution(task_support = "optional")
    )]
    async fn reconcile_moves_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<ReconcileMovesArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args
            .operation_id
            .as_deref()
            .is_some_and(|operation_id| operation_id.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                "operationId cannot be empty",
                None,
            ));
        }
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let result = self
            .agentmail
            .reconcile_moves(
                &args.account,
                args.operation_id.as_deref(),
                progress.callback(),
                Some(&cancel),
            )
            .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(ReconcileMovesOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "add_flags",
        output_schema = rmcp::handler::server::tool::schema_for_output::<AddFlagsOutput>().expect("valid add_flags output schema"),
        description = "Add flags and/or set an Apple Mail color on a message identified by mailbox, uid, and expectedUidValidity. The update fails before mutation if the mailbox UID epoch changed. Flags use union semantics, preserving existing flags. Cannot set \\Deleted (use delete_messages) or \\Recent (read-only).",
        annotations(
            title = "Add Flags / Set Color",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn add_flags_tool(
        &self,
        Parameters(args): Parameters<AddFlagsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        if args.flags.is_empty() && args.color.is_none() {
            return Err(McpError::invalid_params(
                "At least one flag or a color is required",
                None,
            ));
        }
        // Guard dangerous flags
        for flag in &args.flags {
            let lower = flag.to_lowercase();
            if lower == "\\deleted" {
                return Err(McpError::invalid_params(
                    "Cannot set \\Deleted via add_flags — use delete_messages instead",
                    None,
                ));
            }
            if lower == "\\recent" {
                return Err(McpError::invalid_params(
                    "Cannot set \\Recent — it is a read-only server flag",
                    None,
                ));
            }
        }
        match self
            .agentmail
            .add_flags(
                &args.mailbox,
                &args.account,
                args.uid,
                args.expected_uid_validity,
                &args.flags,
                args.color.as_deref(),
            )
            .await
        {
            Ok(data) => compact_result(AddFlagsOutput::new(data, args.expected_uid_validity)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "remove_flags",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RemoveFlagsOutput>().expect("valid remove_flags output schema"),
        description = "Remove flags and/or clear Apple Mail color from a message identified by mailbox, uid, and expectedUidValidity. The update fails before mutation if the mailbox UID epoch changed. Only specified flags are removed; all others remain. Set clearColor=true to clear \\Flagged plus $MailFlagBit keywords. Cannot remove \\Deleted or \\Recent.",
        annotations(
            title = "Remove Flags / Clear Color",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn remove_flags_tool(
        &self,
        Parameters(args): Parameters<RemoveFlagsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        if args.flags.is_empty() && !args.clear_color {
            return Err(McpError::invalid_params(
                "At least one flag or clearColor=true is required",
                None,
            ));
        }
        // Guard dangerous flags
        for flag in &args.flags {
            let lower = flag.to_lowercase();
            if lower == "\\deleted" {
                return Err(McpError::invalid_params(
                    "Cannot remove \\Deleted via remove_flags — use delete_messages instead",
                    None,
                ));
            }
            if lower == "\\recent" {
                return Err(McpError::invalid_params(
                    "Cannot remove \\Recent — it is a read-only server flag",
                    None,
                ));
            }
        }
        match self
            .agentmail
            .remove_flags(
                &args.mailbox,
                &args.account,
                args.uid,
                args.expected_uid_validity,
                &args.flags,
                args.clear_color,
            )
            .await
        {
            Ok(data) => compact_result(RemoveFlagsOutput::new(data, args.expected_uid_validity)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }
}

/// Guess a MIME type from a filename extension. Falls back to application/octet-stream.
fn guess_content_type(filename: &str) -> String {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" | "heif" => "image/heic",
        "svg" => "image/svg+xml",
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "rtf" => "application/rtf",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "wav" => "audio/wav",
        "ics" => "text/calendar",
        "vcf" => "text/vcard",
        "eml" => "message/rfc822",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::guess_content_type;

    #[test]
    fn guess_content_type_maps_known_extensions_and_falls_back() {
        assert_eq!(guess_content_type("report.pdf"), "application/pdf");
        assert_eq!(guess_content_type("photo.PNG"), "image/png"); // case-insensitive
        assert_eq!(guess_content_type("notes.txt"), "text/plain");
        assert_eq!(guess_content_type("forward.eml"), "message/rfc822");
        // Unknown and extension-less both fall back to the generic binary type.
        assert_eq!(
            guess_content_type("mystery.xyz123"),
            "application/octet-stream"
        );
        assert_eq!(guess_content_type("README"), "application/octet-stream");
    }
}
