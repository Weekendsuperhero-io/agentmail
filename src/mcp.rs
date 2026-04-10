use crate::{
    ConnectionStatus, CreateDraftResponse, CreateMailboxResponse, DeleteBySenderResponse,
    DeleteListIdResponse, DeleteMessagesResponse, DownloadAttachmentsResponse,
    FindAttachmentsResponse, GetMessagesResponse, ListAccountsResponse, ListCapabilitiesResponse,
    ListFlagsResponse, ListMailboxesResponse, MoveMessageResponse, RankListIdResponse,
    RankSendersResponse, RankUnsubscribeResponse, SearchMessagesResponse, UnsubscribeResponse,
    UpdateFlagsResponse,
};
use hashbrown::HashMap;
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::{Json, Parameters},
    model::{
        CallToolRequestParams, CallToolResult, CancelTaskParams, CancelTaskResult,
        CreateTaskResult, GetPromptRequestParams, GetPromptResult, GetTaskInfoParams,
        GetTaskPayloadResult, GetTaskResult, GetTaskResultParams, ListPromptsResult,
        ListTasksResult, Meta, PaginatedRequestParams, ProgressNotificationParam, PromptMessage,
        PromptMessageRole, ServerCapabilities, ServerInfo, Task, TaskStatus,
    },
    prompt, prompt_handler, prompt_router,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

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

// ---------------------------------------------------------------------------
// Tool argument structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "No arguments.")]
struct ListAccountsArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for listing mailboxes.")]
struct ListMailboxesArgs {
    #[schemars(
        description = "Optional account name. If omitted, list mailboxes across all accounts."
    )]
    account: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for checking IMAP connection status.")]
struct CheckConnectionArgs {
    #[schemars(description = "Account name to check connectivity for.")]
    account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for listing IMAP server capabilities.")]
struct ListCapabilitiesArgs {
    #[schemars(description = "Account name to query capabilities for.")]
    account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for fetching a paginated chunk of messages.")]
struct GetMessagesArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "Zero-based row offset. Defaults to 0.")]
    offset: Option<u64>,
    #[schemars(description = "Page size. Defaults to 25 and is clamped to 1..50.")]
    limit: Option<u64>,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, include normalized markdown content (trimmed for context window safety)."
    )]
    include_content: bool,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, include the full raw headers map. Off by default — structured fields (subject, sender, to, cc, date, message_id, etc.) are always returned."
    )]
    include_headers: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for mailbox message search with optional filters.")]
struct SearchMessagesArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "General full-text search across message fields (IMAP TEXT search).")]
    query: Option<String>,
    #[schemars(description = "Filter by sender (IMAP FROM search).")]
    sender_contains: Option<String>,
    #[schemars(description = "Filter by subject (IMAP SUBJECT search).")]
    subject_contains: Option<String>,
    #[schemars(description = "Filter by recipient (IMAP TO search).")]
    to_contains: Option<String>,
    #[schemars(description = "Header key for header-based search.")]
    header_key: Option<String>,
    #[schemars(description = "Header value filter (used with header_key).")]
    header_value_contains: Option<String>,
    #[schemars(description = "Filter by flagged status.")]
    flagged: Option<bool>,
    #[schemars(description = "Filter by read/seen status.")]
    read: Option<bool>,
    #[serde(default = "default_false")]
    #[schemars(description = "Include deleted messages. Defaults to false.")]
    deleted: bool,
    #[schemars(description = "Zero-based row offset. Defaults to 0.")]
    offset: Option<u64>,
    #[schemars(description = "Page size. Defaults to 25 and is clamped to 1..50.")]
    limit: Option<u64>,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, include normalized markdown content (trimmed for context window safety)."
    )]
    include_content: bool,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, include the full raw headers map. Off by default — structured fields (subject, sender, to, cc, date, message_id, etc.) are always returned."
    )]
    include_headers: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for listing flags in use.")]
