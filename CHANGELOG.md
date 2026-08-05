---
created: 2026-05-29T19:20
updated: 2026-08-04T00:00
---
# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Evidence-grade RFC822 archive tools** — `download_message_source` writes one
  exact `BODY.PEEK[]` result directly to a create-new private file, returning
  its SHA-256, parsed message metadata, and a contemporaneous DNS-backed local
  DKIM result. `download_thread` applies the same contract to a caller-selected
  set of up to 100 UIDs and creates a JSON manifest.

### Security

- **Archive write confinement** — message source and manifest filenames must be
  portable basenames, output directories are confined to
  `AGENTMAIL_FILE_ROOT`, existing files are never overwritten, every UID is
  guarded by live UIDVALIDITY, and downloads do not mark messages seen.
- **SPF evidence boundary** — archive output does not promote an untrusted
  `Authentication-Results` header to a local SPF verdict because stored RFC822
  bytes lack the delivery-time SMTP peer, HELO, and envelope-sender inputs.

## [0.4.0] - 2026-07-22

### Highlights

- **Domain organization** — added `top_domains`, `delete_by_domain`, and
  `move_by_domain`, with registrable-domain and subdomain breakdowns plus a
  representative subject for each exact domain.
- **Exact requested limits** — ranking pages now return up to the requested
  `limit`; the five-item cap applies only to the documented mailing-list sender
  preview.
- **Recoverable mutations** — MOVE fallbacks use a durable mutation journal,
  operation IDs, pending-operation inspection, and explicit reconciliation
  instead of silently repeating uncertain COPY/delete work.
- **MCP tasks and SDK** — upgraded to `rmcp` 2.2 and the 2025-11-25 MCP task
  model, including background execution, result retrieval, paging, and
  cancellation for long-running tools.
- **Configuration and credential security** — validates account and transport
  settings, supports separate primary email addresses and aliases, requires
  TLS, hides password prompts, writes configuration atomically with private
  Unix permissions, and bounds/redacts credential-helper execution.
- **Optional IMAP compression** — negotiates RFC 4978 `COMPRESS=DEFLATE` after
  login when the server advertises it, while retaining ordinary TLS sessions
  on servers without the capability.

## [0.3.0] - 2026-07-18

