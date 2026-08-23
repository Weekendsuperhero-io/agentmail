---
created: 2026-05-29T19:20
updated: 2026-08-23T00:00
---
# Agentmail MCP — Tool & Prompt Reference

MCP protocol: [2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25) (also negotiates 2025-06-18, 2025-03-26, and 2024-11-05) | [rmcp](https://crates.io/crates/rmcp) (official Rust MCP SDK) | Transport: stdio (standalone) or AsyncRead+AsyncWrite (in-process)

**Supported capabilities:** tools, prompts, resources, completions, tasks (SEP-1686), progress notifications, request cancellation

**Contract conventions:**

- Every paginated result shares one envelope: `offset`, `limit`, `total`
  (the row universe being paged), and `nextOffset` (present only when
  another page exists). Domain statistics keep their own names alongside it
  (e.g. `totalMessages` on `top_*` counts messages scanned, not rows).
- `mailbox` is optional **only** where omitting it means "scan the whole
  account" (`list_flags`, `find_attachments`, `top_*`, `delete_list_id`).
  Everywhere else — single-mailbox readers and every UID consumer — it is
  required; nothing defaults to INBOX, because a UID and
  `expectedUidValidity` are only meaningful with the mailbox they came from.

AgentMail reads, organizes, archives, and saves drafts. It does not expose a
send operation.

## Tools (37)

### Discovery & Connection

| #   | Tool                | Description                                      | Annotations |
| --- | ------------------- | ------------------------------------------------ | ----------- |
| 1   | `list_accounts`     | Return configured IMAP account names             | `read_only` |
| 2   | `list_mailboxes`    | Paginate selectable folders with counts and registered special-use roles | `read_only` |
| 3   | `check_connection`  | Test IMAP connectivity and auth for an account   | `read_only` |
| 4   | `list_capabilities` | Query IMAP extensions (IDLE, MOVE, CONDSTORE)    | `read_only` |

#### Output Schemas

**list_accounts** → `ListAccountsResponse`
```json
{ "accounts": [{ "name", "isDefault": bool }] }
```

The MCP projection intentionally omits IMAP hostnames and login usernames.

**list_mailboxes** → `ListMailboxesResponse`
```json
{ "account", "offset", "limit", "total", "nextOffset?",
  "mailboxes": [{ "name", "totalMessages", "unseenMessages", "delimiter?",
    "noInferiors": bool, "roles?": ["all", "archive", ...] }] }
```

`account` is required. `offset` defaults to 0 and `limit` defaults to 100
(maximum 500). Only selectable mailboxes are exposed, because unselectable
containers cannot be searched or used as mutation targets. `roles` preserves
every recognized registered special-use purpose and is omitted for ordinary
mailboxes. Filtering and pagination happen before per-mailbox `STATUS`, so only
the returned page incurs count queries. The query is always live; counts never
come from the layout catalog.
The currently supported/tested client profile is IMAP4rev1; pure IMAP4rev2
servers are not yet supported end to end.

**check_connection** → `ConnectionStatus`
```json
{ "account", "connected": bool, "error?" }
```

Probe contract: connectivity and auth outcomes are **data** — a configured
but unreachable or rejecting account returns `connected: false` with the
error text, never a protocol error. The one exception is a parameter error:
an unknown account raises `-32602`.

**list_capabilities** → `ListCapabilitiesResponse`
```json
{ "account", "capabilities": ["IDLE", "MOVE", ...] }
```

---

### Read Messages

| #   | Tool               | Description                                                                                         | Annotations            |
| --- | ------------------ | --------------------------------------------------------------------------------------------------- | ---------------------- |
| 5   | `get_messages`     | Paginated metadata discovery from one required mailbox, newest-first. Default: offset=0, limit=25 (max 50) | `read_only`            |
| 6   | `search_messages`  | Paginated IMAP metadata search of one required mailbox with text, headers, status, date, and size filters. | `read_only`            |
| 7   | `list_flags`       | All IMAP flags in use with counts. Resolves Apple $MailFlagBit colors. Omit mailbox to scan all.    | `read_only`, `taskable` |
| 8   | `find_attachments` | Scan for messages with attachments (mixed + related), paginated. Omit mailbox to scan all.          | `read_only`, `taskable` |
| 9   | `top_senders`     | Top senders by volume (email, display name) with counts + date ranges. Omit mailbox to scan all.    | `read_only`, `taskable` |
| 10  | `top_subscriptions` | Top bulk-mail senders with advertised one-click syntax and UIDVALIDITY-guarded samples.             | `read_only`, `taskable` |
| 11  | `top_mailing_lists`     | Top mailing lists by List-Id (RFC 2919). Groups across senders. Omit mailbox to scan all.           | `read_only`, `taskable` |
| 12  | `top_domains`     | Exact canonical Header From domains and subdomains with counts, dates, and a live sample subject.   | `read_only`, `taskable` |
| 13  | `list_pending_moves` | List durable COPY-fallback MOVE operations awaiting reconciliation or review.                       | `read_only`             |
| 14  | `preview_thread_record` | Discover a bounded exact Message-ID graph and return a confirmation digest without writing files. | `read_only`, `taskable` |

#### Output Schemas

**get_messages**
```json
{ "mailbox", "account", "uidValidity", "offset", "limit", "total",
  "nextOffset?", "messages": [MessageMetadata] }
```

**search_messages**
```json
{ "mailbox", "account", "uidValidity", "offset", "limit", "total",
  "nextOffset?", "messages": [MessageMetadata] }
```

Non-ASCII search text is sent with a `CHARSET UTF-8` prefix (accepted by Gmail, Dovecot, Courier, iCloud, and Outlook). Servers that reject UTF-8 search return `-32602`. CR/LF in search text is rejected.

Search filters are **AND-combined** (a message must match all provided filters) and matched as **case-insensitive substrings** (IMAP semantics). `header_key` without a value matches messages that merely *have* that header. Date range: `since`/`before` (YYYY-MM-DD, server internal date → IMAP `SINCE`/`BEFORE`, since=inclusive, before=exclusive). Size: `larger_than`/`smaller_than` in bytes (`LARGER`/`SMALLER`). Arbitrary `OR`/`NOT` boolean expressions are not supported — a recursive query tree would reintroduce `$defs`/`$ref` into the tool schema (which some hosts reject); issue multiple searches instead.

**MessageMetadata** (shared by `get_messages` and `search_messages`)
```json
{ "uid", "subject", "sender", "date?", "flags": [], "size?", "resourceUri" }
```

These two tools are metadata-only over MCP. They do not accept
`includeContent` or `includeHeaders`, and they never return bodies, complete
headers, recipient lists, or raw list-action values. Follow `resourceUri` when
body, exact-header, or raw-source data is actually needed.

**list_flags**
```json
{ "mailbox": "INBOX" | "*", "account", "totalFlags",
  "flags": [{ "flag": "\\Seen", "count": 5000 }],
  "colors?": [{ "color": "red", "count": 8 }],
  "perMailbox?": [{ "mailbox", "totalFlags", "flags": [...] }],
  "perMailboxTotal", "perMailboxTruncated" }
```
`colors` is present when Apple $MailFlagBit flags exist. Account-wide mailbox
breakdowns are capped at 50 rows and include total/truncation metadata.

**find_attachments**
```json
{ "mailbox": "INBOX" | "*", "account", "total", "offset", "limit",
  "nextOffset?",
  "messages": [{ "mailbox", "uidValidity", "uid", "date?", "resourceUri" }],
  "perMailbox?": [{ "mailbox", "count" }],
  "perMailboxTotal", "perMailboxTruncated" }
```

Results are paginated newest-first (default 25, maximum 100). Every hit carries
its owning mailbox and UID epoch, so account-wide UIDs are never ambiguous.
Mailbox breakdowns are capped at 50 rows and include total/truncation metadata.

**top_senders**
```json
{ "mailbox": "INBOX" | "*", "account", "totalMessages", "total",
  "offset", "limit", "nextOffset?",
  "senders": [{
    "address", "displayName", "count", "oldestDate?", "newestDate?",
    "sample": MessageIdentity
  }] }
```

Grouped by `(email, display name)`; the same address with different display
names is separate. On the `top_*` tools, `total` counts the ranked rows
(unique senders/lists/domains — the pagination universe) while `totalMessages`
counts messages scanned. Sender, subscription, and mailing-list rankings
default to 10 rows; domain rankings default to 20. All accept at most 100.

**Windowed providers:** Yahoo/AOL Limited Mode exposes only a recent mailbox
window (see `docs/standards/imap/yahoo-aol-quirks.md`). When the server
advertises RFC 9586 UIDONLY and the persistent cache is enabled, Agentmail uses
UID Mode and a resumable membership walk so rankings cover the full mailbox.
Without UIDONLY, discovery is limited to what the server exposes; destructive
sweeps repeat passes as older mail backfills into view. Account-wide discovery uses one selectable
`\All` mailbox when available. Otherwise it enumerates storage mailboxes,
excludes Trash/Junk/Drafts and virtual All/Flagged/Important views, and
deduplicates by Message-ID across folders. Sender rankings exclude the
account's own address.

**top_subscriptions**
```json
{ "mailbox": "INBOX" | "*", "account", "totalMessages", "total",
  "offset", "limit", "nextOffset?",
  "lists": [{
    "address", "advertisedOneClick": bool,
    "count", "subject?", "oldestDate?", "newestDate?",
    "sample": MessageIdentity
  }] }
```
Grouped only by normalized sender email; display names and `List-Id` values do
not split a sender's row. Sorted: advertised one-click senders first, then by
count. `advertisedOneClick` checks exact local RFC 2369/8058 syntax; it does not
claim DKIM success.
Opaque unsubscribe URLs and recipient tokens are not exposed. Use the nested
`sample` identity for a later `move_subscription` or `unsubscribe_message`
call. `subject` is the sample message's decoded Subject, fetched live for the
returned page only and never persisted in the ranking cache; it is absent when
the sample could not be fetched.

**top_mailing_lists**
```json
{ "mailbox": "INBOX" | "*", "account", "totalMessages", "total",
  "offset", "limit", "nextOffset?",
  "lists": [{
    "listId": "list-id.example.com",
    "displayName": "Example List", "senders": ["noreply@example.com"],
    "senderCount", "count", "subject?", "oldestDate?", "newestDate?",
    "sample": MessageIdentity
  }] }
```

Grouped by List-Id header; the same list with different senders is one entry.
`senders` is a preview of at most five values and `senderCount` reports the
complete count. `subject` is the sample message's decoded Subject (fetched
live per page, never cached), so a caller can see what the list actually is
before deleting or moving it. `top_senders` intentionally has no subject —
one sender spans many lists and subject families, so a single sample would
mislead.

