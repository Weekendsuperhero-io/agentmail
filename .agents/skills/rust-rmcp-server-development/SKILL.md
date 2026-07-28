---
name: rust-rmcp-server-development
description: Build, scaffold, update, debug, or review Rust Model Context Protocol servers, clients, and bridges using the official rmcp SDK. Use when the user mentions rmcp, Rust MCP servers or clients, MCP bridges or proxies in Rust, MCP tools in Rust, stdio or Streamable HTTP transports, ServerHandler, ClientHandler, tool_router, prompts, resources, sampling or elicitation handlers, spawning MCP servers from Rust, schemars or JSON Schema compatibility, structured tool output, MCP OAuth in Rust, Inspector testing, or replacing the outdated rust-mcp-server-generator skill.
license: Apache-2.0
---

# Rust RMCP Server Development

Use this skill to create or modernize Rust MCP servers, clients, and bridges with the official `rmcp` SDK.

## Source Priority

RMCP changes quickly. Before generating version-specific code, check current official sources unless the user explicitly pins a version:

1. `https://docs.rs/rmcp/latest/rmcp/`
2. `https://github.com/modelcontextprotocol/rust-sdk`
3. `https://modelcontextprotocol.io/specification/`

Read `references/protocol-map.md` when orienting on MCP as a whole, explaining how protocol features relate, or deciding which layer (primitive, utility, transport, extension) a requirement belongs to.
Read `references/rmcp-1x-patterns.md` before writing RMCP code.
Read `references/mcp-server-primitives.md` when deciding whether a capability should be a tool, resource, prompt, or a combination of them.
Read `references/mcp-runtime-utilities.md` when designing capability negotiation, client/server/bridge roles, list change notifications, resource subscriptions, cancellation, ping, progress, tasks, timeouts, authorization, logging, completion, pagination, or Streamable HTTP security.
Read `references/tool-schemas-and-output.md` before finalizing tool parameter or output structs, when returning structured output, or when a host drops, rejects, or mishandles tool schemas.
Read `references/rmcp-client-patterns.md` when building an MCP client or test harness, spawning stdio servers from Rust, connecting to Streamable HTTP servers, or handling server-initiated sampling, elicitation, or roots requests.
Read `references/bridge-patterns.md` when one process must be both MCP server and client: aggregating, proxying, filtering, or augmenting upstream servers.
Read `references/http-authorization.md` when an HTTP server must act as an OAuth protected resource, a client needs the OAuth flow, or you enable the rmcp `auth` feature.
Read `references/mcp-apps-auth-bridge.md` when an RMCP server needs MCP Apps UI resources, ext-auth authorization extensions, or TypeScript UI contract sharing.
Read `references/CHANGELOG.md` when the user asks what changed since RMCP 1.5.0, wants release-specific context, or needs evidence for a migration decision.
Read `references/START_STOP_CONTINUE.md` when scaffolding or modernizing code against RMCP 1.5.0 through current release patterns.
Read `references/tool-annotations.md` before defining public tools, reviewing tool safety metadata, or deciding which tools can be auto-approved, retried, or shown with stronger confirmation UI.
Read `references/scaffold-guidance.md` when creating a new project or replacing stale boilerplate.
Read `references/outdated-generator-notes.md` when migrating from `rust-mcp-server-generator` or evaluating old snippets.

## Initial Questions

If the request lacks required details, ask only for what blocks implementation:

- Role: server (default), client, or bridge.
- Project name and crate name.
- Transport: usually `stdio`; use Streamable HTTP only when the user needs a remote or HTTP server.
- Capabilities: tools only, or tools plus prompts/resources/logging/completions/tasks.
- Tool list, inputs, side effects, and data sources.
- Whether the server must run inside an existing workspace.

Default to a tools-only stdio server when the user asks for a quick scaffold.

## Implementation Workflow

1. Inspect any existing Rust project before adding files.
2. Pin `rmcp` to the latest compatible major version or the user's requested version.
3. Use RMCP 1.x service patterns: implement a clonable service, register tools with macros, and run it with `ServiceExt::serve`.
4. Keep stdout clean for stdio servers. Route logs to stderr.
5. Make tool parameters strongly typed with `serde::Deserialize` and `schemars::JsonSchema`; keep schemas host-compatible (flat structs or `#[schemars(inline)]` — see `references/tool-schemas-and-output.md`).
6. Add tool annotations for every public tool so clients have risk and UX hints.
7. Return structured MCP results (`Json<T>` with a useful text fallback) when the output has machine-readable shape.
8. Add focused tests for parameter validation, tool behavior, and handler routing.
9. Add README instructions for `cargo run`, Inspector, and host configuration.
10. Run `cargo fmt`, `cargo test`, and `cargo check` when feasible.

## Design Defaults

- Prefer `edition = "2024"` for new crates unless the surrounding workspace uses another edition.
- Prefer `rmcp = { version = "1", features = ["server", "macros", "transport-io"] }` for stdio servers after confirming current docs.
- For clients, prefer `rmcp = { version = "1", features = ["client", "transport-child-process"] }` after confirming current docs.
- Use `tracing_subscriber` with `with_writer(std::io::stderr)` for stdio.
- Keep tools small and composable; split API clients, parsing, and business logic into ordinary Rust modules.
- Avoid global mutable state. Use a clonable server struct with `Arc`, `Mutex`, or `RwLock` only when shared state is necessary.
- Use `ErrorData` for protocol-facing errors and `anyhow` or domain errors internally.

## Safety Rules

- Never write logs or progress messages to stdout in stdio mode.
- Do not expose filesystem, shell, network, or credential operations without clear user-facing tool descriptions and narrow inputs.
- Treat tool annotations as hints, not security boundaries.
- Validate and normalize all external inputs before side effects.
- Do not scaffold deprecated SSE transport unless the user explicitly needs compatibility with an old host; prefer Streamable HTTP for HTTP-based MCP.

## Verification Checklist

Before handing off:

- `Cargo.toml` uses current `rmcp` features for the chosen transport.
- The server starts through `service.serve(transport).await?` and waits with `service.waiting().await?`.
- Tool schemas come from typed structs, not unvalidated `serde_json::Value`.
- Public tools include appropriate `ToolAnnotations`.
- The stdio server logs only to stderr.
- The README includes an Inspector command.
- Tests cover at least one successful call and one invalid-input or error path.
- Tool input and output schemas contain no `$defs`/`$ref` unless every target host accepts them.
- Structured outputs declare an output schema and keep a useful text fallback in `content`.
- Clients advertise only implemented capabilities; bridges isolate downstream sessions and never forward downstream tokens upstream.
