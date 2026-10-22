//! MCP server: tools, prompts, resources and completions over stdio or
//! in-process transports.

mod args;
mod file_access;
mod prompts;
mod resources;
mod tasks;
mod tools_read;
mod tools_write;
mod wire;

use futures::StreamExt as _;
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext},
    model::{
        CallToolRequestParams, CallToolResponse, CancelTaskParams, CompleteRequestParams,
        CompleteResult, DiscoverRequestMethod, DiscoverResult, GetTaskParams, GetTaskResult,
        Implementation, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
        ProgressNotificationParam, ProtocolVersion, ReadResourceRequestParams, RequestMetaObject,
        ServerCapabilities, ServerConfig, UpdateTaskParams,
    },
    prompt_handler,
    service::RequestContext,
    tool_handler,
};
use std::sync::Arc;

/// The newest MCP revision this server speaks. A 2025-era client still opens
/// with `initialize`, answered in [`ProtocolVersion::LATEST_WITH_INITIALIZE`]
/// (2025-11-25); a 2026-07-28 client runs that revision's stateless lifecycle,
/// carrying its version, identity and capabilities in every request's `_meta`.
/// Named rather than taken from rmcp's `LATEST`, so a newer SDK does not move
/// the protocol on its own.
const MAX_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2026_07_28;

const PREWARM_CONCURRENCY: usize = 3;
const PREWARM_ACCOUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// The session's PRIMARY workspace root, as a JSON string.
pub const WORKSPACE_ROOT_META_KEY: &str = "io.agentmuse/workspaceRoot";
/// The Project's OTHER bound directories, as a JSON array of strings.
///
/// A separate key rather than widening [`WORKSPACE_ROOT_META_KEY`] to
/// string-or-array: the primary root is load-bearing on its own (relative
/// paths join it, an omitted `outputDir` lands in it), so it keeps a field
/// whose type says there is exactly one. Absent or empty means what it always
/// meant — one root — so an older app, or the standalone server, is unchanged.
pub const ADDITIONAL_ROOTS_META_KEY: &str = "io.agentmuse/additionalWorkspaceRoots";

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
///
/// A call that became a task ([`tasks`]) reports on the task instead: its
/// request has been answered, so a `notifications/progress` on that request's
/// token would describe a request that no longer exists.
fn make_progress_fn(meta: &RequestMetaObject, peer: &Peer<RoleServer>) -> ProgressReporter {
    let run = tasks::ToolRun::current();
    let token = meta.get_progress_token();
    if token.is_none() && run.is_none() {
        return ProgressReporter::disabled();
    }
    let (sender, mut receiver) = tokio::sync::watch::channel(None::<ProgressUpdate>);
    let latest = Arc::new(parking_lot::Mutex::new(None::<ProgressUpdate>));
    let callback_latest = Arc::clone(&latest);
    let callback_run = run.clone();
    let callback = Arc::new(move |completed: u64, total: u64| {
        let mut previous = callback_latest.lock();
        let Some(update) = normalized_progress(*previous, completed, total) else {
            return;
        };
        *previous = Some(update);
        if callback_run
            .as_ref()
            .is_some_and(|run| run.report_progress(update.completed, update.total))
        {
            return;
        }
        sender.send_replace(Some(update));
    });
    let worker = token.map(|token| {
        let peer = peer.clone();
        tokio::spawn(async move {
            while receiver.changed().await.is_ok() {
                let Some(update) = *receiver.borrow_and_update() else {
                    continue;
                };
                if run.as_ref().is_some_and(|run| run.is_promoted()) {
                    break;
                }
                let notification =
                    ProgressNotificationParam::new(token.clone(), update.completed as f64)
                        .with_total(update.total as f64);
                if peer.notify_progress(notification).await.is_err() {
                    break;
                }
            }
        })
    });
    ProgressReporter {
        callback: Some(callback),
        worker,
    }
}

