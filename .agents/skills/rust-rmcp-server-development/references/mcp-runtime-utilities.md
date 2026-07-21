# MCP Runtime Utilities

Use this reference when designing runtime behavior around MCP primitives: capability negotiation, request control, notifications, HTTP security, authorization, and gateway forwarding.

Verified on 2026-06-09 against:

- MCP specification `2025-11-25`: lifecycle, transports, authorization.
- MCP base utilities: cancellation, ping, progress, tasks.
- MCP client features: roots, sampling, elicitation.
- MCP server utilities: completion, logging, pagination.
- `rmcp` 1.7.0 docs for `ServerCapabilities`, `ClientCapabilities`, `TasksCapability`, `Peer<RoleServer>`, `ServerHandler`, and task macros.

## Runtime Mental Model

MCP has three layers:

- Primitive layer: `tools`, `resources`, and `prompts`.
- Runtime layer: capabilities, progress, cancellation, ping, timeouts, logging, completion, pagination, subscriptions, and tasks.
- Transport/security layer: stdio, Streamable HTTP, protocol headers, sessions, origin validation, and authorization.

The runtime layer decides whether a primitive is available, how long it may run, how status is reported, how lists stay fresh, and how failures are surfaced.

## Spec Trajectory

The next MCP revision, versioned `2026-07-28` (release candidate locked May 2026), is a major shift: the core becomes stateless — the `initialize` handshake and `Mcp-Session-Id` header are removed — and MCP Apps and Tasks move into a formal extensions framework with a deprecation policy. `rmcp` 1.7.0 targets `2025-11-25`. Before designing behavior that depends on sessions or the initialize lifecycle for anything newer, check the spec changelog and current `rmcp` release notes.

## Client, Server, Gateway

Client role:

- Sends `initialize`, receives server capabilities, and sends `notifications/initialized`.
- Calls server primitives such as `tools/call`, `resources/read`, and `prompts/get`.
- Receives server notifications such as progress, logs, list changes, and resource updates.
- Provides client features such as `roots`, `sampling`, and `elicitation` when negotiated.
- Owns user approval, display, timeout policy, and retry behavior.

Server role:

- Returns `ServerInfo` and `ServerCapabilities` from initialization.
- Exposes `tools`, `resources`, `prompts`, `logging`, `completions`, and server-side `tasks` only when implemented.
- Requests client features such as `roots/list`, `sampling/createMessage`, or `elicitation/create` only when negotiated.
- Emits progress, logs, list-changed notifications, and resource updates when appropriate.
- Enforces auth, scope, input validation, root boundaries, and resource limits.

Gateway role:

- Acts as a server to the downstream host/client and as a client to one or more upstream MCP servers.
- Negotiates capabilities independently on every connection; do not blindly mirror upstream capabilities.
- Advertises only the features the gateway can faithfully proxy, aggregate, filter, secure, and test.
- Maps downstream cancellation, progress, tasks, pagination, and list-changed notifications to upstream operations when possible.
- Keeps auth, roots, task IDs, cursors, resource URIs, and progress tokens scoped per downstream user/session.

Implementation patterns: `rmcp-client-patterns.md` for the client role, `gateway-patterns.md` for the gateway role.

## Capability Negotiation

Capability negotiation happens during `initialize`. Treat negotiated capabilities as runtime gates:

- Do not call a feature the peer did not advertise.
- Do not advertise a feature unless the implementation handles its protocol paths.
- Fail early and clearly when a required capability is missing.
- For HTTP, carry the negotiated `MCP-Protocol-Version` header on later requests.
- For stateful HTTP sessions, carry `MCP-Session-Id` on later requests when the server issued one.

Server sub-capabilities:

- `listChanged`: prompts, resources, and tools can notify clients when their list changes.
- `subscribe`: resources can support subscriptions to individual resource updates.

RMCP mapping:

- Use `ServerCapabilities::builder()` for server capabilities.
- Use list-changed builder methods only when the server emits the corresponding notifications.
- Use resource subscribe support only when `subscribe` and `unsubscribe` paths are implemented.
- Use `TasksCapability::server_default()` or explicit task capabilities only when task-augmented `tools/call` works.
- Use `ClientCapabilities` and peer helpers to check client support before roots, sampling, elicitation, and client-side tasks.

