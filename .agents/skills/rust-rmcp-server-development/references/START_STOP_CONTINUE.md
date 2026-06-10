# Start, Stop, Continue: RMCP 1.5.0 Through 1.7.0

Use this reference when generating or modernizing RMCP server code. It distills the 1.5.0 through 1.7.0 changelog into implementation patterns.

Verified on 2026-06-09 against:

- `references/CHANGELOG.md` (rmcp 1.5.0 through 1.7.0; 1.7.0 confirmed latest on crates.io).

## Start

Adopt these because they come from new features or behavior changes.

- Start targeting the MCP `2025-11-25` protocol version when the client/server environment supports it.
- Start using constructors for non-exhaustive transport error types instead of constructing fields directly.
- Start validating HTTP `Origin` for Streamable HTTP servers.
- Start logging rejected `Host` and `Origin` checks in HTTP servers so deployment failures are diagnosable.
- Start using runtime tool disabling when a tool depends on permissions, credentials, feature flags, or unavailable backend services.
- Start adding an optional session store when Streamable HTTP resumability matters.
- Start configuring `init_timeout` for Streamable HTTP sessions instead of relying on implicit initialization timing.
- Start using HTTP/2 `:authority` as a fallback when normal `Host` handling is unavailable.
- Start using task-based stdio examples as the reference path for long-running tool work over stdio.
- Start testing malformed stdio input paths and expecting JSON-RPC `-32700` parse errors instead of process shutdown.

## Stop

Avoid these because recent fixes identify them as stale, fragile, or wrong.

- Stop treating resource metadata JSON parse failures as fatal when the SDK can degrade them to soft errors.
- Stop assuming stdio parse errors should close the transport or kill the server.
- Stop writing old or ambiguous `Parameters` examples; use the current `Parameters<T>` / `Parameters(...)` syntax from RMCP 1.x docs.
- Stop implementing Streamable HTTP without initialization timeouts.
- Stop assuming HTTP/1-only header behavior; HTTP/2 may provide `:authority` instead of `Host`.
- Stop ignoring rejected `Host` or `Origin` values during HTTP deployment debugging.
- Stop assuming SSE streams can be left undrained without affecting connection reuse.
- Stop treating idle timeout logs as high-severity operational errors unless the current SDK behavior indicates an actual failure.
- Stop enabling unnecessary default dependency features when the SDK has removed them, such as `chrono` defaults.

## Continue

Keep these patterns because the fixes reinforce them.

- Continue routing stdio logs to stderr and reserving stdout for MCP frames.
- Continue using typed parameter structs and the current `Parameters<T>` pattern so examples compile and schemas stay accurate.
- Continue validating HTTP boundary assumptions: `Host`, `Origin`, protocol version, session IDs, and initialization timing.
- Continue designing resource handling to be tolerant of bad optional metadata while still failing clearly on unreadable required content.
- Continue draining response streams where the transport requires it for reuse.
- Continue adding regression tests around protocol-edge behavior: parse errors, initialization timeouts, HTTP/2 headers, session resume, and malformed metadata.
- Continue keeping changelog-derived guidance separate from raw changelog text so generated code can use the guidance while humans can audit the source.

## Code Review Prompts

Use these questions during review:

- Does this server depend on Streamable HTTP? If yes, where are `Origin`, `Host` or `:authority`, session store, and `init_timeout` handled?
- Does this stdio server survive invalid JSON input with a protocol error instead of an abrupt shutdown?
- Are tool parameters written with current RMCP syntax and tested through at least one real handler call?
- Are optional metadata parse failures isolated from primary resource reads?
- Does the implementation need runtime disabling for tools that require credentials, permissions, or unavailable services?
