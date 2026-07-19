//! Mutating MCP tools: mailbox/draft creation, flags, moves, deletes, unsubscribe.

use super::AgentMailServer;
use super::args::*;
use super::wire::{
    AddFlagsOutput, CreateDraftOutput, CreateMailboxOutput, DeleteBySenderOutput,
    DeleteListIdOutput, DeleteMessagesOutput, DownloadAttachmentsOutput, MoveMessageOutput,
    RemoveFlagsOutput, UnsubscribeMessageOutput, compact_result,
};
use super::{make_cancel_fn, make_progress_fn, to_mcp_error};
use crate::{DeleteMode, DraftAttachment, UnsubscribeOptions};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Meta},
    tool, tool_router,
};
use tokio_util::sync::CancellationToken;

/// Map the flat `permanent` tool argument to a `DeleteMode`.
fn delete_mode(permanent: bool) -> DeleteMode {
    if permanent {
        DeleteMode::Permanent
    } else {
        DeleteMode::TrashFirst
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
            Err(e) => Err(to_mcp_error(&e)),
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
        match self
            .agentmail
            .delete_messages(
                &args.mailbox,
                &args.account,
                &args.uids,
                args.expected_uid_validity,
                delete_mode(args.permanent),
                progress.as_ref(),
                Some(&cancel),
            )
            .await
        {
            Ok(data) => compact_result(DeleteMessagesOutput::from(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "delete_by_sender",
        output_schema = rmcp::handler::server::tool::schema_for_output::<DeleteBySenderOutput>().expect("valid delete_by_sender output schema"),
        description = "Delete messages from the exact address + display name represented by a sample message. From top_senders sample, pass sample.mailbox as mailbox, sample.uid as uid, and sample.uidValidity as expectedUidValidity; the action fails before mutation if the epoch changed. Set allMailboxes=true to enumerate selectable storage mailboxes; mutation planning excludes Trash/Junk/Drafts and never writes through \\All, \\Flagged, or \\Important aggregate views. Do not use this for mailing-list cleanup.",
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
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }

        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        match self
            .agentmail
            .delete_by_sender(
                &args.mailbox,
                &args.account,
                args.uid,
                args.expected_uid_validity,
                args.all_mailboxes,
                delete_mode(args.permanent),
                progress.as_ref(),
                Some(&cancel),
            )
            .await
        {
            Ok(data) => compact_result(DeleteBySenderOutput::from(data)),
            Err(e) => Err(to_mcp_error(&e)),
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
        let output_dir = args
            .output_dir
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

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
            Err(e) => Err(to_mcp_error(&e)),
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

        // Load attachments (best-effort per file; surface first error clearly)
        let mut loaded: Vec<DraftAttachment> = Vec::with_capacity(args.attachments.len());
        for (i, a) in args.attachments.iter().enumerate() {
            let data = match tokio::fs::read(&a.path).await {
                Ok(d) => d,
                Err(e) => {
                    return Err(McpError::invalid_params(
                        format!(
                            "Failed to read attachment #{} at '{}': {}",
                            i + 1,
                            a.path,
                            e
                        ),
                        None,
                    ));
                }
            };
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
            Err(e) => Err(to_mcp_error(&e)),
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
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "unsubscribe_message",
        output_schema = rmcp::handler::server::tool::schema_for_output::<UnsubscribeMessageOutput>().expect("valid unsubscribe_message output schema"),
        description = "Perform a live-validated RFC 8058 one-click unsubscribe and optionally clean up the same List-Id account-wide. Map a top_subscriptions row's nested sample to mailbox, uid, and expectedUidValidity, and require explicit confirmOneClick=true. The POST requires exact headers, local DKIM verification covering both action headers, one public HTTPS destination, no redirects, and a direct 2xx. deleteMatching defaults false; when it runs without a DKIM-authenticated List-Id, cleanup falls back to the exact sender's bulk mail (allowSenderFallback defaults true). Cleanup stops after failure and never becomes permanent unless separately authorized.",
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

        match self
            .agentmail
            .unsubscribe_message(
                &args.mailbox,
                &args.account,
                args.uid,
                UnsubscribeOptions {
                    expected_uid_validity: args.expected_uid_validity,
                    confirm_one_click: args.confirm_one_click,
                    delete_matching: args.delete_matching,
                    delete_on_unsubscribe_failure: args.delete_on_unsubscribe_failure,
                    allow_sender_fallback: args.allow_sender_fallback,
                    allow_permanent_fallback: args.allow_permanent_fallback,
                    mode: delete_mode(args.permanent),
                },
                progress.as_ref(),
                Some(&cancel),
            )
            .await
        {
            Ok(data) => compact_result(UnsubscribeMessageOutput::from(data)),
            Err(e) => Err(to_mcp_error(&e)),
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
        match self
            .agentmail
            .delete_list_id(
                args.mailbox.as_deref(),
                &args.account,
                &args.list_id,
                delete_mode(args.permanent),
                progress.as_ref(),
                Some(&cancel),
            )
            .await
        {
            Ok(data) => compact_result(DeleteListIdOutput::from(data)),
            Err(e) => Err(to_mcp_error(&e)),
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
            Err(e) => Err(to_mcp_error(&e)),
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
            Err(e) => Err(to_mcp_error(&e)),
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
