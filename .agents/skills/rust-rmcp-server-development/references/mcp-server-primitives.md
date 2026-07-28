# MCP Server Primitives

Use this reference when deciding whether an MCP server capability should be exposed as a tool, resource, prompt, or some combination of them.

Verified on 2026-06-09 against:

- MCP specification `2025-11-25` server features: tools, resources, prompts.
- MCP specification `2025-11-25` lifecycle and Streamable HTTP transport.
- MCP specification `2025-11-25` client features: roots, sampling, elicitation.
- MCP specification `2025-11-25` task utility.
- `rmcp` 1.7.0 docs for `ServerHandler`, `#[tool]`, `#[tool_router]`, `#[prompt]`, `#[prompt_router]`, and `#[prompt_handler]`.
- `rmcp` 1.7.0 docs for `ServerCapabilities`, `ClientCapabilities`, `InitializeRequestParam`, and `InitializeResult`.
- `rmcp` 1.7.0 docs for `Peer<RoleServer>` methods such as `list_roots`, `create_message`, `create_elicitation`, `notify_progress`, and `supports_sampling_tools`.

## Quick Decision Rule

```text
Tool     = do something
Resource = read something
Prompt   = guide how to do something
```

Many real servers use all three. Do not force everything into tools just because tools are the most visible primitive.

Read `mcp-runtime-utilities.md` for the runtime layer around these primitives: capability negotiation, list change notifications, resource subscriptions, pagination, completion, logging, progress, cancellation, ping, tasks, authorization, timeouts, and bridge behavior.

## Lifecycle And Capabilities

These are session primitives, not server content primitives. They decide which features may be used after initialization.

Initialization negotiates:

- Protocol version.
- Client capabilities.
- Server capabilities.
- Client and server implementation metadata.

Client capabilities describe what the server may ask the client to do:

- `roots`: client can expose filesystem roots.
- `sampling`: server can request LLM sampling through the client.
- `elicitation`: server can ask the client/user for additional input.
- `tasks`: client supports task-augmented client-side requests.
- `experimental`: client supports non-standard features.

Server capabilities describe what the server exposes:

- `tools`: callable actions.
- `resources`: readable context.
- `prompts`: reusable prompt templates.
- `logging`: structured server log messages.
- `completions`: argument autocompletion.
- `tasks`: task-augmented server requests.
- `experimental` and `extensions`: non-standard or extension capabilities.

RMCP mapping:

- Return capabilities from `ServerHandler::get_info()`.
- Use `ServerCapabilities::builder()` instead of constructing non-exhaustive structs directly.
- Enable only features that the server actually implements, such as `enable_tools()`, `enable_resources()`, `enable_prompts()`, or task support.
- Negotiation runtime rules, HTTP headers, sub-capabilities, and capability gating: read `mcp-runtime-utilities.md`.

Example:

```rust
use rmcp::{ServerHandler, model::*};

impl ServerHandler for ProjectServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_tool_list_changed()
                .build(),
            server_info: Implementation::from_build_env(),
            ..Default::default()
        }
    }
}
```

## Client Features As Dependencies

Roots, sampling, and elicitation are client features. A server does not expose them the way it exposes tools, resources, and prompts. Instead, the server may request them from the client after capability negotiation.

Use them as dependencies only when they add real value:

- `roots`: ask the client which filesystem roots are available before reading, writing, indexing, or listing project files.
- `sampling`: ask the client to run an LLM generation when the server needs model reasoning inside a server workflow.
- `elicitation`: ask the client/user for missing input, confirmation, or an out-of-band action.
- `tasks`: wrap long-running or interruptible requests so status, cancellation, polling, deferred results, and `input_required` states are explicit.

How they relate to server primitives:

