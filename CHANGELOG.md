---
created: 2026-05-29T19:20
updated: 2026-10-10T00:00
---
# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Sender identity** — `display_name` in an account's config makes drafts
  `From: Mark Blake <you@example.com>`. `create_draft` and `update_draft` take an
  optional `from` (`Name <address>` or an address) that must be one of the
  account's identities — the primary address, then its aliases — and refuse any
  other address with the list of usable ones. Both results report the `from`
  actually written. The CLI gains `create-draft --from`, and `configure` asks
  for "Your name (shown on drafts)". `AccountConfig::identities()`,
  `with_display_name` and `validate_display_name` are public.
- **`list_identities` tool** (36 tools) — an account's configured identities
  plus the From addresses among its newest Sent messages (default 200, max
  1000; `EXAMINE` + `BODY.PEEK`), each with a count, the display names used and
  when it was last used. Evidence only: a discovered address is not sendable
  until it is added as an alias. `list_accounts` now carries each account's
  `displayName` and `addresses`.
- **`ListIdentitiesResponse` and `SenderIdentity` implement `Deserialize`**, so
  an embedder can read the `list_identities` tool's structured result back into
  the library type (the wire output is asserted to round-trip). Agent Muse's
  Mail Accounts settings use it to offer the Sent addresses as aliases.
- **Every recipient field documents `Name <address>`** — `to`, `cc`, `bcc` and
  `replyTo` on both draft tools, and the handshake instructions — so agents keep
  the names users write instead of stripping them to bare addresses.
- **A stuck move can be dismissed** — `reconcile_moves` with `dismiss: true`
  and an `operationId` (CLI `reconcile-moves --dismiss`, library
  `Agentmail::dismiss_move`) closes a move reconciliation can't finish, once
  both mailboxes have been checked. It moves, copies and deletes nothing; it
  releases the source message and frees both mailboxes for rename and delete.
  Results gain `dismissed` and `errors`.
- **Evidence-grade RFC822 archive tools** — `download_message_source` writes one
  exact `BODY.PEEK[]` result directly to a create-new private file, returning
  its SHA-256, parsed message metadata, and a contemporaneous DNS-backed local
  DKIM result. `download_thread` applies the same contract to a caller-selected
  set of up to 100 UIDs and creates a JSON manifest.

### Changed

- **IMAP's TLS is rustls, with the OS checking certificates.** `native-tls`
  and `tokio-native-tls` are gone; connections use `tokio-rustls` on aws-lc-rs,
  and `rustls-platform-verifier` hands the certificate check to the OS
  (Security.framework, CryptoAPI, the system store on Linux), so roots a person
  or their MDM installed still count. On macOS IMAP now negotiates TLS 1.3:
  native-tls ran Secure Transport there, which never offered it. A server whose
  TLS 1.2 offers only non-forward-secret (RSA key exchange) or CBC suites no
  longer connects; Linux no longer links OpenSSL. **Breaking:**
  `AgentmailError::Tls` carries the handshake's `std::io::Error` instead of a
  `native_tls::Error`, and is no longer a `From` conversion.
  `imap_client::tls_client_config()` is public.
- **Rust 1.99 or newer** (`rust-version`).
- **Replies are From the address the original reached** — the original's own
  `From` for a follow-up to this account's mail, else the first identity in its
  `To`, then `Cc`. `update_draft` keeps the draft's current `From` (name
  included) when it is one of the account's identities instead of overwriting
  it with the primary address.
- **Replies go to every Reply-To address.** `MessageInfo.reply_to` is now
  `Vec<String>` (JSON `replyTo` is an array), where only the first address was
  kept before.
- **Breaking library API (0.7.0)** — `create_draft_with_headers`,
  `create_reply_draft` and `update_draft` take `from: Option<&str>`;
  `CreateDraftResponse`/`UpdateDraftResponse` gain `from`; `AccountConfig`
  gains `display_name`; `AccountInfo` gains `display_name` and `addresses`.
- **A distinct login is never a sending identity** unless listed in `aliases`;
  an alias equal to the login is now kept rather than normalized away.
- **`AGENTMAIL_CACHE_DIR` holds both caches directly** —
  `<dir>/header-cache-v1.sqlite3` beside `<dir>/mutation-journal.sqlite3`, the
  layout the builder's `cache_dir(dir)` already used. The header cache used to
  sit one level deeper (`<dir>/agentmail/`); that copy is no longer read and
  rebuilds on its own.
- **`SecretError` gains `NoEntry` and `KeyringTimedOut`** (breaking for
  exhaustive matches): a missing keychain entry was `Backend(..)`.
- **`list_flags` and `find_attachments` report `skipped`** (with
  `skippedTotal`/`skippedTruncated`, as the sweeps do): the mailboxes an
  account-wide scan did not cover. A mailbox it could not open used to vanish
  from the result without a trace.
- **`reconcile_moves` examines `needsAttention` moves again** and deletes a
  source only right after seeing its copy (the `COPYUID`) in the destination; a
  copy deleted in the meantime keeps the source and asks for review. An attempt
  that learns nothing counts as `pending` and is listed in `errors`, so `failed`
  now means only a COPY the server rejected.
- **The tool router is built once per process**, not on every `call_tool`,
  `list_tools` and `get_tool`.
- **Dependencies** — async-imap 0.11.3 → 0.12.0 (LOGIN no longer fails on a
  `NO` for another tag; unicode banners parse) with the vendored imap-proto
  rebased onto 0.17.0 (nom 8) and its UIDFETCH patch re-applied; mail-auth
  0.12.1 → 0.13.3; dirs 6 → 7; semver-compatible updates across the lockfile.
  rusqlite stays at 0.39 (the embedding app's sqlx-sqlite caps libsqlite3-sys
  below 0.38) and rmcp at 2.2.0 (3.x removes the task API; its own decision in
  the embedding workspace).

