---
created: 2026-05-29T19:20
updated: 2026-05-29T19:20
---
# Agentmail MCP — Tool & Prompt Reference

MCP protocol: [2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25) (also negotiates 2025-06-18, 2025-03-26, and 2024-11-05) | [rmcp](https://crates.io/crates/rmcp) (official Rust MCP SDK) | Transport: stdio (standalone) or AsyncRead+AsyncWrite (in-process)

**Supported capabilities:** tools, prompts, resources, completions, tasks (SEP-1686), progress notifications, request cancellation

## Tools (21)

### Discovery & Connection

| #   | Tool                | Description                                      | Annotations |
| --- | ------------------- | ------------------------------------------------ | ----------- |
| 1   | `list_accounts`     | Return configured IMAP account names             | `read_only` |
| 2   | `list_mailboxes`    | List all folders with counts, attributes, and RFC 6154 roles | `read_only` |
| 3   | `check_connection`  | Test IMAP connectivity and auth for an account   | `read_only` |
| 4   | `list_capabilities` | Query IMAP extensions (IDLE, MOVE, CONDSTORE)    | `read_only` |

#### Output Schemas

**list_accounts** → `ListAccountsResponse`
```json
{ "accounts": [{ "name", "host", "username", "isDefault?" }] }
```

**list_mailboxes** → `ListMailboxesResponse`
```json
{ "mailboxes": [{ "name", "account", "totalMessages", "unseenMessages", "recentMessages", "delimiter?", "path",
    "noSelect?": bool, "noInferiors?": bool, "role?": "trash"|"junk"|"drafts"|"sent"|"archive"|"all"|"flagged" }] }
```
`noSelect` (RFC 3501): mailbox is a virtual container — cannot be selected, searched, or deleted from. `noInferiors`: no child mailboxes exist or can be created. `role` (RFC 6154): server-declared special-use purpose. Omitted for ordinary user mailboxes. `recentMessages` is always 0 on IMAP4rev2-only servers (RFC 9051 removed the RECENT status item).

**check_connection** → `ConnectionStatus`
```json
{ "account", "connected": bool, "error?", "serverGreeting?" }
```

**list_capabilities** → `ListCapabilitiesResponse`
```json
{ "account", "capabilities": ["IDLE", "MOVE", ...] }
```

---

### Read Messages

| #   | Tool               | Description                                                                                         | Annotations            |
| --- | ------------------ | --------------------------------------------------------------------------------------------------- | ---------------------- |
| 5   | `get_messages`     | Paginated fetch, newest-first. Optional body + headers. Default: INBOX, offset=0, limit=25 (max 50) | `read_only`            |
| 6   | `search_messages`  | IMAP SEARCH: sender, subject, to, full-text, read/flagged/deleted, header key/value. Paginated.     | `read_only`            |
| 7   | `list_flags`       | All IMAP flags in use with counts. Resolves Apple $MailFlagBit colors. Omit mailbox to scan all.    | `read_only`, `taskable` |
| 8   | `find_attachments` | Scan for messages with attachments (mixed + related), paginated. Omit mailbox to scan all.          | `read_only`, `taskable` |
| 9   | `top_senders`     | Top senders by volume (email, display name) with counts + date ranges. Omit mailbox to scan all.    | `read_only`, `taskable` |
| 10  | `top_subscriptions` | Top bulk-mail senders by volume. Returns unsubscribe URLs, sample UIDs.                             | `read_only`, `taskable` |
| 11  | `top_mailing_lists`     | Top mailing lists by List-Id (RFC 2919). Groups across senders. Omit mailbox to scan all.           | `read_only`, `taskable` |

#### Output Schemas

**get_messages**
```json
{ "mailbox", "account", "offset", "limit", "total",
  "messages": [MessageInfo] }
```

**search_messages**
```json
{ "mailbox", "account", "offset", "limit", "totalMatches",
  "messages": [MessageInfo] }
```

Non-ASCII search text is sent with a `CHARSET UTF-8` prefix (accepted by Gmail, Dovecot, Courier, iCloud, Outlook, and required by IMAP4rev2). Servers that reject UTF-8 search return `-32602`. CR/LF in search text is rejected.

Search filters are **AND-combined** (a message must match all provided filters) and matched as **case-insensitive substrings** (IMAP semantics). `header_key` without a value matches messages that merely *have* that header. Date range: `since`/`before` (YYYY-MM-DD, server internal date → IMAP `SINCE`/`BEFORE`, since=inclusive, before=exclusive). Size: `larger_than`/`smaller_than` in bytes (`LARGER`/`SMALLER`). Arbitrary `OR`/`NOT` boolean expressions are not supported — a recursive query tree would reintroduce `$defs`/`$ref` into the tool schema (which some hosts reject); issue multiple searches instead.

**MessageInfo** (shared by get_messages, search_messages, get_messages_by_uid)
```json
{ "uid", "subject", "sender", "replyTo", "to": [], "cc": [],
  "mailbox", "account", "date?", "flags": [],
  "size?", "content?", "contentFormat?", "contentTruncated?",
  "listUnsubscribe?", "listUnsubscribePost?", "listId?", "listHelp?",
  "messageId?", "inReplyTo?", "references?": [], "bcc?": [],
  "mimeType?", "attachments?": [{ "name?", "contentType", "size", "contentId?" }],
  "headers?": { "Header-Name": ["value"] } }
```

**list_flags**
```json
{ "mailbox": "INBOX" | "*", "account", "totalFlags",
  "flags": [{ "flag": "\\Seen", "count": 5000 }],
  "colors?": [{ "color": "red", "count": 8 }],
  "perMailbox?": [{ "mailbox", "totalFlags", "flags": [...] }] }
```
`colors` present when Apple $MailFlagBit flags exist. `perMailbox` present when mailbox omitted.

**find_attachments**
```json
{ "mailbox": "INBOX" | "*", "account", "total", "offset", "limit",
  "uids": [501, 498, ...],
  "perMailbox?": [{ "mailbox", "count" }] }
```
`perMailbox` present when mailbox omitted. UIDs paginated (default 25, max 100).

**top_senders**
```json
{ "mailbox": "INBOX" | "*", "account", "totalMessages", "uniqueSenders",
  "senders": [{
    "sender": "Display Name <email>", "address", "displayName",
    "count", "oldestDate?", "newestDate?"
  }] }
```
Grouped by (email, display name) — same email with different display names are separate entries. `limit` defaults to 100 on all three top-N tools; set it higher to return more. Account-wide scans (omit `mailbox`) skip Trash/Junk/Drafts/All Mail, **deduplicate by Message-ID across folders** (so a message under several Gmail labels is counted once — counts reflect unique messages), and **exclude the account's own address** (so your sent mail doesn't rank you as a sender). `top_subscriptions` also excludes self.

