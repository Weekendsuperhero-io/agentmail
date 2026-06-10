# Outdated Generator Notes

Use this reference when comparing against `github/awesome-copilot`'s `rust-mcp-server-generator` skill or other old RMCP examples.

## Useful Ideas To Keep

The old generator had good product-level prompts:

- Ask for project name.
- Ask for server description.
- Ask for transport choice.
- Ask for the tools to include.
- Ask whether prompts and resources are required.
- Generate README, tests, and host configuration snippets.
- Include logging, typed parameters, state management, and integration testing.

Keep those ideas.

## Stale Or Broken Patterns To Avoid

Do not copy these patterns unless the user explicitly pins an old `rmcp` version:

- `rmcp = { version = "0.8.1", features = ["server"] }`
- Separate `rmcp-macros = "0.8"` dependency for ordinary macro use.
- `Server::builder().with_handler(...).build(transport)` for new RMCP 1.x servers.
- `StdioTransport::new()` for current stdio examples.
- Generic `sse` transport as the default HTTP option.
- Untyped or incorrect `Parameters` usage.
- Broken snippets such as `Arc>` without an inner type.
- Writing examples that do not compile before handoff.

## Migration Approach

When updating an old generated server:

1. Check the pinned `rmcp` version and decide whether to upgrade to current 1.x.
2. Replace transport setup with current `ServiceExt::serve` patterns.
3. Replace hand-written routers with `#[tool_router]` macros where appropriate.
4. Move tool inputs into typed structs deriving `Deserialize` and `JsonSchema`.
5. Route logs to stderr for stdio.
6. Update tests around actual current handler methods.
7. Run `cargo fmt`, `cargo test`, and `cargo check`.

## When Not To Upgrade

Do not force an upgrade if:

- The user's host requires a specific old protocol or SDK behavior.
- The project is frozen for compatibility testing.
- The user asks only for a minimal patch and upgrading would broaden the change.

In those cases, state that the code is intentionally staying on the old API and avoid mixing old and new patterns.
