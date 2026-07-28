# RMCP 1.x Patterns

Use this reference before writing Rust code that depends on the `rmcp` API.

Verified on 2026-06-09 against:

- `rmcp` docs.rs latest showed `rmcp` 1.7.0.
- `modelcontextprotocol/rust-sdk` releases listed `rmcp-v1.7.0` as latest.
- The workspace `Cargo.toml` on `main` used `rmcp = "1.7.0"`.

## Current Core Shape

For a simple tools-only stdio server, use the service pattern:

```rust
use anyhow::Result;
use rmcp::{
    handler::server::wrapper::Parameters,
    schemars, tool, tool_router,
    transport::stdio,
    ServiceExt,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AddParams {
    /// Left-hand number.
    a: i32,
    /// Right-hand number.
    b: i32,
}

#[derive(Debug, Clone)]
struct Calculator;

#[tool_router(server_handler)]
impl Calculator {
    #[tool(description = "Add two numbers")]
    fn add(&self, Parameters(AddParams { a, b }): Parameters<AddParams>) -> String {
        (a + b).to_string()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let service = Calculator.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
```

Use this pattern instead of old `Server::builder()`, `StdioTransport::new()`, or separate `rmcp-macros` wiring unless current docs for the pinned version require otherwise.

The service pattern above is the server role. For client and bridge processes, read `rmcp-client-patterns.md` and `bridge-patterns.md`.

## Cargo Dependencies

For a new stdio server, start with:

```toml
[package]
name = "example-mcp-server"
version = "0.1.0"
edition = "2024"

[dependencies]
rmcp = { version = "1", features = ["server", "macros", "transport-io"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt", "std"] }
```

Add domain dependencies only when the requested tools need them, such as `reqwest`, `sqlx`, `walkdir`, or `ignore`.

## Parameters

Use typed parameter structs:

```rust
#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
struct SearchParams {
    /// Query text to search for.
    query: String,
    /// Maximum number of results to return.
    limit: Option<u32>,
}
```

Prefer constraints in code and documentation:

- Normalize strings before use.
- Bound counts and sizes.
- Validate paths against allowed roots.
- Reject shell fragments instead of trying to sanitize them.

Schema caveat: nested or reused custom types in parameter structs become `$defs`/`$ref` entries that some hosts drop or reject. Keep parameter structs flat or mark nested types `#[schemars(inline)]`, and read `tool-schemas-and-output.md` before finalizing schemas.

## Tool Results

For simple text, returning `String` is acceptable.

For structured output, return `Result<Json<T>, ErrorData>`: RMCP serializes `T` into `structuredContent`, generates the `outputSchema`, and adds a JSON text fallback in `content`. Read `tool-schemas-and-output.md` for schema host-compatibility and result construction.

For full control over content blocks, return `Result<CallToolResult, ErrorData>` and include `Content::text(...)` plus optional structured content. Structured results should keep a useful text representation for clients that display only content blocks.

### Protocol Errors

Use `ErrorData` constructors for protocol-facing failures:

```rust
use rmcp::model::ErrorData;

if params.limit > 500 {
    return Err(ErrorData::invalid_params("limit must be <= 500", None));
}
// Also available: internal_error, invalid_request, resource_not_found, parse_error.
```

Reserve `ErrorData` for malformed requests and infrastructure failures. Failures the model should see and react to belong in a normal `CallToolResult` with `is_error: true`.

## Prompts And Resources

Use prompt macros when prompts are part of the requirement:

- `#[prompt_router]`
- `#[prompt]`
- `#[prompt_handler]`

Implement resources on `ServerHandler` when the server exposes readable data:

- `list_resources`
- `read_resource`
- `list_resource_templates` when URI patterns are dynamic

Enable only the capabilities the server actually supports in `get_info()`.

## Transports

Use `stdio()` for local desktop and coding-agent integrations.

Use Streamable HTTP only when the server must be reached over HTTP. Add the `transport-streamable-http-server` feature and follow the current SDK example for server setup, because this API has changed repeatedly.

Avoid new SSE-only scaffolds. SSE appears in old examples and host compatibility discussions, but current MCP HTTP guidance is Streamable HTTP.

## Logging

For stdio:

- Never use `println!`, `print!`, or stdout logging.
- Use `tracing` routed to stderr.
- Disable ANSI output unless the host explicitly supports it.

For HTTP:

- stdout logging is less dangerous, but structured logs are still preferred.

## Useful Commands

```bash
cargo fmt
cargo test
cargo check
npx @modelcontextprotocol/inspector cargo run
```

For a workspace package:

```bash
npx @modelcontextprotocol/inspector cargo run -p <package-name>
```
