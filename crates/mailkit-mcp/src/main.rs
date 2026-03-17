use clap::{Parser, Subcommand};
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
    handler::server::{
        router::prompt::PromptRouter, router::tool::ToolRouter, wrapper::Parameters,
    },
    model::{
        CallToolResult, ClientJsonRpcMessage, ClientNotification, ClientRequest, Content,
        GetPromptRequestParams, GetPromptResult, InitializedNotification, JsonRpcMessage,
        ListPromptsResult, Meta, PaginatedRequestParams, ProgressNotificationParam, PromptMessage,
        PromptMessageRole, ProtocolVersion, ServerCapabilities, ServerInfo, ServerResult,
    },
    prompt, prompt_handler, prompt_router,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::worker::{Worker, WorkerContext, WorkerQuitReason},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn tool_success(value: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::structured(value))
}

fn tool_error(message: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![Content::text(message.into())]))
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

/// Build an optional progress callback from MCP meta + peer.
/// Returns `None` if the client didn't provide a progress token.
fn make_progress_fn(meta: &Meta, peer: &Peer<RoleServer>) -> Option<mailkit::ProgressFn> {
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
    #[schemars(description = "Mailbox to scan. Defaults to INBOX.")]
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
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
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
    description = "Arguments for setting a colored flag on a message (Apple Mail compatible)."
)]
struct SetFlagColorArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "IMAP UID of the message to flag.")]
    uid: u32,
    #[schemars(
        description = "Color name: red, orange, yellow, green, blue, purple, gray. Omit or set to null to remove the flag."
    )]
    color: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "Arguments for adding flags to a message (union — existing flags are preserved)."
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
    #[schemars(
        description = "Flags to add. System flags use backslash prefix (e.g. \"\\\\Seen\", \"\\\\Flagged\"). Custom keywords are plain strings (e.g. \"$MailFlagBit0\")."
    )]
    flags: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(description = "Arguments for removing flags from a message.")]
struct RemoveFlagsArgs {
    #[schemars(description = "Mailbox name. Defaults to INBOX when omitted.")]
    mailbox: Option<String>,
    #[schemars(
        description = "Account name (required). Use list_accounts to discover valid names."
    )]
    account: String,
    #[schemars(description = "IMAP UID of the message.")]
    uid: u32,
    #[schemars(
        description = "Flags to remove. System flags use backslash prefix (e.g. \"\\\\Seen\", \"\\\\Flagged\"). Custom keywords are plain strings."
    )]
    flags: Vec<String>,
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

// ---------------------------------------------------------------------------
// CompatStdioWorker — handles JSON-RPC stdio transport with init patching
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum CompatTransportError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Join(tokio::task::JoinError),
    Closed,
}

impl fmt::Display for CompatTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompatTransportError::Io(err) => write!(f, "io error: {}", err),
            CompatTransportError::Json(err) => write!(f, "json error: {}", err),
            CompatTransportError::Join(err) => write!(f, "join error: {}", err),
            CompatTransportError::Closed => write!(f, "transport closed"),
        }
    }
}

impl std::error::Error for CompatTransportError {}

impl From<std::io::Error> for CompatTransportError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for CompatTransportError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

fn merge_client_info_defaults(obj: &mut Map<String, Value>) {
    if !obj.contains_key("name") {
        obj.insert("name".to_string(), Value::String("inspector".to_string()));
    }
    if !obj.contains_key("version") {
        obj.insert("version".to_string(), Value::String("0".to_string()));
    }
}