### Added
- **Authenticated one-click unsubscribe** — RFC 8058 execution now fetches the complete target transiently and verifies DKIM locally with `mail-auth`; at least one passing signature must include both `List-Unsubscribe` and `List-Unsubscribe-Post` in its `h=` tag. The full message is never cached.
- **Unsubscribe action identity** — `top_subscriptions` returns a nested `(mailbox, uidValidity, uid)` sample; `unsubscribe_message` requires the epoch as `expectedUidValidity` and compares it after live `EXAMINE` before trusting the sample UID.
- **Unsubscribe HTTP integration tests** — socket-level tests cover the exact form POST, direct 2xx success, 3xx rejection without following redirects, non-2xx failure, private-destination blocking, and cancellation while awaiting a response.
- **Search date & size filters** — `search_messages` gains `since`/`before` (YYYY-MM-DD, by server internal date → IMAP `SINCE`/`BEFORE`) and `larger_than`/`smaller_than` (bytes → `LARGER`/`SMALLER`) for "older than" / "bigger than" cleanup queries. Filters are AND-combined; bad dates return `-32602`.
- **Gmail-aware delete** — on Gmail (`X-GM-EXT-1`), deletes route through `[Gmail]/Trash` because in-place `\Deleted`+EXPUNGE only removes a label, leaving the message in All Mail. Permanent deletes also route to Trash on Gmail (Gmail purges Trash on its own).
- **Permanent delete** — `delete_messages`, `delete_by_sender`, `delete_list_id`, and `unsubscribe_message` accept a `permanent` flag (default false). When true, messages are flagged `\Deleted` and UID-expunged directly, bypassing Trash. Backed by a new `DeleteMode` enum in the library API.
- **Capability gating** — per-account `ServerCaps` (cached in the connection pool) selects command variants: UID MOVE when the server advertises MOVE, else COPY + `\Deleted` + UID EXPUNGE; the RECENT STATUS item is requested only when the server advertises IMAP4rev1.
- **Mailbox layout catalog** — mailbox completion and Trash/Drafts resolution share a bounded, five-minute, process-local cache containing only paths, delimiters, attributes, and special-use roles.
- **Account scan planner** — account-wide read discovery uses one selectable `\All` mailbox when available; otherwise it enumerates selectable storage mailboxes. Destructive scans never target aggregate or virtual special-use views.
- **Validated ranking-header cache** — `top_senders`, `top_subscriptions`, and `top_mailing_lists` share a schema-v3 SQLite cache of UID membership and a restricted immutable header projection. Live `UIDVALIDITY`, `UIDNEXT`, and message count validate reuse; mailbox revisions and account mutation generations fence publication. Proven appends fetch only the tail; deletions reconcile membership and reuse unchanged rows. Cache failure degrades to a live scan.
- **MCP resources** — single messages have three UIDVALIDITY-safe templates: `email://{account}/{mailbox}/{uidValidity}/{uid}`, its `/headers` form, and its `/source` form. They respectively expose a 100K-character markdown view, a 64-KiB exact header block, and up to 256 KiB of raw RFC822 source as a lossless base64 MCP blob. Missing UIDs and stale epochs return `-32002` (resource not found).
- **MCP completions** — `completion/complete` for prompt arguments and the `email://` template variables: `account` completes instantly from config; `mailbox` uses the layout catalog, refreshing it with one context-scoped IMAP LIST when cold or expired, and never errors on failure.
- **Cooperative cancellation** — a `CancelFn` callback (mirroring `ProgressFn`) threaded through all scan/delete paths, checked at mailbox and fetch-chunk boundaries; MCP wires it to the request's cancellation token, so `notifications/cancelled` and transport shutdown stop long scans.
- **MCP integration tests** — in-process duplex JSON-RPC tests covering initialize, tools/list, wire schema shape, tool calls, and error codes; plus unit tests pinning schema `$ref`-freedom, tool titles, and `DESTRUCTIVE_TOOLS` ↔ annotation sync.
- **Tool titles** — every MCP tool now carries a human-readable `annotations.title`; `add_flags`/`remove_flags` are marked idempotent and `list_accounts` is marked closed-world.
- **MCP tasks** — added background execution and polling for 10 long-running tools, with a 128-task process cap, 24-hour creation-based retention, newest-first 25-row opaque-cursor pages, repeatable result retrieval, and cancellation on expiry.
- **Mailbox roles** — preserves multiple special-use attributes and recognizes the current registered IMAP roles, including `\Important`, `\Memos`, `\Scheduled`, and `\Snoozed` in addition to the RFC 6154 set.
- **Tool synchronization** — added async mutexes to serialize destructive tool executions per-account.
- **Keychain tests** — added unit tests for `Secret` (Raw/Command paths plus a keyring roundtrip via `keyring_core::mock::Store`) and for the macOS error-code classifier (-25307, -25308, -34018).