## Notifications And Subscriptions

Use list-changed notifications when the set of available primitives changes:

- `notifications/tools/list_changed`: tool inventory changed.
- `notifications/resources/list_changed`: resource inventory changed.
- `notifications/prompts/list_changed`: prompt inventory changed.

Use resource subscriptions when a known resource changes:

- Client subscribes to a URI.
- Server sends `notifications/resources/updated` for that URI.
- Client decides whether to re-read the resource.

Tandem use:

- Use `listChanged` when entries are added, removed, renamed, hidden, or reclassified.
- Use `subscribe` when an existing resource changes but remains the same URI.
- Use pagination on list operations even when list-changed is supported; freshness and result size are separate problems.

RMCP mapping:

- Use peer notification helpers such as `notify_tool_list_changed`, `notify_resource_list_changed`, `notify_prompt_list_changed`, and `notify_resource_updated` when available.
- Implement `subscribe` and `unsubscribe` in `ServerHandler` before advertising resource subscriptions.

## Request Control

Timeouts:

- Configure request timeouts per operation class when possible.
- Resetting a timeout after progress is acceptable, but always enforce a maximum timeout.
- Use shorter timeouts for completion and listing, longer timeouts for tool calls, sampling, elicitation, and tasks.

Cancellation:

- Use `notifications/cancelled` for ordinary in-flight requests.
- Use `tasks/cancel` for task-augmented requests.
- Treat cancellation as best-effort; processing may already have finished.
- Free resources and avoid sending a response for a cancelled ordinary request when possible.

Ping:

- Either side can send `ping` to test liveness.
- Use ping for connection health, not application-level readiness.
- Make ping interval and timeout configurable for long-lived HTTP/SSE connections and gateways.

Progress:

- Use `progressToken` in request `_meta` when the requestor wants progress.
- Emit `notifications/progress` only for active tokens.
- Make progress increase monotonically and stop after terminal completion.
- For tasks, continue using the original progress token until the task reaches a terminal state.

## Tasks

Tasks are experimental durable wrappers around requests. Either side can be a requestor or receiver.

Use tasks for:

- Long-running `tools/call`.
- Deferred result retrieval.
- Polling and cancellation.
- Batch work or external job APIs.
- `input_required` flows where a task pauses for elicitation.

Do not use tasks for:

- Short requests that can return directly.
- Simple progress updates without deferred results.
- Fire-and-forget notifications.

Tandem use:

- A task may wrap `tools/call` and emit progress.
- A task may pause with `input_required`, then use elicitation to collect missing input.
- Client-side tasks may wrap `sampling/createMessage` or `elicitation/create`.
- Gateways should map downstream task IDs to upstream task IDs and never expose upstream IDs directly unless that is part of the gateway contract.

RMCP mapping:

- Use `TasksCapability::server_default()` for server support of task-augmented `tools/call`.
- Use `TasksCapability::client_default()` for client support of task-augmented sampling and elicitation.
- Use `supports_tools_call`, `supports_sampling_create_message`, and `supports_elicitation_create` before task augmentation.
- Use `#[task_handler]` and RMCP task helpers for task lifecycle support after checking current docs.

## Client Features In Tandem

Roots:

- Use before tools or resources touch local files.
- Cache roots only with invalidation for `notifications/roots/list_changed`.
- Validate all paths against roots even after roots are negotiated.

Sampling:

- Use when the server needs model reasoning inside a server workflow.
- Check `sampling.tools` before including tools in a sampling request.
- Keep sampling bounded with max tokens, iteration limits, and explicit tool loops.
- Do not treat sampling as an authorization boundary; client and user approval still matter.

Elicitation:

- Use form mode for non-sensitive structured input.
- Use URL mode for secrets, credentials, payment, OAuth, or other sensitive out-of-band flows.
- Validate elicitation responses against the requested schema.
- Pair elicitation with tasks when a long-running request pauses for user input.