### Fixed

- **Dead pooled sessions passed their liveness check.** async-imap's `NOOP`
  treats end-of-stream as success, and on macOS a reset connection reads as
  end-of-stream, so after sleep or a server `BYE` the pool handed out dead
  sessions and drafts reported a false "APPEND outcome is ambiguous". `NOOP` now
  passes only on its own tagged `OK`.
- **One dropped connection could strand a COPY-fallback move for good.** Any
  failure opening the source mailbox during reconciliation — a lost connection
  included — parked the move as `needsAttention`, which reconciliation then
  returned unexamined, and a pending move blocks rename and delete of both its
  mailboxes. Now only a live server's answer (a new `UIDVALIDITY`,
  `NO [NONEXISTENT]`) parks a move; anything else leaves it as it was.
- **A connection lost right after SELECT could mark a move complete** while
  the source survived, releasing its claim: async-imap reads end-of-stream as
  an empty SEARCH. The check now needs the search's own tagged `OK`, and on a
  Yahoo/AOL Limited Mode session "not found" proves nothing.
- **A dropped connection made account-wide scans and sweeps report mailboxes
  they never read.** On a closed stream SELECT, EXAMINE, SEARCH and FETCH come
  back empty instead of failing, so every remaining mailbox "drained" or
  counted as empty, the dead session went back to the pool, and a hung one
  waited out the timeout (120 s in the app) once per remaining mailbox. A
  mailbox now counts only once the connection answers a checked `NOOP` after
  it; a lost connection ends the scan or sweep at once and lists that mailbox
  and every later one in `skipped`. A single-mailbox `list_flags` or
  `find_attachments`, and `preview_thread_record`, fail instead of returning a
  partial answer.
- **A connection lost after a mutation could report it failed, or pool a dead
  session.** The NOOP that follows a CREATE, delete, move, flag change or
  superseded-draft cleanup was async-imap's unchecked one and was `?`-ed: a
  server that dropped the client after answering turned a finished delete into
  an error, and a closed stream passed and went back to the pool. That NOOP is
  now checked and decides only whether the session is pooled; the mutation's
  own answer stands.
- **A connection lost mid-LIST cached a short mailbox list**, and account-wide
  scans then silently skipped the mailboxes it left out. LIST now counts only
  once the connection answers a checked `NOOP` after it.
- **A connection lost mid-read passed for an empty or short answer.**
  `search_messages`, `get_messages`, `list_identities`, `list_mailboxes` and
  the other retried reads now probe the connection after reading, so a dead
  socket triggers their one retry on a fresh connection instead of returning
  what little arrived. A message is reported missing — and pruned from the
  ranking cache, which the subscription move, unsubscribe and ranking samples
  do — only when a live server leaves it out. `update_flags` reads its result
  back for the message it changed, not another client's concurrent update.
- **Pool and catalog timers stopped while the Mac slept.** Idle eviction, the
  `[LIMIT]` login cooldown and the mailbox-catalog TTL now run on the wall
  clock; a clock set back counts as expired.
- **Replying to a sender with a comma or `@` in their name failed.** Display
  names holding RFC 5322 specials are quoted (`"Blake, Mark" <m@x>`), and an
  agent-written `Blake, Mark <m@x>` is accepted as one mailbox — while
  `Bob <b@y>, Alice <a@x>` is still refused rather than losing Bob.
- **A draft for an account with no email address** failed as "Invalid email
  address 'johnappleseed'"; it now says to set `email`.
- **`download_attachments` returned the bare filename as `path`**; it is now the
  absolute, canonical path, as `download_message_source` returns.
- **An expired task's status watcher waited forever** — the TTL prune aborted
  the worker without publishing a result. Expiry now publishes a cancelled one.
- **The UID Mode walk could loop forever** on a server that ignores the UID
  range, re-fetching the same page until cancelled. A page that does not move
  the range down is now refused.
- **Unsolicited FETCH responses** — another client changing flags while a
  fetch runs — failed `get_messages` ("message UID 0") and could add empty or
  duplicate rows to the ranking projection. Only the requested UIDs, each with
  its fetched section, are kept.
- **The keepalive could open a second connection** on one-connection
  providers (Yahoo/AOL): it took idle sessions out of the pool without a
  connection permit, so an acquire during the ping LOGINed anew. It now pings
  under a permit and skips accounts whose connections are all busy.
- **A locked keychain** could stall every connect for the account (the read
  had no deadline and runs under the connect lock), and without a configured
  `password` it was reported as "No password found". Keychain calls now give
  up after 30 s with a reason, and only a genuinely absent entry reads as
  missing.
- **Attachments named past a filesystem's 255-byte limit could not be
  downloaded** ("File name too long"). The canonical `{uid}_{index}_{name}` is
  now shortened to 240 bytes, its extension kept and never split mid-character,
  and `/info` advertises exactly that name.
- **A `download_attachments` failure partway left files behind**, unreported.
  The download is now all or nothing: every name is checked before writing,
  and a failure removes what was already written.
- **`configure` wrote Rust escapes into TOML** (`\u{301}` for a decomposed
  accent), producing a config file that failed to parse; every string is now
  written by the TOML serializer.
- **README claimed `~/.config/agentmail/config.toml` works on macOS**; only
  `~/Library/Application Support/agentmail/config.toml` (or
  `AGENTMAIL_CONFIG`) is read.

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