### Changed
- **Unsubscribe safety policy (breaking)** — `unsubscribe_message` now requires `confirmOneClick=true`; `deleteMatching` defaults to false and matches exact normalized List-Id. `deleteOnUnsubscribeFailure`, `allowSenderFallback`, and `allowPermanentFallback` are separate opt-ins that default false. The library method now accepts `UnsubscribeOptions`.
- **One-click discovery naming** — `top_subscriptions.oneClick` is now `advertisedOneClick` to make clear that cached header syntax is not a DKIM verification result; execution always re-fetches and validates the message.
- **MCP 0.3 identity contract (breaking)** — every delayed UID action now pairs the UID with required `expectedUidValidity`. This applies to `delete_messages`, `delete_by_sender`, `move_message`, `download_attachments`, `add_flags`, `remove_flags`, and `unsubscribe_message`; a live epoch mismatch fails before the action.
- **Metadata-first message discovery (breaking)** — `get_messages` and `search_messages` no longer accept body/header inclusion switches or return full `MessageInfo` values over MCP. Results contain compact metadata, response-level UIDVALIDITY, and canonical body-resource URIs.
- **Attachment identities (breaking)** — `find_attachments` returns paginated mailbox/UIDVALIDITY/UID identities and resource URIs instead of a flat account-wide UID list.
- **Selectable mailbox paging (breaking)** — `list_mailboxes` now requires `account`, returns selectable mailboxes only, and paginates with offset 0, default limit 100, maximum 500, `total`, and `nextOffset`; filtering and pagination occur before per-mailbox STATUS calls.
- **Compact MCP results (breaking)** — all 21 tools now return one short fallback text block plus one authoritative `structuredContent` object instead of duplicating the full escaped JSON in text. Public output DTOs omit credentials, message bodies, raw unsubscribe values, redundant draft echoes, and other fields that do not support the next action; schemas remain root objects without `$defs`/`$ref`.
- **Tool rename** — `rank_senders`/`rank_unsubscribe`/`rank_list_id` are now **`top_senders`/`top_subscriptions`/`top_mailing_lists`** (clearer: a volume-sorted summary, not an action). Lib fns, MCP tool names, and CLI subcommands renamed to match.
- **Top senders exclude self** — `top_senders` and `top_subscriptions` skip the account's own address, so your own sent mail no longer ranks you as a top sender.
- **Top-N scans** — sender and List-* ranking now share one immutable projection with a marker for every returned UID, so non-list and malformed messages are not repeatedly fetched. Interrupted cold scans retain completed 1,000-UID header chunks for restart-safe resume.
- **Top-tool pagination (breaking)** — `top_senders`, `top_subscriptions`, and `top_mailing_lists` now default to 10 ranked groups, accept at most 100 per page, and expose `offset`/`nextOffset`. Every row includes a nested actionable sample identity; mailing-list sender previews are capped at five with a separate total count.
- **Module layout** — `src/mcp.rs` split into `src/mcp/` modules (args, tools_read, tools_write, prompts, resources, tasks); no behavior change.
- **Library API (breaking, → 0.3.0)** — scan/delete functions gained a `cancel: Option<&CancelFn>` parameter and the delete functions a `mode: DeleteMode` parameter; the top-N library functions are now `top_senders`/`top_subscriptions`/`top_mailing_lists` (was `group_by_sender`/`group_by_list`/`group_by_list_id`); `MailboxInfo` and `MailboxEntry` gained `roles`; `build_search_query_pub` now returns `Result`; `imap_timeout` preserves typed errors.
- **MCP error codes** — input-validation and not-found failures now return `-32602` (invalid params) instead of `-32603` (internal error), so clients can distinguish bad arguments from server faults.
- **MCP tool schemas** — every tool has an explicit output schema, and nested parameter/response types are inlined via `#[schemars(inline)]` so schemas contain no `$defs`/`$ref`; fixes hosts (Gemini CLI, n8n, some gateways) that reject or drop referenced schemas.
- **Mailbox detection** — replaced hardcoded mailbox names with auto-detection using RFC 6154 special-use attributes (`Trash`, `Drafts`).
- **MCP transport** — replaced custom `CompatStdioWorker` with the standard `rmcp` stdio transport.
- **Mailbox info** — updated `MailboxInfo` to expose `no_select`, `no_inferiors`, and `role`.
- **Mailbox info roles** — added `roles` with every recognized special-use role; singular `role` remains as the compatibility projection of the first role.
- **Tool configurations** — updated all applicable tools to include `task_support = "optional"`.
- **rmcp** — bumped to 1.7 (adds 2025-11-25 protocol support and stdio parse-error resilience; Origin validation, session store, and other HTTP-only features are not used since agentmail is stdio-only). Features are now declared explicitly (`server`, `macros`, `transport-io`).
- **macOS keychain** — prefer the data-protection keychain backend, falling back to the legacy file-based keychain when the binary lacks the entitlement. Improves reliability in headless/launchd contexts.
- **Tests** — switched `ci-check.sh` to `cargo nextest run` (with a `cargo test` fallback) and added a `.config/nextest.toml`.

