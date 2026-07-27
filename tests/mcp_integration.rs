//! End-to-end MCP protocol tests over an in-process duplex transport.
//!
//! Each test spins up the real server via `serve_on` with an in-memory config
//! and speaks raw newline-delimited JSON-RPC — the same frames a host sends —
//! so these tests cover the wire format hosts actually see. The dummy account
//! is never connected; only config-backed tools and validation paths run, so
//! no network is touched.

use std::time::Duration;

use agentmail::secret::Secret;
use agentmail::{AccountConfig, Agentmail, Config};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

struct McpClient {
    /// Notifications seen while awaiting responses (see `request`).
    notifications: Vec<Value>,
    reader: BufReader<ReadHalf<tokio::io::DuplexStream>>,
    writer: WriteHalf<tokio::io::DuplexStream>,
    next_id: i64,
    init_result: Value,
}

impl McpClient {
    async fn start() -> Self {
        let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
        let config = Config::from_accounts(vec![(
            "dummy".to_string(),
            AccountConfig {
                host: "imap.invalid".to_string(),
                port: 993,
                username: "dummy@example.invalid".to_string(),
                email: None,
                aliases: Vec::new(),
                password: Some(Secret::new_raw("unused")),
                tls: true,
                max_connections: None,
                auth: agentmail::AuthMethod::Password,
            },
        )]);
        tokio::spawn(async move {
            let _ = agentmail::mcp::serve_on(server_io, Agentmail::new(config)).await;
        });
        let (r, w) = tokio::io::split(client_io);
        let mut client = Self {
            notifications: Vec::new(),
            reader: BufReader::new(r),
            writer: w,
            next_id: 0,
            init_result: Value::Null,
        };
        let resp = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "agentmail-tests", "version": "0"}
                }),
            )
            .await;
        client.init_result = resp["result"].clone();
        client.notify("notifications/initialized", json!({})).await;
        client
    }

    async fn notify(&mut self, method: &str, params: Value) {
        let mut line =
            serde_json::to_string(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
                .unwrap();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    /// Send a request, then read frames (skipping notifications) until the
    /// matching response id arrives. Capped at 10s to keep CI from hanging.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let mut line = serde_json::to_string(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .unwrap();
        line.push('\n');
        tokio::time::timeout(Duration::from_secs(10), async {
            self.writer.write_all(line.as_bytes()).await.unwrap();
            loop {
                let mut buf = String::new();
                let n = self.reader.read_line(&mut buf).await.unwrap();
                assert!(n > 0, "server closed stream awaiting `{method}` response");
                let value: Value = serde_json::from_str(&buf).unwrap();
                if value.get("id").and_then(Value::as_i64) == Some(id) {
                    return value;
                }
                // BUFFER, don't drop. A fast task can publish its terminal
                // status before the enqueue response is even written, so a
                // test that only reads AFTER the response would miss the
                // notification entirely and wrongly conclude the server never
                // pushes.
                if value.get("id").is_none() {
                    self.notifications.push(value);
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timeout waiting for `{method}` response"))
    }
}

impl McpClient {
    /// Read frames until a NOTIFICATION with `method` arrives, discarding
    /// responses along the way.
    ///
    /// `request` deliberately SKIPS notifications, so a test written only with
    /// it cannot tell a server that pushes from one that never does.
    async fn wait_for_notification(&mut self, method: &str, timeout: Duration) -> Option<Value> {
        // Already buffered by an earlier `request`? A fast task's push can beat
        // the very response that started it.
        if let Some(pos) = self
            .notifications
            .iter()
            .position(|v| v.get("method").and_then(Value::as_str) == Some(method))
        {
            return Some(self.notifications.remove(pos));
        }
        tokio::time::timeout(timeout, async {
            loop {
                let mut buf = String::new();
                let n = self.reader.read_line(&mut buf).await.ok()?;
                if n == 0 {
                    return None;
                }
                let Ok(value) = serde_json::from_str::<Value>(&buf) else {
                    continue;
                };
                if value.get("id").is_none()
                    && value.get("method").and_then(Value::as_str) == Some(method)
                {
                    return Some(value);
                }
            }
        })
        .await
        .ok()
        .flatten()
    }
}

/// Walk a schema JSON tree asserting no `$ref`/`$defs` keys — wire-level
/// backstop for the unit test in `src/mcp.rs` (Gemini CLI, n8n, and some
/// gateways reject or drop referenced schemas).
fn assert_no_refs(value: &Value, tool: &str, side: &str) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                assert!(
                    k != "$ref" && k != "$defs",
                    "tool `{tool}` {side} schema contains `{k}` on the wire"
                );
                assert_no_refs(v, tool, side);
            }
        }
        Value::Array(items) => {
            for v in items {
                assert_no_refs(v, tool, side);
            }
        }
        _ => {}
    }
}

/// Return the object branch from a schema that may be nullable. Schemars can
/// represent `Option<T>` with a direct object schema or with a combinator.
fn object_schema(value: &Value) -> Option<&Value> {
    if value["properties"].is_object() {
        return Some(value);
    }

    ["anyOf", "oneOf", "allOf"]
        .into_iter()
        .filter_map(|keyword| value[keyword].as_array())
        .flatten()
        .find_map(object_schema)
}

fn schema_allows_type(value: &Value, expected: &str) -> bool {
    value["type"].as_str() == Some(expected)
        || value["type"]
            .as_array()
            .is_some_and(|types| types.iter().any(|value| value == expected))
        || ["anyOf", "oneOf", "allOf"].into_iter().any(|keyword| {
            value[keyword].as_array().is_some_and(|branches| {
                branches
                    .iter()
                    .any(|branch| schema_allows_type(branch, expected))
            })
        })
}

fn schema_minimum(value: &Value) -> Option<f64> {
    value["minimum"].as_f64().or_else(|| {
        ["anyOf", "oneOf", "allOf"]
            .into_iter()
            .filter_map(|keyword| value[keyword].as_array())
            .flatten()
            .find_map(schema_minimum)
    })
}

fn schema_maximum(value: &Value) -> Option<f64> {
    value["maximum"].as_f64().or_else(|| {
        ["anyOf", "oneOf", "allOf"]
            .into_iter()
            .filter_map(|keyword| value[keyword].as_array())
            .flatten()
            .find_map(schema_maximum)
    })
}

fn find_tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|tool| tool["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("tool `{name}` missing"))
}