**top_domains**
```json
{ "mailbox": "INBOX" | "*", "account", "totalMessages", "total",
  "offset", "limit", "nextOffset?",
  "domains": [{
    "domain": "mail.example.co.uk",
    "registrableDomain?": "example.co.uk", "subdomain?": "mail",
    "count", "subject?", "oldestDate?", "newestDate?",
    "sample": MessageIdentity
  }] }
```

The grouping key is the exact canonical domain from the parsed Header From
address. Parent domains never include subdomains implicitly:
`example.com` and `mail.example.com` are separate rows and separate mutation
selectors. Public Suffix List fields describe the relationship without
changing the grouping. The default page size is 20 and the maximum is 100.
`subject` is decoded from the live sample for the returned page and is never
persisted in the ranking cache. As with every ranking, `limit = N` returns up
to N domain rows; it is not a fixed five-row preview.

```json
MessageIdentity = { "mailbox", "uidValidity", "uid", "resourceUri" }
```

The `top_*` tools share a persistent ranking-header projection. Every reuse is validated with live `EXAMINE` metadata and the RFC identity tuple `(mailbox, UIDVALIDITY, UID)`. Cache hits avoid header fetches, proven appends fetch only new UIDs, and deletions reconcile UID membership without refetching unchanged headers. A busy-mailbox snapshot is returned but remains reconcile-required until a stable snapshot is observed.