fn patch_initialize_value(raw: &str) -> Result<Option<Value>, CompatTransportError> {
    let mut value: Value = serde_json::from_str(raw)?;
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if method != "initialize" {
        return Ok(None);
    }

    let root = match value.as_object_mut() {
        Some(obj) => obj,
        None => return Ok(None),
    };

    let params = root
        .entry("params".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let params_obj = match params.as_object_mut() {
        Some(obj) => obj,
        None => return Ok(None),
    };

    if !params_obj.contains_key("protocolVersion") {
        params_obj.insert(
            "protocolVersion".to_string(),
            Value::String("2025-06-18".to_string()),
        );
    }

    if !params_obj.contains_key("capabilities") {
        params_obj.insert("capabilities".to_string(), Value::Object(Map::new()));
    }

    match params_obj.get_mut("clientInfo") {
        Some(client_info) => {
            if let Some(client_obj) = client_info.as_object_mut() {
                merge_client_info_defaults(client_obj);
            } else {
                params_obj.insert(
                    "clientInfo".to_string(),
                    json!({"name":"inspector","version":"0"}),
                );
            }
        }
        None => {
            params_obj.insert(
                "clientInfo".to_string(),
                json!({"name":"inspector","version":"0"}),
            );
        }
    }

    Ok(Some(value))
}

fn parse_client_message(raw: &str) -> Result<ClientJsonRpcMessage, CompatTransportError> {
    let value: Value = serde_json::from_str(raw)?;
    if let Some(method) = value.get("method").and_then(Value::as_str)
        && method == "initialize"
    {
        let patched = patch_initialize_value(raw)?.unwrap_or(value);
        return serde_json::from_value::<ClientJsonRpcMessage>(patched)
            .map_err(CompatTransportError::Json);
    }
    serde_json::from_value::<ClientJsonRpcMessage>(value).map_err(CompatTransportError::Json)
}

#[derive(Clone, Default)]
struct CompatStdioWorker;

impl Worker for CompatStdioWorker {
    type Error = CompatTransportError;
    type Role = RoleServer;

    fn err_closed() -> Self::Error {
        CompatTransportError::Closed
    }

    fn err_join(e: tokio::task::JoinError) -> Self::Error {
        CompatTransportError::Join(e)
    }

    async fn run(
        self,
        mut context: WorkerContext<Self>,
    ) -> Result<(), WorkerQuitReason<Self::Error>> {
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut stdout = tokio::io::stdout();
        let mut line = String::new();
        let mut should_inject_initialized = false;
        let mut hold_inbound_until_initialized = false;
        let mut pending_after_init: VecDeque<ClientJsonRpcMessage> = VecDeque::new();
        let cancel_token = context.cancellation_token.clone();

        loop {
            if !hold_inbound_until_initialized
                && let Some(next_msg) = pending_after_init.pop_front()
            {
                context.send_to_handler(next_msg).await?;
                continue;
            }

            if should_inject_initialized {
                let notif = ClientNotification::InitializedNotification(InitializedNotification {
                    method: Default::default(),
                    extensions: Default::default(),
                });
                context
                    .send_to_handler(JsonRpcMessage::notification(notif))
                    .await?;
                should_inject_initialized = false;
                hold_inbound_until_initialized = false;
                continue;
            }

            tokio::select! {
                _ = cancel_token.cancelled() => {
                    return Err(WorkerQuitReason::Cancelled);
                }
                send_req = context.recv_from_handler() => {
                    let send_req = send_req?;
                    let json_line = serde_json::to_string(&send_req.message).map_err(|e| {
                        WorkerQuitReason::fatal(
                            CompatTransportError::Json(e),
                            "serializing outbound message",
                        )
                    })?;
                    stdout.write_all(json_line.as_bytes()).await.map_err(|e| {
                        WorkerQuitReason::fatal(
                            CompatTransportError::Io(e),
                            "writing outbound message",
                        )
                    })?;
                    stdout.write_all(b"\n").await.map_err(|e| {
                        WorkerQuitReason::fatal(
                            CompatTransportError::Io(e),
                            "writing outbound newline",
                        )
                    })?;
                    stdout.flush().await.map_err(|e| {
                        WorkerQuitReason::fatal(
                            CompatTransportError::Io(e),
                            "flushing outbound message",
                        )
                    })?;

                    if let JsonRpcMessage::Response(resp) = &send_req.message
                        && matches!(resp.result, ServerResult::InitializeResult(_)) {
                            should_inject_initialized = true;
                        }

                    let _ = send_req.responder.send(Ok(()));
                }
                read_result = reader.read_line(&mut line) => {
                    let read = read_result.map_err(|e| {
                        WorkerQuitReason::fatal(
                            CompatTransportError::Io(e),
                            "reading inbound line",
                        )
                    })?;
                    if read == 0 {
                        return Err(WorkerQuitReason::TransportClosed);
                    }

                    let raw = line.trim().to_string();
                    line.clear();
                    if raw.is_empty() {
                        continue;
                    }

                    let inbound = parse_client_message(&raw).map_err(WorkerQuitReason::fatal_context("parsing inbound message"))?;
                    let is_initialize_request = matches!(
                        &inbound,
                        JsonRpcMessage::Request(req) if matches!(req.request, ClientRequest::InitializeRequest(_))
                    );

                    if is_initialize_request {
                        hold_inbound_until_initialized = true;
                        context.send_to_handler(inbound).await?;
                        continue;
                    }

                    if hold_inbound_until_initialized {
                        pending_after_init.push_back(inbound);
                    } else {
                        context.send_to_handler(inbound).await?;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MailKitServer {
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
    mailkit: Arc<mailkit::Mailkit>,
}

#[tool_router]
impl MailKitServer {
    fn new(mailkit: mailkit::Mailkit) -> Self {
        Self {
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
            mailkit: Arc::new(mailkit),
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
    ) -> Result<CallToolResult, McpError> {
        match self.mailkit.list_accounts().await {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
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
    ) -> Result<CallToolResult, McpError> {
        let account = args.account.filter(|s| !s.trim().is_empty());
        match self.mailkit.list_mailboxes(account.as_deref()).await {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
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
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox_name.trim().is_empty() {
            return tool_error("mailbox_name is required");
        }
        match self
            .mailkit
            .create_mailbox(&args.account, &args.mailbox_name)
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
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
    ) -> Result<CallToolResult, McpError> {
        match self.mailkit.check_connection(&args.account).await {
            Ok(status) => tool_success(serde_json::to_value(status).unwrap_or_default()),
            Err(e) => tool_error(e.to_string()),
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
    ) -> Result<CallToolResult, McpError> {
        match self.mailkit.list_capabilities(&args.account).await {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
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
    ) -> Result<CallToolResult, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        let offset = mailkit::clamp_usize(args.offset, 0, 0, 1_000_000);
        let limit = mailkit::clamp_usize(args.limit, 25, 1, 50);

        match self
            .mailkit
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
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
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
    ) -> Result<CallToolResult, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        let offset = mailkit::clamp_usize(args.offset, 0, 0, 1_000_000);
        let limit = mailkit::clamp_usize(args.limit, 25, 1, 50);

        let criteria = mailkit::SearchCriteria {
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
            .mailkit
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
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
        }
    }

    #[tool(
        name = "list_flags",
        description = "List all IMAP flags in use across messages in a mailbox with counts per flag (e.g. \\Seen: 1234, \\Flagged: 56). Scans all messages. Useful for understanding mailbox state.",
        annotations(read_only_hint = true)
    )]
    async fn list_flags_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<ListFlagsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mailbox = args.mailbox.as_deref().unwrap_or("INBOX");
        let progress = make_progress_fn(&meta, &client);
        match self
            .mailkit
            .list_flags(mailbox, &args.account, progress.as_ref())
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
        }
    }

    #[tool(
        name = "delete_messages",
        description = "Delete one or more messages by UID. Moves to Trash if configured, otherwise flags \\Deleted and expunges. Supports up to 500 UIDs per call.",
        annotations(destructive_hint = true, idempotent_hint = true)
    )]
    async fn delete_messages_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<DeleteMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        if args.uids.is_empty() {
            return tool_error("uids must contain at least one UID");
        }
        if args.uids.len() > 500 {
            return tool_error("uids supports up to 500 UIDs per call");
        }

        let progress = make_progress_fn(&meta, &client);
        match self
            .mailkit
            .delete_messages(&mailbox, &args.account, &args.uids, progress.as_ref())
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
        }
    }

    #[tool(
        name = "delete_by_sender",
        description = "Delete all messages from an exact sender. Takes a UID to identify the sender — extracts the full From header (display name + email) and deletes every message with an identical sender. 'Dutch Bros Coffee <member@rewards.dutchbros.com>' and 'member@rewards.dutchbros.com' are treated as different senders. Set allMailboxes=true to search and delete across the entire account. Ideal for bulk cleanup after rank_senders.",
        annotations(destructive_hint = true)
    )]
    async fn delete_by_sender_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<DeleteBySenderArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());

        let progress = make_progress_fn(&meta, &client);
        match self
            .mailkit
            .delete_by_sender(
                &mailbox,
                &args.account,
                args.uid,
                args.all_mailboxes,
                progress.as_ref(),
            )
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
        }
    }

    #[tool(
        name = "find_attachments",
        description = "Scan a mailbox for messages that have attachments. Returns paginated UIDs (newest-first) and total count. Use download_attachments with a specific UID to save files to disk.",
        annotations(read_only_hint = true)
    )]
    async fn find_attachments_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<FindAttachmentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        let offset = mailkit::clamp_usize(args.offset, 0, 0, 100_000);
        let limit = mailkit::clamp_usize(args.limit, 25, 1, 100);
        let progress = make_progress_fn(&meta, &client);

        match self
            .mailkit
            .find_attachments(&mailbox, &args.account, offset, limit, progress.as_ref())
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
        }
    }

    #[tool(
        name = "download_attachments",
        description = "Download all attachments from a message to disk. Files are saved as {uid}_{originalname}. Returns file paths, content types, and sizes.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn download_attachments_tool(
        &self,
        Parameters(args): Parameters<DownloadAttachmentsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        let output_dir = args
            .output_dir
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

        match self
            .mailkit
            .download_attachments(&mailbox, &args.account, args.uid, &output_dir)
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
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
    ) -> Result<CallToolResult, McpError> {
        match self
            .mailkit
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
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
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
    ) -> Result<CallToolResult, McpError> {
        if args.mailbox.trim().is_empty() {
            return tool_error("mailbox is required");
        }
        if args.destination.trim().is_empty() {
            return tool_error("destination is required");
        }

        match self
            .mailkit
            .move_message(&args.mailbox, &args.account, args.uid, &args.destination)
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
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
    ) -> Result<CallToolResult, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        let progress = make_progress_fn(&meta, &client);

        match self
            .mailkit
            .unsubscribe_message(
                &mailbox,
                &args.account,
                args.uid,
                args.delete_matching,
                progress.as_ref(),
            )
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
        }
    }

    #[tool(
        name = "rank_senders",
        description = "Rank all senders by message count. Omit mailbox to scan the entire account across all mailboxes. Returns each unique sender with message count, display name, and date range (oldest/newest). Sorted by message count descending. Efficient: fetches only FROM+DATE headers using BODY.PEEK.",
        annotations(read_only_hint = true)
    )]
    async fn rank_senders_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<RankSendersArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.map(|v| v as usize);
        let progress = make_progress_fn(&meta, &client);

        match self
            .mailkit
            .group_by_sender(
                args.mailbox.as_deref(),
                &args.account,
                limit,
                progress.as_ref(),
            )
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
        }
    }

    #[tool(
        name = "rank_unsubscribe",
        description = "Rank bulk-mail senders by message count. Omit mailbox to scan the entire account. Includes messages with either List-Unsubscribe or List-Unsubscribe-Post. Grouped by sender (From), sorted by one-click support first then by count. The unsubscribe URL is taken from the newest message per sender. Deletion via unsubscribe_message matches by exact sender + List-Unsubscribe-Post. Returns count, unsubscribe URL, List-Unsubscribe-Post, List-Id, sample UID + mailbox.",
        annotations(read_only_hint = true)
    )]
    async fn rank_unsubscribe_tool(
        &self,
        meta: Meta,
        client: Peer<RoleServer>,
        Parameters(args): Parameters<RankUnsubscribeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.map(|v| v as usize);
        let progress = make_progress_fn(&meta, &client);

        match self
            .mailkit
            .group_by_list(
                args.mailbox.as_deref(),
                &args.account,
                limit,
                progress.as_ref(),
            )
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
        }
    }

    #[tool(
        name = "set_flag_color",
        description = "Set or remove a colored flag on a message (Apple Mail compatible). Colors: red, orange, yellow, green, blue, purple, gray. Sets \\Flagged plus $MailFlagBit0-2 keywords per RFC draft-eggert-mailflagcolors. Existing non-flag keywords are preserved (union semantics). Omit color to unflag.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn set_flag_color_tool(
        &self,
        Parameters(args): Parameters<SetFlagColorArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        match self
            .mailkit
            .set_flag_color(&mailbox, &args.account, args.uid, args.color.as_deref())
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
        }
    }

    #[tool(
        name = "add_flags",
        description = "Add flags to a message using union semantics — existing flags are preserved. Use for system flags (\\Seen, \\Flagged, \\Answered) or custom keywords.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn add_flags_tool(
        &self,
        Parameters(args): Parameters<AddFlagsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        if args.flags.is_empty() {
            return tool_error("At least one flag is required");
        }
        match self
            .mailkit
            .add_flags(&mailbox, &args.account, args.uid, &args.flags)
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
        }
    }

    #[tool(
        name = "remove_flags",
        description = "Remove specific flags from a message. Use for system flags (\\Seen, \\Flagged) or custom keywords. Only the specified flags are removed; all others are preserved.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn remove_flags_tool(
        &self,
        Parameters(args): Parameters<RemoveFlagsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mailbox = args.mailbox.unwrap_or_else(|| "INBOX".to_string());
        if args.flags.is_empty() {
            return tool_error("At least one flag is required");
        }
        match self
            .mailkit
            .remove_flags(&mailbox, &args.account, args.uid, &args.flags)
            .await
        {
            Ok(value) => tool_success(value),
            Err(e) => tool_error(e.to_string()),
        }
    }
}

