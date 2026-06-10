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

#[tokio::test]
async fn initialize_reports_capabilities_and_identity() {
    let client = McpClient::start().await;
    let init = &client.init_result;

    let caps = init["capabilities"]
        .as_object()
        .expect("capabilities object");
    for cap in ["tools", "prompts", "tasks"] {
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
}

#[tokio::test]
async fn invalid_params_yields_32602() {
    let mut client = McpClient::start().await;
    let resp = client
        .request(
            "tools/call",
            json!({
                "name": "delete_messages",
                "arguments": {"account": "dummy", "uids": []}
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
async fn rank_limit_documents_default_of_100() {
    let mut client = McpClient::start().await;
    let resp = client.request("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    for name in ["rank_senders", "rank_unsubscribe", "rank_list_id"] {
        let tool = tools
            .iter()
            .find(|t| t["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("tool `{name}` missing"));
        let desc = tool["inputSchema"]["properties"]["limit"]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(
            desc.contains("Defaults to 100"),
            "`{name}` limit description should document the default: {desc}"
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