The schema-v6 cache contains account mutation state, mailbox snapshot state,
UID membership, sender address/name and canonical sender domain, date,
Message-ID, normalized List-Id and display name, plus booleans for list-header
and advertised-one-click presence.
It does not store List-Unsubscribe URLs, recipient tokens, raw list-action
headers, bodies, subjects, recipients, flags, attachments, passwords,
authentication tokens, keychain secrets, or complete messages. The cache
namespace includes the IMAP host/port/TLS mode and login username to prevent
server identities from colliding, but deliberately excludes the local account
display name so renaming an account reuses the same projection. SQLite uses WAL mode with
`synchronous=NORMAL`. Set
`AGENTMAIL_DISABLE_HEADER_CACHE=1` to disable it or `AGENTMAIL_CACHE_DIR` to
override its root; cache errors fall back to live IMAP.

`list_flags` and `find_attachments` use the same discovery plan. Discovery uses one selectable `\All` mailbox exclusively when available. Enumerated fallback and account-wide destructive tools skip `\All`, `\Drafts`, `\Flagged`, `\Important`, `\Junk`, and `\Trash`, while retaining storage roles including `\Archive`, `\Sent`, `\Memos`, `\Scheduled`, and `\Snoozed`. A caller-provided mailbox bypasses planning and is honored directly. IMAP defines `\NoSelect`, not a separate `\NoScan` attribute; `\NoSelect` is always excluded automatically.

**preview_thread_record**

```json
{
  "account", "seed": MessageIdentity,
  "strategy": "exact-rfc-message-id-graph-v1", "rationale",
  "messages": [{
    "identity": MessageIdentity, "messageId?", "inReplyTo?",
    "references": [], "date?", "from", "subject", "selectionBasis": []
  }],
  "selectionDigest", "confirmationRequired": true,
  "truncated": false, "warnings": []
}
```

The bounded cross-mailbox graph follows only exact normalized `Message-ID`,
`In-Reply-To`, and `References` values. Subject similarity is never a
selection edge. At most 100 storage identities are returned; a truncated
preview cannot be exported. The digest binds the exact identities and headers
shown by the preview to a later `export_thread_record` confirmation.

---

### Write / Mutate

| #   | Tool                   | Description                                                                           | Annotations                              |
| --- | ---------------------- | ------------------------------------------------------------------------------------- | ---------------------------------------- |
| 15  | `delete_messages`      | Delete by UID (up to 500). Moves to Trash, or permanently expunges when `permanent=true`. | `destructive`, `idempotent`, `taskable`   |
| 16  | `delete_by_sender`     | Delete all from an exact sender identity (`email` + `name` from a ranking row). Omit `mailbox` for account-wide. `permanent=true` bypasses Trash. | `destructive`, `taskable`                 |
| 17  | `delete_list_id`       | Delete all messages with an **exact** List-Id across all mailboxes. `permanent=true` bypasses Trash. | `destructive`, `taskable`                 |
| 18  | `delete_by_domain`     | Delete all messages from one exact canonical domain from `top_domains`; subdomains are never implicit. | `destructive`, `taskable`                 |
| 19  | `move_list_id`         | Move all messages with an **exact** List-Id to a destination mailbox in one operation (e.g. archive a statement list). Omit `mailbox` for account-wide; destination excluded. | `taskable`                               |
| 20  | `move_by_sender`       | Move all messages from an exact sender identity (`email` + `name`) to a destination mailbox in one operation. Omit `mailbox` for account-wide; destination excluded. | `taskable`                               |
| 21  | `move_by_domain`       | Move all messages from one exact canonical domain to a destination; subdomains are never implicit. | `taskable`                               |
| 22  | `move_subscription`    | Move the exact bulk-mail subscription represented by a UIDVALIDITY-safe `top_subscriptions` sample; destination excluded. | `taskable`                               |
| 23  | `move_message`         | IMAP MOVE between mailboxes (durable COPY+EXPUNGE fallback when MOVE is unavailable). |                                          |
| 24  | `reconcile_moves`      | Safely resume one or all pending COPY-fallback MOVE operations.                       | `destructive`, `taskable`                |
| 25  | `create_mailbox`       | Create new folder                                                                     | `idempotent`                             |
| 26  | `rename_mailbox`       | Preview, then confirm a guarded mailbox rename.                                       | `destructive`                            |
| 27  | `delete_mailbox`       | Preview, then confirm guarded mailbox deletion.                                       | `destructive`, `idempotent`              |
| 28  | `create_draft`         | Save an RFC822 draft with To/Cc/Bcc/Reply-To, optional threading headers, and attachments. |                                      |
| 29  | `create_reply_draft`   | Derive reply or reply-all recipients and RFC threading headers from a live message.  |                                          |
| 30  | `update_draft`         | Atomically replace a live draft with RFC 8508 REPLACE; refuses APPEND+DELETE.          | `destructive`                            |
| 31  | `download_attachments` | Extract attachments to disk as `{uid}_{index}_{filename}`                             | `taskable`                               |
| 32  | `download_message_source` | Save exact RFC822 bytes directly to disk with SHA-256, metadata, and local DNS-backed DKIM evidence. | `open_world`, `taskable`                 |
| 33  | `download_thread`      | Save a caller-selected set of up to 100 UIDs plus a JSON evidence manifest. Does not discover thread membership. | `open_world`, `taskable`                 |
| 34  | `export_thread_record` | Confirm a preview digest and write PDF, exact EML sources, and an integrity manifest. | `open_world`, `taskable`                 |
| 35  | `unsubscribe_message`  | DKIM-verified RFC 8058 POST; optional matching-message cleanup via the nested `cleanup {when, identity, deletion}` object (omitted = unsubscribe only). | `destructive`, `open_world`, `taskable`  |