/// Build a `CancelFn` from the request's cancellation token. Fires when the
/// client sends `notifications/cancelled` for this request, or on transport
/// shutdown (the token is a child of the serve-loop token). Long scans check
/// it at mailbox and fetch-chunk boundaries.
///
/// In a call that may become a task the token is the operation's own (see
/// [`tasks`]), and a check that finds it cancelled is recorded, so a task the
/// tool stopped for `tasks/cancel` settles as `cancelled`.
fn make_cancel_fn(ct: tokio_util::sync::CancellationToken) -> crate::CancelFn {
    let run = tasks::ToolRun::current();
    Arc::new(move || {
        let cancelled = ct.is_cancelled();
        if cancelled && let Some(run) = &run {
            run.note_cancel_observed();
        }
        cancelled
    })
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
        | E::ImapTimeout { .. }
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
    /// Sandbox for LLM-supplied filesystem paths (attachment reads, download
    /// writes). See [`file_access::FileAccessPolicy`].
    file_access: Option<file_access::FileAccessPolicy>,
    /// This connection's tasks (SEP-2663). See [`tasks`].
    tasks: Arc<tasks::TaskRuntime>,
}

impl AgentMailServer {
    pub fn new(agentmail: crate::Agentmail) -> Self {
        Self {
            agentmail: Arc::new(agentmail),
            file_access: Some(file_access::FileAccessPolicy::from_env()),
            tasks: Arc::new(tasks::TaskRuntime::new(tasks::TaskConfig::DEFAULT)),
        }
    }

    fn new_embedded(agentmail: crate::Agentmail) -> Self {
        Self::new_embedded_with(agentmail, tasks::TaskConfig::DEFAULT)
    }

    fn new_embedded_with(agentmail: crate::Agentmail, tasks: tasks::TaskConfig) -> Self {
        Self {
            agentmail: Arc::new(agentmail),
            file_access: None,
            tasks: Arc::new(tasks::TaskRuntime::new(tasks)),
        }
    }

    fn file_access_for_request(
        &self,
        meta: &RequestMetaObject,
    ) -> Result<file_access::FileAccessPolicy, McpError> {
        if let Some(policy) = &self.file_access {
            return Ok(policy.clone());
        }
        let root = meta
            .get(WORKSPACE_ROOT_META_KEY)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|root| !root.is_empty())
            .ok_or_else(|| {
                McpError::invalid_params(
                    "this embedded AgentMail file operation requires an active session workspace",
                    None,
                )
            })?;
        let root = std::path::Path::new(root);
        if !root.is_absolute() {
            return Err(McpError::invalid_params(
                "the active session workspace must be an absolute path",
                None,
            ));
        }
        // The Project's other bound directories, if the app sent any. Each is
        // filtered to an absolute path by `with_roots`; a malformed entry is
        // dropped rather than failing the call, because the primary root
        // alone is still a working session.
        let additional: Vec<std::path::PathBuf> = meta
            .get(ADDITIONAL_ROOTS_META_KEY)
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(std::path::PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        Ok(file_access::FileAccessPolicy::with_roots(root, additional))
    }

    /// Combined tool router. Built ONCE, into [`TOOL_ROUTER`]: `#[tool_handler]`
    /// evaluates its router expression on every `call_tool`, `list_tools` and
    /// `get_tool`, so calling this there rebuilt all 36 routes per request (and,
    /// on each runtime thread's first request, every schema — rmcp caches them
    /// per thread). The regression tests call it directly.
    fn tool_router() -> ToolRouter<Self> {
        Self::read_tools_router() + Self::write_tools_router()
    }
}

/// The one router every request dispatches through — see
/// [`AgentMailServer::tool_router`]. `list_tools` clones its tools and patches
/// the live accounts into the copies (`Arc::make_mut` copies a shared schema on
/// write), so the shared definitions are never edited.
static TOOL_ROUTER: std::sync::LazyLock<ToolRouter<AgentMailServer>> =
    std::sync::LazyLock::new(AgentMailServer::tool_router);

/// Constrain every `account` argument in a tool's input schema to the accounts
/// this server actually has.
///
/// One level deep on purpose: `account` is a top-level string on every tool
/// that takes one, and a blind recursive walk would also rewrite an `account`
/// field that happened to appear inside some future nested object with a
/// different meaning.
fn patch_account_enum(schema: &mut serde_json::Map<String, serde_json::Value>, allowed: &[String]) {
    let Some(serde_json::Value::Object(account)) = schema
        .get_mut("properties")
        .and_then(|properties| properties.as_object_mut())
        .and_then(|properties| properties.get_mut("account"))
    else {
        return;
    };
    account.insert(
        "enum".to_string(),
        serde_json::Value::Array(
            allowed
                .iter()
                .map(|name| serde_json::Value::String(name.clone()))
                .collect(),
        ),
    );
}