- Tools often consume roots, sampling, elicitation, and tasks because tools are where work happens.
- Resources should use roots to decide which readable URIs are valid and should avoid reading outside negotiated boundaries.
- Prompts can guide the host/model to use tools and resources, but server-side sampling is the server asking the client model to generate content directly.
- Tasks can wrap `tools/call`, and client-side task support can wrap `sampling/createMessage` or `elicitation/create`.

RMCP mapping:

- Use the remote peer from request context to call client features from server handlers.
- Use `Peer<RoleServer>::list_roots()` for client roots.
- Use `Peer<RoleServer>::create_message()` for sampling and `supports_sampling_tools()` before sending sampling requests with tools.
- Use `Peer<RoleServer>::create_elicitation()`, `elicit()`, or `elicit_url()` for user input flows.
- Use `Peer<RoleServer>::notify_progress()` for active request feedback.
- Use task support only when the negotiated `TasksCapability` supports the request type.

Design rules:

- Check capability support before each client-feature request.
- Keep roots as a boundary, not a permission bypass; validate all paths against roots anyway.
- Use sampling for bounded server-side reasoning, not as a replacement for a normal tool result.
- Use form elicitation for non-sensitive structured input and URL elicitation for secrets, credentials, payment, OAuth, or other sensitive flows.
- Prefer task `input_required` when a long-running task pauses for elicitation rather than blocking silently.

## Tools

Tools are executable functions the model can call through `tools/list` and `tools/call`.

Use tools for:

- Querying APIs, databases, search indexes, or local state.
- Creating, updating, deleting, sending, publishing, or triggering workflows.
- Running calculations, transforms, validations, or analysis.
- Producing fresh results that depend on parameters.

Avoid tools for:

- Static context that can be read as a resource.
- Reusable instructions that are better exposed as prompts.
- Large unbounded reads where a resource URI is easier to inspect and cache.

Progress and tasks:

- Use progress notifications for long-running tool calls that need live status, such as indexing, imports, search, or multi-step API work.
- Use tasks when the tool call needs a durable async lifecycle with polling, cancellation, deferred results, or state that outlives the original request.
- A task can wrap a tool call and emit progress; progress alone is just feedback for an active request.
- Do not model progress as a separate tool unless the user needs an explicit query operation such as `get_job_status`.

RMCP mapping:

- Define tool inputs as typed structs deriving `serde::Deserialize` and `schemars::JsonSchema`.
- Wrap input structs with `Parameters<T>`.
- Use `#[tool]` on handler methods.
- Use `#[tool_router(server_handler)]` for tools-only servers.
- Use explicit `#[tool_handler] impl ServerHandler for MyServer` when combining tools with prompts, resources, or tasks.
- Add `annotations(...)` on every public tool.
- Return `String` for simple text, `Result<Json<T>, McpError>` for structured output with a generated output schema (see `tool-schemas-and-output.md`), or `Result<CallToolResult, McpError>` for full control.

Minimal shape:

```rust
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchParams {
    query: String,
    limit: Option<u32>,
}

#[derive(Clone)]
struct SearchServer;

#[tool_router(server_handler)]
impl SearchServer {
    #[tool(
        name = "search",
        description = "Search indexed project documents",
        annotations(title = "Search documents", read_only_hint = true, open_world_hint = false)
    )]
    async fn search(&self, Parameters(params): Parameters<SearchParams>) -> String {
        format!("searching for {}", params.query)
    }
}
```

## Resources

Resources are readable context exposed through `resources/list`, `resources/templates/list`, and `resources/read`.

Use resources for:

- File contents, generated reports, schemas, logs, configs, and documentation.
- Stable or addressable data that benefits from a URI.
- Context the model may browse before deciding which tool to call.
- Large data that should be listed first and read selectively.

Avoid resources for:

- Operations with side effects.
- Parameterized computation where the result is not naturally a readable object.
- Instructions or workflows that belong in prompts.

RMCP mapping:

- Advertise resources with `ServerCapabilities::builder().enable_resources()`.
- Implement `list_resources` for concrete resources.
- Implement `list_resource_templates` for URI patterns such as `repo://file/{path}`.
- Implement `read_resource` to resolve a URI into `ReadResourceResult`.
- Use `Resource`, `RawResource`, `ResourceTemplate`, `RawResourceTemplate`, `ResourceContents`, and related model types.
- Validate URI schemes and paths before reading anything.
- Emit resource list changed notifications when available resources change.

Implementation shape:

```rust
use rmcp::{ErrorData as McpError, ServerHandler, model::*};

impl ServerHandler for ProjectServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_resources().build(),
            ..Default::default()
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            resources: vec![RawResource::new("project://readme", "Project README").into()],
            next_cursor: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        self.read_project_resource(request.uri).await
    }
}
```

Check current `rmcp` docs before using exact constructors; model types are still evolving.

## Prompts

Prompts are reusable message templates exposed through `prompts/list` and `prompts/get`.

Use prompts for:

- Repeatable workflows such as review, triage, migration planning, investigation, or release notes.
- Server-owned instructions that should stay consistent across clients.
- Prompt templates that accept typed arguments and return one or more messages.
- Workflows that combine instructions with resource references or tool-use strategy.

Avoid prompts for:

- Actions that should execute immediately.
- Raw data that should be a resource.
- Tool descriptions. A prompt can guide tool usage, but it should not replace clear tool metadata.

RMCP mapping:

- Define prompt arguments as typed structs deriving `Deserialize` and `JsonSchema`.
- Use `#[prompt]` on prompt methods.
- Use `#[prompt_router]` to generate a prompt router.
- Use `#[prompt_handler] impl ServerHandler for MyServer`.
- Enable prompt capability with `ServerCapabilities::builder().enable_prompts()`.
- Return `Vec<PromptMessage>`, `GetPromptResult`, or `Result<T, McpError>`.
- Notify clients when available prompts change.

Implementation shape:

```rust
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::prompt::PromptRouter, wrapper::Parameters},
    model::*,
    prompt, prompt_handler, prompt_router, schemars,
};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReviewArgs {
    focus: Option<Vec<String>>,
}

#[derive(Clone)]
struct ReviewServer {
    prompt_router: PromptRouter<Self>,
}

#[prompt_router]
impl ReviewServer {
    fn new() -> Self {
        Self { prompt_router: Self::prompt_router() }
    }

    #[prompt(name = "review_change", description = "Review a code change")]
    async fn review_change(
        &self,
        Parameters(args): Parameters<ReviewArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let focus = args.focus.unwrap_or_else(|| vec!["correctness".into()]);
        Ok(GetPromptResult {
            description: Some("Code review prompt".into()),
            messages: vec![PromptMessage::new_text(
                PromptMessageRole::User,
                format!("Review this change for: {}", focus.join(", ")),
            )],
        })
    }
}

#[prompt_handler]
impl ServerHandler for ReviewServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_prompts().build(),
            ..Default::default()
        }
    }
}
```

## Combination Patterns

Use these common designs:

- API server: tools for API calls, resources for cached schemas or object snapshots, prompts for common investigation workflows.
- Filesystem server: resources for file reads, tools for write/delete/search operations, prompts for review or summarization flows.
- Database server: resources for schemas and table metadata, tools for safe queries, prompts for query writing or performance investigation.
- Documentation server: resources for docs pages, tools for search, prompts for synthesis or migration plans.
- Automation server: tools for actions, resources for current state, prompts for operator runbooks.

## Design Checklist

- Prefer a resource when the model needs context before deciding what to do.
- Prefer a tool when the server must execute behavior or compute fresh results.
- Prefer a prompt when the value is reusable task guidance.
- Expose capabilities only for primitives the server actually implements.
- Keep descriptions explicit; the model chooses primitives largely from names and descriptions.
- Test at least one list/read/get/call path for every primitive the server exposes.