**`permanent` flag (delete tools):** default false moves to Trash when a Trash mailbox exists, else permanently deletes. When true, flags `\Deleted` + UID EXPUNGE directly, bypassing Trash — irreversible. Permanent delete requires the server to advertise UIDPLUS; on servers without it the call is refused (plain EXPUNGE would purge unrelated `\Deleted` messages).

No delete silently escalates: Trash unavailability is refused up front
(retry with `permanent=true`), and a failed Trash MOVE is reported as failed
UIDs. `unsubscribe_message` cleanup permits escalation only via
`cleanup.deletion = "trashThenPermanent"`.

`move_list_id`, `move_by_sender`, `move_by_domain`, and `move_subscription`
share the delete tools' mutation and durable recovery machinery. The first
three take their ranking identity directly. `move_subscription` instead
re-fetches the selected `top_subscriptions` sample under its UIDVALIDITY epoch,
then requires the exact canonical sender plus either `List-Unsubscribe` or
`List-Unsubscribe-Post`; when the sample has one usable List-Id, every match
must also carry that exact normalized List-Id. This covers the same bulk-mail
surface as the ranking without sweeping ordinary mail from that sender.

All four bulk movers live-confirm candidates and feed matches into the same
window-draining, UID-Mode-aware move engine and durable COPY-fallback recovery
path. The destination must already exist and is always excluded from
account-wide sweeps.

**Gmail:** on Gmail (`X-GM-EXT-1`), in-place `\Deleted`+EXPUNGE only removes a *label* — the message survives in All Mail. agentmail therefore routes every delete, including `permanent`, through `[Gmail]/Trash` (which removes the message from all labels; Gmail purges Trash on its own schedule). Immediate hard-purge isn't available on Gmail.

`delete_messages` accepts at most 500 explicitly supplied UIDs over MCP. `delete_by_sender`, `delete_by_domain`, `delete_list_id`, `move_by_sender`, `move_by_domain`, `move_list_id`, `move_subscription`, and unsubscribe matching have no total match limit; they split server mutations into 500-UID wire batches. A `top_*` ranking `limit` never becomes a mutation limit.

UID-based tools never accept a bare UID as durable identity, and every UID
consumer requires the `mailbox` the identity came from (there is no INBOX
default). The following arguments are required together with
`expectedUidValidity`:

- `delete_messages`: one or more UIDs discovered in one mailbox epoch.
- `move_message`, `download_attachments`, `download_message_source`,
  `download_thread`, `add_flags`, and `remove_flags`:
  the UID from a current discovery result.
- `unsubscribe_message`: the `sample` from `top_subscriptions`.
- `move_subscription`: the same nested `sample`, plus the destination mailbox.

`delete_by_sender`, `move_by_sender`, `delete_by_domain`, `move_by_domain`,
`delete_list_id`, and `move_list_id` instead take direct identity values
(`email` + `name`, exact `domain`, or `listId`) from
ranking rows and confirm them live in each mailbox — no sample UID or epoch
guard.

Each tool performs a live mailbox selection and fails before acting when the
observed UIDVALIDITY differs. Refresh discovery instead of retrying a stale
identity.

#### Output Schemas

**delete_messages**
```json
{ "mailbox", "account", "deleted": 5, "failed": 0,
  "pending": 0, "needsAttention": 0, "operationIds": [],
  "trashFallback": bool, "permanent": bool }
```

**delete_by_sender**
```json
{ "mailbox": "INBOX" | "*", "account",
  "sender": "Display Name <email>",
  "found", "deleted", "failed", "pending", "needsAttention",
  "operationIds": [],
  "mailboxes": [{ "mailbox", "found", "deleted", "failed",
    "pending", "needsAttention", "operationIds": [] }],
  "mailboxesTotal", "mailboxesTruncated",
  "skipped": [], "skippedTotal", "skippedTruncated", "permanent": bool }
```

