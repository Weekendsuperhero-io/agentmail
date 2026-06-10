---
created: 2026-05-29T19:20
updated: 2026-05-29T19:20
---
# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **MCP resources** — single messages are addressable via two URI templates: `email://{account}/{mailbox}/{uid}` (markdown) and `email://{account}/{mailbox}/{uid}/source` (raw RFC822). Account/mailbox segments are percent-encoded (`/` in mailbox names as `%2F`). Missing UIDs return `-32002` (resource not found).
- **MCP completions** — `completion/complete` for prompt arguments and the `email://` template variables: `account` completes instantly from config; `mailbox` runs a context-scoped IMAP LIST and never errors on failure.
- **Cooperative cancellation** — a `CancelFn` callback (mirroring `ProgressFn`) threaded through all scan/delete paths, checked at mailbox and fetch-chunk boundaries; MCP wires it to the request's cancellation token, so `notifications/cancelled` and transport shutdown stop long scans.
- **MCP integration tests** — in-process duplex JSON-RPC tests covering initialize, tools/list, wire schema shape, tool calls, and error codes; plus unit tests pinning schema `$ref`-freedom, tool titles, and `DESTRUCTIVE_TOOLS` ↔ annotation sync.
- **Tool titles** — every MCP tool now carries a human-readable `annotations.title`; `add_flags`/`remove_flags` are marked idempotent and `list_accounts` is marked closed-world.
- **MCP tasks** — added task management to support background execution and polling of long-running tools.
- **Mailbox roles** — added `role_from_attributes` to parse RFC 6154 roles with fallback logic for older servers.
- **Tool synchronization** — added async mutexes to serialize destructive tool executions per-account.
- **Keychain tests** — added unit tests for `Secret` (Raw/Command paths plus a keyring roundtrip via `keyring_core::mock::Store`) and for the macOS error-code classifier (-25307, -25308, -34018).

### Changed
- **rank tools default limit** — `rank_senders`, `rank_unsubscribe`, and `rank_list_id` now return at most 100 entries over MCP unless a higher `limit` is passed (previously unlimited; CLI behavior unchanged).
- **Module layout** — `src/mcp.rs` split into `src/mcp/` modules (args, tools_read, tools_write, prompts, resources, tasks); no behavior change.
- **Library API (breaking)** — scan/delete functions (`group_by_*`, `list_flags`, `find_attachments`, `delete_*`, `unsubscribe_message`) take a new `cancel: Option<&CancelFn>` parameter; next release should be 0.3.0.
- **MCP error codes** — input-validation and not-found failures now return `-32602` (invalid params) instead of `-32603` (internal error), so clients can distinguish bad arguments from server faults.
- **MCP tool schemas** — nested parameter/response types are inlined via `#[schemars(inline)]` so tool input/output schemas contain no `$defs`/`$ref`; fixes hosts (Gemini CLI, n8n, some gateways) that reject or drop referenced schemas.
- **Mailbox detection** — replaced hardcoded mailbox names with auto-detection using RFC 6154 special-use attributes (`Trash`, `Drafts`).
- **MCP transport** — replaced custom `CompatStdioWorker` with the standard `rmcp` stdio transport.
- **Mailbox info** — updated `MailboxInfo` to expose `no_select`, `no_inferiors`, and `role`.
- **Tool configurations** — updated all applicable tools to include `task_support = "optional"`.
- **rmcp** — bumped to 1.7 (adds 2025-11-25 protocol support and stdio parse-error resilience; Origin validation, session store, and other HTTP-only features are not used since agentmail is stdio-only). Features are now declared explicitly (`server`, `macros`, `transport-io`).
- **macOS keychain** — prefer the data-protection keychain backend, falling back to the legacy file-based keychain when the binary lacks the entitlement. Improves reliability in headless/launchd contexts.
- **Tests** — switched `ci-check.sh` to `cargo nextest run` (with a `cargo test` fallback) and added a `.config/nextest.toml`.

### Fixed
- **MCP server identity** — `initialize` now reports `serverInfo.name = "agentmail"` with the crate version instead of `rmcp/1.7.0` (rmcp's `from_build_env()` bakes in its own crate name).
- **Log hygiene** — stderr logs disable ANSI colors when stderr is not a terminal, keeping MCP-host-captured log files clean.
- **Keychain errors** — surface `errSecNoDefaultKeychain` (-25307), `errSecInteractionNotAllowed` (-25308), and `errSecMissingEntitlement` (-34018) as typed `SecretError` variants with remediation hints, instead of opaque string failures.
- **Keychain init logging** — stopped silently swallowing platform-store initialization failures; they now log via `tracing::warn!`.

### Removed
- **Account configuration** — removed explicit `trash_mailbox` and `drafts_mailbox` settings from `AccountConfig`.
- **Mail providers** — removed the `Outlook` provider from `MailProvider`.

### Added
- **AgentMail MCP server** — added initial MailKit MCP server with 21 tools and 6 prompts for AI assistant email integration.
- **IMAP client** — added a complete implementation with connection pooling, multi-provider support, and HTML to Markdown conversion.
- **CI/CD workflows** — added reusable workflows for PR descriptions, changelogs, cross-platform binary builds, and GitHub Releases.

### Changed
- **Secrets management** — migrated from `secret-lib` to `keyring-core` to utilize native OS keyring stores across platforms.
- **Workspace structure** — restructured into a Rust workspace with separate `agentmail` (library) and `agentmail-mcp` (binary) crates.
- **Performance** — replaced standard library `HashMap` with `hashbrown::HashMap` across the codebase.
- **Dependencies** — upgraded `rmcp` to version 1.3 and updated various workspace dependencies.
- **Documentation** — updated README, DESIGN, and MCP docs to reflect the current tool set, commands, and architecture.

### Fixed
- **Linux CI builds** — added missing `libdbus-1-dev` and `pkg-config` dependencies to the release workflows.
- **Tracing** — fixed application tracing issues.
- **CI jobs** — removed an extra unnecessary job from the pipeline.

### Removed
- **Legacy crates** — removed duplicated legacy code under `crates/agentmail` and `crates/agentmail-mcp` to establish the root crate as the source of truth.

### Security
- **Log privacy** — masked account email addresses and sensitive identifiers in connection logs and standard error output.
- **quinn-proto vulnerability** — bumped `quinn-proto` from 0.11.13 to 0.11.14 to patch a denial of service issue.
