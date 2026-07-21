---
created: 2026-05-29T19:20
updated: 2026-07-18T00:00
---
# agentmail

IMAP email client exposed as both a CLI and an MCP (Model Context Protocol) server, built with Rust.

MCP protocol: [2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25) (also negotiates 2025-06-18, 2025-03-26, and 2024-11-05) | [rmcp](https://crates.io/crates/rmcp) (official Rust MCP SDK)

One binary: `agentmail serve` starts the MCP stdio server, all other subcommands are a direct CLI.

See also: [DESIGN.md](DESIGN.md) for architecture diagrams and design decisions,
[MCP.md](MCP.md) for the full MCP tool and prompt reference with output schemas,
and the [curated IMAP standards reference](docs/standards/imap/README.md) used
during protocol design and review.

## Requirements

- Rust toolchain (edition 2024)
- An IMAP-enabled email account on a server that advertises IMAP4rev1 (Gmail, iCloud, Yahoo, Fastmail, self-hosted, etc.). Dual rev1/rev2 servers are used in rev1 mode; pure IMAP4rev2 support is not yet available.

## Build

```bash
cargo build --release
```

Output binary: `target/release/agentmail`

## Configuration

agentmail reads its config from a single TOML file:

| Location | Path                                                        |
| -------- | ----------------------------------------------------------- |
| Default  | `~/.config/agentmail/config.toml`                           |
| Override | Set the `AGENTMAIL_CONFIG` environment variable to any path |

On macOS the default expands to `~/Library/Application Support/agentmail/config.toml` if `dirs::config_dir()` returns `Library/Application Support`, but `~/.config/agentmail/config.toml` is more conventional and works fine — just pick one.

### Quick start

The fastest way to add an account is the interactive `configure` command:

```bash
# With a provider preset (gmail, icloud, outlook, fastmail, yahoo)
agentmail configure gmail

# Or fully custom
agentmail configure
```

This prompts for your username, password method, writes the config file, and tests the connection.

### Single account

```toml
[accounts.personal]
host = "imap.gmail.com"
username = "you@gmail.com"
password.keyring = "you@gmail.com"
```

### Multiple accounts

Add as many `[accounts.<name>]` sections as you like. Each is a fully independent IMAP connection with its own credentials and settings. You do **not** need separate config files or multiple server instances.

```toml
[accounts.gmail]
host = "imap.gmail.com"
username = "you@gmail.com"
password.keyring = "you@gmail.com"

[accounts.icloud]
host = "imap.mail.me.com"
username = "johnappleseed"
password.cmd = "security find-internet-password -s imap.mail.me.com -a johnappleseed -w"

[accounts.work]
host = "imap.company.com"
username = "you@company.com"
password.cmd = "op read op://Work/Email/password"
```

All accounts are available simultaneously — the MCP tools and CLI commands accept an `account` parameter to select which one to operate on.

### Account config reference

| Field             | Type   | Default      | Description                                         |
| ----------------- | ------ | ------------ | --------------------------------------------------- |
| `host`            | string | **required** | IMAP server hostname                                |
| `port`            | u16    | `993`        | IMAP port                                           |
| `username`        | string | **required** | Login username / email                              |
| `password`        | Secret | —            | Password source (see [Passwords](#passwords) below) |
| `tls`             | bool   | `true`       | Use TLS                                             |
| `max_connections` | usize  | `3`          | Max concurrent IMAP connections for this account    |

Trash and drafts mailboxes are auto-detected at runtime via RFC 6154 special-use attributes (`\Trash`, `\Drafts`), with string-matching fallback for servers that don't support RFC 6154.

### Passwords

agentmail uses [secret-lib](https://crates.io/crates/secret-lib) for flexible credential management. The `password` field supports three sources:

**Shell command** (recommended for reusing existing credentials):

```toml
# Read from Apple Mail / macOS Keychain internet passwords
password.cmd = "security find-internet-password -s imap.mail.me.com -a johnappleseed -w"

# Read from pass (Unix password manager)
password.cmd = "pass show email/gmail"

# Read from 1Password CLI
password.cmd = "op read op://Personal/Gmail/password"

# Read from Bitwarden CLI
password.cmd = "bw get password gmail-imap"
```

The command is executed at connection time and the first line of stdout is used as the password. This is the most flexible option — it works with any password manager that has a CLI.

**System keyring** (recommended for standalone use):

```toml
password.keyring = "you@gmail.com"
```

Stores and retrieves from the system credential store (macOS Keychain, Windows Credential Manager, Linux Secret Service). The value is the keyring entry key; the service name is `"agentmail"`. Store a password with:

```bash
agentmail set-password --account gmail
```

**Raw string** (not recommended — plaintext in config file):

```toml
password.raw = "hunter2"
```

### Using Apple Mail / iCloud passwords

macOS Mail stores IMAP passwords as **internet password** items in the Keychain. You can read them directly using `password.cmd`:

```toml
[accounts.icloud]
host = "imap.mail.me.com"
username = "johnappleseed"
password.cmd = "security find-internet-password -s imap.mail.me.com -a johnappleseed -w"
```

This shells out to `security` at connection time, which reads Apple Mail's stored password. The first time you run this, macOS may prompt you to allow keychain access.

To find the correct server and account values for your setup:

```bash
# List all internet passwords for iCloud Mail
security find-internet-password -s "imap.mail.me.com"

# List for Gmail
security find-internet-password -s "imap.gmail.com"
```

### Password resolution order

When connecting, agentmail tries these sources in order and uses the first one found:

1. `AGENTMAIL_PASSWORD_<ACCOUNT>` environment variable (override for CI/Docker)
2. `password` field in config (command, keyring, or raw)
3. Default keyring lookup under `"agentmail"` service with username as key (backward compat for `set-password` users with no `password` field)

### Environment variable override

For CI, Docker, or headless servers, passwords can be passed via environment variables regardless of what's in the config file:

```bash
export AGENTMAIL_PASSWORD_GMAIL="app-specific-password"
export AGENTMAIL_PASSWORD_WORK="your-password"
```

The variable name is `AGENTMAIL_PASSWORD_` followed by the account name uppercased, with dashes and spaces replaced by underscores.

### Testing your setup

```bash
# 1. Check that the account appears in the config
agentmail list-accounts

# 2. Test IMAP connectivity and authentication
agentmail check-connection --account gmail

# 3. List mailboxes to confirm full access
agentmail list-mailboxes --account gmail
```

### Gmail setup

Gmail requires an [App Password](https://myaccount.google.com/apppasswords) (not your regular Google account password). Generate one, then:

```toml
[accounts.gmail]
host = "imap.gmail.com"
username = "you@gmail.com"
password.keyring = "you@gmail.com"
```

```bash
agentmail set-password --account gmail
# paste the 16-character app password
```

### iCloud Mail setup

iCloud uses your Apple ID with an [app-specific password](https://support.apple.com/en-us/102654). The IMAP login is your iCloud username (not full email):

```toml
[accounts.icloud]
host = "imap.mail.me.com"
username = "johnappleseed"
password.keyring = "johnappleseed"
```

Or reuse the password Apple Mail already stored in the Keychain:

```toml
[accounts.icloud]
host = "imap.mail.me.com"
username = "johnappleseed"
password.cmd = "security find-internet-password -s imap.mail.me.com -a johnappleseed -w"
```

### Migration from previous versions

If you're upgrading from a version that used `keychain_service` or `password = "..."`:

- `keychain_service` has been removed. Use `password.keyring = "your-username"` instead. Passwords previously stored via `set-password` are still found automatically (backward compat fallback).
- `password = "plaintext"` still works but is treated as `password.raw = "plaintext"` internally.

## Usage

### MCP Server

```bash
agentmail serve
```

Starts an MCP stdio server. Logs go to stderr; JSON-RPC on stdin/stdout.

### CLI

```bash
agentmail configure gmail              # interactive account setup
agentmail configure                    # interactive setup (custom provider)
agentmail list-accounts
agentmail list-mailboxes --account gmail
agentmail create-mailbox --account gmail --name "Archive/2024"
agentmail check-connection --account gmail
agentmail list-capabilities --account gmail
agentmail set-password --account gmail
agentmail get-messages --account gmail --mailbox INBOX --limit 10
agentmail get-messages-by-uid --account gmail --uids 123 456
agentmail top-senders --account gmail --limit 20
agentmail top-subscriptions --account gmail --limit 20
agentmail find-attachments --account gmail
agentmail download-attachments --account gmail --uid 123 --output-dir ./downloads
agentmail list-flags --account gmail
agentmail add-flags --account gmail --uid 123 --flags "\\Seen" --color red
agentmail create-draft --account gmail --subject "Hello" --body "Hi there" --to user@example.com
```

Full subcommand list: `agentmail --help`

## MCP Client Configuration

Add to your MCP client config (Claude Desktop, Claude Code, etc.):

```json
{
  "mcpServers": {
    "agentmail": {
      "command": "/path/to/agentmail",
      "args": ["serve"]
    }
  }
}
```

To pass passwords via environment variables instead of keychain:

```json
{
  "mcpServers": {
    "agentmail": {
      "command": "/path/to/agentmail",
      "args": ["serve"],
      "env": {
        "AGENTMAIL_PASSWORD_GMAIL": "your-app-password"
      }
    }
  }
}
```

### Debugging with MCP Inspector

```bash
npx @modelcontextprotocol/inspector /path/to/agentmail serve
```

Opens a web UI to exercise all 21 tools, 6 prompts, and task calls interactively.

## MCP Tools

21 tools covering account discovery, mailbox management, message reading, search,
bulk operations, flag management, and composition. 10 long-running tools support
optional [task-based invocation](https://modelcontextprotocol.io/specification/2025-11-25/server/utilities/tasks)
(SEP-1686) for asynchronous execution.

| Tool                   | Description                                                                           |
| ---------------------- | ------------------------------------------------------------------------------------- |
| `list_accounts`        | Return configured account names (use this first)                                      |
| `list_mailboxes`       | Paginate selectable mailboxes with counts and registered special-use roles             |
| `create_mailbox`       | Create a new mailbox (folder) on the server                                           |
| `check_connection`     | Test IMAP connectivity for an account                                                 |
| `list_capabilities`    | List IMAP server capabilities (IDLE, MOVE, etc.)                                      |
| `get_messages`         | Paginated metadata discovery, newest-first, with safe body resource URIs              |
| `search_messages`      | Paginated IMAP metadata search with safe body resource URIs                           |
| `list_flags`           | List all flags in use with counts; resolves Apple Mail color flags                    |
| `top_senders`         | Top senders by message volume across one or all mailboxes                             |
| `top_subscriptions`     | Top bulk-mail senders with UIDVALIDITY-guarded samples and advertised one-click syntax |
| `top_mailing_lists`         | Top mailing lists by List-Id header (RFC 2919), groups regardless of sender           |
| `find_attachments`     | Scan for messages with attachments (multipart/mixed or multipart/related)              |
| `download_attachments` | Download attachments from a message to disk                                           |
| `delete_messages`      | Delete messages by UID (up to 500 per call, moves to Trash or expunges)               |
| `delete_by_sender`     | Delete all messages from a sender identified by UID, optionally across all mailboxes  |
| `delete_list_id`       | Delete all messages with a specific List-Id across all mailboxes                      |
| `move_message`         | Move a message between mailboxes via IMAP MOVE                                        |
| `create_draft`         | Compose RFC822 draft and append to Drafts folder                                      |
| `unsubscribe_message`  | DKIM-verified RFC 8058 unsubscribe; optional List-Id cleanup is off by default          |
| `add_flags`            | Add flags and/or set Apple Mail color on a message (union semantics)                  |
| `remove_flags`         | Remove flags and/or clear Apple Mail color from a message                             |

### Key parameters

- `account` is **required** for most tools, including `list_mailboxes`. Use
  `list_accounts` to discover valid names. MCP account discovery returns names
  and default status, not IMAP hosts or usernames.
- `mailbox` defaults to `INBOX` when omitted. Omit it on `top_senders`, `top_subscriptions`, `top_mailing_lists`, `list_flags`, and `find_attachments` to scan the entire account. Discovery uses one selectable server-declared `\All` mailbox exclusively when available. Otherwise it enumerates selectable storage mailboxes and excludes roles `\All`, `\Drafts`, `\Flagged`, `\Important`, `\Junk`, and `\Trash`. Storage roles such as `\Archive`, `\Sent`, `\Memos`, `\Scheduled`, and `\Snoozed` remain eligible.
- IMAP defines `\NoSelect`, not a separate `\NoScan` attribute. Automatic plans always skip `\NoSelect`. Exact-name fallback is used only when a server supplies no recognized role; an explicitly supplied mailbox bypasses automatic policy.
- `list_mailboxes` returns selectable mailboxes only and paginates with
  `offset`/`limit` (default 100, maximum 500). The response includes `total` and
  `nextOffset`. Filtering and pagination happen before per-mailbox `STATUS`, so
  unselectable or off-page rows incur no count query. Non-selectable containers
  remain useful internally for planning but are not exposed as actionable MCP
  mailboxes.
- `get_messages` and `search_messages` default to 25 rows and accept at most 50.
  Their MCP results contain compact metadata and a UIDVALIDITY-safe
  `resourceUri`, never message bodies or complete header maps.
- `top_senders`, `top_subscriptions`, and `top_mailing_lists` paginate ranked
  groups with `offset`/`limit` (default 10, maximum 100) and `nextOffset`. A
  ranking page size never limits messages examined or later matched for
  deletion.
- All reads use `BODY.PEEK` to avoid marking messages as `\Seen`.
- Long-running operations (`top_senders`, `top_subscriptions`, `top_mailing_lists`, `find_attachments`, `list_flags`, `delete_messages`, `delete_by_sender`, `delete_list_id`, `download_attachments`, `unsubscribe_message`) support MCP progress notifications and optional task-based invocation.
- Cancelling a request (`notifications/cancelled`) stops long scans at the next mailbox/fetch chunk and polls DNS, DKIM, and HTTP work every 25 ms. An HTTP cancellation cannot retract a POST already received by the remote server, so it never reports that an in-flight request was definitely unsent.
- Delete tools take a `permanent` flag (default false): false moves to Trash when available, true expunges directly (bypassing Trash, irreversible; requires server UIDPLUS).
- `top_senders`, `top_subscriptions`, and `top_mailing_lists` share a persistent, live-validated UID/header cache. Each invocation uses `EXAMINE` before reuse. An unchanged `UIDVALIDITY`/`UIDNEXT`/message-count tuple is a hit; a proven append fetches only new UIDs; deletions and mixed changes reconcile UID membership while reusing unchanged header rows. A changed or missing `UIDVALIDITY` prevents unsafe UID reuse. If discovery enumerates folders, results dedupe by Message-ID and exclude your own address.
- Every discovery result that can lead to a UID action carries the complete
  `(mailbox, uidValidity, uid)` identity and a canonical `resourceUri`.
  `delete_messages`, `move_message`, `download_attachments`, `add_flags`,
  `remove_flags`, and `unsubscribe_message` require `expectedUidValidity` and
  refuse the action if a live `EXAMINE` observes another UID epoch.
  `delete_by_sender` instead takes the exact sender identity (`email` +
  `name`, from a ranking row) and confirms it live in each mailbox, so it
  carries no sample UID or epoch guard.
- `top_subscriptions` returns a nested `sample` identity, not an unsubscribe
  URL or raw list-action header. `unsubscribe_message` additionally requires
  explicit `confirmOneClick=true`. Its `advertisedOneClick` field describes
  cached header syntax only; execution re-fetches the complete message and
  locally verifies a passing DKIM signature that covers both list headers.
- The action-time DKIM source fetch is preceded by `RFC822.SIZE`, capped at 64 MiB, and fetched with a bounded IMAP partial. This per-source safety bound does not limit matching-message cleanup counts.
- RFC 8058 requests accept exactly one parsed HTTPS URI, reject credentials, fragments, HTTP alternatives, private/link-local/loopback destinations, mixed public/private DNS answers, proxies, retries, and redirects, and require a direct 2xx response. The resolved public addresses are pinned for the request.
- Matching-message cleanup is one optional `cleanup {when, identity, deletion}` object; omitting it means unsubscribe only. Defaults are fail-safe: `when: "afterSuccess"` (a failed unsubscribe never triggers cleanup unless `"always"` is explicit), `deletion: "trash"` (never permanent unless `"trashThenPermanent"` or `"permanent"` is explicit). Cleanup matches the normalized RFC 2919 List-Id only when the same passing DKIM signature covered that single List-Id; otherwise `identity: "listIdOrSender"` (default) falls back to the exact sender's bulk mail, scoped to the target's own List-Id whenever the message carries one so sibling lists from the same sender are untouched.
- Account-wide destructive operations use a separate mutation plan: they enumerate selectable storage mailboxes and never issue writes through `\All`, `\Flagged`, or `\Important` aggregate views. An explicitly supplied mailbox is always honored.
- `delete_by_sender`, `delete_list_id`, and unsubscribe matching have no total-message ceiling; server mutations are split into 500-UID wire batches. Only the MCP `delete_messages` tool limits an explicitly supplied UID array to 500 per call.
- `search_messages` supports date range (`since`/`before`, YYYY-MM-DD) and size (`larger_than`/`smaller_than`, bytes) for "older than" / "bigger than" cleanup, plus AND-combined case-insensitive substring text filters. `delete_list_id` matches the List-Id exactly (not as a substring).
- On Gmail, deletes route through `[Gmail]/Trash` (in-place expunge only removes a label); `permanent` also goes to Trash, which Gmail purges on its own.
- Non-ASCII `search_messages` text is sent with `CHARSET UTF-8`. Drafts include `Date` and `Message-ID` headers.
- Tool calls return one compact text summary for compatibility plus one
  authoritative `structuredContent` object. The full JSON value is not repeated
  in the text content block.
- Destructive tasks targeting the same account are automatically serialized to prevent IMAP state conflicts. Tasks are capped at 128 live entries, retained for 24 hours from creation, listed newest-first in opaque-cursor pages of 25, and may have their terminal result retrieved repeatedly until expiry.

### Ranking cache privacy and configuration

The ranking cache defaults to
`dirs::cache_dir()/agentmail/header-cache-v1.sqlite3`. Set
`AGENTMAIL_CACHE_DIR` to override the cache root, or set
`AGENTMAIL_DISABLE_HEADER_CACHE=1` (`true` and `yes` also work) to use live scans
only. SQLite failures automatically fall back to live IMAP behavior.

One-click execution transiently fetches the complete selected message because
DKIM verification must hash its body. That source is held only for the action
and is dropped before optional mailbox cleanup; it is never written to the
ranking cache.

Schema version 3 stores account mutation state, mailbox snapshot state, UID
membership, and an immutable ranking projection: sender address/name, date,
Message-ID, normalized List-Id/display name, and booleans for list-header and
advertised one-click presence. It deliberately does not store
List-Unsubscribe URLs, recipient tokens, raw list-action headers, bodies,
subjects, recipients, flags, attachments, passwords, authentication tokens,
keychain secrets, or complete messages. The cache namespace does include the
configured account name, IMAP host/port/TLS mode, and login username so data
from different server identities cannot collide.

| Table | Primary key | Stored projection |
| --- | --- | --- |
| `account_state` | `account_key` | `mutation_revision` |
| `mailbox_state` | `account_key, mailbox` | `uid_validity`, nullable `uid_next`, `message_count`, `revision`, `projection_version` |
| `membership` | `account_key, mailbox, uid` | Current live UID membership only |
| `header_rows` | `account_key, mailbox, uid_validity, uid, projection_version` | `sender_email`, `sender_name`, nullable `date_unix_ms`, `message_id`, `list_id`, `list_display_name`, `has_list_headers`, `advertised_one_click` |

All four tables use SQLite `WITHOUT ROWID`; this is a derived projection, not
an offline mailbox or source-of-truth message store.

SQLite runs in WAL mode with `synchronous=NORMAL` and foreign keys enabled.
The file is not application-encrypted. On Unix, AgentMail restricts the cache
directory to `0700` and the database file to `0600`. Upgrading an older cache
rebuilds this disposable projection with secure deletion, `VACUUM`, and a
truncated WAL so obsolete token-bearing columns are not retained.

## MCP Prompts

6 prompts provide guided conversation starters for common email workflows:

| Prompt                | Description                                                                        |
| --------------------- | ---------------------------------------------------------------------------------- |
| `inbox-summary`       | Get a comprehensive inbox overview: folder structure, top senders, unread messages |
| `cleanup-sender`      | Find and bulk-delete all emails from a specific sender (with preview)              |
| `find-attachments`    | Scan a mailbox for messages with attachments and list for download                 |
| `compose-email`       | Guided email draft composition                                                     |
| `unsubscribe-cleanup` | Identify lists, obtain consent, then run verified unsubscribe and optional cleanup |
| `list-id-cleanup`     | Identify mailing lists by List-Id and bulk-delete entire lists                     |

## MCP Resources

Single messages are addressable as resources — the read-one-message primitive that complements the paginated tools:

| URI template                                                        | MIME type             | Content                                  |
| ------------------------------------------------------------------- | --------------------- | ---------------------------------------- |
| `email://{account}/{mailbox}/{uidValidity}/{uid}`                   | `text/markdown`       | Normalized body view, capped at 100K chars |
| `email://{account}/{mailbox}/{uidValidity}/{uid}/headers`           | `text/rfc822-headers` | Exact RFC822 header block, maximum 64 KiB |
| `email://{account}/{mailbox}/{uidValidity}/{uid}/source`            | `message/rfc822`      | Lossless base64 MCP blob, maximum 256 KiB |

Encoding rules: `account` and `mailbox` are percent-encoded URI segments — a
`/` inside a mailbox name must be encoded as `%2F`, for example
`email://work/Archive%2F2024/3857529045/1234`. Both UID values must be non-zero.
Every read validates the live UIDVALIDITY before fetching; a stale identity is
reported as resource-not-found rather than reading a recycled UID. Get current
URIs from `get_messages`, `search_messages`, `find_attachments`, or the
`top_*` tools. `resources/list` is intentionally empty because discovery is
template-based. The `/source` representation uses the MCP resource `blob`
field, whose value is base64, so arbitrary RFC822 octets are preserved without
lossy UTF-8 conversion.

## MCP Completions

Argument autocompletion (`completion/complete`) is supported for the prompts and the `email://` resource templates:

- `account` — completes instantly from configured account names.
- `mailbox` — reads a bounded, process-local mailbox-layout catalog scoped to the account from the completion context (or the default account). A cold or expired lookup performs one IMAP LIST; warm lookups use the five-minute catalog. It retains only path, delimiter, attributes, and recognized special-use roles—never counts, UIDs, or message metadata. The same catalog plans account-wide scans. Network failures return an empty list rather than an error.

## Architecture

```
agentmail (binary crate: agentmail-mcp)
  ├── serve                → MCP stdio server (tokio + rmcp)
  │                          21 tools + 6 prompts, tasks, progress notifications
  ├── list-accounts        → CLI
  ├── list-mailboxes       → CLI
  ├── create-mailbox       → CLI
  ├── check-connection     → CLI
  ├── list-capabilities    → CLI
  ├── get-messages         → CLI
  ├── get-messages-by-uid  → CLI
  ├── top-senders          → CLI
  ├── top-subscriptions    → CLI
  ├── find-attachments     → CLI
  ├── download-attachments → CLI
  ├── list-flags           → CLI
  ├── add-flags            → CLI (flags + Apple Mail colors)
  ├── create-draft         → CLI
  ├── set-password         → CLI (keychain store)
  └── configure            → CLI (interactive account setup)

src/ (library + binary)
  ├── lib.rs          → Public API facade (25+ async methods)
  ├── main.rs         → CLI dispatch (clap), account configuration
  ├── mcp/            → MCP server: 21 tools, 6 prompts, tasks, resources, completions
  ├── config.rs       → TOML config loading, default account resolution
  ├── credentials.rs  → Password resolution (env → config secret → default keyring)
  ├── connection.rs   → IMAP connection pool (default 3 sessions/account, configurable)
  ├── imap_client.rs  → IMAP operations (fetch, search, delete, move, create, sync)
  ├── header_cache.rs → Persistent validated UID/membership and ranking-header cache
  ├── mailbox_catalog.rs → Bounded 5-minute mailbox-layout catalog
  ├── scan_plan.rs     → Pure discovery/mutation mailbox selection policy
  ├── parser.rs       → RFC822 → MessageInfo (via mail-parser), attachment extraction
  ├── draft.rs        → RFC822 composition (via lettre)
  ├── content.rs      → HTML→markdown conversion, context window trimming
  ├── provider.rs     → Email provider presets (Gmail, iCloud, Yahoo, Fastmail)
  ├── types.rs        → Shared data structures (MessageInfo, MailboxInfo, etc.)
  └── error.rs        → Error types
```

**Connection pooling:** Each account maintains up to 3 idle IMAP sessions (configurable via `max_connections`). Sessions are validated with NOOP before reuse and replaced when stale. Credentials are resolved on-demand when a new connection is needed.

**Mailbox layout catalog:** Completion, Trash/Drafts resolution, and account scan planning share a five-minute, process-local layout snapshot. Explicit `list_mailboxes` calls remain live so message counts are never served from this catalog. Listings over 4,096 mailboxes or 1 MiB of layout text are returned to the current caller but not retained.

**Ranking-header cache:** SQLite work runs on blocking workers rather than the async runtime. Header chunks commit incrementally so an interrupted 216K-message cold scan can resume without refetching completed chunks; UID membership is published atomically only after every member has a header marker. Account mutation generations and mailbox snapshot revisions prevent an in-flight scan from overwriting newer state.

**Post-mutation sync:** All mutating operations (delete, move, create draft, create mailbox) issue a NOOP after the operation to flush pending server-side state before releasing the session back to the pool.

## Troubleshooting

1. Run `agentmail check-connection --account <name>` to test connectivity.
2. Verify your password: `agentmail set-password --account <name>` to re-store it.
3. Gmail users: ensure you're using an [App Password](https://myaccount.google.com/apppasswords), not your Google account password.
4. Check that your IMAP server allows external clients (some providers disable IMAP by default).
5. If the MCP server appears empty in Inspector, call `initialize` first, then `tools/list`.
