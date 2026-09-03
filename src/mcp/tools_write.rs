//! Mutating MCP tools: mailbox/draft creation, flags, moves, deletes, unsubscribe.

use super::AgentMailServer;
use super::args::*;
use super::wire::{
    CreateDraftOutput, CreateMailboxOutput, DeleteByDomainOutput, DeleteBySenderOutput,
    DeleteListIdOutput, DeleteMailboxOutput, DeleteMessagesOutput, DownloadAttachmentsOutput,
    DownloadMessageSourceOutput, DownloadThreadOutput, MoveByDomainOutput, MoveBySenderOutput,
    MoveListIdOutput, MoveMessageOutput, MoveSubscriptionOutput, ReconcileMovesOutput,
    RenameMailboxOutput, UnsubscribeMessageOutput, UpdateDraftOutput, UpdateFlagsOutput,
    compact_result, tool_error_result,
};
use super::{make_cancel_fn, make_progress_fn};
use crate::{
    CleanupDeletion, CleanupIdentityMode, CleanupPolicy, CleanupWhen, DeleteMode, DraftAttachment,
    ReplyMode, UnsubscribeOptions,
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
const MAX_THREAD_MESSAGES: usize = 100;
const MAX_RECORD_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RECORD_PDF_BODY_CHARS: usize = 500_000;

async fn create_private_dir(path: &std::path::Path) -> crate::Result<()> {
    #[cfg(unix)]
    let builder = {
        let mut builder = tokio::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = tokio::fs::DirBuilder::new();
    builder.create(path).await.map_err(|error| {
        crate::AgentmailError::Other(format!(
            "failed to create thread-record directory '{}': {error}",
            path.display()
        ))
    })?;
    Ok(())
}

fn checked_record_bytes(total: u64, next: usize) -> crate::Result<u64> {
    let total = total.checked_add(next as u64).ok_or_else(|| {
        crate::AgentmailError::Other("thread-record bundle size overflow".to_string())
    })?;
    if total > MAX_RECORD_BUNDLE_BYTES {
        return Err(crate::AgentmailError::Other(format!(
            "thread-record bundle exceeds the {MAX_RECORD_BUNDLE_BYTES}-byte limit"
        )));
    }
    Ok(total)
}

async fn export_thread_record(
    server: &AgentMailServer,
    args: &ExportThreadRecordArgs,
    output_dir: &std::path::Path,
    on_progress: Option<&crate::ProgressFn>,
    cancel: Option<&crate::CancelFn>,
) -> crate::Result<crate::ThreadRecordExportResponse> {
    crate::imap_client::check_cancel(cancel)?;
    let preview = server
        .agentmail
        .preview_thread_record(
            args.mailbox.trim(),
            &args.account,
            args.uid,
            args.expected_uid_validity,
            None,
            cancel,
        )
        .await?;
    if preview.truncated {
        return Err(crate::AgentmailError::Other(
            "thread preview is truncated; refusing to label an incomplete selection as recorded"
                .to_string(),
        ));
    }
    if preview.selection_digest != args.selection_digest {
        return Err(crate::AgentmailError::Other(format!(
            "thread selection changed after preview (expected {}, now {}); review a fresh preview before exporting",
            args.selection_digest, preview.selection_digest
        )));
    }

    let purpose = args.purpose.trim();
    if purpose.is_empty() || purpose.chars().count() > 4_000 {
        return Err(crate::AgentmailError::Other(
            "purpose must contain 1..=4000 characters".to_string(),
        ));
    }
    let bundle_name = args.bundle_name.clone().unwrap_or_else(|| {
        format!(
            "agentmail-thread-record-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        )
    });
    crate::validate_plain_filename(&bundle_name)?;
    let final_path = output_dir.join(&bundle_name);
    if tokio::fs::try_exists(&final_path).await.map_err(|error| {
        crate::AgentmailError::Other(format!(
            "failed to inspect thread-record destination '{}': {error}",
            final_path.display()
        ))
    })? {
        return Err(crate::AgentmailError::Other(format!(
            "refusing to overwrite existing thread-record destination '{}'",
            final_path.display()
        )));
    }
    let temporary_path =
        output_dir.join(format!(".{bundle_name}.partial-{}", uuid::Uuid::new_v4()));
    create_private_dir(&temporary_path).await?;

    let result = async {
        let sources_path = temporary_path.join("sources");
        create_private_dir(&sources_path).await?;
        let mut files = Vec::with_capacity(preview.messages.len());
        let mut presentations = Vec::with_capacity(preview.messages.len());
        let mut total_bytes = 0_u64;
        let presentation_char_limit =
            MAX_RECORD_PDF_BODY_CHARS / preview.messages.len().max(1);

        for (index, selected) in preview.messages.iter().enumerate() {
            crate::imap_client::check_cancel(cancel)?;
            let filename = format!(
                "message-{:03}-{}-{}.eml",
                index + 1,
                selected.identity.uid_validity,
                selected.identity.uid
            );
            let downloaded = server
                .agentmail
                .download_message_source(
                    &selected.identity.mailbox,
                    &args.account,
                    selected.identity.uid,
                    selected.identity.uid_validity,
                    &sources_path,
                    &filename,
                    cancel,
                )
                .await?;
            total_bytes = checked_record_bytes(total_bytes, downloaded.bytes)?;
            let raw = tokio::fs::read(&downloaded.path).await.map_err(|error| {
                crate::AgentmailError::Other(format!(
                    "saved RFC822 source could not be reopened '{}': {error}",
                    downloaded.path
                ))
            })?;
            let reopened_hash = crate::record::sha256_bytes(&raw);
            if raw.len() != downloaded.bytes || reopened_hash != downloaded.sha256 {
                return Err(crate::AgentmailError::Other(format!(
                    "saved RFC822 source failed its reopen/hash check: {filename}"
                )));
            }
            let uid = selected.identity.uid;
            let presentation = tokio::task::spawn_blocking(move || {
                crate::record::analyze_message(&raw, uid, presentation_char_limit)
            })
            .await
            .map_err(|error| {
                crate::AgentmailError::Other(format!(
                    "RFC822 presentation analysis task failed: {error}"
                ))
            })??;
            presentations.push(presentation);
            files.push(crate::ThreadRecordFile {
                identity: selected.identity.clone(),
                filename: format!("sources/{filename}"),
                bytes: downloaded.bytes,
                sha256: downloaded.sha256,
                message_id: downloaded.message_id,
                date: downloaded.date,
                from_header: downloaded.from_header,
                subject: downloaded.subject,
                dkim: downloaded.dkim,
            });
            if let Some(progress) = on_progress {
                progress((index + 1) as u64, preview.messages.len() as u64);
            }
        }

        let mut limitations = vec![
            "Thread membership uses exact RFC Message-ID, In-Reply-To, and References values; it does not infer relationships from similar subjects."
                .to_string(),
            "The PDF is a readable presentation. The complete, integrity-bearing content is preserved in the hashed RFC822 (.eml) sources."
                .to_string(),
            "DKIM was checked against DNS at export time. SPF cannot be independently recomputed from archived message bytes because the SMTP connection and envelope inputs are absent."
                .to_string(),
            "Recorded and submittable describe this bundle's structure and verification, not legal admissibility, authenticity findings, or acceptance by any recipient."
                .to_string(),
        ];
        for warning in &preview.warnings {
            limitations.push(format!("Discovery warning: {warning}"));
        }
        if presentations.iter().any(|message| message.body_truncated) {
            limitations.push(
                "At least one body was shortened in the PDF presentation; its complete bytes remain in the corresponding hashed .eml source."
                    .to_string(),
            );
        }
        let generated_at = chrono::Utc::now().to_rfc3339();
        let pdf_preview = preview.clone();
        let pdf_files = files.clone();
        let pdf_presentations = presentations.clone();
        let pdf_limitations = limitations.clone();
        let pdf_purpose = purpose.to_string();
        let pdf_generated_at = generated_at.clone();
        let pdf = tokio::task::spawn_blocking(move || {
            crate::record::render_thread_record_pdf(
                &pdf_purpose,
                &pdf_generated_at,
                &pdf_preview,
                &pdf_files,
                &pdf_presentations,
                &pdf_limitations,
            )
        })
        .await
        .map_err(|error| {
            crate::AgentmailError::Other(format!("PDF render task failed: {error}"))
        })??;
        total_bytes = checked_record_bytes(total_bytes, pdf.len())?;
        let pdf_filename = "thread-record.pdf";
        let pdf_temporary_path = temporary_path.join(pdf_filename);
        crate::write_new_private_file(&pdf_temporary_path, &pdf).await?;
        let reopened_pdf = tokio::fs::read(&pdf_temporary_path).await.map_err(|error| {
            crate::AgentmailError::Other(format!(
                "generated PDF could not be reopened '{}': {error}",
                pdf_temporary_path.display()
            ))
        })?;
        let pdf_sha256 = crate::record::sha256_bytes(&reopened_pdf);
        let expected_pdf_sha256 = crate::record::sha256_bytes(&pdf);
        if reopened_pdf.len() != pdf.len() || pdf_sha256 != expected_pdf_sha256 {
            return Err(crate::AgentmailError::Other(
                "generated PDF failed its reopen/hash check".to_string(),
            ));
        }
        let pdf_pages = tokio::task::spawn_blocking(move || {
            crate::record::verify_pdf(&reopened_pdf)
        })
        .await
        .map_err(|error| {
            crate::AgentmailError::Other(format!("PDF verification task failed: {error}"))
        })??;

        let submission_explanation = "Structurally ready to submit as a record because it contains a purpose statement, deterministic selection rationale, exact RFC822 sources, live storage identities, SHA-256 hashes, DKIM results, a readable chronology, an attachment inventory, and explicit limitations. The receiving authority still determines acceptance and legal admissibility.".to_string();
        let manifest = serde_json::json!({
            "schemaVersion": "agentmail.thread-record.v1",
            "recorded": true,
            "submittable": true,
            "submissionExplanation": &submission_explanation,
            "generatedAt": &generated_at,
            "purpose": purpose,
            "account": args.account,
            "selection": &preview,
            "artifacts": {
                "presentationPdf": {
                    "filename": pdf_filename,
                    "bytes": pdf.len(),
                    "sha256": pdf_sha256,
                    "pages": pdf_pages,
                },
                "sources": &files,
            },
            "limitations": &limitations,
        });
        let mut manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
            crate::AgentmailError::Other(format!(
                "failed to serialize thread-record manifest: {error}"
            ))
        })?;
        manifest_bytes.push(b'\n');
        total_bytes = checked_record_bytes(total_bytes, manifest_bytes.len())?;
        let manifest_filename = "manifest.json";
        let manifest_temporary_path = temporary_path.join(manifest_filename);
        crate::write_new_private_file(&manifest_temporary_path, &manifest_bytes).await?;
        let reopened_manifest = tokio::fs::read(&manifest_temporary_path)
            .await
            .map_err(|error| {
                crate::AgentmailError::Other(format!(
                    "thread-record manifest could not be reopened: {error}"
                ))
            })?;
        if crate::record::sha256_bytes(&reopened_manifest)
            != crate::record::sha256_bytes(&manifest_bytes)
        {
            return Err(crate::AgentmailError::Other(
                "thread-record manifest failed its reopen/hash check".to_string(),
            ));
        }
        let parsed_manifest: serde_json::Value = serde_json::from_slice(&reopened_manifest)
            .map_err(|error| {
                crate::AgentmailError::Other(format!(
                    "thread-record manifest failed to parse after writing: {error}"
                ))
            })?;
        let manifest_sources = parsed_manifest["artifacts"]["sources"]
            .as_array()
            .map_or(0, Vec::len);
        if parsed_manifest["recorded"] != serde_json::Value::Bool(true)
            || parsed_manifest["submittable"] != serde_json::Value::Bool(true)
            || parsed_manifest["selection"]["selectionDigest"]
                != serde_json::Value::String(args.selection_digest.clone())
            || manifest_sources != preview.messages.len()
        {
            return Err(crate::AgentmailError::Other(
                "thread-record manifest failed its structural verification".to_string(),
            ));
        }

        if tokio::fs::try_exists(&final_path).await.map_err(|error| {
            crate::AgentmailError::Other(format!(
                "failed to recheck thread-record destination '{}': {error}",
                final_path.display()
            ))
        })? {
            return Err(crate::AgentmailError::Other(format!(
                "thread-record destination appeared during export; refusing to overwrite '{}'",
                final_path.display()
            )));
        }
        tokio::fs::rename(&temporary_path, &final_path)
            .await
            .map_err(|error| {
                crate::AgentmailError::Other(format!(
                    "failed to publish verified thread-record bundle '{}': {error}",
                    final_path.display()
                ))
            })?;
        let final_path = tokio::fs::canonicalize(&final_path).await.map_err(|error| {
            crate::AgentmailError::Other(format!(
                "published thread-record bundle could not be resolved: {error}"
            ))
        })?;
        Ok(crate::ThreadRecordExportResponse {
            recorded: true,
            submittable: true,
            submission_explanation,
            account: args.account.clone(),
            purpose: purpose.to_string(),
            selection_digest: args.selection_digest.clone(),
            message_count: preview.messages.len(),
            bundle_path: final_path.display().to_string(),
            pdf_path: final_path.join(pdf_filename).display().to_string(),
            manifest_path: final_path.join(manifest_filename).display().to_string(),
            total_bytes,
            limitations,
        })
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_dir_all(&temporary_path).await;
    }
    result
}

/// Map the flat `permanent` tool argument to a `DeleteMode`.
fn delete_mode(permanent: bool) -> DeleteMode {
    if permanent {
        DeleteMode::Permanent
    } else {
        DeleteMode::TrashFirst
    }
}

fn reply_mode(mode: ReplyModeArg) -> ReplyMode {
    match mode {
        ReplyModeArg::Reply => ReplyMode::Reply,
        ReplyModeArg::ReplyAll => ReplyMode::ReplyAll,
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

fn archive_filename(uid: u32, requested: Option<&str>) -> Result<String, McpError> {
    let filename = requested.map_or_else(|| format!("{uid}.eml"), str::to_string);
    crate::validate_plain_filename(&filename)
        .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
    Ok(filename)
}

async fn archive_target_exists(
    output_dir: &std::path::Path,
    filenames: &[String],
) -> Result<Option<std::path::PathBuf>, McpError> {
    for filename in filenames {
        let path = output_dir.join(filename);
        if tokio::fs::try_exists(&path).await.map_err(|error| {
            McpError::internal_error(
                format!(
                    "failed to inspect archive target '{}': {error}",
                    path.display()
                ),
                None,
            )
        })? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Map the tool's opt-OUT flag to the composer's body format.
///
/// Rendering is the default because an author writing `**bold**` means
/// emphasis, and a reader seeing literal asterisks is the failure. The flag
/// exists for the cases where plain text IS the wire format.
fn body_format(plain_text_only: bool) -> crate::draft::BodyFormat {
    if plain_text_only {
        crate::draft::BodyFormat::PlainOnly
    } else {
        crate::draft::BodyFormat::MarkdownAndHtml
    }
}

/// Read a draft's attachments off disk, through the session's sandbox.
///
/// The sandbox is resolved ONLY when there is a file to read. A draft with no
/// attachments touches no path, so requiring a workspace for one rejected plain
/// text drafts in every session that had none — `create_draft`,
/// `create_reply_draft` and `update_draft` all resolved the policy
/// unconditionally, and an agent asked to save a reply got
/// `-32602 … requires an active session workspace` and fell back to opening a
/// `mailto:` link (2026-09-03). Composing a message is not a file operation;
/// only attaching to one is.
async fn load_draft_attachments(
    server: &AgentMailServer,
    meta: &Meta,
    attachments: &[DraftAttachmentArg],
) -> Result<Vec<DraftAttachment>, McpError> {
    if attachments.is_empty() {
        return Ok(Vec::new());
    }
    if attachments.len() > MAX_DRAFT_ATTACHMENTS {
        return Err(McpError::invalid_params(
            format!("attachments supports at most {MAX_DRAFT_ATTACHMENTS} files"),
            None,
        ));
    }
    let file_access = server.file_access_for_request(meta)?;

    let mut preflight = Vec::with_capacity(attachments.len());
    let mut preflight_total = 0_u64;
    for (index, attachment) in attachments.iter().enumerate() {
        let safe_path = file_access
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
        preflight_total = preflight_total
            .checked_add(size)
            .ok_or_else(|| McpError::invalid_params("attachment aggregate size overflow", None))?;
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

    let mut loaded = Vec::with_capacity(attachments.len());
    let mut loaded_total = 0_u64;
    for (index, (attachment, (preflight_size, file))) in
        attachments.iter().zip(preflight).enumerate()
    {
        let mut data = Vec::with_capacity(preflight_size as usize);
        file.take(MAX_DRAFT_ATTACHMENT_BYTES + 1)
            .read_to_end(&mut data)
            .await
            .map_err(|error| {
                McpError::invalid_params(
                    format!(
                        "Failed to read attachment #{} at '{}': {error}",
                        index + 1,
                        attachment.path
                    ),
                    None,
                )
            })?;
        if data.len() as u64 > MAX_DRAFT_ATTACHMENT_BYTES {
            return Err(McpError::invalid_params(
                format!(
                    "attachment #{} grew beyond the {MAX_DRAFT_ATTACHMENT_BYTES}-byte limit while being read",
                    index + 1
                ),
                None,
            ));
        }
        loaded_total = loaded_total
            .checked_add(data.len() as u64)
            .ok_or_else(|| McpError::invalid_params("attachment aggregate size overflow", None))?;
        if loaded_total > MAX_DRAFT_ATTACHMENT_TOTAL_BYTES {
            return Err(McpError::invalid_params(
                format!(
                    "attachments grew beyond the {MAX_DRAFT_ATTACHMENT_TOTAL_BYTES}-byte aggregate limit while being read"
                ),
                None,
            ));
        }
        let filename = attachment
            .filename
            .clone()
            .or_else(|| {
                std::path::Path::new(&attachment.path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("attachment-{}", index + 1));
        let content_type = attachment
            .content_type
            .clone()
            .unwrap_or_else(|| guess_content_type(&filename));
        loaded.push(DraftAttachment {
            filename,
            content_type,
            data,
        });
    }
    Ok(loaded)
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
        name = "rename_mailbox",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RenameMailboxOutput>().expect("valid rename_mailbox output schema"),
        description = "Preview or confirm a guarded mailbox rename. The first call returns live message count, special-use roles, descendants, and exact confirmations. The confirmed call requires expectedMessageCount from that preview, refuses INBOX and pending MOVE journals, and re-lists after an ambiguous transport outcome.",
        annotations(
            title = "Rename Mailbox",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn rename_mailbox_tool(
        &self,
        Parameters(args): Parameters<RenameMailboxArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() || args.new_mailbox.trim().is_empty() {
            return Err(McpError::invalid_params(
                "mailbox and newMailbox are required",
                None,
            ));
        }
        match self
            .agentmail
            .rename_mailbox(
                &args.account,
                args.mailbox.trim(),
                args.new_mailbox.trim(),
                args.confirm_rename,
                args.expected_message_count,
                args.confirm_special_use,
                args.confirm_descendants,
            )
            .await
        {
            Ok(data) => compact_result(RenameMailboxOutput::from(data)),
            Err(error) => Ok(tool_error_result(&error)),
        }
    }

    #[tool(
        name = "delete_mailbox",
        output_schema = rmcp::handler::server::tool::schema_for_output::<DeleteMailboxOutput>().expect("valid delete_mailbox output schema"),
        description = "Preview or confirm guarded mailbox deletion. The first call returns live message count, special-use roles, descendants, and required confirmations. A confirmed delete requires the exact preview count plus separate acknowledgements for non-empty, special-use, or descendant-bearing mailboxes. INBOX and mailboxes referenced by pending MOVE journals are always blocked; an already-missing mailbox is an idempotent success.",
        annotations(
            title = "Delete Mailbox",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn delete_mailbox_tool(
        &self,
        Parameters(args): Parameters<DeleteMailboxArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        match self
            .agentmail
            .delete_mailbox(
                &args.account,
                args.mailbox.trim(),
                args.confirm_delete,
                args.expected_message_count,
                args.confirm_non_empty,
                args.confirm_special_use,
                args.confirm_descendants,
            )
            .await
        {
            Ok(data) => compact_result(DeleteMailboxOutput::from(data)),
            Err(error) => Ok(tool_error_result(&error)),
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
        description = "Download all attachments from one message to disk. Pass mailbox, uid, and expectedUidValidity from the same find_attachments hit; the download fails before filesystem writes if the mailbox UID epoch changed. Each file is saved as {uid}_{index}_{name} with the name sanitized — the same canonical filename /info reports for that part. Omit outputDir to write into the session workspace. Returns paths, content types, and sizes.",
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
        meta: Meta,
        Parameters(args): Parameters<DownloadAttachmentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        // Confine the LLM-supplied output directory to the file sandbox: a
        // prompt-injection payload must not be able to write attacker bytes
        // into a sensitive directory (e.g. ~/.ssh). Absolute/`..` escapes are
        // rejected; the default lands in the sandbox root.
        let file_access = self.file_access_for_request(&meta)?;
        let output_dir = match file_access.confine_dir(args.output_dir.as_deref()) {
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
        name = "download_message_source",
        output_schema = rmcp::handler::server::tool::schema_for_output::<DownloadMessageSourceOutput>().expect("valid download_message_source output schema"),
        description = "Save one exact RFC822 message source directly from IMAP to disk without passing the bytes through model context. Requires mailbox, uid, and expectedUidValidity from one discovery result. Uses BODY.PEEK[] so the message is not marked read, refuses files above 64 MiB, confines outputDir to the active session workspace (standalone server: AGENTMAIL_FILE_ROOT), and never overwrites. Returns the absolute path, byte count, SHA-256, Message-ID/date/from/subject metadata, and a contemporaneous local DKIM verification against DNS. SPF is omitted because it cannot be independently recomputed from an archived RFC822 message without SMTP connection and envelope data.",
        annotations(
            title = "Download Message Source",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        ),
        execution(task_support = "optional")
    )]
    async fn download_message_source_tool(
        &self,
        meta: Meta,
        ct: CancellationToken,
        Parameters(args): Parameters<DownloadMessageSourceArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        let filename = archive_filename(args.uid, args.filename.as_deref())?;
        let output_dir = self
            .file_access_for_request(&meta)?
            .confine_dir(args.output_dir.as_deref())
            .map_err(|reason| McpError::invalid_params(reason, None))?;
        let cancel = make_cancel_fn(ct);

        match self
            .agentmail
            .download_message_source(
                &args.mailbox,
                &args.account,
                args.uid,
                args.expected_uid_validity,
                &output_dir,
                &filename,
                Some(&cancel),
            )
            .await
        {
            Ok(data) => compact_result(DownloadMessageSourceOutput::from(data)),
            Err(error) => Ok(tool_error_result(&error)),
        }
    }

    #[tool(
        name = "download_thread",
        output_schema = rmcp::handler::server::tool::schema_for_output::<DownloadThreadOutput>().expect("valid download_thread output schema"),
        description = "Save a caller-supplied set of one to 100 exact RFC822 sources from the same mailbox UIDVALIDITY epoch directly to disk as {uid}.eml, then write a JSON evidence manifest. This tool does not discover thread membership; pass the UIDs selected by prior discovery. Bytes never pass through model context. Every source uses BODY.PEEK[], is capped at 64 MiB, receives SHA-256 plus parsed envelope metadata and local DNS-backed DKIM verification, and is created without overwrite inside the active session workspace (standalone server: AGENTMAIL_FILE_ROOT). SPF is omitted because an archived message lacks the SMTP inputs required for independent verification. Returns the manifest path and its complete message entries.",
        annotations(
            title = "Download Message Thread",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        ),
        execution(task_support = "optional")
    )]
    async fn download_thread_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<DownloadThreadArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        if args.uids.is_empty() || args.uids.len() > MAX_THREAD_MESSAGES {
            return Err(McpError::invalid_params(
                format!("uids must contain 1..={MAX_THREAD_MESSAGES} values"),
                None,
            ));
        }
        if args.uids.contains(&0) {
            return Err(McpError::invalid_params("uids must be non-zero", None));
        }
        let unique = args.uids.iter().copied().collect::<hashbrown::HashSet<_>>();
        if unique.len() != args.uids.len() {
            return Err(McpError::invalid_params(
                "uids must not contain duplicates",
                None,
            ));
        }
        let manifest_filename = archive_filename(
            0,
            Some(args.manifest_filename.as_deref().unwrap_or("manifest.json")),
        )?;
        let output_dir = self
            .file_access_for_request(&meta)?
            .confine_dir(args.output_dir.as_deref())
            .map_err(|reason| McpError::invalid_params(reason, None))?;
        let source_filenames = args
            .uids
            .iter()
            .map(|uid| format!("{uid}.eml"))
            .collect::<Vec<_>>();
        let mut all_filenames = source_filenames.clone();
        all_filenames.push(manifest_filename.clone());
        if let Some(path) = archive_target_exists(&output_dir, &all_filenames).await? {
            return Ok(tool_error_result(&crate::AgentmailError::Other(format!(
                "refusing to overwrite existing thread archive '{}'",
                path.display()
            ))));
        }

        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let mut messages = Vec::with_capacity(args.uids.len());
        for (index, (uid, filename)) in args.uids.iter().zip(&source_filenames).enumerate() {
            let downloaded = self
                .agentmail
                .download_message_source(
                    &args.mailbox,
                    &args.account,
                    *uid,
                    args.expected_uid_validity,
                    &output_dir,
                    filename,
                    Some(&cancel),
                )
                .await;
            let downloaded = match downloaded {
                Ok(downloaded) => downloaded,
                Err(error) => {
                    progress.finish().await;
                    let error = crate::AgentmailError::Other(format!(
                        "thread archive stopped after {} of {} messages: {error}",
                        messages.len(),
                        args.uids.len()
                    ));
                    return Ok(tool_error_result(&error));
                }
            };
            messages.push(DownloadMessageSourceOutput::from(downloaded));
            if let Some(callback) = progress.callback() {
                callback((index + 1) as u64, args.uids.len() as u64);
            }
        }

        let manifest_path = output_dir.join(&manifest_filename);
        let output = DownloadThreadOutput {
            account: args.account,
            mailbox: args.mailbox,
            uid_validity: args.expected_uid_validity,
            created_at: chrono::Utc::now().to_rfc3339(),
            manifest_path: manifest_path.display().to_string(),
            messages,
        };
        let mut manifest = serde_json::to_vec_pretty(&output).map_err(|error| {
            McpError::internal_error(
                format!("failed to serialize thread manifest: {error}"),
                None,
            )
        })?;
        manifest.push(b'\n');
        if let Err(error) = crate::write_new_private_file(&manifest_path, &manifest).await {
            progress.finish().await;
            let error = crate::AgentmailError::Other(format!(
                "all {} message sources were saved, but the manifest write failed: {error}",
                output.messages.len()
            ));
            return Ok(tool_error_result(&error));
        }
        progress.finish().await;
        compact_result(output)
    }

    #[tool(
        name = "export_thread_record",
        output_schema = rmcp::handler::server::tool::schema_for_output::<crate::ThreadRecordExportResponse>().expect("valid export_thread_record output schema"),
        description = "After preview_thread_record, export that exact confirmed selection into the active session workspace as a private bundle containing a styled, page-numbered PDF; one immutable RFC822 .eml source per selected storage identity; and a JSON integrity manifest. Re-discovers the graph and refuses selectionDigest drift, never overwrites, caps each source at 64 MiB and the bundle at 512 MiB, verifies DKIM against DNS, then reopens/parses/hash-checks every artifact before returning recorded=true and submittable=true. Those flags describe packet completeness, not legal admissibility. This tool never sends or mutates mail.",
        annotations(
            title = "Export Verified Thread Record",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        ),
        execution(task_support = "optional")
    )]
    async fn export_thread_record_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        ct: CancellationToken,
        Parameters(args): Parameters<ExportThreadRecordArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        if args.selection_digest.len() != 64
            || !args
                .selection_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(McpError::invalid_params(
                "selectionDigest must be a 64-character lowercase SHA-256 value from preview_thread_record",
                None,
            ));
        }
        let output_dir = self
            .file_access_for_request(&meta)?
            .confine_dir(args.output_dir.as_deref())
            .map_err(|reason| McpError::invalid_params(reason, None))?;
        let progress = make_progress_fn(&meta, &client);
        let cancel = make_cancel_fn(ct);
        let result =
            export_thread_record(self, &args, &output_dir, progress.callback(), Some(&cancel))
                .await;
        progress.finish().await;
        match result {
            Ok(data) => compact_result(data),
            Err(error) => Ok(tool_error_result(&error)),
        }
    }

    #[tool(
        name = "create_draft",
        output_schema = rmcp::handler::server::tool::schema_for_output::<CreateDraftOutput>().expect("valid create_draft output schema"),
        description = "Create and save a draft — a fresh message, or a reply to a live one. Resolves the account's selectable \\Drafts special-use mailbox, falls back to Drafts and creates it when needed, then APPENDs the message with the \\Draft flag. Compose fresh by giving recipients: at least one of to, cc or bcc. Reply instead by giving replyToMessage {mailbox, uid, expectedUidValidity, mode} from a discovery result — that derives the recipients (Reply-To before From, excluding this account's own addresses and aliases), a Re: subject you may override, and the RFC In-Reply-To/References headers; to, cc, inReplyTo and references must then be omitted, and bcc is still never inferred. Subject and body are optional; attachments may reference local file paths. The body is read as Markdown and sent as multipart/alternative — the text exactly as written, plus an HTML rendering — so **bold**, lists, links and tables arrive formatted rather than as literal syntax. Raw HTML in the body is escaped, never rendered. Set plainTextOnly=true for a single unrendered text/plain part. Returns the new draft's uid and uidValidity when the server allows recovering them, and links the draft as a resource_link. A draft's UID is NOT durable — re-saving it (here, or in any other mail client) appends a new message and expunges the old one, and some servers discard an APPENDed draft outright. Re-read the link from a fresh get_messages rather than reusing one from an earlier turn. This tool never sends mail.",
        annotations(
            title = "Create Draft",
            read_only_hint = false,
            destructive_hint = false
        )
    )]
    async fn create_draft_tool(
        &self,
        meta: Meta,
        Parameters(args): Parameters<CreateDraftArgs>,
    ) -> Result<CallToolResult, McpError> {
        let loaded = load_draft_attachments(self, &meta, &args.attachments).await?;

        // Replying: the source message supplies the recipients, the subject and
        // the threading headers. Passing them TOO is refused rather than
        // silently ignored or merged — two sources for one field is how a reply
        // quietly goes to the wrong people.
        if let Some(source) = args.reply_to_message.clone() {
            for (field, populated) in [
                ("to", !args.to.is_empty()),
                ("cc", !args.cc.is_empty()),
                ("inReplyTo", args.in_reply_to.is_some()),
                ("references", !args.references.is_empty()),
            ] {
                if populated {
                    return Err(McpError::invalid_params(
                        format!("`{field}` is derived from replyToMessage — omit it"),
                        None,
                    ));
                }
            }
            if source.mailbox.trim().is_empty() {
                return Err(McpError::invalid_params(
                    "replyToMessage.mailbox is required",
                    None,
                ));
            }
            return match self
                .agentmail
                .create_reply_draft(
                    &args.account,
                    source.mailbox.trim(),
                    source.uid,
                    source.expected_uid_validity,
                    reply_mode(source.mode),
                    Some(args.subject.trim()).filter(|subject| !subject.is_empty()),
                    &args.body,
                    &args.bcc,
                    &args.reply_to,
                    &loaded,
                    body_format(args.plain_text_only),
                )
                .await
            {
                Ok(data) => compact_result(CreateDraftOutput::from(data)),
                Err(error) => Ok(tool_error_result(&error)),
            };
        }

        if args.to.is_empty() && args.cc.is_empty() && args.bcc.is_empty() {
            return Err(McpError::invalid_params(
                "At least one recipient (to, cc, or bcc) is required, or replyToMessage to derive them",
                None,
            ));
        }

        match self
            .agentmail
            .create_draft_with_headers(
                &args.account,
                args.subject.trim(),
                &args.body,
                &args.to,
                &args.cc,
                &args.bcc,
                &args.reply_to,
                args.in_reply_to.as_deref(),
                &args.references,
                &loaded,
                None,
                body_format(args.plain_text_only),
            )
            .await
        {
            Ok(data) => compact_result(CreateDraftOutput::from(data)),
            Err(e) => Ok(tool_error_result(&e)),
        }
    }

    #[tool(
        name = "update_draft",
        output_schema = rmcp::handler::server::tool::schema_for_output::<UpdateDraftOutput>().expect("valid update_draft output schema"),
        description = "Replace one live IMAP draft with a complete new draft specification. Requires mailbox, uid, and expectedUidValidity; verifies the target still has the \\Draft flag. Uses RFC 8508 REPLACE where the server has it and otherwise emulates it, so this succeeds on servers without REPLACE (Gmail and iCloud among them) — do not hand-roll create_draft + delete_messages. Returns the NEW uid and uidValidity: a replaced draft always has a new identity, so discard the old one. A `warning` field appears only if the superseded draft could not be removed. Attachments are a complete replacement list, and this tool never sends mail. The body is read as Markdown and sent as multipart/alternative (the text as written plus an HTML rendering); raw HTML in it is escaped, never rendered. Set plainTextOnly=true for a single unrendered text/plain part.",
        annotations(
            title = "Update Draft Atomically",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn update_draft_tool(
        &self,
        meta: Meta,
        Parameters(args): Parameters<UpdateDraftArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        if args.to.is_empty() && args.cc.is_empty() && args.bcc.is_empty() {
            return Err(McpError::invalid_params(
                "At least one recipient (to, cc, or bcc) is required",
                None,
            ));
        }
        let loaded = load_draft_attachments(self, &meta, &args.attachments).await?;
        match self
            .agentmail
            .update_draft(
                &args.account,
                args.mailbox.trim(),
                args.uid,
                args.expected_uid_validity,
                args.subject.trim(),
                &args.body,
                &args.to,
                &args.cc,
                &args.bcc,
                &args.reply_to,
                args.in_reply_to.as_deref(),
                &args.references,
                &loaded,
                body_format(args.plain_text_only),
            )
            .await
        {
            Ok(data) => compact_result(UpdateDraftOutput::from(data)),
            Err(error) => Ok(tool_error_result(&error)),
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
        name = "update_flags",
        output_schema = rmcp::handler::server::tool::schema_for_output::<UpdateFlagsOutput>().expect("valid update_flags output schema"),
        description = "Add flags, remove flags, and set or clear the Apple Mail color on one message — in a single call. Requires mailbox, uid, and expectedUidValidity; fails before any change if the mailbox UID epoch moved. Adding and removing in ONE call is the point: doing them as two calls opens two epoch windows and can leave the first applied and the second refused. Order is remove, then color, then add, so a flag named in both lists ends up SET. Unnamed flags are never touched. Cannot set or remove \\Deleted (use delete_messages) or \\Recent (read-only). Returns the message's complete resulting flag set.",
        annotations(
            title = "Update Flags / Color",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn update_flags_tool(
        &self,
        Parameters(args): Parameters<UpdateFlagsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::invalid_params("mailbox is required", None));
        }
        if args.add.is_empty() && args.remove.is_empty() && args.color.is_none() {
            return Err(McpError::invalid_params(
                "At least one of add, remove, or color is required",
                None,
            ));
        }
        // `\\Deleted` and `\\Recent` are refused in BOTH directions: deletion has
        // its own tool with its own disposal policy, and `\\Recent` is the
        // server's to set.
        for (field, flags) in [("add", &args.add), ("remove", &args.remove)] {
            for flag in flags.iter() {
                match flag.to_lowercase().as_str() {
                    "\\deleted" => {
                        return Err(McpError::invalid_params(
                            format!(
                                "Cannot {field} \\Deleted via update_flags — use delete_messages instead"
                            ),
                            None,
                        ));
                    }
                    "\\recent" => {
                        return Err(McpError::invalid_params(
                            "Cannot change \\Recent — it is a read-only server flag",
                            None,
                        ));
                    }
                    _ => {}
                }
            }
        }
        let color = match args.color.as_deref().map(str::trim) {
            None => crate::FlagColorChange::Leave,
            Some(name) if name.eq_ignore_ascii_case("none") => crate::FlagColorChange::Clear,
            Some(name) => crate::FlagColorChange::Set(name.to_string()),
        };
        match self
            .agentmail
            .update_flags(
                &args.mailbox,
                &args.account,
                args.uid,
                args.expected_uid_validity,
                &args.add,
                &args.remove,
                color,
            )
            .await
        {
            Ok(data) => compact_result(UpdateFlagsOutput::new(data, args.expected_uid_validity)),
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

    /// Composing a message is not a filesystem operation. The embedded server
    /// has no ambient sandbox — it takes one from the session workspace in
    /// `_meta` — so resolving that policy for a draft with NOTHING to attach
    /// made `create_draft` / `create_reply_draft` / `update_draft` fail with
    /// `-32602 … requires an active session workspace` in any session that had
    /// none. Red before green: resolve eagerly here and this returns an error
    /// for an empty attachment list.
    #[tokio::test]
    async fn a_draft_with_no_attachments_needs_no_workspace() {
        let server = super::AgentMailServer::new_embedded(
            crate::Agentmail::builder(crate::Config::empty()).build(),
        );
        // An EMPTY meta is exactly a session that never got a workspace root.
        let meta = rmcp::model::Meta::new();

        let loaded = super::load_draft_attachments(&server, &meta, &[])
            .await
            .expect("a draft with no attachments must not require a workspace");
        assert!(loaded.is_empty(), "nothing to attach, nothing loaded");

        // …but the moment a file IS named, the sandbox is mandatory again.
        let attached = [super::DraftAttachmentArg {
            path: "/etc/passwd".to_string(),
            filename: None,
            content_type: None,
        }];
        let error = super::load_draft_attachments(&server, &meta, &attached)
            .await
            .expect_err("reading a file without a workspace must still refuse");
        assert!(
            error.message.contains("workspace"),
            "the refusal must still name the missing workspace: {error:?}"
        );
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn thread_record_directories_are_private_when_created() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = std::env::temp_dir().join(format!(
            "agentmail-private-record-dir-{}",
            uuid::Uuid::new_v4()
        ));
        super::create_private_dir(&path)
            .await
            .expect("create private dir");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir(&path).expect("cleanup private dir");
    }
}