Mailbox and skipped breakdowns are capped at 50 rows; their total and
truncation fields preserve audit completeness.

**delete_list_id**
```json
{ "mailbox": "INBOX" | "*", "account",
  "listId": "list-id.example.com",
  "found", "deleted", "failed", "pending", "needsAttention",
  "operationIds": [],
  "mailboxes": [{ "mailbox", "found", "deleted", "failed",
    "pending", "needsAttention", "operationIds": [] }],
  "mailboxesTotal", "mailboxesTruncated",
  "skipped": ["mailbox"], "skippedTotal", "skippedTruncated",
  "permanent": bool }
```
`mailboxes` is present when scanning all mailboxes. `skipped` lists planned mailboxes that could not be selected or searched; policy-excluded special-use views are not reported as errors.

**delete_by_domain**
```json
{ "mailbox": "INBOX" | "*", "account", "domain": "mail.example.com",
  "found", "deleted", "failed", "pending", "needsAttention",
  "operationIds": [],
  "mailboxes": [{ "mailbox", "found", "deleted", "failed",
    "pending", "needsAttention", "operationIds": [] }],
  "mailboxesTotal", "mailboxesTruncated",
  "skipped": [], "skippedTotal", "skippedTruncated", "permanent": bool }
```

**move_list_id**
```json
{ "mailbox": "INBOX" | "*", "account",
  "listId": "list-id.example.com", "destination": "Statements",
  "found", "moved", "failed", "pending", "needsAttention",
  "operationIds": [],
  "mailboxes": [{ "mailbox", "found", "moved", "failed",
    "pending", "needsAttention", "operationIds": [] }],
  "mailboxesTotal", "mailboxesTruncated",
  "skipped": ["mailbox"], "skippedTotal", "skippedTruncated" }
```

**move_by_sender**
```json
{ "mailbox": "INBOX" | "*", "account",
  "sender": "Display Name <email>", "destination": "Statements",
  "found", "moved", "failed", "pending", "needsAttention",
  "operationIds": [],
  "mailboxes": [{ "mailbox", "found", "moved", "failed",
    "pending", "needsAttention", "operationIds": [] }],
  "mailboxesTotal", "mailboxesTruncated",
  "skipped": [], "skippedTotal", "skippedTruncated" }
```

**move_by_domain**
```json
{ "mailbox": "INBOX" | "*", "account",
  "domain": "mail.example.com", "destination": "Statements",
  "found", "moved", "failed", "pending", "needsAttention",
  "operationIds": [],
  "mailboxes": [{ "mailbox", "found", "moved", "failed",
    "pending", "needsAttention", "operationIds": [] }],
  "mailboxesTotal", "mailboxesTruncated",
  "skipped": [], "skippedTotal", "skippedTruncated" }
```

**move_message**
```json
{ "mailbox", "account", "uidValidity", "uid", "destination",
  "moved": bool,
  "status": "moved" | "failed" | "reconciliationPending" | "needsAttention",
  "operationId?" }
```

**list_pending_moves**
```json
{ "account", "operations": [PendingMove] }
```

**reconcile_moves**
```json
{ "account", "examined", "completed", "pending", "needsAttention",
  "failed", "operations": [PendingMove] }
```

```json
PendingMove = {
  "operationId", "sourceMailbox", "sourceUidValidity", "sourceUid",
  "destination",
  "status": "reconciliationPending" | "needsAttention",
  "detail?", "createdAt", "updatedAt"
}
```

**move_subscription**
```json
{ "mailbox": "*", "account",
  "sampleMailbox", "sampleUidValidity", "sampleUid",
  "sender", "listId?", "matchedBy", "destination": "Subscriptions",
  "found", "moved", "failed", "pending", "needsAttention",
  "operationIds": [],
  "mailboxes": [{ "mailbox", "found", "moved", "failed",
    "pending", "needsAttention", "operationIds": [] }],
  "mailboxesTotal", "mailboxesTruncated",
  "skipped": [], "skippedTotal", "skippedTruncated" }
```

Native `UID MOVE` does not create a journal record. When the server lacks MOVE,
Agentmail records intent in a separate durable SQLite journal before COPY,
consumes the exact tagged command completion, records `COPYUID` when supplied,
and only then removes the source. An EOF or timeout is ambiguous, so the pooled
session is discarded and the response reports `reconciliationPending` with an
`operationId` rather than claiming success or issuing an unsafe second COPY.
`reconcile_moves` retries COPY only when unchanged destination `UIDNEXT` proves
that the earlier attempt created nothing; otherwise it continues source cleanup
or reports `needsAttention` for explicit review. The journal uses
`synchronous=FULL` and remains enabled even when the disposable ranking cache
is disabled.

**create_mailbox**
```json
{ "account", "mailbox", "created": bool, "alreadyExists": bool }
```

**rename_mailbox** / **delete_mailbox**

The first call leaves `confirmRename` or `confirmDelete` false and returns live
preflight data:

```json
{
  "account", "mailbox", "newMailbox?", "preview": true,
  "renamed?": false, "deleted?": false, "alreadyMissing?": false,
  "preflight": {
    "messageCount", "roles": [], "descendants": [],
    "confirmationsRequired": []
  }
}
```