fn assert_schema_omits_properties(value: &Value, forbidden: &[&str], context: &str) {
    match value {
        Value::Object(map) => {
            if let Some(properties) = map.get("properties").and_then(Value::as_object) {
                for field in forbidden {
                    assert!(
                        !properties.contains_key(*field),
                        "{context} must not expose `{field}`: {value:#}"
                    );
                }
            }
            for child in map.values() {
                assert_schema_omits_properties(child, forbidden, context);
            }
        }
        Value::Array(items) => {
            for child in items {
                assert_schema_omits_properties(child, forbidden, context);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn initialize_reports_capabilities_and_identity() {
    let client = McpClient::start().await;
    let init = &client.init_result;

    let caps = init["capabilities"]
        .as_object()
        .expect("capabilities object");
    for cap in ["tools", "prompts", "resources", "completions", "tasks"] {
        assert!(
            caps.contains_key(cap),
            "missing `{cap}` capability: {caps:?}"
        );
    }
    for path in [
        &["tasks", "list"][..],
        &["tasks", "cancel"][..],
        &["tasks", "requests", "tools", "call"][..],
    ] {
        let mut value = &init["capabilities"];
        for segment in path {
            value = &value[*segment];
        }
        assert!(
            value.is_object(),
            "task capability `{}` must be advertised: {caps:?}",
            path.join(".")
        );
    }

    assert_eq!(
        init["serverInfo"]["name"].as_str(),
        Some("agentmail"),
        "server must announce itself as agentmail, not rmcp"
    );
    let version = init["serverInfo"]["version"]
        .as_str()
        .expect("serverInfo version");
    assert!(
        version.starts_with(env!("CARGO_PKG_VERSION")),
        "version leads with the crate version: {version}"
    );
    assert!(
        version.contains('(') && version.ends_with(')'),
        "version carries the build SHA fingerprint so deploy skew is visible: {version}"
    );
    assert!(
        init["instructions"].as_str().is_some_and(|s| !s.is_empty()),
        "instructions should be present and non-empty"
    );
    let proto = init["protocolVersion"].as_str().expect("protocolVersion");
    assert!(
        ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"].contains(&proto),
        "unexpected protocol version {proto}"
    );
}

#[tokio::test]
async fn tools_list_has_28_annotated_tools() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(
        tools.len(),
        28,
        "tool count drifted — update docs and tests"
    );

    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        assert!(
            tool["description"].as_str().is_some_and(|d| !d.is_empty()),
            "tool `{name}` has no description"
        );
        assert!(
            tool["annotations"]["title"]
                .as_str()
                .is_some_and(|t| !t.is_empty()),
            "tool `{name}` has no annotations.title"
        );
        assert!(
            tool["inputSchema"].is_object(),
            "tool `{name}` has no inputSchema"
        );
        assert!(
            tool["outputSchema"].is_object(),
            "tool `{name}` has no outputSchema"
        );
        assert_eq!(
            tool["outputSchema"]["type"].as_str(),
            Some("object"),
            "tool `{name}` outputSchema must have an object root: {tool:#}"
        );
        assert!(
            tool["outputSchema"]["properties"].is_object(),
            "tool `{name}` outputSchema must declare object properties: {tool:#}"
        );
    }
}

#[tokio::test]
async fn wire_schemas_are_ref_free() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        assert_no_refs(&tool["inputSchema"], name, "input");
        assert_no_refs(&tool["outputSchema"], name, "output");
    }
}

#[tokio::test]
async fn list_accounts_works_without_network() {
    let mut client = McpClient::start().await;
    let resp = client
        .request(
            "tools/call",
            json!({"name": "list_accounts", "arguments": {}}),
        )
        .await;
    assert!(
        resp.get("error").is_none(),
        "list_accounts failed: {resp:#}"
    );
    let result = &resp["result"];
    assert_ne!(
        result["isError"].as_bool(),
        Some(true),
        "isError set: {result:#}"
    );
    let accounts = result["structuredContent"]["accounts"]
        .as_array()
        .expect("structuredContent.accounts array");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["name"].as_str(), Some("dummy"));
    assert_eq!(accounts[0]["isDefault"].as_bool(), Some(true));
    for credential in ["host", "port", "username", "password"] {
        assert!(
            accounts[0].get(credential).is_none(),
            "list_accounts must not expose `{credential}`: {result:#}"
        );
    }

    let content = result["content"].as_array().expect("compact content array");
    assert_eq!(content.len(), 1, "one compact fallback block: {result:#}");
    let fallback = content[0]["text"].as_str().expect("fallback text");
    let fallback_json: Value = serde_json::from_str(fallback).expect("fallback is complete JSON");
    assert_eq!(
        fallback_json, result["structuredContent"],
        "text-only hosts must receive the same complete result as structuredContent"
    );
}

#[tokio::test]
async fn list_accounts_schema_omits_connection_credentials() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let output = &find_tool(tools, "list_accounts")["outputSchema"];
    let account = object_schema(&output["properties"]["accounts"]["items"])
        .expect("account item object schema");
    let properties = account["properties"]
        .as_object()
        .expect("account properties");

    assert!(properties.contains_key("name"));
    assert!(properties.contains_key("isDefault"));
    for credential in ["host", "port", "username", "password"] {
        assert!(
            !properties.contains_key(credential),
            "list_accounts schema must omit `{credential}`: {output:#}"
        );
    }
}

#[tokio::test]
async fn task_result_is_repeatable_related_and_terminal_cancel_is_rejected() {
    let mut client = McpClient::start().await;
    let queued = client
        .request(
            "tools/call",
            json!({
                "name": "unsubscribe_message",
                "arguments": {
                    "account": "dummy",
                    "mailbox": "INBOX",
                    "uid": 1,
                    "expectedUidValidity": 1,
                    "confirmOneClick": false
                },
                "task": {"ttl": 60_000}
            }),
        )
        .await;
    assert!(
        queued.get("error").is_none(),
        "task enqueue failed: {queued:#}"
    );
    let task_id = queued["result"]["task"]["taskId"]
        .as_str()
        .expect("queued task id")
        .to_string();

    // tasks/result is the blocking result endpoint: the call must return the
    // original tool result, never a transient "still running" error.
    let first = client
        .request("tasks/result", json!({"taskId": task_id}))
        .await;
    assert!(
        first.get("error").is_none(),
        "task result failed: {first:#}"
    );
    assert_eq!(first["result"]["isError"], json!(true));
    assert_eq!(
        first["result"]["_meta"]["io.modelcontextprotocol/related-task"]["taskId"],
        json!(task_id)
    );

    let second = client
        .request("tasks/result", json!({"taskId": task_id}))
        .await;
    assert_eq!(
        second["result"], first["result"],
        "retained task results must be repeatable"
    );

    let info = client
        .request("tasks/get", json!({"taskId": task_id}))
        .await;
    assert_eq!(info["result"]["taskId"], json!(task_id));
    assert_eq!(
        info["result"]["status"],
        json!("failed"),
        "an isError tool result is a failed task: {info:#}"
    );

    let cancellation = client
        .request("tasks/cancel", json!({"taskId": task_id}))
        .await;
    assert_eq!(
        cancellation["error"]["code"],
        json!(-32602),
        "terminal task cancellation must be invalid params: {cancellation:#}"
    );

    let listed = client.request("tasks/list", json!({})).await;
    let tasks = listed["result"]["tasks"]
        .as_array()
        .expect("task list array");
    assert!(
        tasks.iter().any(|task| task["taskId"] == task_id),
        "retained task should be listed: {listed:#}"
    );
    assert!(
        listed["result"].get("total").is_none(),
        "tasks/list must not add a non-spec total field: {listed:#}"
    );
}