struct ListFlagsArgs {
    #[schemars(description = "Mailbox to scan. Omit to scan all mailboxes in the account.")]
    mailbox: Option<String>,
    #[schemars(description = "Account name (required).")]
    account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for deleting one or more messages.")]
struct DeleteMessagesArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "Array of IMAP UIDs to delete. One or more UIDs, up to 500.")]
    uids: Vec<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "Arguments for deleting all messages from a specific sender. The sender string is matched as a substring against the full From header (covers both display name and email address)."
)]
struct DeleteBySenderArgs {
    #[schemars(description = "Mailbox containing the target UID. Defaults to INBOX when omitted.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(
        description = "UID of a message from the sender to delete. The exact sender (email + display name) is extracted from this message and used to find all other messages from the same sender."
    )]
    uid: u32,
    #[serde(default = "default_false")]
    #[schemars(
        description = "When true, search and delete across ALL mailboxes in the account (not just the source mailbox). Defaults to false."
    )]
    all_mailboxes: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for finding messages with attachments.")]
struct FindAttachmentsArgs {
    #[schemars(description = "Mailbox name. Omit to scan all mailboxes in the account.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "Number of UIDs to skip. Defaults to 0.")]
    offset: Option<u64>,
    #[schemars(description = "Max UIDs to return. Defaults to 25, max 100.")]
    limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for creating a draft message.")]
struct CreateDraftArgs {
    #[schemars(
        description = "Account name (required). Draft is saved to this account's Drafts folder."
    )]
    account: String,
    #[serde(default)]
    #[schemars(description = "Draft subject line.")]
    subject: String,
    #[serde(default)]
    #[schemars(description = "Draft body content.")]
    body: String,
    #[serde(default)]
    #[schemars(description = "To recipient email addresses.")]
    to: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Cc recipient email addresses.")]
    cc: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Bcc recipient email addresses.")]
    bcc: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for moving a message between mailboxes.")]
struct MoveMessageArgs {
    #[schemars(description = "Source mailbox name.")]
    mailbox: String,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "IMAP UID of the message to move.")]
    uid: u32,
    #[schemars(description = "Destination mailbox name.")]
    destination: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for unsubscribe + optional list cleanup.")]
struct UnsubscribeMessageArgs {
    #[schemars(
        description = "Mailbox containing the target message. Defaults to INBOX. When deleting matching messages, all mailboxes are searched regardless of this value."
    )]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "IMAP UID of the message.")]
    uid: u32,
    #[serde(default = "default_true")]
    #[schemars(
        description = "If true, bulk-delete matching messages. For List-Unsubscribe messages: deletes all from the exact sender with a List-Unsubscribe header. For List-Id-only messages: deletes all with the same List-Id."
    )]
    delete_matching: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for ranking senders by message count.")]
struct RankSendersArgs {
    #[schemars(description = "Mailbox name. When omitted, scans ALL mailboxes in the account.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(
        description = "Maximum number of senders to return. If omitted, returns all senders."
    )]
    limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for ranking mailing-list senders by message count.")]
struct RankUnsubscribeArgs {
    #[schemars(description = "Mailbox name. When omitted, scans ALL mailboxes in the account.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "Maximum number of lists to return. If omitted, returns all lists.")]
    limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for ranking mailing lists by List-Id header.")]
struct RankListIdArgs {
    #[schemars(description = "Mailbox name. When omitted, scans ALL mailboxes in the account.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "Maximum number of lists to return. If omitted, returns all lists.")]
    limit: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for deleting all messages with a specific List-Id.")]
struct DeleteListIdArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "The List-Id header value to match (from rank_list_id).")]
    list_id: String,
    #[schemars(description = "Mailbox to search. Omit to search all mailboxes.")]
    mailbox: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for creating a new mailbox on the server.")]
