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
                password: Some(Secret::new_raw("unused")),
                tls: true,
                max_connections: None,
            },
        )]);
        tokio::spawn(async move {
            let _ = agentmail::mcp::serve_on(server_io, Agentmail::new(config)).await;
        });
        let (r, w) = tokio::io::split(client_io);
        let mut client = Self {
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
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timeout waiting for `{method}` response"))
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

    assert_eq!(
        init["serverInfo"]["name"].as_str(),
        Some("agentmail"),
        "server must announce itself as agentmail, not rmcp"
    );
    assert_eq!(
        init["serverInfo"]["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
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
async fn tools_list_has_21_annotated_tools() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(
        tools.len(),
        21,
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
    let structured_json = serde_json::to_string(&result["structuredContent"]).unwrap();
    assert_ne!(
        fallback, structured_json,
        "text fallback must not duplicate structuredContent JSON"
    );
    assert!(
        !fallback.trim_start().starts_with('{'),
        "fallback should be a concise summary, not serialized JSON: {fallback}"
    );
    assert!(fallback.contains("dummy"));
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
async fn invalid_params_yields_32602() {
    let mut client = McpClient::start().await;
    let resp = client
        .request(
            "tools/call",
            json!({
                "name": "delete_messages",
                "arguments": {
                    "account": "dummy",
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

    for name in [
        "delete_messages",
        "delete_by_sender",
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

    for arguments in [
        json!({}),
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
    for name in [
        "delete_messages",
        "delete_by_sender",
        "delete_list_id",
        "unsubscribe_message",
    ] {
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
    for field in [
        "deleteMatching",
        "deleteOnUnsubscribeFailure",
        "allowSenderFallback",
        "allowPermanentFallback",
        "permanent",
    ] {
        assert_eq!(
            schema["properties"][field]["default"],
            json!(false),
            "{field} must default fail-closed"
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
    assert_eq!(missing["error"]["code"], json!(-32602));

    let no_consent = client
        .request(
            "tools/call",
            json!({
                "name": "unsubscribe_message",
                "arguments": {
                    "account": "dummy",
                    "uid": 1,
                    "expectedUidValidity": 1,
                    "confirmOneClick": false
                }
            }),
        )
        .await;
    assert_eq!(no_consent["error"]["code"], json!(-32602));
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