## Server Utilities In Tandem

Completion:

- Supports interactive argument completion for prompts and resource templates.
- Use for prompt arguments, resource template variables, IDs, names, paths, and enum-like values.
- Rate limit and avoid leaking sensitive suggestions.

Completion RMCP mapping:

- Implement `complete` on `ServerHandler` (`CompleteRequestParams` in, `CompleteResult` out) and advertise it with `ServerCapabilities::builder().enable_completions()`.
- The reference shape is `examples/servers/src/completion_stdio.rs` in the rust-sdk.
- Clients call `complete` directly or use the `complete_prompt_simple` / `complete_resource_simple` peer helpers.

Logging:

- Advertise `logging` only when server log notifications are implemented.
- Use structured `notifications/message` with level, optional logger, and JSON data.
- Strip secrets, credentials, PII, and attack-enabling internals.
- For stdio servers, continue routing process logs to stderr; MCP logging is protocol-level logging, not stdout printing.

Pagination:

- Use opaque cursors for `resources/list`, `resources/templates/list`, `prompts/list`, and `tools/list`.
- Clients must not parse or persist cursors across sessions.
- Servers should provide stable cursors and return `-32602` for invalid cursors.

## Streamable HTTP Security

Required server-side behavior:

- Validate `Origin` on incoming connections to reduce DNS rebinding risk.
- Return HTTP 403 when `Origin` is present and invalid; the body may be a JSON-RPC error without `id`.
- Bind local servers to `127.0.0.1` instead of `0.0.0.0` unless the user explicitly needs network exposure.
- Implement proper authentication for all non-trivial HTTP deployments.
- Validate `MCP-Protocol-Version` and `MCP-Session-Id` where applicable.

Client-side behavior:

- Send `Accept: application/json, text/event-stream` for POST requests.
- Send `Accept: text/event-stream` for GET streams.
- Send `Authorization: Bearer <access-token>` on every authorized HTTP request.
- Do not treat SSE disconnection as cancellation; send cancellation explicitly.

Gateway behavior:

- Validate downstream `Origin` and auth separately from upstream auth.
- Do not forward downstream bearer tokens upstream unless that is explicitly the auth model.
- Maintain per-user sessions and avoid cross-user replay of SSE events, cursors, task IDs, roots, or progress tokens.

## Authorization

MCP authorization applies to HTTP transports. Stdio servers should normally receive credentials through environment or local configuration instead.

Role responsibilities:

- Server: act as an OAuth protected resource; publish discovery metadata; validate bearer tokens and audience binding on every request; `401` for missing/invalid/expired tokens, `403` for insufficient scope; never accept or transit tokens issued for another resource.
- Client: discover metadata from `WWW-Authenticate` or well-known URIs; run the PKCE authorization-code flow with the `resource` parameter; send tokens only in the `Authorization` header.
- Gateway: keep downstream identity separate from upstream credentials; apply the downstream user's permissions before upstream calls; token passthrough is forbidden.

RFC specifics, the `rmcp` `auth` feature, and server middleware patterns: read `http-authorization.md`.

## Error Handling

Handle these as first-class failures:

- Protocol version mismatch during initialization.
- Failure to negotiate required capabilities.
- Request timeout.
- Invalid cursor.
- Unsupported completion, logging, subscription, or task capability.
- Invalid, expired, missing, or insufficient-scope authorization.
- Invalid task ID, expired task, or cancellation of a terminal task.

Use protocol errors for protocol failures and task status for task execution failures. For task-wrapped tool calls, a tool result with `isError: true` should move the task to `failed`.

## Design Checklist

- Have all advertised capabilities been implemented and tested?
- Are optional capabilities checked before use?
- Are list change and resource update notifications distinct?
- Are per-request and maximum timeouts configured?
- Does progress stop after completion and remain monotonic?
- Are ordinary cancellation and task cancellation handled separately?
- Is HTTP protected against invalid Origin, missing auth, token audience mistakes, and session leakage?
- Does gateway code isolate downstream users from upstream sessions, cursors, tasks, and tokens?
