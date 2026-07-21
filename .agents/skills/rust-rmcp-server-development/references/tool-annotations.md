# Tool Annotations

Use this reference before defining or reviewing public RMCP tools.

Verified on 2026-06-09 against:

- MCP specification `2025-11-25` schema reference.
- `rmcp` 1.7.0 docs for `rmcp::model::ToolAnnotations`.
- `rmcp` 1.7.0 docs for the `#[tool]` macro.

## What They Are

Tool annotations are optional behavioral hints attached to a tool definition. They help clients decide how to display, confirm, retry, and risk-rank a tool.

They are not security guarantees. Clients must treat annotations from untrusted servers as untrusted hints.

## Standard Fields

| MCP JSON field | RMCP field | Default | Use when |
| --- | --- | --- | --- |
| `title` | `title` | none | The UI should show a human-readable name instead of the programmatic tool name. |
| `readOnlyHint` | `read_only_hint` | `false` | The tool does not modify files, databases, remote services, process state, or other environment state. |
| `destructiveHint` | `destructive_hint` | `true` | The tool may delete, overwrite, spend money, send messages, mutate permissions, or otherwise make destructive changes. Meaningful only when `readOnlyHint == false`. |
| `idempotentHint` | `idempotent_hint` | `false` | Repeating the same call with the same arguments has no additional effect after the first success. Meaningful only when `readOnlyHint == false`. |
| `openWorldHint` | `open_world_hint` | `true` | The tool may interact with external systems, untrusted content, or an open-ended world such as the web, email, Slack, GitHub, or arbitrary APIs. Use `false` for closed-domain local calculations or bounded internal state. |

The defaults are intentionally cautious. An unannotated tool is assumed to be mutating, potentially destructive, non-idempotent, and open-world.

## RMCP Macro Syntax

Use the `annotations(...)` argument on `#[tool]`:

```rust
use rmcp::{handler::server::wrapper::Parameters, schemars, tool};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReadFileParams {
    path: String,
}

#[tool(
    name = "read_file",
    description = "Read a file from the allowed project root",
    annotations(
        title = "Read file",
        read_only_hint = true,
        open_world_hint = false
    )
)]
async fn read_file(&self, Parameters(params): Parameters<ReadFileParams>) -> String {
    self.read_file_contents(params.path).await
}
```

For a mutating but non-destructive, idempotent operation:

```rust
#[tool(
    name = "ensure_label",
    description = "Ensure an issue has a label",
    annotations(
        title = "Ensure issue label",
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = true
    )
)]
async fn ensure_label(&self, Parameters(params): Parameters<EnsureLabelParams>) -> String {
    self.github.ensure_label(params.issue, params.label).await
}
```

For a destructive operation:

```rust
#[tool(
    name = "delete_file",
    description = "Delete a file from the allowed project root",
    annotations(
        title = "Delete file",
        read_only_hint = false,
        destructive_hint = true,
        idempotent_hint = false,
        open_world_hint = false
    )
)]
async fn delete_file(&self, Parameters(params): Parameters<DeleteFileParams>) -> String {
    self.delete_file(params.path).await
}
```

## Builder Syntax

When constructing `Tool` manually, use `ToolAnnotations` and `with_annotations` or `annotate`:

```rust
use rmcp::model::{Tool, ToolAnnotations};

let tool = Tool::new("status", "Get server status", schema)
    .with_annotations(
        ToolAnnotations::new()
            .with_title("Server status")
            .read_only(true)
            .open_world(false),
    );
```

Because `ToolAnnotations` is non-exhaustive, prefer its constructors and builder methods instead of struct literals.

## Classification Rules

Use these defaults when deciding annotations:

- Pure calculation: `readOnlyHint: true`, `openWorldHint: false`.
- Local read from an allowed root: `readOnlyHint: true`, `openWorldHint: false`.
- Web/API search or fetch: `readOnlyHint: true`, `openWorldHint: true`.
- Create-only operation: `readOnlyHint: false`, `destructiveHint: false`, set `idempotentHint` based on whether duplicates are prevented.
- Upsert or ensure operation: `readOnlyHint: false`, `destructiveHint: false`, often `idempotentHint: true`.
- Delete, overwrite, send, publish, charge, or permission change: `readOnlyHint: false`, usually `destructiveHint: true`.

## Gotchas

- Do not use annotations as authorization. Validate permissions and inputs in code.
- Do not mark a tool `readOnlyHint: true` if it writes caches, updates access timestamps in a meaningful backend, sends analytics, or mutates remote state.
- Do not mark a tool idempotent merely because duplicate calls usually fail. Idempotent means repeated successful calls have no additional effect.
- Do not set `openWorldHint: false` for tools that read attacker-controlled or internet-controlled content.
- Do not rely on annotations to prevent prompt injection. They help clients reason about risk; they do not harden tool output.
