# agentmail

IMAP email client exposed as both a CLI and an MCP (Model Context Protocol) server, built with Rust.

MCP protocol: [2025-06-18](https://modelcontextprotocol.io/specification/2025-06-18) (also negotiates 2025-03-26 and 2024-11-05) | rmcp 1.4

One binary: `agentmail serve` starts the MCP stdio server, all other subcommands are a direct CLI.

See also: [DESIGN.md](DESIGN.md) for architecture diagrams and design decisions, [MCP.md](MCP.md) for the full MCP tool & prompt reference with output schemas.

## Requirements

- Rust toolchain (edition 2024)
- An IMAP-enabled email account (Gmail, iCloud, Yahoo, Fastmail, self-hosted, etc.)

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
agentmail rank-senders --account gmail --limit 20
agentmail rank-unsubscribe --account gmail --limit 20
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

## MCP Tools

21 tools covering account discovery, mailbox management, message reading, search, bulk operations, flag management, and composition. 9 long-running tools support optional [task-based invocation](https://modelcontextprotocol.io/specification/2025-06-18/server/utilities/tasks) (SEP-1686) for async fire-and-forget execution.

| Tool                   | Description                                                                           |
| ---------------------- | ------------------------------------------------------------------------------------- |
| `list_accounts`        | Return configured account names (use this first)                                      |
| `list_mailboxes`       | List mailboxes with counts, attributes, and RFC 6154 special-use roles                |
| `create_mailbox`       | Create a new mailbox (folder) on the server                                           |
| `check_connection`     | Test IMAP connectivity for an account                                                 |
| `list_capabilities`    | List IMAP server capabilities (IDLE, MOVE, etc.)                                      |
| `get_messages`         | Paginated message fetch, newest-first by UID                                          |
| `search_messages`      | IMAP SEARCH with text, header, sender, subject, and status filters                    |
| `list_flags`           | List all flags in use with counts; resolves Apple Mail color flags                    |
| `rank_senders`         | Rank senders by message count across one or all mailboxes                             |
| `rank_unsubscribe`     | Rank bulk-mail senders by List-Unsubscribe presence, sorted by one-click support      |
| `rank_list_id`         | Rank mailing lists by List-Id header (RFC 2919), groups regardless of sender          |
| `find_attachments`     | Scan for messages with attachments (multipart/mixed or multipart/related)              |
| `download_attachments` | Download attachments from a message to disk                                           |
| `delete_messages`      | Delete messages by UID (up to 500 per call, moves to Trash or expunges)               |
| `delete_by_sender`     | Delete all messages from a sender identified by UID, optionally across all mailboxes  |
| `delete_list_id`       | Delete all messages with a specific List-Id across all mailboxes                      |
| `move_message`         | Move a message between mailboxes via IMAP MOVE                                        |
| `create_draft`         | Compose RFC822 draft and append to Drafts folder                                      |
| `unsubscribe_message`  | RFC 8058 one-click unsubscribe, optionally delete matching bulk mail across all boxes  |
| `add_flags`            | Add flags and/or set Apple Mail color on a message (union semantics)                  |
| `remove_flags`         | Remove flags and/or clear Apple Mail color from a message                             |

### Key parameters

- `account` is **required** for most tools. Use `list_accounts` to discover valid names.
- `mailbox` defaults to `INBOX` when omitted. Omit it on `rank_senders`, `rank_unsubscribe`, `rank_list_id`, `list_flags`, and `find_attachments` to scan the entire account (auto-skips Trash, Junk, Spam, Drafts).
- `limit` defaults to 25, clamped to 1..50.
- `includeContent` (default false) returns normalized markdown body text, trimmed for context window safety.
- All reads use `BODY.PEEK` to avoid marking messages as `\Seen`.
- Long-running operations (`rank_senders`, `rank_unsubscribe`, `rank_list_id`, `find_attachments`, `list_flags`, `delete_messages`, `delete_by_sender`, `delete_list_id`, `download_attachments`) support MCP progress notifications and optional task-based invocation.
- Destructive tasks targeting the same account are automatically serialized to prevent IMAP state conflicts.

## MCP Prompts

6 prompts provide guided conversation starters for common email workflows:

| Prompt                | Description                                                                        |
| --------------------- | ---------------------------------------------------------------------------------- |
| `inbox-summary`       | Get a comprehensive inbox overview: folder structure, top senders, unread messages |
| `cleanup-sender`      | Find and bulk-delete all emails from a specific sender (with preview)              |
| `find-attachments`    | Scan a mailbox for messages with attachments and list for download                 |
| `compose-email`       | Guided email draft composition                                                     |
| `unsubscribe-cleanup` | Identify high-volume mailing lists, unsubscribe and bulk-delete                    |
| `list-id-cleanup`     | Identify mailing lists by List-Id and bulk-delete entire lists                     |

## Architecture

```
agentmail (binary crate: agentmail-mcp)
  ├── serve                → MCP stdio server (tokio + rmcp 1.4)
  │                          21 tools + 6 prompts, tasks, progress notifications
  ├── list-accounts        → CLI
  ├── list-mailboxes       → CLI
  ├── create-mailbox       → CLI
  ├── check-connection     → CLI
  ├── list-capabilities    → CLI
  ├── get-messages         → CLI
  ├── get-messages-by-uid  → CLI
  ├── rank-senders         → CLI
  ├── rank-unsubscribe     → CLI
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
  ├── mcp.rs          → MCP server: 21 tools, 6 prompts, task manager, serve_on()/serve_stdio()
  ├── config.rs       → TOML config loading, default account resolution
  ├── credentials.rs  → Password resolution (env → config secret → default keyring)
  ├── connection.rs   → IMAP connection pool (default 3 sessions/account, configurable)
  ├── imap_client.rs  → IMAP operations (fetch, search, delete, move, create, sync)
  ├── parser.rs       → RFC822 → MessageInfo (via mail-parser), attachment extraction
  ├── draft.rs        → RFC822 composition (via lettre)
  ├── content.rs      → HTML→markdown conversion, context window trimming
  ├── provider.rs     → Email provider presets (Gmail, iCloud, Yahoo, Fastmail)
  ├── types.rs        → Shared data structures (MessageInfo, MailboxInfo, etc.)
  └── error.rs        → Error types
```

**Connection pooling:** Each account maintains up to 3 idle IMAP sessions (configurable via `max_connections`). Sessions are validated with NOOP before reuse and replaced when stale. Credentials are resolved on-demand when a new connection is needed.

**Post-mutation sync:** All mutating operations (delete, move, create draft, create mailbox) issue a NOOP after the operation to flush pending server-side state before releasing the session back to the pool.

## Troubleshooting

1. Run `agentmail check-connection --account <name>` to test connectivity.
2. Verify your password: `agentmail set-password --account <name>` to re-store it.
3. Gmail users: ensure you're using an [App Password](https://myaccount.google.com/apppasswords), not your Google account password.
4. Check that your IMAP server allows external clients (some providers disable IMAP by default).
5. If the MCP server appears empty in Inspector, call `initialize` first, then `tools/list`.
