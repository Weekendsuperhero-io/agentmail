# Tool Schemas And Structured Output

Use this reference before finalizing tool parameter or output structs, when returning structured output, or when a host drops, rejects, or mishandles a tool schema.

Verified on 2026-06-09 against:

- `rmcp` 1.7.0 source: `handler/server/common.rs`, `handler/server/wrapper/json.rs`, `model.rs`, and its `schemars = "1.0"` dependency (resolved 1.2.1).
- MCP specification 2025-11-25 (JSON Schema 2020-12 as default dialect, per SEP-1613).
- `modelcontextprotocol/rust-sdk` `examples/servers/src/structured_output.rs`.
- Host incompatibility reports: `google-gemini/gemini-cli` issue 13326, `n8n-io/n8n` issue 25964.

## How RMCP Produces Schemas

- `inputSchema` comes from `schemars::JsonSchema` derived on the struct inside `Parameters<T>`.
- `outputSchema` plus `structuredContent` come from returning `Json<T>` where `T: serde::Serialize + schemars::JsonSchema`.
- RMCP generates every schema with `SchemaSettings::draft2020_12()` and caches it per type. There is no public hook to change generator settings; control the emitted schema through struct shape and `schemars` attributes.

```rust
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::ErrorData,
    schemars, tool,
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct WeatherParams {
    /// City name to look up.
    pub city: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct WeatherReport {
    pub temperature_c: f64,
    pub summary: String,
}

#[tool(description = "Get current weather for a city")]
async fn get_weather(
    &self,
    Parameters(params): Parameters<WeatherParams>,
) -> Result<Json<WeatherReport>, ErrorData> {
    Ok(Json(WeatherReport {
        temperature_c: 21.0,
        summary: format!("Clear skies in {}", params.city),
    }))
}
```

Behavior to rely on:

- `Json<T>` places the serialized value in `structuredContent` and also fills `content` with the same JSON as a text block, so hosts that only render content blocks still see the data.
- `outputSchema` must have root type `object`. RMCP rejects `Json<Vec<_>>`, `Json<String>`, and other non-object roots; wrap them in a struct with named fields.
- Field doc comments become schema `description` values. Write them for the model.
- Do not add a direct `schemars` dependency. Use rmcp's re-export (`use rmcp::schemars;`) — it carries the exact version rmcp was built against, so derives, `#[schemars(inline)]`, and `schema_for!` cannot drift from the SDK (compile-verified without a direct dependency).

## The Compatibility Caveat

RMCP emits valid JSON Schema 2020-12, which is exactly what the MCP spec requires. The problem is downstream: when a parameter or output struct references other named types, schemars factors those types into `$defs` entries referenced by `$ref` — and several real hosts, clients, and bridges mishandle that:

- Gemini CLI rejected tool schemas containing `$defs`/`$ref` (gemini-cli issue 13326).
- n8n discarded `$defs` and `$ref` when importing MCP tool schemas, silently breaking validation (n8n issue 25964).
- Assorted bridges and smaller hosts strip keywords they do not understand.

This is a host-side validation gap, not an RMCP or schemars bug, but it becomes your bug at integration time. Symptoms:

- The tool never appears in the host's tool list.
- The model sees parameters as untyped or missing and sends malformed arguments.
- Valid calls fail host-side schema validation.

The same applies to `outputSchema`, and to any shared Rust types you also export to TypeScript: the TS export is unaffected, but the schemars output for those nested types still carries `$defs`.

## Keeping Schemas Host-Compatible

In order of preference:

1. **Keep tool parameter and output structs flat.** Primitives, `Option<T>` of primitives, `Vec<T>` of primitives, and unit-variant enums generate self-contained schemas with no `$defs`.
2. **Inline nested types with `#[schemars(inline)]`.** When nesting is genuinely useful, mark the nested type so schemars expands it in place instead of emitting `$defs`/`$ref`:

   ```rust
   #[derive(serde::Serialize, schemars::JsonSchema)]
   #[schemars(inline)]
   pub struct DashboardRow {
       pub label: String,
       pub value: f64,
   }

   #[derive(serde::Serialize, schemars::JsonSchema)]
   pub struct Dashboard {
       pub title: String,
       pub rows: Vec<DashboardRow>, // inlined, no $ref
   }
   ```

3. **Hand-write the schema only as a last resort.** If a type cannot be restructured, construct the `Tool` manually with a hand-written `inputSchema` instead of the macro-generated one.
4. **Look at the real wire output before shipping.** Inspector shows the exact `tools/list` JSON. A regression test keeps it that way:

   ```rust
   #[test]
   fn tool_schemas_stay_ref_free() {
       use rmcp::schemars;
       // schemars 1.x defaults to draft 2020-12, matching RMCP's settings.
       let schema = serde_json::to_string(&schemars::schema_for!(Dashboard)).unwrap();
       assert!(!schema.contains("$ref"), "schema regressed to $ref: {schema}");
   }
   ```

Decide per target host: if every host you support handles `$defs` (Claude and the official SDK-based hosts do), nested schemas are fine. If the host set is open-ended — especially behind a bridge — stay flat or inline.

## structuredContent Versus content

- `content` is for the model and for hosts that only display content blocks. Always useful text, never empty.
- `structuredContent` is for machine consumers: UI views, schema-validating clients, downstream automation. Emit it whenever the output has machine-readable shape, via `Json<T>`.
- The automatic text fallback from `Json<T>` is the raw JSON string. When the model needs a readable summary instead, build the result yourself:

  ```rust
  let mut result = rmcp::model::CallToolResult::structured(serde_json::to_value(&report)?);
  result.content = vec![rmcp::model::Content::text(format!(
      "{}: {:.1} C", report.summary, report.temperature_c
  ))];
  Ok(result)
  ```

- Error results follow the same split: `CallToolResult::structured_error(value)` for machine-readable failures, `ErrorData` only for protocol-level errors.

## Gotchas

- `Option<T>` fields become type unions with `null` in 2020-12 output. Most hosts accept this; strict integrations may not. Test before relying on optional fields.
- Data-carrying enums generate `oneOf` with `$defs`. For tool inputs across unknown hosts, prefer a flat `kind` field plus optional payload fields, or `#[schemars(inline)]` on every variant payload type.
- `serde` renames apply to the schema. Keep `#[serde(rename_all = "...")]` consistent with what the TypeScript side expects.
- Elicitation schemas are stricter than tool schemas: flat objects of primitive fields only. RMCP enforces this through the `elicit_safe!` marker macro; do not reuse complex tool types for elicitation.
- Never expose raw RMCP model types as your tool contract; wrap them in your own structs so SDK upgrades cannot change your wire format.
