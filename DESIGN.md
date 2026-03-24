# Agentmail Design

## Overview

Agentmail is a cross-platform IMAP email client library with an MCP (Model Context Protocol) server for AI assistant integration. It provides 20 tools and 5 prompts for reading, searching, composing, organizing, and managing email across multiple accounts.

No Mail.app dependency. Pure IMAP over TLS. Works on macOS, Linux, and Windows.

## Architecture

```mermaid
graph TB
    subgraph "Agent App (Tauri)"
        UI[Settings UI]
        DB[(SQLite<br/>mail_accounts)]
        KR[OS Keyring<br/>passwords]
        GW[MCP Gateway]
    end

    subgraph "agentmail-mcp (in-process)"
        MCP[AgentMailServer<br/>20 tools, 5 prompts]
        MK[Agentmail Facade]
        POOL[ConnectionPool<br/>3 sessions/account]
        CRED[Credential Resolver]
    end

    subgraph "IMAP Servers"
        GMAIL[imap.gmail.com]
        ICLOUD[imap.mail.me.com]
        OUTLOOK[outlook.office365.com]
    end

    UI -->|add account| DB
    UI -->|store password| KR
    DB -->|load configs| GW
    GW <-->|DuplexStream| MCP
    MCP --> MK
    MK --> POOL
    POOL --> CRED
    CRED -->|1. env var| ENV[AGENTMAIL_PASSWORD_*]
    CRED -->|2. config secret| CFG[config.toml]
    CRED -->|3. keyring| KR
    POOL <-->|TLS| GMAIL
    POOL <-->|TLS| ICLOUD
    POOL <-->|TLS| OUTLOOK
```

## Two Operating Modes

```mermaid
graph LR
    subgraph "Standalone"
        CLI[agentmail serve] -->|stdio| MCP1[AgentMailServer]
        MCP1 --> MK1[Agentmail]
        MK1 -->|config.toml| FS[~/.config/agentmail/]
    end

    subgraph "In-Process (Agent App)"
        GW2[Gateway] <-->|DuplexStream| MCP2[AgentMailServer]
        MCP2 --> MK2[Agentmail]
        MK2 -->|Config::from_accounts| DB2[Agent DB]
    end
```

|                 | Standalone                        | In-Process                                      |
| --------------- | --------------------------------- | ----------------------------------------------- |
| Binary          | `agentmail serve`                 | None (library)                                  |
| Transport       | stdio                             | DuplexStream                                    |
| Account config  | `~/.config/agentmail/config.toml` | Passed at spawn via `serve_on()`                |
| Password source | keyring (agentmail service)       | keyring (agent service)                         |
| Entry point     | `main.rs`                         | `agentmail_mcp::serve_on(transport, agentmail)` |

## Crate Structure

```
agentmail-mcp/
  crates/
    agentmail/          # Library — IMAP client, no MCP dependency
      src/
        lib.rs        # Agentmail facade (25+ async methods)
        imap_client.rs # Raw IMAP operations (SELECT, FETCH, SEARCH, STORE)
        connection.rs  # Per-account session pool + semaphore concurrency
        config.rs      # AccountConfig, Config (file + programmatic)
        credentials.rs # Password resolution: env → config → keyring
        provider.rs    # MailProvider enum (Gmail, iCloud, Outlook, etc.)
        parser.rs      # RFC822 parsing via mail-parser
        content.rs     # HTML → Markdown, truncation, cleanup
        draft.rs       # RFC822 composition via lettre
        types.rs       # MessageInfo, MailboxInfo, SearchCriteria, etc.
        error.rs       # AgentmailError enum
    agentmail-mcp/      # Binary + library — MCP server + CLI
      src/
        lib.rs        # AgentMailServer, 20 tools, 5 prompts, serve_on()
        main.rs       # CLI dispatch (clap), account configuration
```

## Connection Pool

```mermaid
sequenceDiagram
    participant Tool as MCP Tool
    participant Pool as ConnectionPool
    participant Sem as Semaphore (3)
    participant Sessions as Idle Sessions
    participant IMAP as IMAP Server

    Tool->>Pool: acquire("gmail")
    Pool->>Sem: acquire permit
    Sem-->>Pool: permit granted
    Pool->>Sessions: pop idle session
    alt Session exists
        Pool->>IMAP: NOOP (validate)
        alt Session alive
            Pool-->>Tool: PooledSession
        else Session stale
            Pool->>Pool: drop stale session
            Pool->>IMAP: connect + LOGIN
            IMAP-->>Pool: new session
            Pool-->>Tool: PooledSession
        end
    else No idle session
        Pool->>IMAP: connect + LOGIN
        IMAP-->>Pool: new session
        Pool-->>Tool: PooledSession
    end
    Note over Tool: use session...
    Tool->>Pool: release()
    Pool->>Sessions: push back
    Pool->>Sem: release permit
```