#[tool_handler(router = TOOL_ROUTER)]
#[prompt_handler]
impl ServerHandler for AgentMailServer {
    /// The tool list, with the LIVE account names patched into every `account`
    /// argument as a JSON Schema `enum`.
    ///
    /// The configured accounts are the one thing about this server's schema
    /// that cannot be known at compile time, and they are also the first thing
    /// every tool needs. Without them a model has no way to learn a valid
    /// selector except to call `list_accounts` — which is what agents did, once
    /// per session, before touching mail, even though the accounts were already
    /// visible as `email://{account}` resources. A resource an agent must read
    /// and interpret is not the same affordance as a value in the schema of the
    /// argument that needs it. (Same technique the bridge uses for `subagent`'s
    /// runtime `composer_id` enum — RULES §8b.)
    ///
    /// Skipped when no account is configured: an empty `enum` is a parameter
    /// nothing can satisfy, which is worse than an unconstrained string.
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, McpError> {
        let mut tools = TOOL_ROUTER.list_all();
        let accounts = self.agentmail.account_names();
        if !accounts.is_empty() {
            for tool in &mut tools {
                let schema = std::sync::Arc::make_mut(&mut tool.input_schema);
                patch_account_enum(schema, &accounts);
            }
        }
        Ok(rmcp::model::ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
            ..Default::default()
        })
    }

    /// A call to one of `tasks::TASK_TOOLS` from a client that declared the
    /// tasks extension may become a task once it has run for a while (see
    /// `tasks`). Every other call is dispatched exactly as the generated
    /// handler did.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        mut context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let tasks_declared = context
            .client_capabilities()
            .is_some_and(|capabilities| capabilities.supports_tasks());
        if !(tasks_declared && tasks::TASK_TOOLS.contains(&request.name.as_ref())) {
            return TOOL_ROUTER
                .call(ToolCallContext::new(self, request, context))
                .await;
        }
        let operation_ct = self.tasks.operation_token();
        let request_ct = std::mem::replace(&mut context.ct, operation_ct.clone());
        let tool = request.name.clone();
        let server = self.clone();
        self.tasks
            .run_call(&tool, request_ct, operation_ct, async move {
                TOOL_ROUTER
                    .call(ToolCallContext::new(&server, request, context))
                    .await
            })
            .await
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        self.tasks.get(&request.task_id)
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.tasks.update(&request.task_id, request.input_responses)
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.tasks.cancel(&request.task_id)
    }

    /// Every revision up to `MAX_PROTOCOL_VERSION` and nothing newer, so a
    /// newer rmcp cannot widen what this server claims. This one list bounds
    /// `server/discover`, the `initialize` negotiation and the version a
    /// request names in its own `_meta`.
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        std::borrow::Cow::Borrowed(ProtocolVersion::known_up_to(&MAX_PROTOCOL_VERSION))
    }

    /// `server/discover` opens the 2026-07-28 lifecycle, so it is answered
    /// only at a revision without a handshake. A probe at a 2025 version is
    /// told the method does not exist, which is how a 2025-11-25 server
    /// answers it: rmcp's default would answer and let the client run the
    /// stateless lifecycle at a revision that defines none. (A version this
    /// server does not speak at all never gets here: rmcp refuses it with
    /// `-32022` first.)
    ///
    /// `supportedVersions` lists every revision this server implements, the
    /// same list a `-32022` carries: a client must be able to read the two
    /// answers as one fact (the official suite's `sep-2575` checks hold
    /// them to it).
    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, McpError> {
        if context
            .meta
            .protocol_version()
            .is_none_or(|version| version.has_initialize())
        {
            return Err(McpError::method_not_found::<DiscoverRequestMethod>());
        }
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        ))
    }

    fn get_info(&self) -> ServerConfig {
        // `tasks` is the SEP-2663 extension (`extensions`), offered on both
        // revisions: a client opts in by declaring it, and only a slow call
        // from one that did ever becomes a task. The SEP-1686 `capabilities.
        // tasks` slot retired in 0.8.0 stays gone.
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources()
            .enable_completions()
            .enable_tasks()
            .build();
        ServerConfig::new(capabilities)
            // The `initialize` answer: the newest revision that still has a
            // handshake. rmcp answers a client asking for 2026-07-28 there
            // with this too.
            .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
            // Without this the server announces itself as "rmcp" — rmcp's
            // Implementation::from_build_env() bakes in its own crate name.
            // Version carries the build SHA so `initialize` responses and logs
            // identify the exact running build — deploy skew (an app compiled
            // from a stale agentmail checkout) becomes visible instead of being
            // inferred from behavioral fingerprints.
            .with_server_info(Implementation::new(
                "agentmail",
                concat!(
                    env!("CARGO_PKG_VERSION"),
                    " (",
                    env!("AGENTMAIL_BUILD_SHA"),
                    ")"
                ),
            ))
            .with_instructions(
                "# AgentMail\n\
             \n\
             Read, organize, draft and archive IMAP mail. **This server never sends.**\n\
             Drafts are saved to the server; nothing is transmitted to a recipient.\n\
             \n\
             ## Accounts\n\
             \n\
             The configured accounts are already listed in every `account` argument's\n\
             schema — choose one from there. Call `list_accounts` only when you need to know\n\
             which account is the DEFAULT, or which addresses an account sends as.\n\
             `list_identities` adds the From addresses its recent Sent mail used.\n\
             \n\
             `list_mailboxes` takes one account and returns selectable mailboxes only, 100\n\
             per page.\n\
             \n\
             ## Reading\n\
             \n\
             `get_messages` and `search_messages` return metadata only, newest first, with\n\
             the mailbox UIDVALIDITY on every row.\n\
             \n\
             Each row rides the result as a `resource_link`: the message as markdown, next\n\
             to its `/info` (headers, raw source and attachment URIs).\n\
             \n\
             **Links are the only place a result carries a URI.** Follow them. Never build a\n\
             URI by hand, and never lift one out of JSON.\n\
             \n\
             All reads use `BODY.PEEK`, so nothing is marked as read.\n\
             \n\
             ## The UID rule\n\
             \n\
             Every action that takes a UID also needs the mailbox and the non-zero\n\
             `expectedUidValidity` from the SAME discovery result. If the mailbox's UID epoch\n\
             changed, the action fails before touching anything.\n\
             \n\
             `Message not found` on a ranking sample means it was deleted since the scan.\n\
             Re-run the ranking for a fresh sample rather than retrying the UID.\n\
             \n\
             ## Drafts\n\
             \n\
             `create_draft` and `update_draft`. Both accept Reply-To, Bcc,\n\
             attachments and RFC threading headers. Every recipient field takes\n\
             `Name <address>` or a bare address, one recipient per entry.\n\
             \n\
             A draft is From the account's primary address under its display name. Pass\n\
             `from` (`Name <address>` or an address) to use another of the account's own\n\
             addresses — any other address is refused. A reply is From the address the\n\
             original was sent to; `update_draft` keeps the draft's current sender. The\n\
             result's `from` is the sender actually written.\n\
\n\
             To reply, give `create_draft` a `replyToMessage` — the mailbox, uid,\n\
             expectedUidValidity and mode of the live message. It derives the\n\
             recipients, the Re: subject and the threading headers; omit `to`, `cc`,\n\
             `inReplyTo` and `references`, which it refuses if you also send them.\n\
             \n\
             Bodies are Markdown. They ship as `multipart/alternative` — your text exactly as\n\
             written, plus an HTML rendering of it. Pass `plainTextOnly: true` for a single\n\
             unrendered text part.\n\
             \n\
             `update_draft` works on every server: RFC 8508 REPLACE where it exists, an\n\
             equivalent emulation where it does not. Never hand-roll `create_draft` +\n\
             `delete_messages` to edit a draft.\n\
             \n\
             A draft's UID is not durable. Every update mints a new one, and other mail\n\
             clients re-save drafts the same way. Use the UID `update_draft` returns, and\n\
             re-read links from a fresh `get_messages`.\n\
             \n\
             ## Organizing\n\
             \n\
             Move or delete with `move_message` and `delete_messages`, or the bulk forms\n\
             `move_by_sender`, `move_by_domain`, `move_list_id`, `move_subscription`,\n\
             `delete_by_sender`, `delete_by_domain` and `delete_list_id`.\n\
             \n\
             A bulk form handles every exact match in ONE call. Never loop `move_message`\n\
             per UID for that.\n\
             \n\
             `update_flags` adds flags, removes flags and sets or clears the Apple Mail\n\
             color in ONE call — doing those separately opens two UIDVALIDITY windows.\n\
             Pass `color: \"none\"` to clear it.\n\
\n\
             Mailboxes: `create_mailbox`, `rename_mailbox`, `delete_mailbox`. Rename and\n\
             delete are preview-then-confirm. INBOX and any mailbox referenced by a pending\n\
             MOVE recovery are never eligible; non-empty, special-use and descendant-bearing\n\
             mailboxes each need their own acknowledgement.\n\
             \n\
             A server without native MOVE may return `reconciliationPending` or\n\
             `needsAttention` with an `operationId`. Inspect it with `list_pending_moves` and\n\
             retry with `reconcile_moves`, which examines `needsAttention` moves again. One it\n\
             still can't finish is closed with `reconcile_moves` `dismiss: true` and its\n\
             `operationId`, after checking both mailboxes; that moves and deletes nothing.\n\
             \n\
             ## Rankings\n\
             \n\
             `top_senders`, `top_domains`, `top_subscriptions`, `top_mailing_lists`,\n\
             `list_flags` and `find_attachments` take an OPTIONAL mailbox — omit it to scan\n\
             the whole account.\n\
             \n\
             Pages are live: 100 rows maximum, default 20 for `top_domains` and 10 for the\n\
             others. Pages shift as mail changes.\n\
             \n\
             Each ranked row carries a sample `{mailbox, uidValidity, uid}` and its\n\
             `resource_link`. Map those to `mailbox`, `expectedUidValidity` and `uid` for a\n\
             later action.\n\
             \n\
             Pair a ranking with its action:\n\
             \n\
             - `top_senders` to `move_by_sender` or `delete_by_sender`\n\
             - `top_domains` to `move_by_domain` or `delete_by_domain`\n\
             - `top_mailing_lists` to `move_list_id` or `delete_list_id`\n\
             - `top_subscriptions` to `move_subscription` to file, or `unsubscribe_message`\n\
             \n\
             Grouping is exact and never widens:\n\
             \n\
             - `top_senders` groups by (email, display name). One address under two display\n\
               names is two rows.\n\
             - `top_domains` keeps parents and subdomains apart. Acting on `example.com`\n\
               never touches `mail.example.com`; `registrableDomain` and `subdomain` are\n\
               context, not scope.\n\
             - `top_mailing_lists` groups by List-Id (RFC 2919) across senders. Use\n\
               `delete_list_id` to remove a whole list.\n\
             \n\
             **Never use `delete_by_sender` to clean up a mailing list.** It deletes every\n\
             message from that sender, including their non-bulk mail.\n\
             \n\
             ## Unsubscribing\n\
             \n\
             `advertisedOneClick` on `top_subscriptions` is syntactic only —\n\
             `unsubscribe_message` re-fetches the exact headers and verifies DKIM itself.\n\
             \n\
             It requires `confirmOneClick: true`. Matching-message cleanup is the optional\n\
             nested `cleanup {when, identity, deletion}` object; omit it to unsubscribe and\n\
             nothing else. Cleanup prefers the DKIM-authenticated List-Id, and otherwise\n\
             requires an exact sender address, `List-Unsubscribe-Post`, and the sample's\n\
             List-Id when present. It stops after a failed POST unless `when` is `always`,\n\
             and never escalates a failed Trash move to permanent deletion unless `deletion`\n\
             is `trashThenPermanent`.\n\
             \n\
             ## Account-wide scans\n\
             \n\
             Discovery uses one selectable All mailbox where the server has it. Otherwise it\n\
             enumerates storage mailboxes and skips Trash, Junk, Spam, Drafts and virtual\n\
             aggregate views. A destructive scan never writes through an aggregate view.\n\
             \n\
             On a provider with a visible-window limit (Yahoo, AOL), rankings and\n\
             account-wide sweeps cover the ENTIRE mailbox through RFC 9586 UID Mode when a\n\
             persistent cache is available. Without one, a sweep repeats passes as older mail\n\
             backfills into view.\n\
             \n\
             ## Saving to disk\n\
             \n\
             `download_attachments`, `download_message_source` and `download_thread` write\n\
             files. Omit `outputDir` to write into the session workspace — that is the\n\
             default and usually what you want.\n\
             \n\
             `find_attachments` hits carry `{mailbox, uidValidity, uid, date}` and a link;\n\
             pass that identity to `download_attachments`. An attachment's canonical filename\n\
             in `/info` is `{uid}_{index}_{name}`, the same name `download_attachments`\n\
             writes.\n\
             \n\
             Use `download_message_source` when exact RFC822 bytes must reach disk without\n\
             crossing model context, and `download_thread` for a chosen UID set plus a JSON\n\
             integrity manifest. Both validate UIDVALIDITY, use `BODY.PEEK`, refuse to\n\
             overwrite, and return SHA-256 plus local DKIM results.\n\
             \n\
             For a self-contained thread record: call `preview_thread_record` on one live\n\
             message, review the Message-ID / In-Reply-To / References graph and the\n\
             `selectionDigest`, then pass that digest and a purpose to\n\
             `export_thread_record`. Export re-discovers and refuses drift, writes private\n\
             PDF, EML and manifest artifacts, and reports `recorded` / `submittable` only\n\
             after reopening and integrity checks. Those flags describe packet completeness,\n\
             not legal admissibility.\n\
             \n\
             ## Resources\n\
             \n\
             `resources/list` publishes one `email://{account}` catalog root per account.\n\
             Read a root for its mailbox URIs, then `email://{account}/{mailbox}` for a page\n\
             of message metadata.\n\
             \n\
             A message is `email://{account}/{mailbox}/{uidValidity}/{uid}` (markdown), plus:\n\
             \n\
             - `/headers` — exact headers\n\
             - `/source` — bounded raw RFC822\n\
             - `/info` — JSON metadata and the sibling URIs\n\
             - `/attachments/{index}` — one attachment as a blob, 4 MiB limit\n\
             \n\
             Percent-encode the account and mailbox segments, including a `/` inside a\n\
             mailbox name as `%2F`.\n\
             \n\
             `list_flags` resolves Apple Mail `$MailFlagBit` colors to names: red, orange,\n\
             yellow, green, blue, purple, gray.",
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

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(
            resources::account_resources(self.agentmail.account_names()),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, McpError> {
        self.read_email_resource(&request.uri).await.map(Into::into)
    }

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        self.handle_complete(request).await
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
    serve_server(transport, AgentMailServer::new_embedded(mk)).await
}

/// Serve one connection, then end its tasks: nothing can poll them once the
/// client is gone.
async fn serve_server<T>(
    transport: T,
    server: AgentMailServer,
) -> Result<(), Box<dyn std::error::Error>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let tasks = Arc::clone(&server.tasks);
    let service = server.serve(transport).await.inspect_err(|e| {
        eprintln!("agentmail: server error: {}", e);
    })?;
    let result = service.waiting().await;
    tasks.shutdown().await;
    result?;
    Ok(())
}

/// Serve the MCP server over stdio.
///
/// This is the entry point for the standalone `agentmail serve` binary.
pub async fn serve_stdio(mk: crate::Agentmail) -> Result<(), Box<dyn std::error::Error>> {
    let server = AgentMailServer::new(mk);
    let prewarm_agentmail = Arc::clone(&server.agentmail);
    let tasks = Arc::clone(&server.tasks);
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
    tasks.shutdown().await;
    result?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn server_with_accounts(names: &[&str]) -> AgentMailServer {
        let accounts = names
            .iter()
            .map(|name| {
                (
                    (*name).to_string(),
                    crate::config::AccountConfig {
                        host: "imap.example.com".to_string(),
                        port: 993,
                        username: format!("{name}@example.com"),
                        email: None,
                        aliases: Vec::new(),
                        display_name: None,
                        password: None,
                        tls: true,
                        max_connections: None,
                        auth: crate::config::AuthMethod::Password,
                    },
                )
            })
            .collect();
        AgentMailServer::new_embedded(crate::Agentmail::new(crate::Config::from_accounts(
            accounts,
        )))
    }

    fn account_enum(tool: &rmcp::model::Tool) -> Option<Vec<String>> {
        Some(
            tool.input_schema
                .get("properties")?
                .as_object()?
                .get("account")?
                .as_object()?
                .get("enum")?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        )
    }

    /// The configured accounts are the only part of this schema unknowable at
    /// compile time, and the first thing every tool needs. Publishing them as
    /// an `enum` is what stops an agent opening every session with a
    /// `list_accounts` call it does not need — a resource it must read and
    /// interpret is not the same affordance as a value in the schema of the
    /// argument that wants it.
    #[test]
    fn every_account_argument_advertises_the_configured_accounts() {
        let server = server_with_accounts(&["Gmail", "iCloud"]);
        let accounts = server.agentmail.account_names();
        let mut tools = AgentMailServer::tool_router().list_all();

        let mut patched = Vec::new();
        for tool in &mut tools {
            assert!(
                account_enum(tool).is_none(),
                "`{}` must not carry a compile-time account enum",
                tool.name
            );
            let schema = std::sync::Arc::make_mut(&mut tool.input_schema);
            patch_account_enum(schema, &accounts);
            if let Some(values) = account_enum(tool) {
                assert_eq!(
                    values,
                    vec!["Gmail".to_string(), "iCloud".to_string()],
                    "`{}` offers the live accounts",
                    tool.name
                );
                patched.push(tool.name.to_string());
            }
        }

        assert!(
            patched.contains(&"get_messages".to_string())
                && patched.contains(&"create_draft".to_string()),
            "the tools that take an account get the enum; patched: {patched:?}"
        );
        // `list_accounts` takes no account — the patch must not invent the
        // property on a schema that never had it.
        assert!(
            !patched.contains(&"list_accounts".to_string()),
            "a tool without an `account` argument is left alone"
        );
    }

    /// An empty `enum` is a parameter nothing can satisfy, so `list_tools`
    /// skips the patch entirely when no account is configured — the caller then
    /// gets "no such account" from the server rather than a schema rejection
    /// with no explanation. This pins the shape that guard exists to avoid.
    #[test]
    fn an_empty_account_list_would_be_unsatisfiable_hence_the_guard() {
        let mut schema = serde_json::Map::new();
        schema.insert(
            "properties".to_string(),
            serde_json::json!({ "account": { "type": "string" } }),
        );
        patch_account_enum(&mut schema, &[]);
        assert_eq!(
            schema["properties"]["account"]["enum"],
            serde_json::json!([]),
            "which is why `list_tools` never calls this with an empty list"
        );
    }

    /// The handshake instructions are the first thing a model reads, and they
    /// spent a long time as one unbroken 7 KB paragraph of semicolons. The
    /// structure IS the contract: sections it can scan for, lines short enough
    /// to hold, and no guidance that contradicts the schema it also receives.
    #[test]
    fn the_handshake_instructions_stay_scannable() {
        use rmcp::ServerHandler as _;
        let instructions = server_with_accounts(&["work"])
            .get_info()
            .instructions
            .expect("instructions are advertised");

        for section in [
            "## Accounts",
            "## Reading",
            "## The UID rule",
            "## Drafts",
            "## Organizing",
            "## Rankings",
            "## Unsubscribing",
            "## Account-wide scans",
            "## Saving to disk",
            "## Resources",
        ] {
            assert!(
                instructions.contains(section),
                "missing section `{section}`"
            );
        }

        let unwrapped: Vec<&str> = instructions
            .lines()
            .filter(|line| line.chars().count() > 90)
            .collect();
        assert!(
            unwrapped.is_empty(),
            "long lines are how this collapses back into one paragraph: {unwrapped:#?}"
        );

        assert!(
            !instructions.contains("Start with list_accounts"),
            "the accounts are in every `account` enum — instructing a discovery \
             call contradicts the schema and is what made agents open every \
             session with one"
        );
    }

    /// `#[tool_handler]` evaluates its router expression on EVERY call_tool,
    /// list_tools and get_tool. Rebuilt per call, the router re-creates all 36
    /// routes each time, and on each runtime worker thread its first request
    /// regenerates every schema (rmcp caches them per thread). Shared, a request
    /// on any thread gets the same schema allocation back.
    #[test]
    fn the_tool_router_is_built_once_not_per_request() {
        use rmcp::ServerHandler as _;
        let server = server_with_accounts(&["work"]);
        let here = server.get_tool("create_draft").expect("create_draft");
        let elsewhere = std::thread::scope(|scope| {
            scope
                .spawn(|| server.get_tool("create_draft"))
                .join()
                .expect("thread")
        })
        .expect("create_draft");
        assert!(
            std::sync::Arc::ptr_eq(&here.input_schema, &elsewhere.input_schema),
            "the router was rebuilt for a request on another thread"
        );
    }
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

    /// The concrete case the bridge rejected: a mailbox with no special-use
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
    /// hosts (Gemini CLI, n8n, some bridges) reject or drop referenced
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
            36,
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
            "download_message_source",
            "download_thread",
            "move_message",
            "unsubscribe_message",
            "update_flags",
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
            if matches!(name, "delete_messages" | "download_thread") {
                assert_eq!(
                    schema["properties"]["uids"]["items"]["minimum"],
                    serde_json::json!(1),
                    "tool `{name}` must reject UID zero in its schema"
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

/// Tasks over the real JSON-RPC wire, with the promotion budget at zero so a
/// call that has to touch the network is promoted at once.
#[cfg(test)]
mod task_wire_tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

    struct Wire {
        reader: BufReader<ReadHalf<DuplexStream>>,
        writer: WriteHalf<DuplexStream>,
        next_id: i64,
    }

    impl Wire {
        /// A server whose every eligible call is promoted at once, and a
        /// session that declared the tasks extension.
        async fn start() -> Self {
            let config = crate::Config::from_accounts(vec![(
                "dummy".to_string(),
                crate::config::AccountConfig {
                    // Never connects: `.invalid` cannot resolve, so the call
                    // has to wait on the network before it fails.
                    host: "imap.invalid".to_string(),
                    port: 993,
                    username: "dummy@example.invalid".to_string(),
                    email: None,
                    aliases: Vec::new(),
                    display_name: None,
                    password: Some(crate::secret::Secret::new_raw("unused")),
                    tls: true,
                    max_connections: None,
                    auth: crate::config::AuthMethod::Password,
                },
            )]);
            let server = AgentMailServer::new_embedded_with(
                crate::Agentmail::new(config),
                tasks::TaskConfig {
                    promote_after: std::time::Duration::ZERO,
                    ..tasks::TaskConfig::DEFAULT
                },
            );
            let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
            tokio::spawn(async move {
                let _ = serve_server(server_io, server).await;
            });
            let (reader, writer) = tokio::io::split(client_io);
            let mut wire = Self {
                reader: BufReader::new(reader),
                writer,
                next_id: 0,
            };
            let init = wire
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {
                            "extensions": { "io.modelcontextprotocol/tasks": {} }
                        },
                        "clientInfo": { "name": "agentmail-tests", "version": "0" }
                    }),
                )
                .await;
            assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init:#}");
            wire.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .await;
            wire
        }

        async fn send(&mut self, frame: Value) {
            let mut line = serde_json::to_string(&frame).unwrap();
            line.push('\n');
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }

        async fn request(&mut self, method: &str, params: Value) -> Value {
            self.next_id += 1;
            let id = self.next_id;
            self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
                .await;
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    let mut line = String::new();
                    let read = self.reader.read_line(&mut line).await.unwrap();
                    assert!(read > 0, "server closed the stream awaiting `{method}`");
                    let frame: Value = serde_json::from_str(&line).unwrap();
                    if frame["id"].as_i64() == Some(id) {
                        return frame;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("no answer to `{method}`"))
        }
    }

    /// The whole flow a task-aware client sees: the call is answered with a
    /// task handle, `tasks/get` reports it working, then completed, and the
    /// result is the tool's own (an `isError` result here, since the account
    /// cannot connect, which SEP-2663 counts as `completed`).
    #[tokio::test]
    async fn a_slow_call_becomes_a_task_the_client_polls_to_its_result() {
        let mut wire = Wire::start().await;
        let call = wire
            .request(
                "tools/call",
                json!({ "name": "top_senders", "arguments": { "account": "dummy" } }),
            )
            .await;
        let created = &call["result"];
        assert_eq!(created["resultType"], "task", "{call:#}");
        assert_eq!(created["status"], "working", "{call:#}");
        assert_eq!(created["statusMessage"], "Running top_senders", "{call:#}");
        let task_id = created["taskId"].as_str().expect("a task id").to_string();

        let mut last = Value::Null;
        for _ in 0..300 {
            last = wire
                .request("tasks/get", json!({ "taskId": task_id }))
                .await;
            if last["result"]["status"] != "working" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(last["result"]["status"], "completed", "{last:#}");
        assert_eq!(last["result"]["result"]["isError"], true, "{last:#}");
    }

    /// A tool outside the task list is never promoted, however long it takes.
    #[tokio::test]
    async fn a_tool_outside_the_task_list_answers_inline() {
        let mut wire = Wire::start().await;
        let call = wire
            .request(
                "tools/call",
                json!({ "name": "check_connection", "arguments": { "account": "dummy" } }),
            )
            .await;
        assert!(call["result"].get("taskId").is_none(), "{call:#}");
        assert!(call["result"]["content"].is_array(), "{call:#}");
    }
}