#[tokio::test]
async fn draft_attachment_limits_are_in_schema_and_enforced_before_io() {
    let mut client = McpClient::start().await;
    let listed = client.request("tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().expect("tools array");
    let attachments = &find_tool(tools, "create_draft")["inputSchema"]["properties"]["attachments"];
    assert_eq!(attachments["maxItems"], json!(20));
    let description = attachments["description"].as_str().unwrap_or_default();
    assert!(
        description.contains("25 MiB") && description.contains("40 MiB"),
        "attachment byte limits must be discoverable: {description}"
    );

    let too_many = (0..21)
        .map(|index| json!({"path": format!("missing-{index}.txt")}))
        .collect::<Vec<_>>();
    let rejected = client
        .request(
            "tools/call",
            json!({
                "name": "create_draft",
                "arguments": {
                    "account": "dummy",
                    "to": ["recipient@example.invalid"],
                    "attachments": too_many
                }
            }),
        )
        .await;
    assert_eq!(
        rejected["error"]["code"],
        json!(-32602),
        "attachment count must fail before filesystem access: {rejected:#}"
    );
}

#[tokio::test]
async fn invalid_params_yields_32602() {
    let mut client = McpClient::start().await;
    let resp = client
        .request(
            "tools/call",
            json!({
                "name": "delete_messages",
                "arguments": {
                    "account": "dummy",
                    "mailbox": "INBOX",
                    "uids": [],
                    "expectedUidValidity": 1
                }
            }),
        )
        .await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "empty uids should be rejected as invalid params: {resp:#}"
    );
}

#[tokio::test]
async fn uid_actions_require_a_nonzero_expected_uidvalidity() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    // delete_by_sender is absent by design: it deletes by a direct sender
    // identity (email + displayName from a ranking row), not a sample UID, so
    // it carries no UIDVALIDITY guard — discovery confirms the identity live.
    for name in [
        "delete_messages",
        "move_message",
        "unsubscribe_message",
        "download_attachments",
        "add_flags",
        "remove_flags",
    ] {
        let schema = &find_tool(tools, name)["inputSchema"];
        let required = schema["required"].as_array().expect("required fields");
        assert!(
            required
                .iter()
                .any(|field| field.as_str() == Some("expectedUidValidity")),
            "`{name}` must require expectedUidValidity: {schema:#}"
        );
        let identity = &schema["properties"]["expectedUidValidity"];
        assert!(
            schema_allows_type(identity, "integer"),
            "`{name}` expectedUidValidity must be an integer: {identity:#}"
        );
        assert_eq!(
            schema_minimum(identity),
            Some(1.0),
            "`{name}` expectedUidValidity must reject epoch zero: {identity:#}"
        );
    }
}