The confirmed call must echo `expectedMessageCount`. A changed count fails
closed. INBOX and mailboxes referenced by a pending MOVE journal are never
eligible. Special-use and descendant-bearing mailboxes need separate
acknowledgements; deleting a non-empty mailbox needs `confirmNonEmpty` as well.
The rename destination must not exist. A missing delete target is an idempotent
success. After a transport error, AgentMail re-lists the mailbox catalog and
reports success only when the resulting state is unambiguous.

**create_draft** / **create_reply_draft**
```json
{ "created": true, "account", "draftsMailbox", "attachmentCount",
  "replyToCount", "threadingApplied", "warning?",
  "uidValidity?", "uid?", "resourceUri?" }
```

The compact result confirms placement without echoing the subject, recipients,
local input paths, or filenames. `create_draft` composes a complete RFC822
message with Date, Message-ID, Apple Mail draft markers, and optional Bcc,
Reply-To, In-Reply-To, and References headers, then appends it to a selectable
Drafts mailbox with the `\Draft` flag. Bcc is deliberately retained in the
stored draft. The identity fields are best-effort: after APPEND the generated
Message-ID is searched when an APPENDUID is unavailable. If APPEND loses its
tagged completion, AgentMail discards that connection and searches the same
Message-ID on a fresh one. It reports success only when the draft is found;
otherwise it directs the caller to inspect Drafts before retrying.

`create_reply_draft` starts from a live UIDVALIDITY-safe message. It uses the
source Reply-To before From; reply-all adds source To/Cc while excluding the
configured account address and aliases; Bcc is never inferred. It applies one
`Re:` prefix and extends exact RFC threading headers. A source without a
Message-ID still produces a draft but returns `threadingApplied: false` with a
warning. Neither tool sends mail.

**update_draft**

```json
{ "updated": true, "account", "draftsMailbox",
  "previousUidValidity", "previousUid",
  "uidValidity?", "uid?", "resourceUri?" }
```

The input is a complete replacement specification, including attachments.
AgentMail verifies the live UIDVALIDITY and `\Draft` flag, preserves the Apple
draft UUID, and requires server-advertised RFC 8508 REPLACE. It never emulates
replacement with APPEND+DELETE because a disconnect can leave duplicates; an
ambiguous REPLACE error instructs the caller to inspect Drafts before retrying.

**download_attachments**
```json
{ "mailbox", "account", "uidValidity", "uid",
  "downloaded": [{ "index", "filename", "path", "contentType", "size" }] }
```

**download_message_source**

```json
{ "account", "mailbox", "uidValidity", "uid", "path", "bytes", "sha256",
  "messageId?", "date?", "from?", "subject?", "downloadedAt",
  "dkim": { "result", "domain?", "detail?", "checkedAt" }, "spf?" }
```

The tool fetches the complete message with `BODY.PEEK[]` only after a live
UIDVALIDITY check and `RFC822.SIZE` preflight, with a 64 MiB per-message cap.
It writes a private create-new file under the active session workspace, so
neither an existing file nor a path outside that workspace can be overwritten. SHA-256 and
DKIM are computed from the exact saved bytes. DKIM is verified locally against
current DNS; an `Authentication-Results` header is not accepted as proof. SPF
is omitted because an RFC822 archive lacks the SMTP client IP, HELO, and
envelope sender needed to independently recompute it.

**download_thread**

```json
{ "account", "mailbox", "uidValidity", "createdAt", "manifestPath",
  "messages": [DownloadMessageSourceOutput] }
```

This bulk convenience tool accepts one to 100 unique UIDs already selected by
the caller, saves them as `{uid}.eml`, and writes the complete response as a
JSON manifest. It does not discover or infer thread membership. All UIDs must
belong to the same mailbox and UIDVALIDITY epoch; existing source or manifest
filenames cause a no-overwrite failure.

For embedded AgentMail, Agent Muse injects the trusted absolute workspace root
as request metadata (`io.agentmuse/workspaceRoot`) on both direct and
task-augmented calls. Only the in-process backend named `agentmail` receives
it; a missing or invalid root fails closed, and other backends never receive
the value. Standalone `agentmail serve` uses `AGENTMAIL_FILE_ROOT` or its
`~/.agentmail/files` default instead.

**export_thread_record**

```json
{
  "recorded": true, "submittable": true, "submissionExplanation",
  "account", "purpose", "selectionDigest", "messageCount",
  "bundlePath", "pdfPath", "manifestPath", "totalBytes",
  "limitations": []
}
```

The call requires the exact digest from `preview_thread_record` plus a
user-supplied purpose explanation. It re-discovers the graph and refuses any
drift or truncation. The no-overwrite private bundle contains a styled,
page-numbered PDF, one exact RFC822 `.eml` for every selected storage identity,
and a JSON manifest with identities, Message-IDs, hashes, metadata, and current
DNS-backed DKIM results. Each source is capped at 64 MiB and the bundle at 512
MiB. Every source and the PDF are reopened and parsed, hashes are rechecked,
and the manifest is written last and reopened before `recorded` or
`submittable` becomes true. These flags describe packet completeness and
readiness to hand to a recipient; they do not assert authentication, legal
admissibility, or acceptance by any recipient.

**unsubscribe_message**

Required action identity and consent:

```json
{
  "mailbox": "INBOX",
  "account": "work",
  "uid": 42,
  "expectedUidValidity": 3857529045,
  "confirmOneClick": true,
  "cleanup": {
    "when": "afterSuccess",
    "identity": "listIdOrSender",
    "deletion": "trash"
  }
}
```

