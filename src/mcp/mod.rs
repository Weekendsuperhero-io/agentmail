//! MCP server: tools, prompts, and tasks over stdio or in-process transports.

mod args;
mod file_access;
mod prompts;
mod resources;
mod tasks;
mod tools_read;
mod tools_write;
mod wire;

use self::tasks::{DESTRUCTIVE_TOOLS, TaskCompletion, TaskManager, extract_account};
use futures::{FutureExt as _, StreamExt as _};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
    handler::server::router::tool::ToolRouter,
    model::{
        CallToolRequestParams, CallToolResult, CancelTaskParams, CancelTaskResult,
        CompleteRequestParams, CompleteResult, CreateTaskResult, GetTaskParams,
        GetTaskPayloadParams, GetTaskPayloadResult, GetTaskResult, Implementation,
        ListResourceTemplatesResult, ListTasksResult, Meta, PaginatedRequestParams,
        ProgressNotificationParam, ReadResourceRequestParams, ReadResourceResult,
        RelatedTaskMetadata, ServerCapabilities, ServerInfo, TasksCapability,
    },
    prompt_handler,
    service::RequestContext,
    tool_handler,
};
use std::{panic::AssertUnwindSafe, sync::Arc};

const PREWARM_CONCURRENCY: usize = 3;
const PREWARM_ACCOUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

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

fn insert_related_task(meta: &mut Meta, task_id: &str) {
    meta.0.insert(
        RelatedTaskMetadata::META_KEY.to_string(),
        serde_json::json!({ "taskId": task_id }),
    );
}

