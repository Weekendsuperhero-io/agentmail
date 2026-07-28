# Bridge Patterns

Use this reference when one Rust process must act as an MCP server to downstream clients and as an MCP client to upstream servers: aggregators, proxies, filters, and bridges that add their own tools or data.

Verified on 2026-06-09 against:

- `rmcp` 1.7.0 source: `handler/server.rs` (ServerHandler signatures), `service/client.rs` and `service/server.rs` (peer methods), `service.rs` (`ServiceError`, `NotificationContext`).
- MCP specification 2025-11-25 security best practices (token passthrough, confused deputy, session hijacking).
- `modelcontextprotocol/rust-sdk` ships no official bridge example as of 1.7.0; these patterns compose its documented client and server APIs.

The role rules (what a bridge may advertise, what must stay isolated) live in `mcp-runtime-utilities.md`. This file is the implementation shape. Read `rmcp-client-patterns.md` first; a bridge is that client embedded inside a `ServerHandler`.

## Process Shape

```rust
use std::{collections::HashMap, sync::Arc};

use rmcp::{RoleClient, service::RunningService};
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct Bridge {
    /// Upstream connections keyed by a stable name ("github", "tickets").
    upstreams: Arc<RwLock<HashMap<String, RunningService<RoleClient, ()>>>>,
}
```

Decisions to make explicitly:

- **Connect eagerly or lazily.** Eager (at startup) fails fast and lets you compute capabilities before accepting downstreams; lazy starts faster but surfaces connection errors mid-request. Watch `waiting()`/`QuitReason` per upstream and reconnect deliberately.
- **Shared versus per-downstream upstream connections.** Share a connection only when the upstream is stateless for your use. Roots, sampling, elicitation, subscriptions, and auth context live per connection — sharing one upstream session across downstream users leaks all of them. When upstreams are user-scoped, key connections by `(downstream_user, upstream)` instead.

## Forwarding Tools

Namespace upstream tool names so they cannot collide with each other or with the bridge's own tools, and split the name again on the way in:

```rust
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams},
    service::{RequestContext, ServiceError},
};

fn upstream_error(e: ServiceError) -> McpError {
    match e {
        // Preserve real protocol errors (code, message) from the upstream.
        ServiceError::McpError(e) => e,
        other => McpError::internal_error(format!("upstream failure: {other}"), None),
    }
}

impl ServerHandler for Bridge {
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = Vec::new(); // start with the bridge's own tools
        for (name, upstream) in self.upstreams.read().await.iter() {
            for mut tool in upstream.list_all_tools().await.map_err(upstream_error)? {
                tool.name = format!("{name}__{}", tool.name).into();
                tools.push(tool);
            }
        }
        Ok(ListToolsResult { tools, ..Default::default() })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let (upstream_name, tool_name) = request
            .name
            .split_once("__")
            .ok_or_else(|| McpError::invalid_params("unknown tool", None))?;
        let upstreams = self.upstreams.read().await;
        let upstream = upstreams
            .get(upstream_name)
            .ok_or_else(|| McpError::invalid_params("unknown upstream", None))?;
        // Build params fresh: never blind-forward downstream `_meta` (progress
        // tokens) or `task` metadata — map those IDs explicitly per session.
        let mut forward = CallToolRequestParams::new(tool_name.to_owned());
        forward.arguments = request.arguments;
        upstream.call_tool(forward).await.map_err(upstream_error)
    }
}
```

Notes on this shape:

- Tool-level failures arrive as `Ok(CallToolResult { is_error: Some(true), .. })` and pass through untouched; only transport/protocol failures need `upstream_error`.
- Filter and rewrite at the boundary: drop upstream tools you do not want exposed, strip or rewrite upstream `_meta` you do not understand, and apply your own `ToolAnnotations` policy (an upstream claiming `read_only_hint: true` is a claim, not a fact).
- Resources and prompts follow the same delegate-and-merge shape. For resource URIs, keep a routing map of which upstream owns which URI; rewrite URIs only when two upstreams genuinely collide. If you proxy MCP Apps servers, namespace `ui://` URIs per the sibling skill `mcp-apps-auth-rmcp-development`, `references/apps-security.md`.