struct CreateMailboxArgs {
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(
        description = "Mailbox name to create. Use delimiter (usually '/') for nested mailboxes, e.g. 'Archive/2024'."
    )]
    mailbox_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for downloading message attachments to disk.")]
struct DownloadAttachmentsArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "IMAP UID of the message.")]
    uid: u32,
    #[schemars(description = "Directory to save attachments to. Defaults to current directory.")]
    output_dir: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "Arguments for adding flags and/or setting an Apple Mail color on a message."
)]
struct AddFlagsArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "IMAP UID of the message.")]
    uid: u32,
    #[serde(default)]
    #[schemars(
        description = "Flags to add. System flags use backslash prefix (e.g. \"\\\\Seen\", \"\\\\Flagged\"). Custom keywords are plain strings. Cannot include \\\\Deleted or \\\\Recent."
    )]
    flags: Vec<String>,
    #[schemars(
        description = "Apple Mail color to set (case-insensitive): red, orange, yellow, green, blue, purple, gray. Sets \\\\Flagged + $MailFlagBit keywords. Replaces any existing color."
    )]
    color: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "Arguments for removing flags and/or clearing Apple Mail color from a message."
)]
struct RemoveFlagsArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "IMAP UID of the message.")]
    uid: u32,
    #[serde(default)]
    #[schemars(
        description = "Flags to remove. System flags use backslash prefix (e.g. \"\\\\Seen\"). Cannot include \\\\Deleted or \\\\Recent."
    )]
    flags: Vec<String>,
    #[serde(default = "default_false")]
    #[schemars(
        description = "If true, removes the Apple Mail color flag (\\\\Flagged + all $MailFlagBit keywords). Defaults to false."
    )]
    color: bool,
}

