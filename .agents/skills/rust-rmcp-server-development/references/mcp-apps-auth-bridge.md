# MCP Apps And Auth Bridge

Use this reference when an RMCP server needs interactive UI through MCP Apps, ext-auth authorization extensions, or TypeScript UI contracts.

For full guidance, use the sibling skill `mcp-apps-auth-rmcp-development`; its `references/rmcp-bridge.md` is this file's counterpart on the apps side.

## RMCP Summary

RMCP can participate in MCP Apps and ext-auth at the protocol level, but current official helper coverage differs from TypeScript:

- TypeScript has `@modelcontextprotocol/ext-apps/server` helpers for registering app tools and UI resources.
- RMCP exposes core MCP model types and metadata paths; implement MCP Apps wire-format metadata manually unless newer RMCP docs add helpers.
- ext-auth is mostly HTTP authorization, capability extension negotiation, and request principal propagation; keep it outside tool arguments.

## MCP Apps In RMCP

Implement:

- Tool `_meta.ui.resourceUri`.
- `ui://` resource URIs.
- `text/html;profile=mcp-app` resource content.
- `_meta.ui.csp`, `_meta.ui.domain`, `_meta.ui.permissions`, and `_meta.ui.prefersBorder` where needed.
- Tool visibility such as `["app"]` for UI-only actions.
- Useful non-UI fallback `content`.

Check current RMCP docs for exact constructors and metadata APIs before coding.

## Auth In RMCP

For HTTP deployments:

- Validate bearer tokens before MCP request handling.
- Bind principal, scopes, resource/audience, and auth extension support into request state.
- Enforce authorization again inside tools and resources.
- Never expose raw tokens in tool input, tool output, resources, prompts, UI HTML, logs, or `structuredContent`.

Core OAuth mechanics (RFC 9728 discovery, audience validation, server middleware) and the `rmcp` `auth` feature: read `http-authorization.md`.

For bridges:

- Separate downstream user identity from upstream credentials.
- Scope app state, UI resource URIs, task IDs, cursors, and auth tokens per downstream session.
- Avoid forwarding downstream bearer tokens upstream unless that is explicitly the intended model.

## ts-rs Bridge

Use `ts-rs` when Rust owns the tool and UI data contracts:

- Derive `serde::Serialize`, `serde::Deserialize`, `schemars::JsonSchema`, and `ts_rs::TS` on app boundary types.
- Use `schemars` for MCP schemas and `ts-rs` for TypeScript declarations.
- Export tool inputs, `structuredContent`, app-only tool contracts, and stable resource data types.
- Keep generated TypeScript files in the UI package or document the generation command.
- Shared contract types also feed `schemars`; keep them flat or `#[schemars(inline)]` so tool schemas stay host-compatible (see `tool-schemas-and-output.md`).