**top_subscriptions**
```json
{ "mailbox": "INBOX" | "*", "account", "totalMessages", "uniqueLists",
  "lists": [{
    "sender": "Newsletter <email>", "address",
    "unsubscribeUrl?", "listUnsubscribePost?", "oneClick": bool,
    "sampleUid", "sampleMailbox?",
    "count", "oldestDate?", "newestDate?"
  }] }
```
Sorted: one-click senders first, then by count. `sampleMailbox` needed because UIDs are per-mailbox.

**top_mailing_lists**
```json
{ "mailbox": "INBOX" | "*", "account", "totalMessages", "uniqueLists",
  "lists": [{
    "listId": "list-id.example.com",
    "displayName": "Example List",
    "senders": ["noreply@example.com"],
    "count", "sampleUid", "sampleMailbox?",
    "oldestDate?", "newestDate?"
  }] }
```
Grouped by List-Id header — same list with different senders are merged into one entry.

---

### Write / Mutate

| #   | Tool                   | Description                                                                           | Annotations                              |
| --- | ---------------------- | ------------------------------------------------------------------------------------- | ---------------------------------------- |
| 12  | `delete_messages`      | Delete by UID (up to 500). Moves to Trash, or permanently expunges when `permanent=true`. | `destructive`, `idempotent`, `taskable`   |
| 13  | `delete_by_sender`     | Delete all from exact sender. `allMailboxes=true` scans entire account. `permanent=true` bypasses Trash. | `destructive`, `taskable`                 |
| 14  | `delete_list_id`       | Delete all messages with an **exact** List-Id across all mailboxes. `permanent=true` bypasses Trash. | `destructive`, `taskable`                 |
| 15  | `move_message`         | IMAP MOVE between mailboxes (COPY+EXPUNGE fallback when MOVE unsupported)             |                                          |
| 16  | `create_mailbox`       | Create new folder                                                                     | `idempotent`                             |
| 17  | `create_draft`         | Compose RFC822 to Drafts folder (to/cc/bcc required; creates Drafts mailbox if missing). Supports optional local file attachments. |                                          |
| 18  | `download_attachments` | Extract attachments to disk as `{uid}_{filename}`                                     | `taskable`                               |
| 19  | `unsubscribe_message`  | RFC 8058 one-click unsubscribe POST + bulk delete matching bulk mail. `permanent=true` bypasses Trash. | `destructive`, `open_world`              |