### Fixed
- **RFC 8058 trust and SSRF boundary** — one-click now rejects duplicate or inexact headers, HTTP URLs, multiple HTTPS URLs, embedded credentials, fragments, non-public IPs, empty or mixed public/private DNS results, proxies, retries, redirects, and every response outside direct 2xx. DNS answers are validated once and pinned into the request.
- **Unsubscribe cleanup trust boundary** — matching cleanup uses the normalized identifier inside `List-Id` only when the same passing DKIM signature also covers that single header, so an unauthenticated List-Id cannot authorize an account-wide sweep. Exact-sender matching remains available only through an explicit fallback policy.
- **Unsubscribe delete escalation** — the unsubscribe path no longer turns a failed Trash move into UID EXPUNGE unless `allowPermanentFallback=true`; the response reports actual fallback and completeness.
- **Gmail unsubscribe cleanup** — a failed move to Gmail Trash is never converted to in-place EXPUNGE because that operation only removes the current label. A Gmail `permanent=true` cleanup safely reports the actual Trash disposition instead of claiming a hard delete.
- **Unsubscribe cancellation** — DNS lookup, DKIM verification, and the outbound HTTP wait are cancellation-aware instead of waiting for their full timeouts.
- **Bounded DKIM source fetch** — one-click execution preflights `RFC822.SIZE` and uses a capped IMAP partial fetch, limiting the transient complete-message allocation to 64 MiB while leaving matching-message deletion counts unlimited.
- **Transient login failures** — `connect` now retries a transient connect/auth failure up to twice with backoff (fresh connection each time). iCloud and Gmail routinely reply `[AUTHENTICATIONFAILED]` to a login and then accept the same credentials moments later; previously that one-off surfaced to the host as `-32603`. The retry count is small so a genuinely wrong password still fails fast without risking account lockout.
- **Cross-folder double-counting** — `top_senders`/`top_subscriptions`/`top_mailing_lists` deduplicate by `Message-ID` across folders, so a message that appears under several Gmail labels (or in All Mail) is counted once. Counts now reflect unique messages; messages without a Message-ID can't be deduped and are counted each.
- **All Mail in account scans** — read-only account discovery now scans a selectable `\All` mailbox exclusively (including a conservative `All Mail` name fallback), avoiding repeated work across label/folder views. Mutation scans exclude it.
- **`delete_list_id` over-match** — confirms the exact `List-Id` per candidate before deleting; IMAP `HEADER` search is substring-only, so `"news"` could otherwise delete `"newsletter"` lists.
- **Non-ASCII search** — SEARCH queries with non-ASCII text now send a `CHARSET UTF-8` prefix (previously the text was sent as an invalid 7-bit quoted string and servers rejected or silently mismatched it). Server rejections surface as `-32602` invalid-params over MCP.
- **SEARCH command injection** — search text containing CR/LF was written to the wire unescaped (async-imap sends command bytes raw); such input is now rejected.
- **Draft Message-ID** — drafts now carry a generated `Message-ID` header (lettre adds `Date` automatically but not Message-ID); its absence broke threading and tripped some spam filters.
- **STATUS capability gating** — `list_mailboxes` requests RECENT only when IMAP4rev1 is advertised, avoiding unsupported STATUS items.
- **Permanent-delete safety** — refuses to expunge on servers lacking UIDPLUS, where plain EXPUNGE would remove unrelated `\Deleted` messages.
- **MCP server identity** — `initialize` now reports `serverInfo.name = "agentmail"` with the crate version instead of `rmcp/1.7.0` (rmcp's `from_build_env()` bakes in its own crate name).
- **Log hygiene** — stderr logs disable ANSI colors when stderr is not a terminal, keeping MCP-host-captured log files clean.
- **Keychain errors** — surface `errSecNoDefaultKeychain` (-25307), `errSecInteractionNotAllowed` (-25308), and `errSecMissingEntitlement` (-34018) as typed `SecretError` variants with remediation hints, instead of opaque string failures.
- **Keychain init logging** — stopped silently swallowing platform-store initialization failures; they now log via `tracing::warn!`.
- **List-Id-only ranking** — `top_mailing_lists` now retains messages with `List-Id` even when they have no List-Unsubscribe header.

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
- **Cache privacy** — schema v3 stores normalized ranking facts and booleans rather than List-Unsubscribe URLs, raw list-action headers, or recipient tokens. It excludes bodies, subjects, recipients, flags, attachments, passwords, authentication tokens, keychain secrets, and complete messages; its namespace does include the configured account/server/login identity to prevent collisions. SQLite uses WAL mode; upgrades enable secure deletion, rebuild the disposable projection, run `VACUUM`, and truncate the WAL. Unix cache directories and database files are restricted to `0700` and `0600`.
- **Log privacy** — masked account email addresses and sensitive identifiers in connection logs and standard error output.
- **quinn-proto vulnerability** — bumped `quinn-proto` from 0.11.13 to 0.11.14 to patch a denial of service issue.