The fields through `confirmOneClick` identify a live-ranked message and record
explicit RFC 8058 consent. Omit `cleanup` to perform only the unsubscribe POST.
When cleanup is present, `identity: "listIdOrSender"` first prefers the single
List-Id covered by the passing DKIM signature. Its fallback requires the exact
normalized sender email plus `List-Unsubscribe-Post`; when the sampled message
has one usable List-Id, matching messages must carry that same normalized
List-Id too. Display names do not affect fallback matching.

Cleanup defaults are `when: "afterSuccess"`, `identity: "listIdOrSender"`, and
`deletion: "trash"`. `"always"` permits cleanup after a failed unsubscribe.
`"trashThenPermanent"` permits an irreversible UID EXPUNGE fallback when Trash
cannot be used, while `"permanent"` requests hard deletion directly. Gmail
routes permanent disposal through Trash because in-place EXPUNGE only removes
a label.

```json
{ "mailbox", "account", "uid", "uidValidity", "listId?",
  "dkimVerified": bool, "listIdAuthenticated": bool, "dkimDomain?",
  "unsubscribed": { "success": bool, "httpStatus?", "reason?" },
  "matchingMessages?": {
    "matchedBy": "list-id" | "sender-email-list-id-fallback" | "sender-email-fallback",
    "sender", "listId?", "found", "deleted", "failed",
    "mailboxes": [{ "mailbox", "found", "deleted", "failed" }],
    "mailboxesTotal", "mailboxesTruncated",
    "skipped": [], "skippedTotal", "skippedTruncated",
    "permanent": bool, "trashFallback": bool, "complete": bool
  },
  "cleanupSkippedReason?"
}
```

Neither the advertised URL, raw unsubscribe headers, recipient token, nor a
duplicated method/pathway value is returned. Failure text is URL-redacted.

The POST runs only after an exact header parse, local DKIM verification covering
both RFC 8058 headers, public-destination DNS validation and pinning, and a
second cancellation check. Redirects are disabled and only a direct 2xx is
success. `matchingMessages` is present only when requested cleanup actually
runs; otherwise `cleanupSkippedReason` explains a safety-policy stop.

The transient DKIM source is preflighted with `RFC822.SIZE` and capped at 64
MiB. This bounds one selected message only; account-wide cleanup has no total
message ceiling and continues to mutate in 500-UID batches.

---

### Flag Management

| #   | Tool           | Description                                                                          | Annotations |
| --- | -------------- | ------------------------------------------------------------------------------------ | ----------- |
| 36  | `add_flags`    | Add flags and/or set Apple Mail `color` (a color-name string; union semantics). Colors: red, orange, yellow, green, blue, purple, gray. | `idempotent` |
| 37  | `remove_flags` | Remove specific flags and/or clear the Apple Mail color with `clearColor: true`. Others preserved. | `idempotent` |

#### Output Schemas

**add_flags** / **remove_flags**
```json
{ "mailbox", "account", "uidValidity", "uid",
  "flags": ["\\Seen", "\\Flagged", ...], "color?" }
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
| 5   | `unsubscribe-cleanup` | Rank subscriptions; offer filing or verified unsubscribe | `account`                    |
| 6   | `list-id-cleanup`     | Identify mailing lists by List-Id, bulk-delete    | `account`                    |

## Task Support (SEP-1686)

The tools listed below support `execution.taskSupport = "optional"` — clients can invoke them normally (synchronous with progress notifications) or as background tasks (enqueue, poll, retrieve result).

**Taskable tools:** `search_messages`, `list_flags`, `find_attachments`, `top_senders`, `top_domains`, `top_subscriptions`, `top_mailing_lists`, `preview_thread_record`, `delete_messages`, `delete_by_sender`, `delete_by_domain`, `delete_list_id`, `move_list_id`, `move_by_sender`, `move_by_domain`, `move_subscription`, `reconcile_moves`, `download_attachments`, `download_message_source`, `download_thread`, `export_thread_record`, `unsubscribe_message`

**Destructive task serialization:** Destructive tasks (`delete_messages`, `delete_mailbox`, `delete_by_sender`, `delete_by_domain`, `delete_list_id`, `rename_mailbox`, `update_draft`, `reconcile_moves`, and `unsubscribe_message`) targeting the same account are serialized — each waits for the previous destructive task to finish before starting. The serialization list also protects those names if they become taskable later. Non-destructive tasks run concurrently without restriction.

**Task lifecycle:** `tasks/list`, `tasks/get`, `tasks/result`, `tasks/cancel`

**Cancellation:** `tasks/cancel` first cancels the cooperative task token and then aborts the async future; active SQLite publication checks that token and rolls back. For direct calls, `notifications/cancelled` stops scans at mailbox/fetch-chunk boundaries and interrupts unsubscribe DNS, DKIM, and HTTP waits through a 25 ms cancellation poll. Cancellation during an HTTP send is inherently ambiguous because the endpoint may already have received the POST.

Tasks are retained for 24 hours from creation, including completed, failed, and
cancelled metadata. At most 128 live tasks/reservations are accepted per server
process. `tasks/list` is newest-first in pages of 25 and uses an opaque,
process-local cursor. `tasks/result` is repeatable until expiry; retrieving a
result does not evict it. Expired active tasks are cancelled and removed.

## Tool Result Encoding

Every tool returns one short text block for clients that do not consume
structured output and one authoritative `structuredContent` object. The text
block is a summary capped at 8,000 characters, not a second escaped copy of the
JSON payload. All tool output schemas are root objects with nested definitions
inlined, so they contain no `$defs` or `$ref`.

Potentially long mailbox/skipped breakdowns are capped at 50 rows and include
`*Total` and `*Truncated` fields. Mailing-list sender previews are capped at
five addresses. These caps reduce model-context payloads without discarding
destructive-operation counts or audit state.

## Resources (6 templates plus account roots)

`resources/list` returns one `email://{account}` root for every configured
account. Reading a root yields its selectable mailbox catalog; reading a
mailbox resource yields a bounded newest-first page of message metadata. This
makes the resource surface navigable while avoiding an unbounded static list.

