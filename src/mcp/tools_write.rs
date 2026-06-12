//! Mutating MCP tools: mailbox/draft creation, flags, moves, deletes, unsubscribe.

use super::AgentMailServer;
use super::args::*;
use super::{make_cancel_fn, make_progress_fn, to_mcp_error};
use crate::{
    CreateDraftResponse, CreateMailboxResponse, DeleteBySenderResponse, DeleteListIdResponse,
    DeleteMessagesResponse, DeleteMode, DownloadAttachmentsResponse, DraftAttachment,
    MoveMessageResponse, UnsubscribeResponse, UpdateFlagsResponse,
};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer,
    handler::server::wrapper::{Json, Parameters},
    model::Meta,
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
        description = "Create a new mailbox (folder) on the IMAP server. Use delimiter (usually '/') for nested mailboxes.",
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
    ) -> Result<Json<CreateMailboxResponse>, McpError> {
        if args.mailbox_name.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox_name is required", None));
        }
        match self
            .agentmail
            .create_mailbox(&args.account, &args.mailbox_name)
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "delete_messages",
        description = "Delete one or more messages by UID. Moves to Trash if configured, otherwise flags \\Deleted and expunges. Supports up to 500 UIDs per call.",
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
    ) -> Result<Json<DeleteMessagesResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
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
                &mailbox,
                &args.account,
                &args.uids,
                delete_mode(args.permanent),
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
        name = "delete_by_sender",
        description = "Delete all messages from an exact sender. Takes a UID to identify the sender — extracts the full From header (display name + email) and deletes every message with an identical sender. Set allMailboxes=true to search and delete across the entire account. Ideal for bulk cleanup after top_senders. For mailing list cleanup, use unsubscribe_message instead — it attempts one-click unsubscribe and only deletes bulk mail.",
        annotations(title = "Delete by Sender", destructive_hint = true),
        execution(task_support = "optional")
    )]
    async fn delete_by_sender_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<DeleteBySenderArgs>,
    ) -> Result<Json<DeleteBySenderResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());

        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        match self
            .agentmail
            .delete_by_sender(
                &mailbox,
                &args.account,
                args.uid,
                args.all_mailboxes,
                delete_mode(args.permanent),
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
        name = "download_attachments",
        description = "Download all attachments from a message to disk. Files are saved as {uid}_{originalname}. Returns file paths, content types, and sizes.",
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
    ) -> Result<Json<DownloadAttachmentsResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        let output_dir = args
            .output_dir
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

        match self
            .agentmail
            .download_attachments(&mailbox, &args.account, args.uid, &output_dir)
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "create_draft",
        description = "Create and save a draft email. Composes an RFC822 message and appends it to the account's Drafts folder (creating the Drafts mailbox if necessary). Requires at least one recipient (to, cc, or bcc). Subject and body are optional. Supports optional attachments via local file paths.",
        annotations(
            title = "Create Draft",
            read_only_hint = false,
            destructive_hint = false
        )
    )]
    async fn create_draft_tool(
        &self,
        Parameters(args): Parameters<CreateDraftArgs>,
    ) -> Result<Json<CreateDraftResponse>, McpError> {
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
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "move_message",
        description = "Move a single message from one mailbox to another by UID. Uses IMAP MOVE command. Requires source mailbox, destination mailbox, and the message UID.",
        annotations(
            title = "Move Message",
            read_only_hint = false,
            destructive_hint = false
        )
    )]
    async fn move_message_tool(
        &self,
        Parameters(args): Parameters<MoveMessageArgs>,
    ) -> Result<Json<MoveMessageResponse>, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        if args.destination.trim().is_empty() {
            return Err(McpError::invalid_params("destination is required", None));
        }

        match self
            .agentmail
            .move_message(&args.mailbox, &args.account, args.uid, &args.destination)
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "unsubscribe_message",
        description = "Unsubscribe from a mailing list and delete matching messages across ALL mailboxes. Requires the message to have a List-Unsubscribe header. Attempts RFC 8058 one-click unsubscribe POST (best-effort — if it fails, messages are still deleted). When delete_matching is true, searches every mailbox for messages from the exact sender that have a List-Unsubscribe-Post header and deletes them. This ensures only bulk/marketing mail is removed, not legitimate messages from the same sender.",
        annotations(
            title = "Unsubscribe from Mailing List",
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn unsubscribe_message_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<UnsubscribeMessageArgs>,
    ) -> Result<Json<UnsubscribeResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);

        match self
            .agentmail
            .unsubscribe_message(
                &mailbox,
                &args.account,
                args.uid,
                args.delete_matching,
                delete_mode(args.permanent),
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
        name = "delete_list_id",
        description = "Delete all messages with a specific List-Id across all mailboxes. Identifies the list by its List-Id header value (from top_mailing_lists). Deletes ALL messages from that mailing list regardless of sender address. Omit mailbox to search the entire account.",
        annotations(title = "Delete Mailing List by List-Id", destructive_hint = true),
        execution(task_support = "optional")
    )]
    async fn delete_list_id_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<DeleteListIdArgs>,
    ) -> Result<Json<DeleteListIdResponse>, McpError> {
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
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "add_flags",
        description = "Add flags and/or set an Apple Mail color on a message. Flags use union semantics — existing flags are preserved. Use color for Apple Mail colored flags (red, orange, yellow, green, blue, purple, gray). Cannot set \\Deleted (use delete_messages) or \\Recent (read-only).",
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
    ) -> Result<Json<UpdateFlagsResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
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
                &mailbox,
                &args.account,
                args.uid,
                &args.flags,
                args.color.as_deref(),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(to_mcp_error(&e)),
        }
    }

    #[tool(
        name = "remove_flags",
        description = "Remove flags and/or clear Apple Mail color from a message. Only specified flags are removed; all others preserved. Set color=true to remove the colored flag (\\Flagged + all $MailFlagBit keywords). Cannot remove \\Deleted (use delete_messages) or \\Recent (read-only).",
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
    ) -> Result<Json<UpdateFlagsResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        if args.flags.is_empty() && !args.color {
            return Err(McpError::invalid_params(
                "At least one flag or color=true is required",
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
            .remove_flags(&mailbox, &args.account, args.uid, &args.flags, args.color)
            .await
        {
            Ok(data) => Ok(Json(data)),
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