## ID, Cursor, And Token Mapping

Anything that identifies in-flight work upstream must be re-keyed before it reaches a downstream session, or one user can replay another user's handles:

- **Pagination cursors**: simplest is to merge lists and return one page (`next_cursor: None`). If you paginate, mint your own opaque cursor that encodes `(upstream, upstream_cursor)` and reject cursors that do not belong to the requesting session with `-32602`.
- **Progress tokens**: when the downstream request carries a `progressToken`, forward the call with a token you mint, keep a `upstream_token -> (downstream_session, downstream_token)` map, and translate each upstream `notify_progress` back through the bridge's server peer.
- **Task IDs**: same indirection. Upstream task IDs never appear downstream; expired or foreign task IDs get a protocol error, not a pass-through.
- **Sessions**: downstream `Mcp-Session-Id` values are yours; generate them with a secure RNG and bind them to the authenticated user (the spec's security page recommends keying session state as `<user_id>:<session_id>`).

## Forwarding Notifications

Upstream notifications arrive in the bridge's `ClientHandler`; re-emit them downstream through each affected session's `Peer<RoleServer>` (capture those peers from `RequestContext` or your connection-accept loop, and drop them when sessions close):

```rust
use rmcp::{ClientHandler, RoleClient, service::NotificationContext};

impl ClientHandler for UpstreamListener {
    async fn on_tool_list_changed(&self, _ctx: NotificationContext<RoleClient>) {
        self.invalidate_tool_cache().await;
        for peer in self.downstream_server_peers().await {
            let _ = peer.notify_tool_list_changed().await;
        }
    }
}
```

Fan out only to sessions entitled to that upstream, and map `resources/updated` URIs through the same routing table used for reads.

## Capability Intersection

Advertise only what the bridge implements end to end. Compute after upstream initialization:

- Tools/prompts/resources: advertise if the bridge serves its own or proxies at least one upstream that does.
- `listChanged`: advertise only if you actually re-emit the notification (code above).
- `subscribe`, completions, tasks: advertise only if every routed upstream supports it, or the bridge shims it (for example, polling upstream and synthesizing `resources/updated`).
- Client-direction features (sampling, elicitation, roots) do not pass through transparently: an upstream's `create_message` request terminates at the bridge's `ClientHandler`. Either answer it there or implement explicit relay logic to the downstream client — never advertise sampling/elicitation support to upstreams you cannot actually satisfy.

## Auth Separation

The spec's security best practices are blunt here, and they bind bridges directly:

- Token passthrough is "explicitly forbidden": the bridge MUST NOT accept tokens that were not issued to the bridge, and MUST NOT forward downstream bearer tokens upstream. Downstream tokens authenticate the user *to the bridge* (audience = bridge, RFC 8707); upstream calls use the bridge's own credentials or a proper per-user token exchange.
- Proxying a third-party authorization server invites the confused-deputy problem: keep a per-user registry of approved downstream `client_id`s and obtain consent before the first forwarding for each client.
- Validate downstream auth on every request; never use the session ID as proof of identity.

Implementation patterns for both sides are in `http-authorization.md`.

## Bridge Checklist

- Tool names and resource URIs cannot collide across upstreams or with bridge-own primitives.
- No downstream bearer token ever appears in an upstream request, log, or error message.
- Upstream cursors, task IDs, and progress tokens never reach a downstream session unmapped; foreign handles are rejected per session.
- Advertised capabilities match what is actually proxied or shimmed — nothing more.
- Upstream disconnects are handled: reconnect or degrade, and emit `notify_tool_list_changed` when the merged surface changes.
- Per-downstream isolation holds even under shared upstream connections (or connections are per-user).