**`permanent` flag (delete tools):** default false moves to Trash when a Trash mailbox exists, else permanently deletes. When true, flags `\Deleted` + UID EXPUNGE directly, bypassing Trash — irreversible. Permanent delete requires the server to advertise UIDPLUS; on servers without it the call is refused (plain EXPUNGE would purge unrelated `\Deleted` messages).

**Gmail:** on Gmail (`X-GM-EXT-1`), in-place `\Deleted`+EXPUNGE only removes a *label* — the message survives in All Mail. agentmail therefore routes every delete, including `permanent`, through `[Gmail]/Trash` (which removes the message from all labels; Gmail purges Trash on its own schedule). Immediate hard-purge isn't available on Gmail.

#### Output Schemas

**delete_messages**
```json
{ "mailbox", "account", "deleted": 5, "failed": 0 }
```

**delete_by_sender**
```json
{ "mailbox": "INBOX" | "*", "account",
  "sender": "Display Name <email>",
  "found", "deleted", "failed",
  "mailboxes?": [{ "mailbox", "found", "deleted", "failed" }] }
```
`mailboxes` present when `allMailboxes=true`.

**delete_list_id**
```json
{ "mailbox": "INBOX" | "*", "account",
  "listId": "list-id.example.com",
  "found", "deleted", "failed",
  "mailboxes?": [{ "mailbox", "found", "deleted", "failed" }],
  "skipped?": ["Trash", "Junk"] }
```
`mailboxes` present when scanning all mailboxes. `skipped` lists mailboxes excluded from scan.

**move_message**
```json
{ "mailbox", "account", "uid", "destination", "moved": true }
```

**create_mailbox**
```json
{ "account", "mailbox", "created": true }
```

**create_draft**
```json
{ "created": true, "account", "draftsMailbox",
  "subject", "recipients": { "to": [], "cc": [], "bcc": [] },
  "attachments?": ["report.pdf", "photo.jpg"] }
```
`attachments` lists the filenames that were successfully attached (empty when none provided).

**download_attachments**
```json
{ "mailbox", "account", "uid",
  "downloaded": [{ "filename", "path", "contentType", "size" }] }
```

**unsubscribe_message**
```json
{ "mailbox", "account", "uid",
  "listUnsubscribe?", "listUnsubscribePost?", "listId?",
  "pathway?": "list-unsubscribe",
  "unsubscribed": { "success": bool, "method?": "one-click", "url?", "httpStatus?", "reason?" },
  "matchingMessages?": {
    "matchedBy": "sender+list-unsubscribe",
    "sender", "found", "deleted", "failed",
    "mailboxes": [{ "mailbox", "found", "deleted", "failed" }]
  } }
```
`matchingMessages` present when `deleteMatching=true`. `unsubscribed.success` is best-effort.

---

### Flag Management

| #   | Tool           | Description                                                                          | Annotations |
| --- | -------------- | ------------------------------------------------------------------------------------ | ----------- |
| 20  | `add_flags`    | Add flags and/or Apple Mail color (union semantics). Colors: red, orange, yellow, green, blue, purple, gray. | `idempotent` |
| 21  | `remove_flags` | Remove specific flags and/or clear Apple Mail color. Others preserved.                | `idempotent` |