- Max 3 concurrent IMAP operations per account (well within provider limits)
- Sessions validated with NOOP before reuse
- Stale sessions dropped, fresh ones created on demand
- `PooledSession` auto-releases semaphore permit on drop

## Credential Resolution

```mermaid
flowchart TD
    Start[get_password] --> Env{Env var?<br/>AGENTMAIL_PASSWORD_*}
    Env -->|found| Return[Return password]
    Env -->|not set| Config{Config secret?<br/>raw / cmd / keyring}
    Config -->|found| Return
    Config -->|none| Default{Default keyring<br/>service=agentmail<br/>key=username}
    Default -->|found| Return
    Default -->|not found| Error[Error: no password]
```

When running in-process, the agent app calls `init_keyring_with_service("agent")` so passwords are stored under the agent's keyring service, not "agentmail". The signed agent app avoids macOS Keychain popups.

## MCP Tools (20)

### Read Operations (read_only_hint = true)

| Tool                  | Description                                     |
| --------------------- | ----------------------------------------------- |
| `list_accounts`       | List configured IMAP accounts                   |
| `list_mailboxes`      | List mailboxes with message counts              |
| `list_capabilities`   | Query IMAP server capabilities                  |
| `check_connection`    | Test IMAP connectivity                          |
| `get_messages`        | Paginated fetch, newest-first by UID            |
| `get_messages_by_uid` | Fetch specific messages by UID                  |
| `search_messages`     | IMAP SEARCH with text/header/flag filters       |
| `list_flags`          | List all flags in use with counts               |
| `find_attachments`    | Scan for messages with attachments              |
| `rank_senders`        | Group senders by message count                  |
| `rank_unsubscribe`    | Rank mailing lists by volume (List-Unsubscribe) |

### Write Operations

| Tool                   | Description                           |
| ---------------------- | ------------------------------------- |
| `delete_messages`      | Delete by UID (up to 500)             |
| `delete_by_sender`     | Delete all from a sender              |
| `move_message`         | IMAP MOVE between mailboxes           |
| `create_mailbox`       | Create new folder                     |
| `create_draft`         | Compose RFC822 → Drafts folder        |
| `set_flag_color`       | Apple Mail compatible color flags     |
| `add_flags`            | Add flags (system + custom keywords)  |
| `remove_flags`         | Remove flags                          |
| `unsubscribe_message`  | Extract unsubscribe URL + bulk delete |
| `download_attachments` | Extract attachments to disk           |

## MCP Prompts (5)

| Prompt                | Description                               |
| --------------------- | ----------------------------------------- |
| `inbox-summary`       | Comprehensive inbox overview              |
| `cleanup-sender`      | Find & bulk-delete from a sender          |
| `find-attachments`    | Scan for downloadable attachments         |
| `compose-email`       | Guided draft composition                  |
| `unsubscribe-cleanup` | Identify & unsubscribe from mailing lists |

## Provider Defaults

The `MailProvider` enum provides sensible IMAP defaults per provider. Users only need to enter their email and app password.

| Provider | Host                    | Trash              | Drafts           |
| -------- | ----------------------- | ------------------ | ---------------- |
| Gmail    | `imap.gmail.com`        | `[Gmail]/Trash`    | `[Gmail]/Drafts` |
| iCloud   | `imap.mail.me.com`      | `Deleted Messages` | `Drafts`         |
| Outlook  | `outlook.office365.com` | `Deleted`          | `Drafts`         |
| Yahoo    | `imap.mail.yahoo.com`   | `Trash`            | `Draft`          |
| Fastmail | `imap.fastmail.com`     | `Trash`            | `Drafts`         |

## Content Processing

Email content flows through a pipeline:

1. **RFC822 parsing** (`mail-parser`) — extract headers, body parts, attachments
2. **Format selection** — prefer `text/plain`, fall back to `text/html`
3. **HTML conversion** (`fast_html2md`) — convert to Markdown
4. **Cleanup** — strip tracking pixels, collapse blank lines, decode entities
5. **Truncation** — cap at 100K chars for LLM context safety
6. **BODY.PEEK** — never marks messages as `\Seen` (read-only fetch)

## Key Design Decisions

- **Pure IMAP, no Mail.app** — cross-platform, works with any IMAP provider
- **Connection pooling** — 3 sessions/account avoids provider rate limits while enabling parallelism
- **BODY.PEEK throughout** — reading never has side effects
- **App passwords over OAuth** — simpler for users, no client ID registration needed
- **Config file for standalone, runtime injection for in-process** — same library code, different config sources
- **Passwords in OS keyring, never in DB** — proper security, no key management burden
- **Tool annotations** — `read_only_hint`, `destructive_hint`, `idempotent_hint` per MCP 2025-06-18 spec
- **Progress notifications** — long operations (rank_senders, find_attachments) report progress to MCP client