#[tokio::test]
async fn list_mailboxes_requires_account_and_enforces_page_bounds() {
    let mut client = McpClient::start().await;
    let listed = client.request("tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().expect("tools array");
    let input = &find_tool(tools, "list_mailboxes")["inputSchema"];
    let required = input["required"].as_array().expect("required fields");
    assert!(
        required
            .iter()
            .any(|field| field.as_str() == Some("account")),
        "list_mailboxes must require an account selector: {input:#}"
    );
    let limit_description = input["properties"]["limit"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(limit_description.contains("Defaults to 100"));
    assert!(limit_description.contains("maximum 500"));
    assert_eq!(schema_minimum(&input["properties"]["limit"]), Some(1.0));
    assert_eq!(schema_maximum(&input["properties"]["limit"]), Some(500.0));

    let missing = client
        .request(
            "tools/call",
            json!({"name": "list_mailboxes", "arguments": {}}),
        )
        .await;
    assert_eq!(missing["result"]["isError"], json!(true));
    assert!(
        missing["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|message| message.contains("missing field `account`")),
        "deserialization error should identify the missing field: {missing:#}"
    );

    for arguments in [
        json!({"account": "dummy", "limit": 0}),
        json!({"account": "dummy", "limit": 501}),
    ] {
        let resp = client
            .request(
                "tools/call",
                json!({"name": "list_mailboxes", "arguments": arguments}),
            )
            .await;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(-32602),
            "invalid list_mailboxes page must fail before IMAP: {resp:#}"
        );
    }
}

#[tokio::test]
async fn discovery_outputs_have_safe_complete_message_identities() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let attachments = &find_tool(tools, "find_attachments")["outputSchema"];
    assert!(
        attachments["properties"].get("uids").is_none(),
        "find_attachments must not expose mailbox-ambiguous flat UIDs: {attachments:#}"
    );
    let attachment = object_schema(&attachments["properties"]["messages"]["items"])
        .expect("attachment message identity object");
    let attachment_properties = attachment["properties"]
        .as_object()
        .expect("attachment message properties");
    for field in ["mailbox", "uidValidity", "uid", "resourceUri"] {
        assert!(
            attachment_properties.contains_key(field),
            "attachment hit must expose `{field}`: {attachment:#}"
        );
    }
    for field in ["uidValidity", "uid"] {
        assert_eq!(
            schema_minimum(&attachment_properties[field]),
            Some(1.0),
            "attachment identity `{field}` must be nonzero: {attachment:#}"
        );
    }

    for name in ["get_messages", "search_messages"] {
        let tool = find_tool(tools, name);
        let input = &tool["inputSchema"];
        for removed in ["includeContent", "includeHeaders"] {
            assert!(
                input["properties"].get(removed).is_none(),
                "`{name}` must be metadata-only and omit `{removed}`: {input:#}"
            );
        }

        let output = &tool["outputSchema"];
        assert_eq!(
            schema_minimum(&output["properties"]["uidValidity"]),
            Some(1.0),
            "`{name}` must return a nonzero mailbox UID epoch: {output:#}"
        );
        let message = object_schema(&output["properties"]["messages"]["items"])
            .expect("message metadata object");
        let message_properties = message["properties"]
            .as_object()
            .expect("message metadata properties");
        for field in ["uid", "subject", "sender", "date", "flags", "resourceUri"] {
            assert!(
                message_properties.contains_key(field),
                "`{name}` message metadata must expose `{field}`: {message:#}"
            );
        }
        for removed in [
            "content",
            "headers",
            "attachments",
            "listUnsubscribe",
            "to",
            "cc",
        ] {
            assert!(
                !message_properties.contains_key(removed),
                "`{name}` message metadata must omit `{removed}`: {message:#}"
            );
        }
    }
}

#[tokio::test]
async fn resources_templates_list_five_uidvalidity_safe_templates() {
    let mut client = McpClient::start().await;
    let resp = client.request("resources/templates/list", json!({})).await;
    let templates = resp["result"]["resourceTemplates"]
        .as_array()
        .expect("resourceTemplates array");
    assert_eq!(templates.len(), 5, "template count drifted: {templates:#?}");

    let body = &templates[0];
    assert_eq!(
        body["uriTemplate"].as_str(),
        Some("email://{account}/{mailbox}/{uidValidity}/{uid}")
    );
    assert_eq!(body["mimeType"].as_str(), Some("text/markdown"));

    let headers = &templates[1];
    assert_eq!(
        headers["uriTemplate"].as_str(),
        Some("email://{account}/{mailbox}/{uidValidity}/{uid}/headers")
    );
    assert_eq!(headers["mimeType"].as_str(), Some("text/rfc822-headers"));

    let source = &templates[2];
    assert_eq!(
        source["uriTemplate"].as_str(),
        Some("email://{account}/{mailbox}/{uidValidity}/{uid}/source")
    );
    assert_eq!(source["mimeType"].as_str(), Some("message/rfc822"));

    let info = &templates[3];
    assert_eq!(
        info["uriTemplate"].as_str(),
        Some("email://{account}/{mailbox}/{uidValidity}/{uid}/info")
    );
    assert_eq!(info["mimeType"].as_str(), Some("application/json"));
    assert!(
        info["description"]
            .as_str()
            .expect("info description")
            .contains("attachment inventory"),
        "info template should advertise the attachment inventory"
    );

    let attachment = &templates[4];
    assert_eq!(
        attachment["uriTemplate"].as_str(),
        Some("email://{account}/{mailbox}/{uidValidity}/{uid}/attachments/{index}")
    );
    assert!(
        attachment["mimeType"].is_null(),
        "attachment mime type varies per part and must not be pinned"
    );
    assert!(
        attachment["description"]
            .as_str()
            .expect("attachment description")
            .contains("download_attachments"),
        "attachment template should point large files at download_attachments"
    );
}

#[tokio::test]
async fn resources_list_is_empty() {
    // Discovery is template-only; this also pins that resources/list is
    // served (not method_not_found) now that the capability is advertised.
    let mut client = McpClient::start().await;
    let resp = client.request("resources/list", json!({})).await;
    assert!(
        resp.get("error").is_none(),
        "resources/list failed: {resp:#}"
    );
    let resources = resp["result"]["resources"]
        .as_array()
        .expect("resources array");
    assert!(resources.is_empty(), "expected empty list: {resources:#?}");
}

#[tokio::test]
async fn resources_read_malformed_uri_is_32602() {
    let mut client = McpClient::start().await;
    for uri in [
        "email://dummy/INBOX",
        "email://dummy/INBOX/1",
        "email://dummy/INBOX/0/1",
        "email://dummy/INBOX/1/0",
        "notemail://x/y/1/1",
    ] {
        let resp = client.request("resources/read", json!({"uri": uri})).await;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(-32602),
            "malformed uri `{uri}` should be invalid params: {resp:#}"
        );
    }
}

#[tokio::test]
async fn resources_read_unknown_account_is_32602() {
    let mut client = McpClient::start().await;
    let resp = client
        .request("resources/read", json!({"uri": "email://ghost/INBOX/1/1"}))
        .await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "unknown account should be invalid params: {resp:#}"
    );
}

#[tokio::test]
async fn resources_read_decodes_mailbox_and_fails_at_connect() {
    // Proves the percent-decode path executes end-to-end: the URI parses
    // (Archive%2F2024 → Archive/2024), the account resolves, and the failure
    // happens at the IMAP connect to imap.invalid (NXDOMAIN per RFC 6761,
    // fails in milliseconds) → internal error, not invalid params.
    let mut client = McpClient::start().await;
    let resp = client
        .request(
            "resources/read",
            json!({"uri": "email://dummy/Archive%2F2024/9/7"}),
        )
        .await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32603),
        "decoded read should fail at connect: {resp:#}"
    );
}

#[tokio::test]
async fn completion_for_prompt_account_returns_dummy() {
    let mut client = McpClient::start().await;
    let resp = client
        .request(
            "completion/complete",
            json!({
                "ref": {"type": "ref/prompt", "name": "inbox-summary"},
                "argument": {"name": "account", "value": "d"}
            }),
        )
        .await;
    assert_eq!(
        resp["result"]["completion"]["values"],
        json!(["dummy"]),
        "prefix 'd' should complete to dummy: {resp:#}"
    );

    let resp = client
        .request(
            "completion/complete",
            json!({
                "ref": {"type": "ref/prompt", "name": "inbox-summary"},
                "argument": {"name": "account", "value": "z"}
            }),
        )
        .await;
    assert_eq!(resp["result"]["completion"]["values"], json!([]));
}

#[tokio::test]
async fn completion_for_resource_template_account() {
    let mut client = McpClient::start().await;
    let resp = client
        .request(
            "completion/complete",
            json!({
                "ref": {
                    "type": "ref/resource",
                    "uri": "email://{account}/{mailbox}/{uidValidity}/{uid}"
                },
                "argument": {"name": "account", "value": ""}
            }),
        )
        .await;
    assert_eq!(
        resp["result"]["completion"]["values"],
        json!(["dummy"]),
        "resource template account completion: {resp:#}"
    );
}

#[tokio::test]
async fn completion_mailbox_swallows_network_errors() {
    // A cold catalog refresh needs an IMAP LIST, which fails against
    // imap.invalid. Completion must still succeed with no values.
    let mut client = McpClient::start().await;
    let resp = client
        .request(
            "completion/complete",
            json!({
                "ref": {"type": "ref/prompt", "name": "find-attachments"},
                "argument": {"name": "mailbox", "value": ""},
                "context": {"arguments": {"account": "dummy"}}
            }),
        )
        .await;
    assert!(resp.get("error").is_none(), "must not error: {resp:#}");
    assert_eq!(resp["result"]["completion"]["values"], json!([]));
}

