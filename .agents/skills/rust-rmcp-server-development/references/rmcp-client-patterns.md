# RMCP Client Patterns

Use this reference when building an MCP client, host, or test harness in Rust, when spawning MCP servers from Rust code, or when handling server-initiated sampling, elicitation, or roots requests. For the client half of a gateway, read this first, then `gateway-patterns.md`.

Verified on 2026-06-09 against:

- `rmcp` 1.7.0 source: `service/client.rs` (`Peer<RoleClient>` request methods), `handler/client.rs` (`ClientHandler` trait), `transport/child_process.rs`, `model/capabilities.rs`.
- `modelcontextprotocol/rust-sdk` examples: `examples/clients/src/everything_stdio.rs`, `examples/clients/src/sampling_stdio.rs`, `examples/clients/src/streamable_http.rs`.

## Cargo Features

```toml
[dependencies]
rmcp = { version = "1", features = ["client", "transport-child-process"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "process"] }
```

- `client` is not a default feature; defaults are `server`, `macros`, `base64`.
- `transport-child-process` enables `TokioChildProcess` for spawning stdio servers.
- `transport-streamable-http-client-reqwest` enables the Streamable HTTP client.
- `auth` enables the OAuth client machinery (read `http-authorization.md`).

## Minimal Client

`()` implements `ClientHandler`, so a client that never receives server-initiated requests needs no handler type:

```rust
use rmcp::{
    ServiceExt,
    model::CallToolRequestParams,
    object,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use tokio::process::Command;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = ()
        .serve(TokioChildProcess::new(Command::new("npx").configure(|cmd| {
            cmd.arg("-y").arg("@modelcontextprotocol/server-everything");
        }))?)
        .await?;

    // Server identity and negotiated capabilities, available after initialize.
    let server_info = client.peer_info();
    tracing::info!(?server_info, "connected");

    let tools = client.list_all_tools().await?;
    tracing::info!(count = tools.len(), "tools listed");

    let result = client
        .call_tool(
            CallToolRequestParams::new("echo").with_arguments(object!({ "message": "hello" })),
        )
        .await?;
    tracing::info!(content = ?result.content, "tool returned");

    client.cancel().await?;
    Ok(())
}
```

`serve` performs the full initialize handshake and returns `RunningService<RoleClient, _>`, which exposes the request methods directly and the underlying `Peer<RoleClient>` via `.peer()`.

## Requests A Client Can Send

From `Peer<RoleClient>`:

- Tools: `list_tools(Option<PaginatedRequestParams>)`, `call_tool`, plus `list_all_tools()` which follows cursors to exhaustion.
- Prompts: `list_prompts`, `get_prompt`, `list_all_prompts`.
- Resources: `list_resources`, `read_resource`, `list_resource_templates`, `subscribe`, `unsubscribe`, plus `list_all_resources` and `list_all_resource_templates`.
- Completion: `complete`, with helpers `complete_prompt_simple`, `complete_resource_simple`, `complete_prompt_argument`, `complete_resource_argument`.
- Logging: `set_level` to choose the minimum server log level.
- Notifications out: `notify_cancelled`, `notify_progress`, `notify_roots_list_changed`.

Treat cursors from `list_*` as opaque; pass them back verbatim or use the `list_all_*` helpers.

## Spawning Stdio Servers

`TokioChildProcess::new` accepts a `tokio::process::Command`; `ConfigureCommandExt::configure` keeps construction inline:

```rust
let transport = TokioChildProcess::new(Command::new("cargo").configure(|cmd| {
    cmd.arg("run").arg("-p").arg("my-mcp-server").env("RUST_LOG", "info");
}))?;
```

The child's stdout/stdin become the MCP channel. Leave the child's stderr alone (or pipe it to your logs); never write to its stdin outside the transport.

## Streamable HTTP Client

```rust
use rmcp::transport::StreamableHttpClientTransport;

let transport = StreamableHttpClientTransport::from_uri("http://127.0.0.1:8000/mcp");
let client = ().serve(transport).await?;
```

The transport manages the `MCP-Protocol-Version` and `Mcp-Session-Id` headers and SSE stream handling. For servers that require OAuth, wrap the HTTP client with the `auth` feature's `AuthClient` instead of injecting headers by hand — read `http-authorization.md`.

## Handling Server-Initiated Requests

Servers may call back into the client for sampling, roots, and elicitation. Implement the matching `ClientHandler` methods and advertise only the capabilities you actually implement — the mirror of the server-side rule:

```rust
use rmcp::{
    ClientHandler, RoleClient,
    model::{
        ClientCapabilities, ClientInfo, CreateElicitationRequestParams,
        CreateElicitationResult, CreateMessageRequestParams, CreateMessageResult,
        ElicitationAction, ErrorData, Implementation, ListRootsResult,
    },
    service::RequestContext,
};

#[derive(Debug, Clone)]
struct MyClient;

impl ClientHandler for MyClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::builder()
                .enable_roots()
                .enable_roots_list_changed()
                .enable_sampling()
                .build(),
            Implementation::from_build_env(),
        )
    }

    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _ctx: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, ErrorData> {
        // Route to your LLM. Respect params.max_tokens and model_preferences,
        // and require human approval before honoring sampling requests.
        Err(ErrorData::internal_error("no model wired up", None))
    }

    async fn list_roots(
        &self,
        _ctx: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        // Return the file:// URIs the server is allowed to touch.
        Ok(ListRootsResult::default())
    }

    async fn create_elicitation(
        &self,
        params: CreateElicitationRequestParams,
        _ctx: RequestContext<RoleClient>,
    ) -> Result<CreateElicitationResult, ErrorData> {
        // Present params.message and the requested schema to the user.
        Ok(CreateElicitationResult {
            action: ElicitationAction::Decline,
            content: None,
            meta: None,
        })
    }
}
```

Notification hooks are available on the same trait when you need them: `on_progress`, `on_logging_message`, `on_resource_updated`, `on_resource_list_changed`, `on_tool_list_changed`, `on_prompt_list_changed`, `on_cancelled`.

Rules:

- Sampling is a human-in-the-loop feature: surface the request to a user (or an explicit policy) before calling a model, and never echo secrets into the sampled prompt.
- When roots change, send `notify_roots_list_changed()`; servers re-fetch lazily.
- Keep handler bodies fast; long work inside a callback delays the rest of the session.

## Shutdown

- `client.waiting().await?` parks until the server closes the transport and returns the `QuitReason`.
- `client.cancel().await?` shuts the session down explicitly; for child processes this tears down the child as well.

## Gotchas

- Calling a capability the server never advertised (for example `subscribe` without `resources.subscribe`) yields a protocol error; gate on `client.peer_info()` capabilities instead of trying and catching.
- `list_all_*` helpers can issue many requests against large servers; prefer single pages in latency-sensitive paths.
- Stdio servers log to stderr. If you capture the child's stderr, drain it — a full pipe blocks the server.
- For OAuth-protected HTTP servers, the first request returns 401 with metadata discovery info; the `auth` feature handles the flow (read `http-authorization.md`).
