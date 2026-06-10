# Scaffold Guidance

Use this reference when creating a new Rust RMCP project.

Verified on 2026-06-09 against:

- MCP specification `2025-11-25` tool naming guidance.
- `rmcp` 1.7.0 conventions (see `rmcp-1x-patterns.md`).

## Project Layout

Use this layout for a non-trivial server:

```text
project-name/
├── Cargo.toml
├── README.md
├── src/
│   ├── main.rs
│   ├── server.rs
│   ├── error.rs
│   ├── tools/
│   │   ├── mod.rs
│   │   └── example.rs
│   ├── prompts/
│   │   └── mod.rs
│   └── resources/
│       └── mod.rs
└── tests/
    └── smoke.rs
```

For a tiny tools-only server, it is acceptable to keep everything in `src/main.rs` initially.

## Generation Steps

1. Create or update `Cargo.toml`.
2. Create a clonable server type such as `AppServer`.
3. Add typed parameter structs near their tool implementations.
4. Register tools with `#[tool_router(server_handler)]` for tools-only servers.
5. Use explicit `ServerHandler` plus `#[tool_handler]`, `#[prompt_handler]`, or resource methods when the server has multiple capabilities.
6. Configure tracing to stderr in `main.rs`.
7. Add a README with install, run, Inspector, and host configuration examples.
8. Add tests that exercise pure tool logic separately from protocol wiring when possible.

## README Sections

Include:

- What the server exposes.
- Required environment variables and permissions.
- How to run locally.
- How to test with Inspector.
- Example MCP host configuration.
- Safety notes for tools with side effects.

## Host Configuration

Use absolute paths in client configs:

```json
{
  "mcpServers": {
    "project-name": {
      "command": "/absolute/path/to/project-name/target/release/project-name",
      "args": []
    }
  }
}
```

For development, prefer `cargo run` only when the host supports command plus arguments reliably:

```json
{
  "mcpServers": {
    "project-name": {
      "command": "cargo",
      "args": ["run", "--manifest-path", "/absolute/path/to/project-name/Cargo.toml"]
    }
  }
}
```

## Testing Strategy

Use three layers:

- Unit tests for ordinary Rust functions.
- Async tests for tool methods and validation.
- Manual or scripted Inspector tests for actual MCP handshake, tool listing, and tool calls.

For tools with side effects, add tests for refusal or validation paths:

- Invalid path outside allowed root.
- Missing environment variable.
- Invalid enum or mode.
- Request over configured maximum size.

## Review Checklist

- Does every tool name follow MCP naming guidance: 1-128 chars, no spaces, unique within server?
- Does every tool description explain side effects?
- Does every public tool include annotations for title, read-only/destructive behavior, idempotency, and open-world access?
- Do parameter descriptions make generated JSON Schema useful?
- Do parameter and output structs avoid `$defs`/`$ref` for the target hosts (flat structs or `#[schemars(inline)]`)?
- Are prompts and resources omitted unless requested?
- Is logging safe for stdio?
- Are old `rmcp` imports or builder APIs absent?