#[tokio::test]
async fn delete_tools_expose_permanent_flag() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    // unsubscribe_message is absent by design: its disposal policy lives in
    // the nested cleanup.deletion enum, not a flat permanent boolean.
    for name in ["delete_messages", "delete_by_sender", "delete_list_id"] {
        let tool = tools
            .iter()
            .find(|t| t["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("tool `{name}` missing"));
        let prop = &tool["inputSchema"]["properties"]["permanent"];
        assert_eq!(
            prop["type"].as_str(),
            Some("boolean"),
            "`{name}` should expose a boolean `permanent` arg: {prop:#}"
        );
        assert!(
            prop["description"]
                .as_str()
                .is_some_and(|d| d.contains("Irreversible")),
            "`{name}` permanent arg should warn it is irreversible"
        );
    }
}

#[tokio::test]
async fn unsubscribe_schema_requires_identity_and_consent_with_safe_defaults() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "unsubscribe_message")
        .expect("unsubscribe_message tool");
    let schema = &tool["inputSchema"];
    let required = schema["required"].as_array().expect("required fields");
    for field in ["account", "uid", "expectedUidValidity", "confirmOneClick"] {
        assert!(
            required.iter().any(|value| value == field),
            "unsubscribe_message should require {field}: {schema:#}"
        );
    }
    // Cleanup is a single optional nested policy object — never required, so
    // omitting it means "unsubscribe only" structurally, not via a boolean.
    assert!(
        !required.iter().any(|value| value == "cleanup"),
        "cleanup must stay optional: {required:?}"
    );
    let cleanup = &schema["properties"]["cleanup"];
    for (axis, values, default) in [
        ("when", vec!["afterSuccess", "always"], "afterSuccess"),
        (
            "identity",
            vec!["listIdOnly", "listIdOrSender"],
            "listIdOrSender",
        ),
        (
            "deletion",
            vec!["trash", "trashThenPermanent", "permanent"],
            "trash",
        ),
    ] {
        let field = &cleanup["properties"][axis];
        // schemars emits documented enums as oneOf/[{const}]; accept a plain
        // enum array too so the pin survives representation changes.
        let allowed: Vec<&str> = if field["enum"].is_array() {
            field["enum"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect()
        } else {
            field["oneOf"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|variant| variant["const"].as_str())
                .collect()
        };
        assert_eq!(
            allowed, values,
            "cleanup.{axis} must expose exactly these policies: {cleanup:#}"
        );
        assert_eq!(
            field["default"],
            json!(default),
            "cleanup.{axis} must default to {default}: {field:#}"
        );
    }
    // The old flat booleans are gone — one representation per policy.
    for legacy in [
        "deleteMatching",
        "deleteOnUnsubscribeFailure",
        "allowSenderFallback",
        "allowPermanentFallback",
        "permanent",
    ] {
        assert!(
            schema["properties"][legacy].is_null(),
            "legacy flat flag {legacy} must not survive the cleanup object"
        );
    }
    assert_eq!(tool["execution"]["taskSupport"], json!("optional"));

    let missing = client
        .request(
            "tools/call",
            json!({
                "name": "unsubscribe_message",
                "arguments": {"account": "dummy", "uid": 1}
            }),
        )
        .await;
    assert_eq!(missing["result"]["isError"], json!(true));
    assert!(
        missing["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|message| message.contains("missing field")),
        "missing arguments must remain visible to the model: {missing:#}"
    );

    let no_consent = client
        .request(
            "tools/call",
            json!({
                "name": "unsubscribe_message",
                "arguments": {
                    "account": "dummy",
                    "mailbox": "INBOX",
                    "uid": 1,
                    "expectedUidValidity": 1,
                    "confirmOneClick": false
                }
            }),
        )
        .await;
    // Consent-required is an operational STOP on a well-formed request, so it is
    // an isError RESULT the agent can act on (re-call with confirmOneClick:
    // true) — not a protocol error, which the gateway would read as the whole
    // backend failing.
    assert!(
        no_consent.get("error").is_none(),
        "consent-required must not be a protocol error: {no_consent:#}"
    );
    assert_eq!(
        no_consent["result"]["isError"].as_bool(),
        Some(true),
        "consent-required must be an isError result: {no_consent:#}"
    );

    // rmcp surfaces argument-deserialization failures as isError tool results
    // so the model sees the offending field/value rather than losing the
    // detail in a transport-level failure.
    let unknown_cleanup_field = client
        .request(
            "tools/call",
            json!({
                "name": "unsubscribe_message",
                "arguments": {
                    "account": "dummy",
                    "mailbox": "INBOX",
                    "uid": 1,
                    "expectedUidValidity": 1,
                    "confirmOneClick": true,
                    "cleanup": {"deleteMatching": true}
                }
            }),
        )
        .await;
    assert_eq!(unknown_cleanup_field["result"]["isError"], json!(true));
    assert!(
        unknown_cleanup_field["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|message| message.contains("deleteMatching")),
        "unknown cleanup fields must be named: {unknown_cleanup_field:#}"
    );
    let bad_cleanup_value = client
        .request(
            "tools/call",
            json!({
                "name": "unsubscribe_message",
                "arguments": {
                    "account": "dummy",
                    "mailbox": "INBOX",
                    "uid": 1,
                    "expectedUidValidity": 1,
                    "confirmOneClick": true,
                    "cleanup": {"deletion": "shred"}
                }
            }),
        )
        .await;
    assert_eq!(bad_cleanup_value["result"]["isError"], json!(true));
    assert!(
        bad_cleanup_value["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|message| message.contains("shred")),
        "out-of-enum cleanup value must be named: {bad_cleanup_value:#}"
    );

    // Omitted cleanup and an empty cleanup object (all axes defaulted) both
    // pass policy validation: the call proceeds to the network and fails at
    // connect as an isError RESULT, NOT rejected as an invalid policy (-32602).
    for arguments in [
        json!({
            "account": "dummy",
            "mailbox": "INBOX",
            "uid": 1,
            "expectedUidValidity": 1,
            "confirmOneClick": true
        }),
        json!({
            "account": "dummy",
            "mailbox": "INBOX",
            "uid": 1,
            "expectedUidValidity": 1,
            "confirmOneClick": true,
            "cleanup": {}
        }),
    ] {
        let accepted = client
            .request(
                "tools/call",
                json!({"name": "unsubscribe_message", "arguments": arguments}),
            )
            .await;
        // The connect failure is an operational isError RESULT — not a -32602
        // policy rejection, and not a protocol error the gateway would misread
        // as the whole backend being down.
        assert!(
            accepted.get("error").is_none(),
            "a connect failure is an operational result, not a protocol error: {accepted:#}"
        );
        assert_eq!(
            accepted["result"]["isError"].as_bool(),
            Some(true),
            "valid policies proceed to the network and fail there as an isError result: {accepted:#}"
        );
    }
}

#[tokio::test]
async fn unsubscribe_tool_declares_destructive_open_world_behavior() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "unsubscribe_message")
        .expect("unsubscribe_message tool");

    assert_eq!(tool["annotations"]["destructiveHint"], json!(true));
    assert_eq!(tool["annotations"]["openWorldHint"], json!(true));
}

#[tokio::test]
async fn unsubscribe_output_schema_exposes_verification_and_cleanup_state() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "unsubscribe_message")
        .expect("unsubscribe_message tool");
    let output = &tool["outputSchema"];
    let properties = output["properties"]
        .as_object()
        .expect("unsubscribe_message output properties");

    for field in [
        "uidValidity",
        "dkimVerified",
        "listIdAuthenticated",
        "dkimDomain",
        "unsubscribed",
        "matchingMessages",
        "cleanupSkippedReason",
    ] {
        assert!(
            properties.contains_key(field),
            "unsubscribe_message output should expose {field}: {output:#}"
        );
    }
    for (field, expected_type) in [
        ("uidValidity", "integer"),
        ("dkimVerified", "boolean"),
        ("listIdAuthenticated", "boolean"),
        ("dkimDomain", "string"),
    ] {
        assert!(
            schema_allows_type(&properties[field], expected_type),
            "unsubscribe_message output {field} should allow {expected_type}: {output:#}"
        );
    }

    let cleanup = object_schema(&properties["matchingMessages"])
        .expect("matchingMessages should contain an object schema");
    let cleanup_properties = cleanup["properties"]
        .as_object()
        .expect("matchingMessages properties");
    for field in [
        "matchedBy",
        "listId",
        "found",
        "deleted",
        "failed",
        "pending",
        "needsAttention",
        "operationIds",
        "trashFallback",
        "complete",
    ] {
        assert!(
            cleanup_properties.contains_key(field),
            "matchingMessages should expose {field}: {cleanup:#}"
        );
    }
}