| URI template                                                              | MIME type             | Content                                      |
| ------------------------------------------------------------------------- | --------------------- | -------------------------------------------- |
| `email://{account}/{mailbox}{?offset,limit}`                              | `application/json`    | Paged metadata, default 25 and maximum 50    |
| `email://{account}/{mailbox}/{uidValidity}/{uid}`                         | `text/markdown`       | Normalized message body, capped at 100K chars |
| `email://{account}/{mailbox}/{uidValidity}/{uid}/headers`                 | `text/rfc822-headers` | Exact RFC822 header block, maximum 64 KiB    |
| `email://{account}/{mailbox}/{uidValidity}/{uid}/source`                  | `message/rfc822`      | Lossless base64 MCP blob, maximum 256 KiB    |
| `email://{account}/{mailbox}/{uidValidity}/{uid}/info`                    | `application/json`    | Message metadata + attachment inventory      |
| `email://{account}/{mailbox}/{uidValidity}/{uid}/attachments/{index}`     | per part              | One MIME attachment blob, maximum 4 MiB      |

`account` and `mailbox` are percent-encoded URI segments; a `/` inside a
mailbox name must be encoded as `%2F`, for example
`email://work/Archive%2F2024/3857529045/1234`. `uidValidity` and `uid` must be
non-zero. Old three-segment UID-only URIs are invalid because an IMAP server may
reuse a UID after changing UIDVALIDITY.

`/info` is the discovery entry point for a single message: a compact JSON
document with the message identity (`account`, `mailbox`, `uidValidity`,
`uid`), headline metadata (`subject`, `from`, `to`, `cc`, `date`, `flags`,
`size`, `messageId`, `listId`, `mimeType`), `attachmentCount`, the attachment
inventory, and the sibling `resources` URIs (`body`, `headers`, `source`).
Absent optional fields are omitted, never `null`.

Each inventory entry carries `index`, the original `name` (omitted for
nameless parts), the canonical `filename`, `contentType`, `size`, an optional
`contentId`, and the part's `resourceUri`. Attachments follow one naming
nomenclature everywhere: the canonical filename is
`{uid}_{index}_{sanitized-name}` (`unnamed` for nameless parts) — exactly what
`download_attachments` writes to disk. `/attachments/{index}` (zero-based part
index, stable within a UIDVALIDITY epoch) returns the part as an MCP resource
`blob` served with the part's own content type; parts above 4 MiB fail with
guidance to use the `download_attachments` tool instead.

Every resource read selects the mailbox and validates the expected epoch before
fetching. A missing UID, unavailable UIDVALIDITY, changed epoch, or
out-of-range attachment index returns resource-not-found. Body, info, and
attachment reads may transiently fetch at most 64 MiB before rendering the
bounded view; oversized headers/source representations fail with guidance to
use the narrower resource or filesystem-writing tools. `/source` returns an MCP
resource `blob` whose field value is base64, preserving the original RFC822
bytes without lossy UTF-8 conversion. For evidence archives or complete sources
that should not traverse model context, use `download_message_source` or
`download_thread` instead.

Account roots, mailbox catalogs, and message representations carry MCP
resource annotations. Catalog/body/info resources target the assistant at
priorities appropriate to discovery, headers/source are lower-priority exact
evidence views, and attachment blobs target both user and assistant. Agent Muse
preserves `audience`, `priority`, and `lastModified` when mapping backend
resources and displays them in the MCP Inspector.

**Error codes for `resources/read`:**

| Code     | Meaning                                                    |
| -------- | ---------------------------------------------------------- |
| `-32602` | Malformed URI, unknown account                             |
| `-32002` | UID is missing or its UIDVALIDITY identity is stale        |
| `-32603` | Transport/IMAP failure (including unselectable mailboxes)  |

## Completions

`completion/complete` is supported for prompt arguments and the `email://` resource-template variables:

- `account` — instant, from configured account names, prefix-filtered.
- `mailbox` — reads a bounded, five-minute, process-local layout catalog scoped to the `account` from the completion context (falling back to the default account). Cold or expired lookups issue one IMAP LIST; warm lookups do not use the network. Only path, delimiter, attributes, and recognized special-use roles are retained—never counts, UIDs, or message metadata. The same catalog plans account-wide scans. Resource-template values are percent-encoded for substitution. Failures yield an empty list, never an error.
- Other arguments (`uidValidity`, `uid`, `index`, `sender`, `to`, `subject`) are not enumerable and return no values.

## Annotations Key

| Annotation    | Meaning                                                        |
| ------------- | -------------------------------------------------------------- |
| `read_only`   | Does not modify any server state                               |
| `destructive` | Permanently deletes or modifies messages                       |
| `idempotent`  | Safe to call multiple times with same arguments                |
| `open_world`  | Makes external HTTP requests (e.g. one-click unsubscribe POST) |
| `taskable`    | Supports `execution.taskSupport = "optional"` (SEP-1686)       |