#### Output Schemas

**add_flags** / **remove_flags**
```json
{ "mailbox", "account", "uid", "flags": ["\\Seen", "\\Flagged", ...] }
```
Returns the full updated flag set after the operation.

---

## Prompts (6)

| #   | Prompt                | Description                                       | Arguments                    |
| --- | --------------------- | ------------------------------------------------- | ---------------------------- |
| 1   | `inbox-summary`       | Full inbox overview: folders, top senders, unread | `account`                    |
| 2   | `cleanup-sender`      | Find & bulk-delete from a specific sender         | `account`, `sender`          |
| 3   | `find-attachments`    | Scan for downloadable attachments                 | `account`, `mailbox?`        |
| 4   | `compose-email`       | Guided draft composition (supports attachments via create_draft) | `account`, `to?`, `subject?` |
| 5   | `unsubscribe-cleanup` | Identify high-volume lists, unsubscribe + delete  | `account`                    |
| 6   | `list-id-cleanup`     | Identify mailing lists by List-Id, bulk-delete    | `account`                    |

## Task Support (SEP-1686)

9 long-running tools support `execution.taskSupport = "optional"` — clients can invoke them normally (synchronous with progress notifications) or as background tasks (enqueue, poll, retrieve result).

**Taskable tools:** `list_flags`, `find_attachments`, `top_senders`, `top_subscriptions`, `top_mailing_lists`, `delete_messages`, `delete_by_sender`, `delete_list_id`, `download_attachments`

**Destructive task serialization:** Destructive tasks (`delete_messages`, `delete_by_sender`, `delete_list_id`, `unsubscribe_message`) targeting the same account are serialized — each waits for the previous destructive task to finish before starting. Non-destructive tasks run concurrently without restriction.

**Task lifecycle:** `tasks/list`, `tasks/get`, `tasks/getResult`, `tasks/cancel`

**Cancellation:** `tasks/cancel` aborts the task's future at its next await point. For direct (non-task) calls, `notifications/cancelled` stops long scans cooperatively at the next mailbox or fetch-chunk boundary; transport shutdown triggers the same path.

## Resources (2 templates)

Single messages are addressable as resources. `resources/list` is intentionally empty — discovery is template-based (`resources/templates/list`), since mailboxes hold thousands of messages.

| URI template                               | MIME type        | Content                                       |
| ------------------------------------------ | ---------------- | --------------------------------------------- |
| `email://{account}/{mailbox}/{uid}`        | `text/markdown`  | Message rendered as markdown (headers + body) |
| `email://{account}/{mailbox}/{uid}/source` | `message/rfc822` | Raw RFC822 source with all headers and MIME   |

`account` and `mailbox` are percent-encoded URI segments; a `/` inside a mailbox name must be encoded as `%2F` (e.g. `email://work/Archive%2F2024/1234`). The markdown body is the same normalized, length-capped content `get_messages` returns with `includeContent=true`.

**Error codes for `resources/read`:**

| Code     | Meaning                                                    |
| -------- | ---------------------------------------------------------- |
| `-32602` | Malformed URI, unknown account                             |
| `-32002` | UID does not exist in the mailbox (resource not found)     |
| `-32603` | Transport/IMAP failure (including unselectable mailboxes)  |

## Completions

`completion/complete` is supported for prompt arguments and the `email://` resource-template variables:

- `account` — instant, from configured account names, prefix-filtered.
- `mailbox` — IMAP LIST scoped to the `account` from the completion context (falls back to the default account); for resource templates the values are returned percent-encoded, ready for substitution. Failures yield an empty list, never an error.
- Other arguments (`uid`, `sender`, `to`, `subject`) are not enumerable and return no values.

## Annotations Key

| Annotation    | Meaning                                                        |
| ------------- | -------------------------------------------------------------- |
| `read_only`   | Does not modify any server state                               |
| `destructive` | Permanently deletes or modifies messages                       |
| `idempotent`  | Safe to call multiple times with same arguments                |
| `open_world`  | Makes external HTTP requests (e.g. one-click unsubscribe POST) |
| `taskable`    | Supports `execution.taskSupport = "optional"` (SEP-1686)       |