#[tokio::test]
async fn mutation_outputs_expose_ambiguous_move_recovery_state() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    for name in [
        "delete_messages",
        "delete_by_sender",
        "delete_by_domain",
        "delete_list_id",
        "move_list_id",
        "move_by_sender",
        "move_by_domain",
    ] {
        let properties = find_tool(tools, name)["outputSchema"]["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("`{name}` output properties"));
        for field in ["pending", "needsAttention", "operationIds"] {
            assert!(
                properties.contains_key(field),
                "`{name}` must expose durable recovery field `{field}`"
            );
        }
    }

    let move_message = &find_tool(tools, "move_message")["outputSchema"]["properties"];
    for field in ["moved", "status", "operationId"] {
        assert!(
            move_message.get(field).is_some(),
            "move_message must expose `{field}`: {move_message:#}"
        );
    }
}

#[tokio::test]
async fn ranking_and_unsubscribe_outputs_do_not_expose_recipient_tokens() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let forbidden = [
        "listUnsubscribe",
        "listUnsubscribePost",
        "unsubscribeUrl",
        "url",
        "pathway",
        "method",
    ];

    for name in [
        "top_senders",
        "top_domains",
        "top_subscriptions",
        "top_mailing_lists",
        "unsubscribe_message",
    ] {
        let output = &find_tool(tools, name)["outputSchema"];
        assert_schema_omits_properties(output, &forbidden, name);
    }
}

#[tokio::test]
async fn rank_outputs_expose_nested_action_identities() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    for (name, rows_field) in [
        ("top_senders", "senders"),
        ("top_domains", "domains"),
        ("top_subscriptions", "lists"),
        ("top_mailing_lists", "lists"),
    ] {
        let output = &find_tool(tools, name)["outputSchema"];
        let row = object_schema(&output["properties"][rows_field]["items"])
            .unwrap_or_else(|| panic!("`{name}` ranked rows should be objects"));
        let row_properties = row["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("`{name}` ranked row properties"));
        let sample = object_schema(&row_properties["sample"])
            .unwrap_or_else(|| panic!("`{name}` nested sample identity"));
        let sample_properties = sample["properties"]
            .as_object()
            .expect("sample identity properties");
        for field in ["mailbox", "uidValidity", "uid", "resourceUri"] {
            assert!(
                sample_properties.contains_key(field),
                "`{name}` sample must expose `{field}`: {sample:#}"
            );
        }
        for field in ["uidValidity", "uid"] {
            assert_eq!(
                schema_minimum(&sample_properties[field]),
                Some(1.0),
                "`{name}` sample identity `{field}` must be nonzero: {sample:#}"
            );
        }
    }

    let subscriptions = &find_tool(tools, "top_subscriptions")["outputSchema"];
    let subscription = object_schema(&subscriptions["properties"]["lists"]["items"])
        .expect("subscription row object");
    assert!(
        schema_allows_type(&subscription["properties"]["advertisedOneClick"], "boolean"),
        "advertisedOneClick should be boolean: {subscription:#}"
    );
}

#[tokio::test]
async fn domain_tools_expose_exact_hierarchy_and_action_contracts() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let top = find_tool(tools, "top_domains");
    let row = object_schema(&top["outputSchema"]["properties"]["domains"]["items"])
        .expect("domain rank row");
    let properties = row["properties"].as_object().expect("domain properties");
    for field in [
        "domain",
        "registrableDomain",
        "subdomain",
        "count",
        "subject",
        "oldestDate",
        "newestDate",
        "sample",
    ] {
        assert!(
            properties.contains_key(field),
            "top_domains row must expose `{field}`: {row:#}"
        );
    }
    assert!(
        top["description"].as_str().is_some_and(
            |description| description.contains("example.com never includes mail.example.com")
        ),
        "top_domains must state exact-domain semantics: {top:#}"
    );

    for name in ["delete_by_domain", "move_by_domain"] {
        let tool = find_tool(tools, name);
        let input = &tool["inputSchema"];
        let required = input["required"].as_array().expect("required fields");
        assert!(required.iter().any(|field| field == "domain"));
        assert!(
            input["properties"]["domain"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("never includes")),
            "`{name}` must document exact-domain scope: {input:#}"
        );
        assert_eq!(tool["execution"]["taskSupport"], json!("optional"));
    }
    assert_eq!(
        find_tool(tools, "delete_by_domain")["annotations"]["destructiveHint"],
        json!(true)
    );
    assert_eq!(
        find_tool(tools, "move_by_domain")["annotations"]["destructiveHint"],
        json!(false)
    );
}