#[prompt_router]
impl MailKitServer {
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
                 and delete_matching=true. Deletion matches by exact sender name + List-Unsubscribe-Post \
                 header to ensure only bulk mail is removed. The unsubscribe POST is best-effort — \
                 if it fails, the messages are still deleted across all mailboxes.",
                params.0.account
            ),
        )]
    }
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for MailKitServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2025_06_18)
        .with_instructions(
            "MailKit is a full-featured IMAP email client. \
             Start with list_accounts to discover configured accounts. \
             Use list_mailboxes to see folder structure and message counts. \
             Read messages with get_messages (paginated, newest-first) or search_messages (with filters). \
             Use search_messages to find specific messages by sender, subject, or content. \
             Manage email: delete_messages, delete_by_sender, move_message, create_draft, create_mailbox, unsubscribe_message. \
             rank_senders and rank_unsubscribe accept an optional mailbox — omit it to scan the entire account across all mailboxes. \
             rank_unsubscribe includes messages with either List-Unsubscribe or List-Unsubscribe-Post, grouped by sender. \
             rank_unsubscribe uses the newest message's URL per sender for the most valid one-click unsubscribe. \
             rank_unsubscribe returns sample UIDs + mailboxes that can be passed directly to unsubscribe_message. \
             delete_by_sender supports allMailboxes=true to delete a sender's messages across all mailboxes. \
             unsubscribe_message deletes by matching sender + List-Unsubscribe-Post header when delete_matching=true; the unsubscribe POST is best-effort and never blocks deletion. \
             find_attachments scans for messages with attachments; download_attachments saves them to disk. \
             All reads use BODY.PEEK to avoid marking messages as read.",
        )
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "mailkit", about = "IMAP email client and MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Start the MCP server (JSON-RPC over stdio)
    Serve,
    /// List configured accounts
    ListAccounts,
    /// List mailboxes for an account
    ListMailboxes {
        #[arg(long)]
        account: Option<String>,
    },
    /// Create a new mailbox on the server
    CreateMailbox {
        #[arg(long)]
        account: String,
        #[arg(long)]
        name: String,
    },
    /// Check IMAP connection for an account
    CheckConnection {
        #[arg(long)]
        account: String,
    },
    /// List IMAP server capabilities for an account
    ListCapabilities {
        #[arg(long)]
        account: String,
    },
    /// Store a password in the system keychain for an account
    SetPassword {
        #[arg(long)]
        account: String,
    },
    /// Interactively configure a new IMAP account
    Configure {
        /// Provider preset: gmail, icloud, outlook, fastmail, or omit for custom
        provider: Option<String>,
    },
    /// List all flags in use across messages in a mailbox
    ListFlags {
        #[arg(long)]
        account: String,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
    },
    /// Rank senders by message count (omit --mailbox to scan all mailboxes)
    RankSenders {
        #[arg(long)]
        account: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Rank bulk-mail senders by List-Unsubscribe-Post presence, then count (omit --mailbox to scan all)
    RankUnsubscribe {
        #[arg(long)]
        account: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Find messages with attachments
    FindAttachments {
        #[arg(long)]
        account: String,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long, default_value = "0")]
        offset: usize,
        #[arg(long, default_value = "25")]
        limit: usize,
    },
    /// Download attachments from a message
    DownloadAttachments {
        #[arg(long)]
        account: String,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long)]
        uid: u32,
        #[arg(long, default_value = ".")]
        output_dir: String,
    },
    /// Fetch messages from a mailbox (for testing)
    GetMessages {
        #[arg(long)]
        account: String,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long, default_value = "0")]
        offset: usize,
        #[arg(long, default_value = "10")]
        limit: usize,
        /// Include normalized markdown body content
        #[arg(long)]
        include_content: bool,
        /// Include the full raw headers map
        #[arg(long)]
        include_headers: bool,
    },
    /// Fetch specific messages by UID
    GetMessagesByUid {
        #[arg(long)]
        account: String,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long, num_args = 1..)]
        uids: Vec<u32>,
        #[arg(long, default_value = "false")]
        include_content: bool,
    },
    /// Set a colored flag on a message (Apple Mail compatible)
    SetFlagColor {
        #[arg(long)]
        account: String,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long)]
        uid: u32,
        /// Color: red, orange, yellow, green, blue, purple, gray. Omit to unflag.
        #[arg(long)]
        color: Option<String>,
    },
    /// Create a draft email
    CreateDraft {
        #[arg(long)]
        account: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long, num_args = 1..)]
        to: Vec<String>,
        #[arg(long, num_args = 0..)]
        cc: Vec<String>,
        #[arg(long, num_args = 0..)]
        bcc: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing — logs to stderr so MCP JSON-RPC on stdout is unaffected.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    mailkit::credentials::init_keyring();
    let cli = Cli::parse();

    match cli.command.unwrap_or(CliCommand::Serve) {
        CliCommand::Serve => {
            let mk = mailkit::Mailkit::from_default_config().map_err(|e| {
                eprintln!("mailkit: failed to load config: {}", e);
                e
            })?;

            // Pre-warm: validate credentials and open one connection per account.
            // This surfaces keychain dialogs or credential errors at startup,
            // before the MCP server accepts tool calls.
            for account in mk.account_names() {
                match mk.check_connection(&account).await {
                    Ok(status) if status.connected => {
                        eprintln!("mailkit: {} connected", account);
                    }
                    Ok(status) => {
                        eprintln!(
                            "mailkit: {} connection failed: {}",
                            account,
                            status.error.as_deref().unwrap_or("unknown")
                        );
                    }
                    Err(e) => {
                        eprintln!("mailkit: {} credential error: {}", account, e);
                    }
                }
            }

            let server = MailKitServer::new(mk);
            let worker = CompatStdioWorker;
            let service = server.serve(worker).await.inspect_err(|e| {
                eprintln!("mailkit: server error: {}", e);
            })?;
            service.waiting().await?;
            Ok(())
        }
        CliCommand::ListAccounts => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk.list_accounts().await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::ListMailboxes { account } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk.list_mailboxes(account.as_deref()).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::CreateMailbox { account, name } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk.create_mailbox(&account, &name).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::CheckConnection { account } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let status = mk.check_connection(&account).await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        }
        CliCommand::ListCapabilities { account } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk.list_capabilities(&account).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::SetPassword { account } => {
            let config = mailkit::Config::load()?;
            let acct_config = config
                .accounts
                .get(&account)
                .ok_or_else(|| format!("Account '{}' not found in config", account))?;

            eprint!(
                "Enter password for {} ({}): ",
                account, acct_config.username
            );
            let mut password = String::new();
            std::io::stdin().read_line(&mut password)?;
            let password = password.trim();

            mailkit::credentials::set_password(&account, acct_config, password).await?;
            eprintln!("Password stored successfully.");
            Ok(())
        }
        CliCommand::Configure { provider } => configure_account(provider.as_deref()).await,
        CliCommand::ListFlags { account, mailbox } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk.list_flags(&mailbox, &account, None).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::DownloadAttachments {
            account,
            mailbox,
            uid,
            output_dir,
        } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk
                .download_attachments(&mailbox, &account, uid, std::path::Path::new(&output_dir))
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::FindAttachments {
            account,
            mailbox,
            offset,
            limit,
        } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk
                .find_attachments(&mailbox, &account, offset, limit, None)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::RankSenders {
            account,
            mailbox,
            limit,
        } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk
                .group_by_sender(mailbox.as_deref(), &account, limit, None)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::RankUnsubscribe {
            account,
            mailbox,
            limit,
        } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk
                .group_by_list(mailbox.as_deref(), &account, limit, None)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::GetMessages {
            account,
            mailbox,
            offset,
            limit,
            include_content,
            include_headers,
        } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk
                .get_messages(
                    &mailbox,
                    &account,
                    offset,
                    limit,
                    include_content,
                    include_headers,
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::GetMessagesByUid {
            account,
            mailbox,
            uids,
            include_content,
        } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk
                .get_messages_by_uid(&mailbox, &account, &uids, include_content, false)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::SetFlagColor {
            account,
            mailbox,
            uid,
            color,
        } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk
                .set_flag_color(&mailbox, &account, uid, color.as_deref())
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::CreateDraft {
            account,
            subject,
            body,
            to,
            cc,
            bcc,
        } => {
            let mk = mailkit::Mailkit::from_default_config()?;
            let value = mk
                .create_draft(&account, &subject, &body, &to, &cc, &bcc)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Interactive account configuration
// ---------------------------------------------------------------------------

fn prompt(label: &str) -> Result<String, Box<dyn std::error::Error>> {
    eprint!("{}", label);
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

fn prompt_default(label: &str, default: &str) -> Result<String, Box<dyn std::error::Error>> {
    eprint!("{} [{}]: ", label, default);
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    let val = buf.trim();
    if val.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(val.to_string())
    }
}

struct ProviderPreset {
    host: &'static str,
    port: u16,
    username_hint: &'static str,
}

fn provider_preset(name: &str) -> Option<ProviderPreset> {
    match name {
        "gmail" => Some(ProviderPreset {
            host: "imap.gmail.com",
            port: 993,
            username_hint: "you@gmail.com",
        }),
        "icloud" => Some(ProviderPreset {
            host: "imap.mail.me.com",
            port: 993,
            username_hint: "your iCloud username (not full email)",
        }),
        "outlook" | "hotmail" | "live" => Some(ProviderPreset {
            host: "outlook.office365.com",
            port: 993,
            username_hint: "you@outlook.com",
        }),
        "fastmail" => Some(ProviderPreset {
            host: "imap.fastmail.com",
            port: 993,
            username_hint: "you@fastmail.com",
        }),
        "yahoo" => Some(ProviderPreset {
            host: "imap.mail.yahoo.com",
            port: 993,
            username_hint: "you@yahoo.com",
        }),
        _ => None,
    }
}

async fn configure_account(provider: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("mailkit account setup\n");

    // 1. Resolve provider
    let provider_name = match provider {
        Some(p) => p.to_lowercase(),
        None => {
            let val = prompt("Provider (gmail, icloud, outlook, fastmail, yahoo, custom): ")?;
            val.to_lowercase()
        }
    };
    let preset = provider_preset(&provider_name);

    // 2. Account name
    let default_name = if preset.is_some() {
        provider_name.clone()
    } else {
        String::new()
    };
    let account_name = if default_name.is_empty() {
        prompt("Account name: ")?
    } else {
        prompt_default("Account name", &default_name)?
    };
    if account_name.is_empty() {
        return Err("Account name cannot be empty".into());
    }

    // 3. Host / port / username
    let (host, port, username) = if let Some(ref p) = preset {
        let host = prompt_default("IMAP host", p.host)?;
        let port_str = prompt_default("IMAP port", &p.port.to_string())?;
        let port: u16 = port_str.parse().unwrap_or(p.port);
        eprintln!("  (hint: {})", p.username_hint);
        let username = prompt("Username: ")?;
        (host, port, username)
    } else {
        let host = prompt("IMAP host: ")?;
        let port_str = prompt_default("IMAP port", "993")?;
        let port: u16 = port_str.parse().unwrap_or(993);
        let username = prompt("Username: ")?;
        (host, port, username)
    };
    if host.is_empty() || username.is_empty() {
        return Err("Host and username are required".into());
    }

    // 4. Password method
    eprintln!("\nPassword storage:");
    eprintln!("  1. keyring  - Store in system keychain (recommended)");
    eprintln!("  2. command  - Read from a shell command at runtime");
    eprintln!("  3. raw      - Store in config file (not recommended)");
    let method = prompt_default("Method", "keyring")?;

    let (password_toml, need_store_password) = match method.as_str() {
        "command" | "cmd" | "2" => {
            let default_cmd = format!(
                "security find-internet-password -s {} -a {} -w",
                host, username
            );
            eprintln!("  (hint: use the default to read Apple Mail's stored password)");
            let cmd = prompt_default("Command", &default_cmd)?;
            (format!("password.cmd = {:?}", cmd), false)
        }
        "raw" | "3" => {
            let pw = prompt("Password: ")?;
            (format!("password.raw = {:?}", pw), false)
        }
        _ => {
            // keyring (default)
            (format!("password.keyring = {:?}", username), true)
        }
    };

    // 5. Write config file
    let config_path = mailkit::Config::default_path();
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Check if account already exists
    if config_path.exists() {
        let content = std::fs::read_to_string(&config_path)?;
        let key = format!("[accounts.{}]", account_name);
        if content.contains(&key) {
            return Err(format!(
                "Account '{}' already exists in {}. Edit the file directly to modify it.",
                account_name,
                config_path.display()
            )
            .into());
        }
    }

    let mut section = format!("\n[accounts.{}]\n", account_name);
    section.push_str(&format!("host = {:?}\n", host));
    if port != 993 {
        section.push_str(&format!("port = {}\n", port));
    }
    section.push_str(&format!("username = {:?}\n", username));
    section.push_str(&format!("{}\n", password_toml));

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config_path)?;
    file.write_all(section.as_bytes())?;
    eprintln!(
        "\nWrote account '{}' to {}",
        account_name,
        config_path.display()
    );

    // 6. Store password in keyring if needed
    if need_store_password {
        eprint!("Enter password for {} ({}): ", account_name, username);
        let mut pw = String::new();
        std::io::stdin().read_line(&mut pw)?;
        let pw = pw.trim();

        let entry =
            secret::keyring::KeyringEntry::try_new(&username).map_err(|e| format!("{}", e))?;
        let mut secret = secret::Secret::new_keyring_entry(entry);
        secret
            .set(pw)
            .await
            .map_err(|e| format!("Failed to store password: {}", e))?;
        eprintln!("Password stored in system keychain.");
    }

    // 7. Test connection
    let test = prompt_default("\nTest connection?", "y")?;
    if test.starts_with('y') || test.starts_with('Y') {
        eprintln!("Connecting to {}:{}...", host, port);
        let config = mailkit::Config::load()?;
        let mk = mailkit::Mailkit::new(config);
        let status = mk.check_connection(&account_name).await?;
        if status.connected {
            eprintln!("Connected successfully!");
        } else {
            eprintln!(
                "Connection failed: {}",
                status.error.as_deref().unwrap_or("unknown error")
            );
        }
    }

    Ok(())
}
