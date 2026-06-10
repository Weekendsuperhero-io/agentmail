//! MCP server: tools, prompts, and tasks over stdio or in-process transports.

mod args;
mod prompts;
mod resources;
mod tasks;
mod tools_read;
mod tools_write;

use self::tasks::{DESTRUCTIVE_TOOLS, ManagedTask, TaskManager, extract_account};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
    handler::server::router::tool::ToolRouter,
    model::{
        CallToolRequestParams, CallToolResult, CancelTaskParams, CancelTaskResult,
        CompleteRequestParams, CompleteResult, CreateTaskResult, GetPromptRequestParams,
        GetPromptResult, GetTaskInfoParams, GetTaskPayloadResult, GetTaskResult,
        GetTaskResultParams, Implementation, ListPromptsResult, ListResourceTemplatesResult,
        ListTasksResult, Meta, PaginatedRequestParams, ProgressNotificationParam,
        ReadResourceRequestParams, ReadResourceResult, ServerCapabilities, ServerInfo, Task,
        TaskStatus,
    },
    prompt_handler,
    service::RequestContext,
    tool_handler,
};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn utc_now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn mask_prefix_for_log(value: &str) -> String {
    let char_count = value.chars().count();
    if char_count <= 1 {
        return "***".to_string();
    }

    let visible_len = 3_usize.min(char_count - 1);
    let visible: String = value.chars().take(visible_len).collect();
    format!("{visible}***")
}

fn account_log_hint(account: &str) -> String {
    let account = account.trim();
    if account.is_empty() {
        return "<empty>".to_string();
    }

    if let Some((local, domain)) = account.rsplit_once('@')
        && !local.is_empty()
        && !domain.is_empty()
    {
        return format!("{}@{}", mask_prefix_for_log(local), domain);
    }

    mask_prefix_for_log(account)
}

/// Build an optional progress callback from MCP meta + peer.
/// Returns `None` if the client didn't provide a progress token.
fn make_progress_fn(meta: &Meta, peer: &Peer<RoleServer>) -> Option<crate::ProgressFn> {
    let token = meta.get_progress_token()?.clone();
    let peer = peer.clone();
    Some(Arc::new(move |completed: u64, total: u64| {
        let peer = peer.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let _ = peer
                .notify_progress(
                    ProgressNotificationParam::new(token, completed as f64)
                        .with_total(total as f64),
                )
                .await;
        });
    }))
}

/// Build a `CancelFn` from the request's cancellation token. Fires when the
/// client sends `notifications/cancelled` for this request, or on transport
/// shutdown (the token is a child of the serve-loop token). Long scans check
/// it at mailbox and fetch-chunk boundaries.
fn make_cancel_fn(ct: tokio_util::sync::CancellationToken) -> crate::CancelFn {
    Arc::new(move || ct.is_cancelled())
}