#[tokio::test]
async fn reconciliation_tools_expose_durable_operation_state() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let list = find_tool(tools, "list_pending_moves");
    assert_eq!(list["annotations"]["readOnlyHint"], json!(true));
    let operation = object_schema(&list["outputSchema"]["properties"]["operations"]["items"])
        .expect("pending move row");
    let properties = operation["properties"]
        .as_object()
        .expect("pending move properties");
    for field in [
        "operationId",
        "sourceMailbox",
        "sourceUidValidity",
        "sourceUid",
        "destination",
        "status",
        "detail",
        "createdAt",
        "updatedAt",
    ] {
        assert!(
            properties.contains_key(field),
            "pending move must expose `{field}`: {operation:#}"
        );
    }

    let reconcile = find_tool(tools, "reconcile_moves");
    assert_eq!(reconcile["annotations"]["destructiveHint"], json!(true));
    assert_eq!(reconcile["annotations"]["idempotentHint"], json!(true));
    assert_eq!(reconcile["execution"]["taskSupport"], json!("optional"));
    for field in [
        "examined",
        "completed",
        "pending",
        "needsAttention",
        "failed",
        "operations",
    ] {
        assert!(
            reconcile["outputSchema"]["properties"].get(field).is_some(),
            "reconcile_moves must expose `{field}`: {reconcile:#}"
        );
    }
}

#[tokio::test]
async fn rank_limit_documents_default_of_10_and_maximum_of_100() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    for name in ["top_senders", "top_subscriptions", "top_mailing_lists"] {
        let tool = tools
            .iter()
            .find(|t| t["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("tool `{name}` missing"));
        let desc = tool["inputSchema"]["properties"]["limit"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            desc.contains("Defaults to 10"),
            "`{name}` limit description should document the default: {desc}"
        );
        assert!(
            desc.contains("maximum 100"),
            "`{name}` limit description should document the maximum: {desc}"
        );
        assert_eq!(
            schema_minimum(&tool["inputSchema"]["properties"]["limit"]),
            Some(1.0),
            "`{name}` limit schema should reject zero"
        );
        assert_eq!(
            schema_maximum(&tool["inputSchema"]["properties"]["limit"]),
            Some(100.0),
            "`{name}` limit schema should expose the maximum"
        );
    }

    let domains = find_tool(tools, "top_domains");
    let description = domains["inputSchema"]["properties"]["limit"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(
        description.contains("Defaults to 20") && description.contains("maximum 100"),
        "top_domains must document its 20-row default and 100-row maximum: {description}"
    );
    assert_eq!(
        schema_minimum(&domains["inputSchema"]["properties"]["limit"]),
        Some(1.0)
    );
    assert_eq!(
        schema_maximum(&domains["inputSchema"]["properties"]["limit"]),
        Some(100.0)
    );
}

#[tokio::test]
async fn prompts_list_has_6_prompts() {
    let mut client = McpClient::start().await;
    let resp = client.request("prompts/list", json!({})).await;
    let prompts = resp["result"]["prompts"].as_array().expect("prompts array");
    assert_eq!(
        prompts.len(),
        6,
        "prompt count drifted — update docs and tests"
    );
    for prompt in prompts {
        let name = prompt["name"].as_str().unwrap_or("<unnamed>");
        assert!(
            prompt["description"]
                .as_str()
                .is_some_and(|d| !d.is_empty()),
            "prompt `{name}` has no description"
        );
    }
}

// ---------------------------------------------------------------------------
// Contract-validation tests: the cleaned-up 0.3.0 tool contract.
// ---------------------------------------------------------------------------

/// Every paginated tool shares one envelope: `offset`, `limit`, `total`, and
/// an optional `nextOffset` — no legacy total names.
#[tokio::test]
async fn paginated_outputs_share_one_envelope() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    for (name, rows_field) in [
        ("list_mailboxes", "mailboxes"),
        ("get_messages", "messages"),
        ("search_messages", "messages"),
        ("find_attachments", "messages"),
        ("top_senders", "senders"),
        ("top_domains", "domains"),
        ("top_subscriptions", "lists"),
        ("top_mailing_lists", "lists"),
    ] {
        let output = &find_tool(tools, name)["outputSchema"];
        let properties = output["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("`{name}` output properties"));
        for field in ["offset", "limit", "total", "nextOffset", rows_field] {
            assert!(
                properties.contains_key(field),
                "`{name}` output must expose `{field}`: {output:#}"
            );
        }
        for legacy in ["totalMatches", "uniqueSenders", "uniqueLists"] {
            assert!(
                !properties.contains_key(legacy),
                "`{name}` output must not expose legacy `{legacy}`"
            );
        }
        let required: Vec<&str> = output["required"]
            .as_array()
            .map(|values| values.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        for field in ["offset", "limit", "total"] {
            assert!(
                required.contains(&field),
                "`{name}` `{field}` must be required: {required:?}"
            );
        }
        assert!(
            !required.contains(&"nextOffset"),
            "`{name}` nextOffset is page-dependent and must be optional"
        );
    }
}

/// One mailbox idiom: `mailbox` is optional only where omitting it means
/// "scan the whole account"; single-mailbox readers and every UID consumer
/// require it, and nothing defaults to INBOX.
#[tokio::test]
async fn mailbox_argument_follows_one_idiom() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    for name in [
        "get_messages",
        "search_messages",
        "delete_messages",
        "download_attachments",
        "unsubscribe_message",
        "add_flags",
        "remove_flags",
        "move_message",
        "create_mailbox",
    ] {
        let input = &find_tool(tools, name)["inputSchema"];
        let required: Vec<&str> = input["required"]
            .as_array()
            .map(|values| values.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert!(
            required.contains(&"mailbox"),
            "`{name}` must require mailbox: {required:?}"
        );
    }

    for name in [
        "list_flags",
        "find_attachments",
        "top_senders",
        "top_domains",
        "top_subscriptions",
        "top_mailing_lists",
        "delete_list_id",
        "delete_by_sender",
        "delete_by_domain",
        "move_list_id",
        "move_by_sender",
        "move_by_domain",
    ] {
        let input = &find_tool(tools, name)["inputSchema"];
        assert!(
            input["properties"]["mailbox"].is_object(),
            "`{name}` must accept an optional mailbox"
        );
        let required: Vec<&str> = input["required"]
            .as_array()
            .map(|values| values.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert!(
            !required.contains(&"mailbox"),
            "`{name}` mailbox omission means account-wide and must stay optional"
        );
        let description = input["properties"]["mailbox"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            description.contains("Omit"),
            "`{name}` optional mailbox must document account-wide omission: {description}"
        );
    }

    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        let description = tool["inputSchema"]["properties"]["mailbox"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            !description.contains("Defaults to INBOX"),
            "`{name}` must not default mailbox to INBOX: {description}"
        );
    }
}

/// `add_flags.color` is a color name; `remove_flags` uses `clearColor: bool`
/// — the shared `color` key with two meanings is gone.
#[tokio::test]
async fn flag_color_arguments_are_typed_consistently() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let add = &find_tool(tools, "add_flags")["inputSchema"];
    assert!(
        schema_allows_type(&add["properties"]["color"], "string"),
        "add_flags color is a color-name string: {add:#}"
    );

    let remove = &find_tool(tools, "remove_flags")["inputSchema"];
    let properties = remove["properties"]
        .as_object()
        .expect("remove_flags input properties");
    assert!(
        !properties.contains_key("color"),
        "remove_flags renamed `color` to `clearColor`"
    );
    assert!(
        schema_allows_type(&properties["clearColor"], "boolean"),
        "remove_flags clearColor is a boolean switch: {remove:#}"
    );
}

/// `create_mailbox` uses the same `mailbox` argument name as every other tool.
#[tokio::test]
async fn create_mailbox_argument_is_named_mailbox() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let input = &find_tool(tools, "create_mailbox")["inputSchema"];
    let properties = input["properties"]
        .as_object()
        .expect("create_mailbox input properties");
    assert!(
        properties.contains_key("mailbox") && !properties.contains_key("mailboxName"),
        "create_mailbox takes `mailbox`, not `mailboxName`: {input:#}"
    );
}

/// `create_draft` output advertises the recovered draft identity as optional
/// nonzero uid/uidValidity plus a resourceUri.
#[tokio::test]
async fn create_draft_output_exposes_optional_draft_identity() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let output = &find_tool(tools, "create_draft")["outputSchema"];
    let properties = output["properties"]
        .as_object()
        .expect("create_draft output properties");
    for field in ["uid", "uidValidity", "resourceUri"] {
        assert!(
            properties.contains_key(field),
            "create_draft output must expose `{field}`: {output:#}"
        );
    }
    let required: Vec<&str> = output["required"]
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for field in ["uid", "uidValidity", "resourceUri"] {
        assert!(
            !required.contains(&field),
            "draft identity recovery is best-effort; `{field}` must be optional"
        );
    }
    for field in ["uid", "uidValidity"] {
        assert_eq!(
            schema_minimum(&properties[field]),
            Some(1.0),
            "create_draft `{field}` must be nonzero when present"
        );
    }
}