// ---------------------------------------------------------------------------
// Prompt argument structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct InboxSummaryArgs {
    #[schemars(description = "Account name to summarize.")]
    account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CleanupSenderArgs {
    #[schemars(description = "Account name.")]
    account: String,
    #[schemars(description = "Sender email address or name to clean up.")]
    sender: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct FindAttachmentsPromptArgs {
    #[schemars(description = "Account name.")]
    account: String,
    #[schemars(description = "Mailbox to search. Defaults to INBOX.")]
    mailbox: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ComposeEmailArgs {
    #[schemars(description = "Account name to send from.")]
    account: String,
    #[schemars(description = "Recipient email address.")]
    to: Option<String>,
    #[schemars(description = "Email subject line.")]
    subject: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct UnsubscribeCleanupArgs {
    #[schemars(description = "Account name.")]
    account: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ListIdCleanupArgs {
    #[schemars(description = "Account name.")]
    account: String,
}

// ---------------------------------------------------------------------------
// Task Manager
// ---------------------------------------------------------------------------

/// Tools whose `annotations.destructive_hint` is `true`.
/// Destructive tasks targeting the same account are serialized — each waits for
/// the previous destructive task to finish before starting.
const DESTRUCTIVE_TOOLS: &[&str] = &[
    "delete_messages",
    "delete_by_sender",
    "delete_list_id",
    "unsubscribe_message",
];

/// Try to extract the `account` field from a tool call's JSON arguments.
fn extract_account(args: &Option<serde_json::Map<String, serde_json::Value>>) -> Option<String> {
    args.as_ref()?.get("account")?.as_str().map(String::from)
}

struct ManagedTask {
    meta: Task,
    result: Arc<parking_lot::Mutex<Option<Result<CallToolResult, McpError>>>>,
    handle: JoinHandle<()>,
}

struct TaskManager {
    tasks: HashMap<String, ManagedTask>,
    /// Per-account async mutex that serializes destructive tasks.
    /// These must be `tokio::sync::Mutex` because the guard is held across
    /// the entire `call_tool().await` execution.
    /// Non-destructive tasks bypass these locks entirely.
    destructive_locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl TaskManager {
    fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            destructive_locks: HashMap::new(),
        }
    }

    /// Get or create the destructive-task serialization lock for an account.
    fn destructive_lock(&mut self, account: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.destructive_locks
            .entry(account.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Check if a specific task's spawned future has finished and update its
    /// metadata accordingly.
    fn refresh_status(&mut self, task_id: &str) {
        if let Some(managed) = self.tasks.get_mut(task_id) {
            if managed.meta.status != TaskStatus::Working {
                return;
            }
            if managed.handle.is_finished() {
                let now = utc_now_iso8601();
                // Try to determine if it succeeded or failed by checking the
                // result slot synchronously.
                let status = match managed.result.try_lock() {
                    Some(guard) => match guard.as_ref() {
                        Some(Ok(_)) => TaskStatus::Completed,
                        Some(Err(_)) => TaskStatus::Failed,
                        None => {
                            // Handle finished but no result written → aborted/panicked
                            TaskStatus::Failed
                        }
                    },
                    None => {
                        // Lock contention — treat as still completing
                        return;
                    }
                };
                managed.meta.status = status;
                managed.meta.last_updated_at = now;
            }
        }
    }

    /// Refresh the status of all tracked tasks.
    fn refresh_all(&mut self) {
        let ids: Vec<String> = self.tasks.keys().cloned().collect();
        for id in ids {
            self.refresh_status(&id);
        }
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

#[tool_router]
impl AgentMailServer {
    pub fn new(agentmail: crate::Agentmail) -> Self {
        Self {
            agentmail: Arc::new(agentmail),
            task_manager: Arc::new(parking_lot::Mutex::new(TaskManager::new())),
        }
    }

    #[tool(
        name = "list_accounts",
        description = "Return configured IMAP account names. Use this first to discover valid account selectors.",
        annotations(read_only_hint = true)
    )]
    async fn list_accounts_tool(
        &self,
        Parameters(_args): Parameters<ListAccountsArgs>,
    ) -> Result<Json<ListAccountsResponse>, McpError> {
        match self.agentmail.list_accounts().await {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "list_mailboxes",
        description = "List all mailboxes (folders) with message counts: total, unseen, and recent. Shows the full folder tree. Optionally filter to a single account.",
        annotations(read_only_hint = true)
    )]
    async fn list_mailboxes_tool(
        &self,
        Parameters(args): Parameters<ListMailboxesArgs>,
    ) -> Result<Json<ListMailboxesResponse>, McpError> {
        let account = args.account.filter(|s| !s.trim().is_empty());
        match self.agentmail.list_mailboxes(account.as_deref()).await {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "create_mailbox",
        description = "Create a new mailbox (folder) on the IMAP server. Use delimiter (usually '/') for nested mailboxes.",
        annotations(
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
            return Err(McpError::internal_error("mailbox_name is required", None));
        }
        match self
            .agentmail
            .create_mailbox(&args.account, &args.mailbox_name)
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "check_connection",
        description = "Test IMAP connectivity for an account. Connects, authenticates, and reports status.",
        annotations(read_only_hint = true)
    )]
    async fn check_connection_tool(
        &self,
        Parameters(args): Parameters<CheckConnectionArgs>,
    ) -> Result<Json<ConnectionStatus>, McpError> {
        match self.agentmail.check_connection(&args.account).await {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "list_capabilities",
        description = "List IMAP server capabilities for an account. Shows supported extensions like IDLE, MOVE, CONDSTORE, etc.",
        annotations(read_only_hint = true)
    )]
    async fn list_capabilities_tool(
        &self,
        Parameters(args): Parameters<ListCapabilitiesArgs>,
    ) -> Result<Json<ListCapabilitiesResponse>, McpError> {
        match self.agentmail.list_capabilities(&args.account).await {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "get_messages",
        description = "Fetch a paginated list of messages from a mailbox, newest-first. Returns metadata (subject, from, date, flags, UID) by default. Set include_content=true to also get the message body as markdown. Set include_headers=true for the full raw headers map. Defaults: mailbox=INBOX, offset=0, limit=25 (max 50).",
        annotations(read_only_hint = true)
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
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "search_messages",
        description = "Search messages with filters: sender_contains, subject_contains, to_contains, query (full-text), read, flagged, and header key/value. Returns paginated results newest-first. Content excluded by default — set include_content=true to get message bodies. Set include_headers=true for the full raw headers map.",
        annotations(read_only_hint = true)
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
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "list_flags",
        description = "List all IMAP flags in use with counts per flag (e.g. \\Seen: 1234, \\Flagged: 56). Omit mailbox to scan the entire account across all mailboxes. Resolves Apple Mail $MailFlagBit color flags to color names (red, orange, yellow, green, blue, purple, gray).",
        annotations(read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn list_flags_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<ListFlagsArgs>,
    ) -> Result<Json<ListFlagsResponse>, McpError> {
        let progress = make_progress_fn(&meta, &client);
        match self
            .agentmail
            .list_flags(args.mailbox.as_deref(), &args.account, progress.as_ref())
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "delete_messages",
        description = "Delete one or more messages by UID. Moves to Trash if configured, otherwise flags \\Deleted and expunges. Supports up to 500 UIDs per call.",
        annotations(destructive_hint = true, idempotent_hint = true),
        execution(task_support = "optional")
    )]
    async fn delete_messages_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<DeleteMessagesArgs>,
    ) -> Result<Json<DeleteMessagesResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        if args.uids.is_empty() {
            return Err(McpError::internal_error(
                "uids must contain at least one UID",
                None,
            ));
        }
        if args.uids.len() > 500 {
            return Err(McpError::internal_error(
                "uids supports up to 500 UIDs per call",
                None,
            ));
        }

        let progress = make_progress_fn(&meta, &client);
        match self
            .agentmail
            .delete_messages(&mailbox, &args.account, &args.uids, progress.as_ref())
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "delete_by_sender",
        description = "Delete all messages from an exact sender. Takes a UID to identify the sender — extracts the full From header (display name + email) and deletes every message with an identical sender. Set allMailboxes=true to search and delete across the entire account. Ideal for bulk cleanup after rank_senders. For mailing list cleanup, use unsubscribe_message instead — it attempts one-click unsubscribe and only deletes bulk mail.",
        annotations(destructive_hint = true),
        execution(task_support = "optional")
    )]
    async fn delete_by_sender_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<DeleteBySenderArgs>,
    ) -> Result<Json<DeleteBySenderResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());

        let progress = make_progress_fn(&meta, &client);
        match self
            .agentmail
            .delete_by_sender(
                &mailbox,
                &args.account,
                args.uid,
                args.all_mailboxes,
                progress.as_ref(),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "find_attachments",
        description = "Scan for messages with attachments (multipart/mixed or multipart/related). Returns paginated UIDs (newest-first) and total count. Omit mailbox to scan the entire account with a per-mailbox breakdown. Use download_attachments with a specific UID to save files to disk.",
        annotations(read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn find_attachments_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<FindAttachmentsArgs>,
    ) -> Result<Json<FindAttachmentsResponse>, McpError> {
        let offset = crate::clamp_usize(args.offset, 0, 0, 100_000);
        let limit = crate::clamp_usize(args.limit, 25, 1, 100);
        let progress = make_progress_fn(&meta, &client);

        match self
            .agentmail
            .find_attachments(
                args.mailbox.as_deref(),
                &args.account,
                offset,
                limit,
                progress.as_ref(),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "download_attachments",
        description = "Download all attachments from a message to disk. Files are saved as {uid}_{originalname}. Returns file paths, content types, and sizes.",
        annotations(
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
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "create_draft",
        description = "Create and save a draft email. Composes an RFC822 message and appends it to the account's Drafts folder. Requires subject, body, and at least one recipient (to, cc, or bcc).",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn create_draft_tool(
        &self,
        Parameters(args): Parameters<CreateDraftArgs>,
    ) -> Result<Json<CreateDraftResponse>, McpError> {
        if args.to.is_empty() && args.cc.is_empty() && args.bcc.is_empty() {
            return Err(McpError::internal_error(
                "At least one recipient (to, cc, or bcc) is required",
                None,
            ));
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
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "move_message",
        description = "Move a single message from one mailbox to another by UID. Uses IMAP MOVE command. Requires source mailbox, destination mailbox, and the message UID.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn move_message_tool(
        &self,
        Parameters(args): Parameters<MoveMessageArgs>,
    ) -> Result<Json<MoveMessageResponse>, McpError> {
        if args.mailbox.trim().is_empty() {
            return Err(McpError::internal_error("mailbox is required", None));
        }
        if args.destination.trim().is_empty() {
            return Err(McpError::internal_error("destination is required", None));
        }

        match self
            .agentmail
            .move_message(&args.mailbox, &args.account, args.uid, &args.destination)
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "unsubscribe_message",
        description = "Unsubscribe from a mailing list and delete matching messages across ALL mailboxes. Requires the message to have a List-Unsubscribe header. Attempts RFC 8058 one-click unsubscribe POST (best-effort — if it fails, messages are still deleted). When delete_matching is true, searches every mailbox for messages from the exact sender that have a List-Unsubscribe-Post header and deletes them. This ensures only bulk/marketing mail is removed, not legitimate messages from the same sender.",
        annotations(destructive_hint = true, open_world_hint = true)
    )]
    async fn unsubscribe_message_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<UnsubscribeMessageArgs>,
    ) -> Result<Json<UnsubscribeResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        let progress = make_progress_fn(&meta, &client);

        match self
            .agentmail
            .unsubscribe_message(
                &mailbox,
                &args.account,
                args.uid,
                args.delete_matching,
                progress.as_ref(),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "rank_senders",
        description = "Rank all senders by message count. Omit mailbox to scan the entire account across all mailboxes. Groups by (email, display name) — 'Find My <noreply@apple.com>' and 'iCloud <noreply@apple.com>' are separate entries. Sorted by message count descending. Efficient: fetches only FROM+DATE headers using BODY.PEEK.",
        annotations(read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn rank_senders_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<RankSendersArgs>,
    ) -> Result<Json<RankSendersResponse>, McpError> {
        let limit = args.limit.map(|v| v as usize);
        let progress = make_progress_fn(&meta, &client);

        match self
            .agentmail
            .group_by_sender(
                args.mailbox.as_deref(),
                &args.account,
                limit,
                progress.as_ref(),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "rank_unsubscribe",
        description = "Rank bulk-mail senders by message count. Omit mailbox to scan the entire account. Includes messages with either List-Unsubscribe or List-Unsubscribe-Post. Grouped by sender (From), sorted by one-click support first then by count. To clean up a sender, pass the sampleUid and sampleMailbox to unsubscribe_message (not delete_by_sender). Returns count, unsubscribe URL, one-click flag, sample UID + mailbox.",
        annotations(read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn rank_unsubscribe_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<RankUnsubscribeArgs>,
    ) -> Result<Json<RankUnsubscribeResponse>, McpError> {
        let limit = args.limit.map(|v| v as usize);
        let progress = make_progress_fn(&meta, &client);

        match self
            .agentmail
            .group_by_list(
                args.mailbox.as_deref(),
                &args.account,
                limit,
                progress.as_ref(),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "rank_list_id",
        description = "Rank mailing lists by List-Id header (RFC 2919). Groups all messages from the same mailing list regardless of sender address — useful for lists like GitHub notifications where multiple senders share one List-Id. Omit mailbox to scan the entire account. Use delete_list_id to remove all messages from a list.",
        annotations(read_only_hint = true),
        execution(task_support = "optional")
    )]
    async fn rank_list_id_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<RankListIdArgs>,
    ) -> Result<Json<RankListIdResponse>, McpError> {
        let limit = args.limit.map(|v| v as usize);
        let progress = make_progress_fn(&meta, &client);

        match self
            .agentmail
            .group_by_list_id(
                args.mailbox.as_deref(),
                &args.account,
                limit,
                progress.as_ref(),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "delete_list_id",
        description = "Delete all messages with a specific List-Id across all mailboxes. Identifies the list by its List-Id header value (from rank_list_id). Deletes ALL messages from that mailing list regardless of sender address. Omit mailbox to search the entire account.",
        annotations(destructive_hint = true),
        execution(task_support = "optional")
    )]
    async fn delete_list_id_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<DeleteListIdArgs>,
    ) -> Result<Json<DeleteListIdResponse>, McpError> {
        let progress = make_progress_fn(&meta, &client);
        match self
            .agentmail
            .delete_list_id(
                args.mailbox.as_deref(),
                &args.account,
                &args.list_id,
                progress.as_ref(),
            )
            .await
        {
            Ok(data) => Ok(Json(data)),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "add_flags",
        description = "Add flags and/or set an Apple Mail color on a message. Flags use union semantics — existing flags are preserved. Use color for Apple Mail colored flags (red, orange, yellow, green, blue, purple, gray). Cannot set \\Deleted (use delete_messages) or \\Recent (read-only).",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn add_flags_tool(
        &self,
        Parameters(args): Parameters<AddFlagsArgs>,
    ) -> Result<Json<UpdateFlagsResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        if args.flags.is_empty() && args.color.is_none() {
            return Err(McpError::internal_error(
                "At least one flag or a color is required",
                None,
            ));
        }
        // Guard dangerous flags
        for flag in &args.flags {
            let lower = flag.to_lowercase();
            if lower == "\\deleted" {
                return Err(McpError::internal_error(
                    "Cannot set \\Deleted via add_flags — use delete_messages instead",
                    None,
                ));
            }
            if lower == "\\recent" {
                return Err(McpError::internal_error(
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
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "remove_flags",
        description = "Remove flags and/or clear Apple Mail color from a message. Only specified flags are removed; all others preserved. Set color=true to remove the colored flag (\\Flagged + all $MailFlagBit keywords). Cannot remove \\Deleted (use delete_messages) or \\Recent (read-only).",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn remove_flags_tool(
        &self,
        Parameters(args): Parameters<RemoveFlagsArgs>,
    ) -> Result<Json<UpdateFlagsResponse>, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        if args.flags.is_empty() && !args.color {
            return Err(McpError::internal_error(
                "At least one flag or color=true is required",
                None,
            ));
        }
        // Guard dangerous flags
        for flag in &args.flags {
            let lower = flag.to_lowercase();
            if lower == "\\deleted" {
                return Err(McpError::internal_error(
                    "Cannot remove \\Deleted via remove_flags — use delete_messages instead",
                    None,
                ));
            }
            if lower == "\\recent" {
                return Err(McpError::internal_error(
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
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }
}

#[prompt_router]
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
             to save it. Show me a preview of the draft before saving.",
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

#[tool_handler]
#[prompt_handler]
impl ServerHandler for AgentMailServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_tasks()
                .build(),
        )
        .with_instructions(
            "AgentMail is a full-featured IMAP email client. \
             Start with list_accounts to discover configured accounts. \
             Use list_mailboxes to see folder structure and message counts. \
             Read messages with get_messages (paginated, newest-first) or search_messages (with filters). \
             Use search_messages to find specific messages by sender, subject, or content. \
             Manage email: delete_messages, delete_by_sender, delete_list_id, move_message, create_draft, create_mailbox, unsubscribe_message. \
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
             All reads use BODY.PEEK to avoid marking messages as read.",
        )
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