/// Map a library error to an MCP error code. Model-actionable failures
/// (bad account/mailbox/message selectors, config and credential problems)
/// map to `invalid_params` so the caller can correct its input; transport
/// and server failures map to `internal_error`.
fn to_mcp_error(e: &crate::AgentmailError) -> McpError {
    use crate::AgentmailError as E;
    match e {
        E::AccountNotFound(_)
        | E::MailboxNotFound(_)
        | E::MessageNotFound(_)
        | E::Config(_)
        | E::Credential(_) => McpError::invalid_params(e.to_string(), None),
        E::Imap(_)
        | E::Tls(_)
        | E::Io(_)
        | E::Parse(_)
        | E::NotConnected
        | E::PoolExhausted
        | E::Other(_) => McpError::internal_error(e.to_string(), None),
    }
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AgentMailServer {
    agentmail: Arc<crate::Agentmail>,
    task_manager: Arc<parking_lot::Mutex<TaskManager>>,
}

impl AgentMailServer {
    pub fn new(agentmail: crate::Agentmail) -> Self {
        Self {
            agentmail: Arc::new(agentmail),
            task_manager: Arc::new(parking_lot::Mutex::new(TaskManager::new())),
        }
    }

    /// Combined tool router — referenced by `#[tool_handler]`'s default
    /// `Self::tool_router()` expression and by the regression tests.
    fn tool_router() -> ToolRouter<Self> {
        Self::read_tools_router() + Self::write_tools_router()
    }
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for AgentMailServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .enable_completions()
                .enable_tasks()
                .build(),
        )
        // Without this the server announces itself as "rmcp" — rmcp's
        // Implementation::from_build_env() bakes in its own crate name.
        .with_server_info(Implementation::new("agentmail", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "AgentMail is a full-featured IMAP email client. \
             Start with list_accounts to discover configured accounts. \
             Use list_mailboxes to see folder structure and message counts. \
             Read messages with get_messages (paginated, newest-first) or search_messages (with filters). \
             Use search_messages to find specific messages by sender, subject, or content. \
             Manage email: delete_messages, delete_by_sender, delete_list_id, move_message, create_draft (supports attachments), create_mailbox, unsubscribe_message. \
             rank_senders, rank_unsubscribe, rank_list_id, list_flags, and find_attachments accept an optional mailbox — omit it to scan the entire account. \
             All-mailbox scans automatically skip Trash, Junk, Spam, and Drafts. \
             Two cleanup workflows: (1) rank_senders → delete_by_sender for unwanted personal senders, (2) rank_unsubscribe → unsubscribe_message for mailing lists. \
             Never use delete_by_sender for mailing list cleanup — it deletes ALL messages from a sender including non-bulk ones. \
             rank_list_id groups by List-Id header (RFC 2919) — all messages from the same mailing list regardless of sender. Use delete_list_id to remove an entire list. \
             rank_senders groups by (email, display name) — same email with different display names are separate entries. \
             rank_unsubscribe returns sample UIDs + mailboxes that can be passed directly to unsubscribe_message. \
             unsubscribe_message deletes by matching sender + either unsubscribe header when delete_matching=true; the unsubscribe POST is best-effort and never blocks deletion. \
             list_flags resolves Apple Mail $MailFlagBit color flags to named colors (red, orange, yellow, green, blue, purple, gray). \
             find_attachments detects multipart/mixed and multipart/related; download_attachments saves them to disk. \
             Single messages are also readable as resources: email://{account}/{mailbox}/{uid} (markdown) and email://{account}/{mailbox}/{uid}/source (raw RFC822); percent-encode account and mailbox, encoding '/' in mailbox names as %2F. \
             All reads use BODY.PEEK to avoid marking messages as read.",
        )
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult::with_all_items(
            resources::email_resource_templates(),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        self.read_email_resource(&request.uri).await
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        self.handle_complete(request).await
    }

    async fn enqueue_task(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CreateTaskResult, McpError> {
        let task_id = uuid::Uuid::new_v4().to_string();
        let now = utc_now_iso8601();

        let is_destructive = DESTRUCTIVE_TOOLS.contains(&request.name.as_ref());
        let destructive_lock = if is_destructive {
            let account = extract_account(&request.arguments).ok_or_else(|| {
                McpError::invalid_params(
                    "destructive tools require an 'account' argument for task queuing",
                    None,
                )
            })?;
            Some(self.task_manager.lock().destructive_lock(&account))
        } else {
            None
        };

        let result_slot: Arc<parking_lot::Mutex<Option<Result<CallToolResult, McpError>>>> =
            Arc::new(parking_lot::Mutex::new(None));

        let server = self.clone();
        let slot = Arc::clone(&result_slot);
        let handle = tokio::spawn(async move {
            // If destructive, acquire the per-account lock first.
            // This serializes destructive tasks — the task waits here until
            // any previously-enqueued destructive task on the same account
            // finishes.
            let _guard = match destructive_lock {
                Some(ref lock) => Some(lock.lock().await),
                None => None,
            };
            // Cancellation: `tasks/cancel` → JoinHandle::abort() is the
            // effective cancel path for task-based execution (rmcp keeps the
            // original request's cancellation token alive after the
            // CreateTaskResult response, but spec-compliant clients never
            // cancel an already-responded request id). The cooperative
            // CancelFn threaded through `context.ct` serves direct tools/call
            // requests and transport shutdown.
            let result = server.call_tool(request, context).await;
            *slot.lock() = Some(result);
        });

        let task = Task::new(task_id.clone(), TaskStatus::Working, now.clone(), now)
            .with_poll_interval(2000);

        let managed = ManagedTask {
            meta: task.clone(),
            result: result_slot,
            handle,
        };

        self.task_manager.lock().tasks.insert(task_id, managed);

        Ok(CreateTaskResult::new(task))
    }

    async fn list_tasks(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListTasksResult, McpError> {
        let mut mgr = self.task_manager.lock();
        mgr.refresh_all();
        let tasks: Vec<Task> = mgr.tasks.values().map(|m| m.meta.clone()).collect();
        Ok(ListTasksResult::new(tasks))
    }

    async fn get_task_info(
        &self,
        request: GetTaskInfoParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        let mut mgr = self.task_manager.lock();
        mgr.refresh_status(&request.task_id);
        let managed = mgr.tasks.get(&request.task_id).ok_or_else(|| {
            McpError::invalid_params(format!("unknown task: {}", request.task_id), None)
        })?;
        Ok(GetTaskResult {
            meta: None,
            task: managed.meta.clone(),
        })
    }

    async fn get_task_result(
        &self,
        request: GetTaskResultParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskPayloadResult, McpError> {
        let mut mgr = self.task_manager.lock();
        mgr.refresh_status(&request.task_id);
        let managed = mgr.tasks.get(&request.task_id).ok_or_else(|| {
            McpError::invalid_params(format!("unknown task: {}", request.task_id), None)
        })?;
        match managed.meta.status {
            TaskStatus::Working => Err(McpError::invalid_params("task is still running", None)),
            TaskStatus::Cancelled => Err(McpError::invalid_params("task was cancelled", None)),
            _ => {
                // Take the result out of the slot.
                let result = managed.result.try_lock().and_then(|mut guard| guard.take());
                match result {
                    Some(Ok(call_result)) => {
                        let value = serde_json::to_value(call_result)
                            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                        Ok(GetTaskPayloadResult::new(value))
                    }
                    Some(Err(e)) => Err(e),
                    None => Err(McpError::internal_error(
                        "task result already consumed",
                        None,
                    )),
                }
            }
        }
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CancelTaskResult, McpError> {
        let mut mgr = self.task_manager.lock();
        let managed = mgr.tasks.get_mut(&request.task_id).ok_or_else(|| {
            McpError::invalid_params(format!("unknown task: {}", request.task_id), None)
        })?;
        if managed.meta.status == TaskStatus::Working {
            managed.handle.abort();
            managed.meta.status = TaskStatus::Cancelled;
            managed.meta.last_updated_at = utc_now_iso8601();
        }
        Ok(CancelTaskResult {
            meta: None,
            task: managed.meta.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Public API — serve functions
// ---------------------------------------------------------------------------

/// Serve the MCP server over an arbitrary `AsyncRead + AsyncWrite` transport.
///
/// This is intended for in-process callers (e.g. the Tauri host) that provide
/// a `DuplexStream` or similar transport instead of stdio.
pub async fn serve_on<T>(
    transport: T,
    mk: crate::Agentmail,
) -> Result<(), Box<dyn std::error::Error>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let server = AgentMailServer::new(mk);
    let service = server.serve(transport).await.inspect_err(|e| {
        eprintln!("agentmail: server error: {}", e);
    })?;
    service.waiting().await?;
    Ok(())
}

/// Serve the MCP server over stdio.
///
/// This is the entry point for the standalone `agentmail serve` binary.
pub async fn serve_stdio(mk: crate::Agentmail) -> Result<(), Box<dyn std::error::Error>> {
    // Pre-warm: validate credentials and open one connection per account.
    for account in mk.account_names() {
        let account_hint = account_log_hint(&account);
        match mk.check_connection(&account).await {
            Ok(status) if status.connected => {
                eprintln!("agentmail: {} connected", account_hint);
            }
            Ok(status) => {
                eprintln!(
                    "agentmail: {} connection failed: {}",
                    account_hint,
                    status.error.as_deref().unwrap_or("unknown")
                );
            }
            Err(e) => {
                eprintln!("agentmail: {} credential error: {}", account_hint, e);
            }
        }
    }

    let server = AgentMailServer::new(mk);
    let service = server
        .serve(rmcp::transport::io::stdio())
        .await
        .inspect_err(|e| {
            eprintln!("agentmail: server error: {}", e);
        })?;
    service.waiting().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk a schema JSON tree asserting no `$ref`/`$defs` keys — several MCP
    /// hosts (Gemini CLI, n8n, some gateways) reject or drop referenced
    /// schemas, so every nested type must carry `#[schemars(inline)]`.
    fn assert_no_refs(value: &serde_json::Value, path: &str, tool: &str, side: &str) {
        match value {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    assert!(
                        k != "$ref" && k != "$defs",
                        "tool `{tool}` {side} schema has `{k}` at {path} — \
                         add #[schemars(inline)] to the nested type"
                    );
                    assert_no_refs(v, &format!("{path}/{k}"), tool, side);
                }
            }
            serde_json::Value::Array(items) => {
                for (i, v) in items.iter().enumerate() {
                    assert_no_refs(v, &format!("{path}/{i}"), tool, side);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn tool_schemas_are_ref_free() {
        let tools = AgentMailServer::tool_router().list_all();
        assert_eq!(
            tools.len(),
            21,
            "tool count drifted — update docs and tests"
        );
        for tool in &tools {
            let input = serde_json::to_value(tool.input_schema.as_ref()).unwrap();
            assert_no_refs(&input, "#", &tool.name, "input");
            let output = tool
                .output_schema
                .as_ref()
                .unwrap_or_else(|| panic!("tool `{}` lost its output schema", tool.name));
            let output = serde_json::to_value(output.as_ref()).unwrap();
            assert_no_refs(&output, "#", &tool.name, "output");
        }
    }

    #[test]
    fn every_tool_has_title_and_description() {
        for tool in AgentMailServer::tool_router().list_all() {
            assert!(
                tool.description.as_deref().is_some_and(|d| !d.is_empty()),
                "tool `{}` has no description",
                tool.name
            );
            let title = tool
                .annotations
                .as_ref()
                .and_then(|a| a.title.as_deref())
                .unwrap_or_default();
            assert!(
                !title.is_empty(),
                "tool `{}` has no annotations.title",
                tool.name
            );
        }
    }

    /// `DESTRUCTIVE_TOOLS` gates task serialization and must stay in sync with
    /// the per-tool annotations (MCP default: destructive unless read-only).
    #[test]
    fn destructive_tools_const_matches_annotations() {
        for tool in AgentMailServer::tool_router().list_all() {
            let destructive = tool.annotations.as_ref().is_none_or(|a| {
                !a.read_only_hint.unwrap_or(false) && a.destructive_hint.unwrap_or(true)
            });
            assert_eq!(
                DESTRUCTIVE_TOOLS.contains(&tool.name.as_ref()),
                destructive,
                "DESTRUCTIVE_TOOLS drifted for `{}` — update the const or the annotations",
                tool.name
            );
        }
    }

    #[test]
    fn cancel_fn_reflects_token_state() {
        let ct = tokio_util::sync::CancellationToken::new();
        let cancel = make_cancel_fn(ct.clone());
        assert!(!cancel());
        assert!(crate::imap_client::check_cancel(Some(&cancel)).is_ok());
        ct.cancel();
        assert!(cancel());
        let err = crate::imap_client::check_cancel(Some(&cancel)).unwrap_err();
        assert_eq!(err.to_string(), "cancelled by client");
    }
}
