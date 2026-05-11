# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **MCP tasks** — added task management to support background execution and polling of long-running tools.
- **Mailbox roles** — added `role_from_attributes` to parse RFC 6154 roles with fallback logic for older servers.
- **Tool synchronization** — added async mutexes to serialize destructive tool executions per-account.
- **Keychain tests** — added unit tests for `Secret` (Raw/Command paths plus a keyring roundtrip via `keyring_core::mock::Store`) and for the macOS error-code classifier (-25307, -25308, -34018).

### Changed
- **Mailbox detection** — replaced hardcoded mailbox names with auto-detection using RFC 6154 special-use attributes (`Trash`, `Drafts`).
- **MCP transport** — replaced custom `CompatStdioWorker` with the standard `rmcp` stdio transport.
- **Mailbox info** — updated `MailboxInfo` to expose `no_select`, `no_inferiors`, and `role`.
- **Tool configurations** — updated all applicable tools to include `task_support = "optional"`.
- **rmcp** — bumped to 1.6 (adds 2025-11-25 protocol support; Origin validation, session store, and other HTTP-only features are not used since agentmail is stdio-only).
- **macOS keychain** — prefer the data-protection keychain backend, falling back to the legacy file-based keychain when the binary lacks the entitlement. Improves reliability in headless/launchd contexts.
- **Tests** — switched `ci-check.sh` to `cargo nextest run` (with a `cargo test` fallback) and added a `.config/nextest.toml`.

### Fixed
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