fn attach_related_task(result: &mut CallToolResult, task_id: &str) {
    let meta = result.meta.get_or_insert_with(Meta::new);
    insert_related_task(meta, task_id);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProgressUpdate {
    completed: u64,
    total: u64,
}

struct ProgressReporter {
    callback: Option<crate::ProgressFn>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl ProgressReporter {
    fn disabled() -> Self {
        Self {
            callback: None,
            worker: None,
        }
    }

    fn callback(&self) -> Option<&crate::ProgressFn> {
        self.callback.as_ref()
    }

    /// Flush the latest coalesced update before the tool result is returned.
    async fn finish(mut self) {
        self.callback.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
    }
}

fn normalized_progress(
    previous: Option<ProgressUpdate>,
    completed: u64,
    total: u64,
) -> Option<ProgressUpdate> {
    if previous.is_some_and(|previous| completed < previous.completed) {
        return None;
    }
    let total = previous
        .map_or(total, |previous| total.max(previous.total))
        .max(completed);
    let update = ProgressUpdate { completed, total };
    (previous != Some(update)).then_some(update)
}

/// Build a bounded, coalescing progress stream. One worker serializes all
/// notifications for the request, preserving monotonic progress and avoiding
/// a detached Tokio task per callback.
fn make_progress_fn(meta: &Meta, peer: &Peer<RoleServer>) -> ProgressReporter {
    let Some(token) = meta.get_progress_token() else {
        return ProgressReporter::disabled();
    };
    let related_task = meta.0.get(RelatedTaskMetadata::META_KEY).cloned();
    let peer = peer.clone();
    let (sender, mut receiver) = tokio::sync::watch::channel(None::<ProgressUpdate>);
    let latest = Arc::new(parking_lot::Mutex::new(None::<ProgressUpdate>));
    let callback_latest = Arc::clone(&latest);
    let callback = Arc::new(move |completed: u64, total: u64| {
        let mut previous = callback_latest.lock();
        let Some(update) = normalized_progress(*previous, completed, total) else {
            return;
        };
        *previous = Some(update);
        sender.send_replace(Some(update));
    });
    let worker = tokio::spawn(async move {
        while receiver.changed().await.is_ok() {
            let Some(update) = *receiver.borrow_and_update() else {
                continue;
            };
            let mut notification =
                ProgressNotificationParam::new(token.clone(), update.completed as f64)
                    .with_total(update.total as f64);
            if let Some(related_task) = related_task.clone() {
                let mut meta = Meta::new();
                meta.0
                    .insert(RelatedTaskMetadata::META_KEY.to_string(), related_task);
                notification.meta = Some(meta);
            }
            if peer.notify_progress(notification).await.is_err() {
                break;
            }
        }
    });
    ProgressReporter {
        callback: Some(callback),
        worker: Some(worker),
    }
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
        | E::UidValidityUnavailable { .. }
        | E::UidValidityChanged { .. }
        | E::UnsubscribeConsentRequired
        | E::InvalidUnsubscribePolicy(_)
        | E::InvalidSearch(_)
        | E::Config(_)
        | E::Credential(_) => McpError::invalid_params(e.to_string(), None),
        E::Imap(_)
        | E::Tls(_)
        | E::Io(_)
        | E::JournalSqlite(_)
        | E::Parse(_)
        | E::NotConnected
        | E::PoolExhausted
        | E::Cancelled
        | E::Other(_) => McpError::internal_error(e.to_string(), None),
    }
}

fn bounded_usize(
    value: Option<u64>,
    default: usize,
    min: usize,
    max: usize,
    name: &str,
) -> Result<usize, McpError> {
    let value = value.unwrap_or(default as u64);
    let value = usize::try_from(value).map_err(|_| {
        McpError::invalid_params(format!("{name} is too large; maximum is {max}"), None)
    })?;
    if !(min..=max).contains(&value) {
        return Err(McpError::invalid_params(
            format!("{name} must be between {min} and {max}"),
            None,
        ));
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Pagination {
    pub(super) offset: usize,
    pub(super) limit: usize,
}

impl Pagination {
    /// Validate the common MCP offset/limit envelope in one place.
    pub(super) fn new(
        offset: Option<u64>,
        limit: Option<u64>,
        default_limit: usize,
        max_limit: usize,
    ) -> Result<Self, McpError> {
        Ok(Self {
            offset: bounded_usize(offset, 0, 0, 1_000_000, "offset")?,
            limit: bounded_usize(limit, default_limit, 1, max_limit, "limit")?,
        })
    }
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AgentMailServer {
    agentmail: Arc<crate::Agentmail>,
    task_manager: Arc<parking_lot::Mutex<TaskManager>>,
    /// Sandbox for LLM-supplied filesystem paths (attachment reads, download
    /// writes). See [`file_access::FileAccessPolicy`].
    file_access: file_access::FileAccessPolicy,
}

impl AgentMailServer {
    pub fn new(agentmail: crate::Agentmail) -> Self {
        Self {
            agentmail: Arc::new(agentmail),
            task_manager: Arc::new(parking_lot::Mutex::new(TaskManager::new())),
            file_access: file_access::FileAccessPolicy::from_env(),
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
        let mut capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources()
            .enable_completions()
            .enable_tasks()
            .build();
        // `enable_tasks()` only switches the capability on. The protocol also
        // requires the server to declare the supported lifecycle methods and
        // which requests may be task-augmented.
        capabilities.tasks = Some(TasksCapability::server_default());
        ServerInfo::new(capabilities)
        // Without this the server announces itself as "rmcp" — rmcp's
        // Implementation::from_build_env() bakes in its own crate name.
        // Version carries the build SHA so `initialize` responses and logs
        // identify the exact running build — deploy skew (an app compiled
        // from a stale agentmail checkout) becomes visible instead of being
        // inferred from behavioral fingerprints.
        .with_server_info(Implementation::new(
            "agentmail",
            concat!(env!("CARGO_PKG_VERSION"), " (", env!("AGENTMAIL_BUILD_SHA"), ")"),
        ))
        .with_instructions(
            "AgentMail is a full-featured IMAP email client. \
             Start with list_accounts to discover configured accounts. \
             list_mailboxes requires one account and returns selectable mailboxes only, paginated with a default of 100. \
             get_messages and search_messages return metadata only, newest-first, with the mailbox UIDVALIDITY and a UIDVALIDITY-safe resourceUri for each row. \
             Read resourceUri for markdown content; append /headers for exact headers, /source for bounded raw RFC822, /info for JSON metadata with the attachment inventory, or /attachments/{index} for one attachment blob. \
             Manage email: delete_messages, delete_by_sender, delete_by_domain, delete_list_id, move_message, move_list_id, move_by_sender, move_by_domain, create_draft (supports attachments), create_mailbox, unsubscribe_message. \
             Bulk filing: move_list_id and move_by_sender move every match to an existing destination mailbox in one call (e.g. statements into a folder) — never loop move_message per UID for that. \
             top_senders, top_domains, top_subscriptions, top_mailing_lists, list_flags, and find_attachments accept an optional mailbox — omit it to scan the entire account. \
             Ranked tools use live offset pages with a maximum of 100; top_domains defaults to 20 rows and the other ranked tools default to 10. Pages may shift when mail changes. \
             On providers with a visible-window limit (Yahoo/AOL), rankings and account-wide delete/move sweeps cover the ENTIRE mailbox via RFC 9586 UID Mode when a persistent cache is available; without it, sweeps repeat passes as older mail backfills into view. \
             If an action reports Message not found for a ranking sample, the message was deleted since the scan — re-run the ranking for a fresh sample instead of retrying the same UID. \
             Account-wide discovery uses one selectable All mailbox when available; otherwise it skips Trash, Junk, Spam, Drafts, and virtual aggregate views. Destructive scans never write through aggregate views. \
             Every action that consumes a UID requires the same mailbox and non-zero expectedUidValidity returned during discovery, and fails closed if the mailbox UID epoch changed. \
             Three cleanup workflows: (1) top_senders → delete_by_sender for unwanted personal senders, (2) top_domains → delete_by_domain or move_by_domain for one exact Header From domain, (3) top_subscriptions → unsubscribe_message for mailing lists. \
             top_domains keeps parent domains and subdomains separate — actions on example.com never include mail.example.com implicitly; registrableDomain and subdomain are grouping context only. \
             Never use delete_by_sender for mailing list cleanup — it deletes ALL messages from a sender including non-bulk ones. \
             top_mailing_lists groups by List-Id header (RFC 2919) — all messages from the same mailing list regardless of sender. Use delete_list_id to remove an entire list. \
             top_senders groups by (email, display name) — same email with different display names are separate entries. \
             A non-native MOVE can return reconciliationPending or needsAttention with an operationId; use list_pending_moves to inspect durable operations and reconcile_moves to safely retry one operation or all pending operations. \
             Ranking rows include a nested sample {mailbox, uidValidity, uid, resourceUri}; map those fields to mailbox, expectedUidValidity, and uid for a later action. \
             top_subscriptions advertisedOneClick is syntactic only; unsubscribe_message re-fetches exact headers and verifies DKIM. \
             unsubscribe_message requires explicit confirmOneClick=true. Optional matching-message cleanup is the nested cleanup {when, identity, deletion} object (omit it to only unsubscribe); it prefers the DKIM-authenticated List-Id, stops after a failed POST unless when=\"always\", and never silently escalates a Trash failure to permanent deletion unless deletion=\"trashThenPermanent\". \
             list_flags resolves Apple Mail $MailFlagBit color flags to named colors (red, orange, yellow, green, blue, purple, gray). \
             find_attachments returns mailbox-safe {mailbox, uidValidity, uid, date, resourceUri} hits; pass that identity to download_attachments. \
             Message resources are email://{account}/{mailbox}/{uidValidity}/{uid} (markdown), plus /headers (exact headers), /source (bounded raw RFC822), /info (JSON metadata: subject, sender, date, flags, size, attachment inventory), and /attachments/{index} (one attachment as a blob with its own content type, 4 MiB limit). Percent-encode account and mailbox, including '/' in mailbox names as %2F. \
             Read /info first to discover attachment indices; each attachment carries the canonical filename {uid}_{index}_{name} — the same name download_attachments writes to disk. \
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
        mut context: RequestContext<RoleServer>,
    ) -> Result<CreateTaskResult, McpError> {
        let task_id = uuid::Uuid::new_v4().to_string();

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

        // Reserve before spawning so capacity rejection cannot start work.
        let task = self.task_manager.lock().reserve_task(task_id.clone())?;

        let server = self.clone();
        let completion = Arc::new(TaskCompletion::new());
        let worker_completion = Arc::clone(&completion);
        // Keep transport cancellation inherited from the request while also
        // giving tasks/cancel a token that reaches cooperative library work
        // and spawn_blocking SQLite publication.
        let task_cancel = context.ct.child_token();
        context.ct = task_cancel.clone();
        insert_related_task(&mut context.meta, &task_id);
        // Captured BEFORE `context` moves into the worker: the push target.
        let peer = context.peer.clone();
        let notify_id = task_id.clone();
        let notify_manager = Arc::clone(&self.task_manager);
        let handle = tokio::spawn(async move {
            // If destructive, acquire the per-account lock first.
            // This serializes destructive tasks — the task waits here until
            // any previously-enqueued destructive task on the same account
            // finishes.
            let _guard = match destructive_lock {
                Some(ref lock) => Some(lock.lock().await),
                None => None,
            };
            let result = AssertUnwindSafe(server.call_tool(request, context))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    Err(McpError::internal_error(
                        "task worker panicked while executing the tool",
                        None,
                    ))
                });
            worker_completion.complete(result);
        });

        self.task_manager.lock().commit_task(
            &task_id,
            Arc::clone(&completion),
            task_cancel,
            handle,
        )?;

        // Status watcher — the single producer of `notifications/tasks/status`.
        //
        // Spawned AFTER `commit_task` on purpose. The worker above can finish
        // before the task is committed (a fast failure beats the commit to the
        // lock), and `task_info` cannot resolve a task that is still only
        // RESERVED — pushing from the worker therefore dropped the
        // notification exactly when the task failed fastest.
        //
        // Waiting on the completion also unifies the two ways a task ends:
        // `cancel_task` publishes a cancelled result, so cancellation wakes
        // this same watcher rather than needing its own push (which would
        // double-notify).
        tokio::spawn(async move {
            // A never-cancelled token: this watcher waits for the TASK, and
            // must not be torn down by any one request's cancellation.
            let _ = completion
                .wait(&tokio_util::sync::CancellationToken::new())
                .await;
            // Read back through `task_info`, which runs the same lazy refresh
            // a `tasks/get` would — so the pushed status can never contradict
            // a subsequent poll.
            let Ok(task) = notify_manager.lock().task_info(&notify_id) else {
                return;
            };
            let notification = rmcp::model::ServerNotification::TaskStatusNotification(
                rmcp::model::TaskStatusNotification::new(
                    rmcp::model::TaskStatusNotificationParam::new(task),
                ),
            );
            if let Err(error) = peer.send_notification(notification).await {
                tracing::debug!(
                    target: "agentmail",
                    task = %notify_id,
                    "tasks/status push failed — client gone: {error}"
                );
            }
        });

        Ok(CreateTaskResult::new(task))
    }

    async fn list_tasks(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListTasksResult, McpError> {
        self.task_manager
            .lock()
            .list_page(request.as_ref().and_then(|params| params.cursor.as_deref()))
    }

    async fn get_task_info(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        let task = self.task_manager.lock().task_info(&request.task_id)?;
        Ok(GetTaskResult::new(task))
    }

    async fn get_task_result(
        &self,
        request: GetTaskPayloadParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetTaskPayloadResult, McpError> {
        let completion = self.task_manager.lock().task_completion(&request.task_id)?;
        let mut result = completion.wait(&context.ct).await?;
        attach_related_task(&mut result, &request.task_id);
        let value = serde_json::to_value(result)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(GetTaskPayloadResult::new(value))
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CancelTaskResult, McpError> {
        // No push here: `cancel_task` publishes a cancelled result, which wakes
        // the status watcher spawned in `enqueue_task`. That single watcher is
        // the ONLY producer of `notifications/tasks/status`, so cancel and
        // natural completion cannot double-notify.
        let task = self.task_manager.lock().cancel_task(&request.task_id)?;
        Ok(CancelTaskResult::new(task))
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
    // One self-identifying line per server start: the exact build serving
    // this process, so log forensics never have to infer the version from
    // behavior again.
    tracing::info!(
        target: "agentmail",
        version = env!("CARGO_PKG_VERSION"),
        build = env!("AGENTMAIL_BUILD_SHA"),
        "agentmail MCP server starting"
    );
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
    let server = AgentMailServer::new(mk);
    let prewarm_agentmail = Arc::clone(&server.agentmail);
    let service = server
        .serve(rmcp::transport::io::stdio())
        .await
        .inspect_err(|e| {
            eprintln!("agentmail: server error: {}", e);
        })?;
    // Initialization is already complete. Connection warming is bounded and
    // never delays the MCP handshake or availability of config-backed tools.
    let prewarm = tokio::spawn(async move {
        futures::stream::iter(prewarm_agentmail.account_names())
            .for_each_concurrent(PREWARM_CONCURRENCY, |account| {
                let agentmail = Arc::clone(&prewarm_agentmail);
                async move {
                    let account_hint = account_log_hint(&account);
                    match tokio::time::timeout(
                        PREWARM_ACCOUNT_TIMEOUT,
                        agentmail.check_connection(&account),
                    )
                    .await
                    {
                        Ok(Ok(status)) if status.connected => {
                            eprintln!("agentmail: {} connected", account_hint);
                        }
                        Ok(Ok(status)) => {
                            eprintln!(
                                "agentmail: {} connection failed: {}",
                                account_hint,
                                status.error.as_deref().unwrap_or("unknown")
                            );
                        }
                        Ok(Err(error)) => {
                            eprintln!("agentmail: {} credential error: {}", account_hint, error);
                        }
                        Err(_) => {
                            eprintln!(
                                "agentmail: {} connection prewarm timed out after {}s",
                                account_hint,
                                PREWARM_ACCOUNT_TIMEOUT.as_secs()
                            );
                        }
                    }
                }
            })
            .await;
    });
    let result = service.waiting().await;
    prewarm.abort();
    let _ = prewarm.await;
    result?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_applies_defaults_and_bounds_consistently() {
        assert_eq!(
            Pagination::new(None, None, 20, 100).expect("defaults should be valid"),
            Pagination {
                offset: 0,
                limit: 20,
            }
        );
        assert!(Pagination::new(Some(1_000_001), Some(1), 20, 100).is_err());
        assert!(Pagination::new(None, Some(0), 20, 100).is_err());
        assert!(Pagination::new(None, Some(101), 20, 100).is_err());
        assert_eq!(
            Pagination::new(Some(7), Some(7), 20, 100).expect("explicit page is preserved"),
            Pagination {
                offset: 7,
                limit: 7,
            }
        );
        assert_eq!(
            Pagination::new(Some(1_000_000), Some(100), 20, 100)
                .expect("documented inclusive maxima are accepted"),
            Pagination {
                offset: 1_000_000,
                limit: 100,
            }
        );
    }

    #[test]
    fn progress_updates_are_monotonic_and_deduplicated() {
        let previous = ProgressUpdate {
            completed: 5,
            total: 10,
        };
        assert_eq!(normalized_progress(Some(previous), 4, 10), None);
        assert_eq!(normalized_progress(Some(previous), 5, 10), None);
        assert_eq!(
            normalized_progress(Some(previous), 7, 6),
            Some(ProgressUpdate {
                completed: 7,
                total: 10,
            })
        );
        assert_eq!(
            normalized_progress(Some(previous), 12, 8),
            Some(ProgressUpdate {
                completed: 12,
                total: 12,
            })
        );
    }

    /// Output schemas mark every non-Option field `required`, and strict MCP
    /// hosts validate `structuredContent` against them — so a field that is
    /// omitted when empty/false (`skip_serializing_if` on a `Vec`, `bool`, or
    /// map) makes the tool result fail validation exactly when the value is
    /// boring (observed live: `roles` on list_mailboxes for a role-less
    /// mailbox, `skipped` on unsubscribe_message). Output types must always
    /// serialize such fields; only `Option` fields (not required in the
    /// schema) may be skipped.
    #[test]
    fn output_types_never_skip_required_fields() {
        for (file, source) in [
            ("src/types.rs", include_str!("../types.rs")),
            ("src/mcp/wire.rs", include_str!("wire.rs")),
        ] {
            for skipper in ["Vec::is_empty", "Not::not", "HashMap::is_empty", "is_empty"] {
                let needle = format!("skip_serializing_if = \"{skipper}");
                let hits = source.lines().filter(|line| line.contains(&needle)).count();
                assert_eq!(
                    hits, 0,
                    "{file} skips a schema-required field via {skipper}; strict hosts reject the structuredContent"
                );
            }
        }
    }

    /// The concrete case the gateway rejected: a mailbox with no special-use
    /// role must still serialize `roles: []` because the output schema
    /// requires the key.
    #[test]
    fn empty_collections_and_false_flags_serialize_explicitly() {
        let mailbox = serde_json::to_value(wire::MailboxOutput {
            name: "SavedIMs".to_string(),
            total_messages: 0,
            unseen_messages: 0,
            delimiter: Some("/".to_string()),
            no_inferiors: false,
            roles: Vec::new(),
        })
        .expect("serialize");
        assert_eq!(
            mailbox["roles"],
            serde_json::json!([]),
            "role-less mailboxes must still carry roles: {mailbox}"
        );
    }

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
            28,
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

    #[test]
    fn uid_actions_require_nonzero_uid_validity() {
        let tools = AgentMailServer::tool_router().list_all();
        // delete_by_sender is absent by design: it takes a direct sender
        // identity (email + name) rather than a sample UID, so it carries no
        // UIDVALIDITY guard — discovery re-finds and confirms live.
        for name in [
            "delete_messages",
            "download_attachments",
            "move_message",
            "unsubscribe_message",
            "add_flags",
            "remove_flags",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == name)
                .unwrap_or_else(|| panic!("missing tool `{name}`"));
            let schema = serde_json::to_value(tool.input_schema.as_ref()).unwrap();
            let required = schema["required"]
                .as_array()
                .unwrap_or_else(|| panic!("tool `{name}` has no required array"));
            assert!(
                required
                    .iter()
                    .any(|field| field.as_str() == Some("expectedUidValidity")),
                "tool `{name}` must require expectedUidValidity"
            );
            assert_eq!(
                schema["properties"]["expectedUidValidity"]["minimum"],
                serde_json::json!(1),
                "tool `{name}` must reject UIDVALIDITY zero in its schema"
            );
            if name == "delete_messages" {
                assert_eq!(
                    schema["properties"]["uids"]["items"]["minimum"],
                    serde_json::json!(1),
                    "delete_messages must reject UID zero in its schema"
                );
            } else {
                assert_eq!(
                    schema["properties"]["uid"]["minimum"],
                    serde_json::json!(1),
                    "tool `{name}` must reject UID zero in its schema"
                );
            }
        }
    }

    #[test]
    fn list_mailboxes_requires_account() {
        let tools = AgentMailServer::tool_router().list_all();
        let tool = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "list_mailboxes")
            .expect("missing list_mailboxes tool");
        let schema = serde_json::to_value(tool.input_schema.as_ref()).unwrap();
        let required = schema["required"]
            .as_array()
            .expect("list_mailboxes has no required array");

        assert!(
            required
                .iter()
                .any(|field| field.as_str() == Some("account"))
        );
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