/// Unknown parameters are rejected in an isError tool result with the
/// offending field named, not
/// silently ignored — an agent passing a removed parameter (e.g. the old
/// `includeContent`) learns the current contract from the error instead of
/// wondering why the flag "did nothing".
#[tokio::test]
async fn unknown_parameters_are_rejected_not_ignored() {
    let mut client = McpClient::start().await;
    let resp = client
        .request(
            "tools/call",
            json!({
                "name": "get_messages",
                "arguments": {
                    "account": "dummy",
                    "mailbox": "INBOX",
                    "includeContent": true
                }
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], json!(true));
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|message| message.contains("includeContent")),
        "the error should name the unknown field: {resp:#}"
    );
}

/// The probe contract: a configured-but-unreachable account is a data outcome
/// (`connected: false` + error text), never a protocol error.
#[tokio::test]
async fn check_connection_reports_failure_as_data_not_error() {
    let mut client = McpClient::start().await;
    let resp = client
        .request(
            "tools/call",
            json!({"name": "check_connection", "arguments": {"account": "dummy"}}),
        )
        .await;

    assert!(
        resp.get("error").is_none(),
        "connectivity failure must not raise a protocol error: {resp:#}"
    );
    let result = &resp["result"];
    assert_ne!(
        result["isError"],
        json!(true),
        "connectivity failure is a successful probe result: {result:#}"
    );
    let structured = &result["structuredContent"];
    assert_eq!(structured["connected"], json!(false));
    assert!(
        structured["error"].as_str().is_some_and(|e| !e.is_empty()),
        "failed probe must carry the error text: {structured:#}"
    );
}

/// The probe's one exception: an unknown account is a parameter error and
/// raises invalid params.
#[tokio::test]
async fn check_connection_unknown_account_is_an_iserror_result() {
    let mut client = McpClient::start().await;
    let resp = client
        .request(
            "tools/call",
            json!({"name": "check_connection", "arguments": {"account": "no-such-account"}}),
        )
        .await;

    // An operational failure (the account doesn't exist) is a tool RESULT with
    // isError: true — NOT a JSON-RPC protocol error. The gateway classifies a
    // protocol error from a tool call as `BackendConnectionFailed` (the whole
    // backend "down"), so a bad account name must not present that way; the
    // agent should see the message and retry with a valid account.
    assert!(
        resp.get("error").is_none(),
        "operational failure must not be a protocol error: {resp:#}"
    );
    let result = &resp["result"];
    assert_eq!(
        result["isError"].as_bool(),
        Some(true),
        "unknown account must be an isError result: {result:#}"
    );
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("Account not found"),
        "the error message reaches the caller: {result:#}"
    );
}

/// A task-augmented call must PUSH `notifications/tasks/status` when it
/// reaches a terminal state.
///
/// Task status is computed LAZILY here (`refresh_managed_task` runs on read),
/// so before this the transition was observable only by polling `tasks/get` —
/// a client that started a `delete_by_domain` over thousands of messages had
/// no way to be told it finished.
///
/// Asserting on the notification specifically matters: every other task test
/// in this file uses `request`, which skips notifications, so all of them
/// passed against a server that never pushed at all.
#[tokio::test]
async fn task_terminal_transition_is_pushed_as_a_notification() {
    let mut client = McpClient::start().await;
    let queued = client
        .request(
            "tools/call",
            json!({
                "name": "unsubscribe_message",
                "arguments": {
                    "account": "dummy",
                    "mailbox": "INBOX",
                    "uid": 1,
                    "expectedUidValidity": 1,
                    "confirmOneClick": false
                },
                "task": {"ttl": 60_000}
            }),
        )
        .await;
    assert!(queued.get("error").is_none(), "enqueue failed: {queued:#}");
    let task_id = queued["result"]["task"]["taskId"]
        .as_str()
        .expect("task id")
        .to_string();

    let note = client
        .wait_for_notification("notifications/tasks/status", Duration::from_secs(10))
        .await
        .expect("server must PUSH tasks/status on the terminal transition");

    assert_eq!(note["params"]["taskId"], task_id.as_str());
    let pushed = note["params"]["status"].as_str().unwrap_or("");
    assert!(
        matches!(pushed, "completed" | "failed"),
        "pushed status must be terminal, got {pushed:?}"
    );

    // The PUSHED status must agree with the POLLED one — they are produced by
    // the same lazy refresh, and a disagreement would be worse than no push.
    let polled = client
        .request("tasks/get", json!({"taskId": task_id}))
        .await;
    assert_eq!(
        polled["result"]["status"].as_str(),
        Some(pushed),
        "pushed status must match a subsequent tasks/get: {polled:#}"
    );
}
