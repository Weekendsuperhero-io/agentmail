---
created: 2026-05-29T19:20
updated: 2026-07-18T00:00
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

## Tools (21)

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
names is separate. On all three `top_*` tools, `total` counts the ranked rows
(unique senders/lists — the pagination universe) while `totalMessages` counts
messages scanned. All three use `offset`/`limit` pagination with
a default of 10 and maximum of 100.

**Windowed providers:** Yahoo/AOL expose only the newest ~10,000 messages of
a mailbox to IMAP (`EXISTS`, `STATUS`, `UID SEARCH`, and direct `UID FETCH`
are all capped — see `docs/standards/imap/yahoo-aol-quirks.md`). Rankings
there cover the visible window; account-wide deletes are unaffected because
they repeat passes as older mail backfills into view. Account-wide discovery uses one selectable
`\All` mailbox when available. Otherwise it enumerates storage mailboxes,
excludes Trash/Junk/Drafts and virtual All/Flagged/Important views, and
deduplicates by Message-ID across folders. Sender rankings exclude the
account's own address.

**top_subscriptions**
```json
{ "mailbox": "INBOX" | "*", "account", "totalMessages", "total",
  "offset", "limit", "nextOffset?",
  "lists": [{
    "address", "displayName", "advertisedOneClick": bool,
    "count", "oldestDate?", "newestDate?", "sample": MessageIdentity
  }] }
```
Sorted: advertised one-click senders first, then by count. `advertisedOneClick`
checks exact local RFC 2369/8058 syntax; it does not claim DKIM success.
Opaque unsubscribe URLs and recipient tokens are not exposed. Use the nested
`sample` identity for a later `unsubscribe_message` call.

**top_mailing_lists**
```json
{ "mailbox": "INBOX" | "*", "account", "totalMessages", "total",
  "offset", "limit", "nextOffset?",
  "lists": [{
    "listId": "list-id.example.com",
    "displayName": "Example List", "senders": ["noreply@example.com"],
    "senderCount", "count", "oldestDate?", "newestDate?",
    "sample": MessageIdentity
  }] }
```

Grouped by List-Id header; the same list with different senders is one entry.
`senders` is a preview of at most five values and `senderCount` reports the
complete count.

```json
MessageIdentity = { "mailbox", "uidValidity", "uid", "resourceUri" }
```

The three `top_*` tools share a persistent ranking-header projection. Every reuse is validated with live `EXAMINE` metadata and the RFC identity tuple `(mailbox, UIDVALIDITY, UID)`. Cache hits avoid header fetches, proven appends fetch only new UIDs, and deletions reconcile UID membership without refetching unchanged headers. A busy-mailbox snapshot is returned but remains reconcile-required until a stable snapshot is observed.

The schema-v3 cache contains account mutation state, mailbox snapshot state,
UID membership, sender address/name, date, Message-ID, normalized List-Id and
display name, plus booleans for list-header and advertised-one-click presence.
It does not store List-Unsubscribe URLs, recipient tokens, raw list-action
headers, bodies, subjects, recipients, flags, attachments, passwords,
authentication tokens, keychain secrets, or complete messages. The cache
namespace necessarily includes the configured account name, IMAP
host/port/TLS mode, and login username to prevent identities from different
servers colliding. SQLite uses WAL mode with
`synchronous=NORMAL`. Set
`AGENTMAIL_DISABLE_HEADER_CACHE=1` to disable it or `AGENTMAIL_CACHE_DIR` to
override its root; cache errors fall back to live IMAP.

`list_flags` and `find_attachments` use the same discovery plan. Discovery uses one selectable `\All` mailbox exclusively when available. Enumerated fallback and account-wide destructive tools skip `\All`, `\Drafts`, `\Flagged`, `\Important`, `\Junk`, and `\Trash`, while retaining storage roles including `\Archive`, `\Sent`, `\Memos`, `\Scheduled`, and `\Snoozed`. A caller-provided mailbox bypasses planning and is honored directly. IMAP defines `\NoSelect`, not a separate `\NoScan` attribute; `\NoSelect` is always excluded automatically.

---

### Write / Mutate

| #   | Tool                   | Description                                                                           | Annotations                              |
| --- | ---------------------- | ------------------------------------------------------------------------------------- | ---------------------------------------- |
| 12  | `delete_messages`      | Delete by UID (up to 500). Moves to Trash, or permanently expunges when `permanent=true`. | `destructive`, `idempotent`, `taskable`   |
| 13  | `delete_by_sender`     | Delete all from an exact sender identity (`email` + `name` from a ranking row). Omit `mailbox` for account-wide. `permanent=true` bypasses Trash. | `destructive`, `taskable`                 |
| 14  | `delete_list_id`       | Delete all messages with an **exact** List-Id across all mailboxes. `permanent=true` bypasses Trash. | `destructive`, `taskable`                 |
| 15  | `move_list_id`         | Move all messages with an **exact** List-Id to a destination mailbox in one operation (e.g. archive a statement list). Omit `mailbox` for account-wide; destination excluded. | `taskable`                               |
| 16  | `move_by_sender`       | Move all messages from an exact sender identity (`email` + `name`) to a destination mailbox in one operation. Omit `mailbox` for account-wide; destination excluded. | `taskable`                               |
| 17  | `move_message`         | IMAP MOVE between mailboxes (COPY+EXPUNGE fallback when MOVE unsupported)             |                                          |
| 18  | `create_mailbox`       | Create new folder                                                                     | `idempotent`                             |
| 19  | `create_draft`         | Compose RFC822 to Drafts folder (to/cc/bcc required; creates Drafts mailbox if missing). Supports optional local file attachments. Returns the draft identity when recoverable. |                                          |
| 20  | `download_attachments` | Extract attachments to disk as `{uid}_{index}_{filename}`                             | `taskable`                               |
| 21  | `unsubscribe_message`  | DKIM-verified RFC 8058 POST; optional matching-message cleanup via the nested `cleanup {when, identity, deletion}` object (omitted = unsubscribe only). | `destructive`, `open_world`, `taskable`  |

**`permanent` flag (delete tools):** default false moves to Trash when a Trash mailbox exists, else permanently deletes. When true, flags `\Deleted` + UID EXPUNGE directly, bypassing Trash — irreversible. Permanent delete requires the server to advertise UIDPLUS; on servers without it the call is refused (plain EXPUNGE would purge unrelated `\Deleted` messages).

No delete silently escalates: Trash unavailability is refused up front
(retry with `permanent=true`), and a failed Trash MOVE is reported as failed
UIDs. `unsubscribe_message` cleanup permits escalation only via
`cleanup.deletion = "trashThenPermanent"`.

`move_list_id` and `move_by_sender` share the delete tools' discovery
(server search, cached List-Id projection fast-path, live exact confirm,
UID-Mode full-mailbox sweep on Yahoo/AOL) but MOVE matches to the required
`destination` instead of deleting. The destination must already exist and is
always excluded from account-wide sweeps.

**Gmail:** on Gmail (`X-GM-EXT-1`), in-place `\Deleted`+EXPUNGE only removes a *label* — the message survives in All Mail. agentmail therefore routes every delete, including `permanent`, through `[Gmail]/Trash` (which removes the message from all labels; Gmail purges Trash on its own schedule). Immediate hard-purge isn't available on Gmail.

`delete_messages` accepts at most 500 explicitly supplied UIDs over MCP. `delete_by_sender`, `delete_list_id`, `move_by_sender`, `move_list_id`, and unsubscribe matching have no total match limit; they split server mutations into 500-UID wire batches. A `top_*` ranking `limit` never becomes a deletion limit.

UID-based tools never accept a bare UID as durable identity, and every UID
consumer requires the `mailbox` the identity came from (there is no INBOX
default). The following arguments are required together with
`expectedUidValidity`:

- `delete_messages`: one or more UIDs discovered in one mailbox epoch.
- `move_message`, `download_attachments`, `add_flags`, and `remove_flags`:
  the UID from a current discovery result.
- `unsubscribe_message`: the `sample` from `top_subscriptions`.

`delete_by_sender`, `move_by_sender`, `delete_list_id`, and `move_list_id`
instead take direct identity values (`email` + `name`, or `listId`) from
ranking rows and confirm them live in each mailbox — no sample UID or epoch
guard.

Each tool performs a live mailbox selection and fails before acting when the
observed UIDVALIDITY differs. Refresh discovery instead of retrying a stale
identity.

#### Output Schemas

**delete_messages**
```json
{ "mailbox", "account", "deleted": 5, "failed": 0,
  "trashFallback": bool, "permanent": bool }
```

**delete_by_sender**
```json
{ "mailbox": "INBOX" | "*", "account",
  "sender": "Display Name <email>",
  "found", "deleted", "failed",
  "mailboxes?": [{ "mailbox", "found", "deleted", "failed" }],
  "mailboxesTotal", "mailboxesTruncated",
  "skipped?": [], "skippedTotal", "skippedTruncated", "permanent": bool }
```

Mailbox and skipped breakdowns are capped at 50 rows; their total and
truncation fields preserve audit completeness.

**delete_list_id**
```json
{ "mailbox": "INBOX" | "*", "account",
  "listId": "list-id.example.com",
  "found", "deleted", "failed",
  "mailboxes?": [{ "mailbox", "found", "deleted", "failed" }],
  "mailboxesTotal", "mailboxesTruncated",
  "skipped?": ["mailbox"], "skippedTotal", "skippedTruncated",
  "permanent": bool }
```
`mailboxes` is present when scanning all mailboxes. `skipped` lists planned mailboxes that could not be selected or searched; policy-excluded special-use views are not reported as errors.

**move_list_id**
```json
{ "mailbox": "INBOX" | "*", "account",
  "listId": "list-id.example.com", "destination": "Statements",
  "found", "moved", "failed",
  "mailboxes?": [{ "mailbox", "found", "moved", "failed" }],
  "mailboxesTotal", "mailboxesTruncated",
  "skipped?": ["mailbox"], "skippedTotal", "skippedTruncated" }
```

**move_by_sender**
```json
{ "mailbox": "INBOX" | "*", "account",
  "sender": "Display Name <email>", "destination": "Statements",
  "found", "moved", "failed",
  "mailboxes?": [{ "mailbox", "found", "moved", "failed" }],
  "mailboxesTotal", "mailboxesTruncated",
  "skipped?": [], "skippedTotal", "skippedTruncated" }
```

**move_message**
```json
{ "mailbox", "account", "uidValidity", "uid", "destination" }
```

**create_mailbox**
```json
{ "account", "mailbox", "created": bool, "alreadyExists": bool }
```

**create_draft**
```json
{ "created": true, "account", "draftsMailbox", "attachmentCount",
  "uidValidity?", "uid?", "resourceUri?" }
```

The compact result confirms placement without echoing the subject, recipients,
local input paths, or filenames. `create_draft` composes a complete RFC822
message with Date and Message-ID and appends it to a selectable Drafts mailbox
with the `\Draft` flag. The identity fields are best-effort: async-imap does
not expose UIDPLUS `APPENDUID`, so the server is asked for the generated
Message-ID after APPEND; when that recovery succeeds the response carries the
new draft's nonzero `uid`/`uidValidity` and a UIDVALIDITY-safe `resourceUri`.

**download_attachments**
```json
{ "mailbox", "account", "uidValidity", "uid",
  "downloaded": [{ "index", "filename", "path", "contentType", "size" }] }
```

**unsubscribe_message**

Required action identity and consent:

```json
{
  "mailbox": "INBOX",
  "account": "work",
  "uid": 42,
  "expectedUidValidity": 3857529045,
  "confirmOneClick": true,
  "deleteMatching": false,
  "deleteOnUnsubscribeFailure": false,
  "allowSenderFallback": true,
  "allowPermanentFallback": false,
  "permanent": false
}
```

The first four fields after `mailbox` identify a live-ranked message and record
explicit RFC 8058 consent. The destructive switches (`deleteMatching`,
`deleteOnUnsubscribeFailure`, `allowPermanentFallback`, `permanent`) default
to `false`. `allowSenderFallback` defaults to `true`: it only activates when
`deleteMatching` was already requested and no DKIM-authenticated List-Id
exists, narrowing cleanup to the verified exact sender's bulk mail instead of
refusing outright. List-Id cleanup requires the same passing DKIM signature
to cover the single `List-Id` header. `permanent=true` is an explicit
hard-delete request on standard IMAP; on Gmail it safely moves to Trash because
in-place EXPUNGE only removes a label. `allowPermanentFallback=true` separately
permits escalation only when a Trash-first cleanup cannot use Trash, and is
never applied on Gmail.

```json
{ "mailbox", "account", "uid", "uidValidity", "listId?",
  "dkimVerified": bool, "listIdAuthenticated": bool, "dkimDomain?",
  "unsubscribed": { "success": bool, "httpStatus?", "reason?" },
  "matchingMessages?": {
    "matchedBy": "list-id" | "exact-sender-fallback",
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
| 20  | `add_flags`    | Add flags and/or set Apple Mail `color` (a color-name string; union semantics). Colors: red, orange, yellow, green, blue, purple, gray. | `idempotent` |
| 21  | `remove_flags` | Remove specific flags and/or clear the Apple Mail color with `clearColor: true`. Others preserved. | `idempotent` |

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
| 5   | `unsubscribe-cleanup` | Rank lists, obtain consent, verified unsubscribe  | `account`                    |
| 6   | `list-id-cleanup`     | Identify mailing lists by List-Id, bulk-delete    | `account`                    |

## Task Support (SEP-1686)

10 long-running tools support `execution.taskSupport = "optional"` — clients can invoke them normally (synchronous with progress notifications) or as background tasks (enqueue, poll, retrieve result).

**Taskable tools:** `list_flags`, `find_attachments`, `top_senders`, `top_subscriptions`, `top_mailing_lists`, `delete_messages`, `delete_by_sender`, `delete_list_id`, `move_list_id`, `move_by_sender`, `download_attachments`, `unsubscribe_message`

**Destructive task serialization:** Destructive tasks (`delete_messages`, `delete_by_sender`, `delete_list_id`, `unsubscribe_message`) targeting the same account are serialized — each waits for the previous destructive task to finish before starting. Non-destructive tasks run concurrently without restriction.

**Task lifecycle:** `tasks/list`, `tasks/get`, `tasks/getResult`, `tasks/cancel`

**Cancellation:** `tasks/cancel` first cancels the cooperative task token and then aborts the async future; active SQLite publication checks that token and rolls back. For direct calls, `notifications/cancelled` stops scans at mailbox/fetch-chunk boundaries and interrupts unsubscribe DNS, DKIM, and HTTP waits through a 25 ms cancellation poll. Cancellation during an HTTP send is inherently ambiguous because the endpoint may already have received the POST.

Tasks are retained for 24 hours from creation, including completed, failed, and
cancelled metadata. At most 128 live tasks/reservations are accepted per server
process. `tasks/list` is newest-first in pages of 25 and uses an opaque,
process-local cursor. `tasks/getResult` is repeatable until expiry; retrieving a
result does not evict it. Expired active tasks are cancelled and removed.

## Tool Result Encoding

Every tool returns one short text block for clients that do not consume
structured output and one authoritative `structuredContent` object. The text
block is a summary capped at 8,000 characters, not a second escaped copy of the
JSON payload. All 21 output schemas are root objects with nested definitions
inlined, so they contain no `$defs` or `$ref`.

Potentially long mailbox/skipped breakdowns are capped at 50 rows and include
`*Total` and `*Truncated` fields. Mailing-list sender previews are capped at
five addresses. These caps reduce model-context payloads without discarding
destructive-operation counts or audit state.

## Resources (5 templates)

Single messages are addressable as resources. `resources/list` is intentionally empty — discovery is template-based (`resources/templates/list`), since mailboxes hold thousands of messages.

| URI template                                                              | MIME type             | Content                                      |
| ------------------------------------------------------------------------- | --------------------- | -------------------------------------------- |
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
use the narrower resource or attachment tools. `/source` returns an MCP
resource `blob` whose field value is base64, preserving the original RFC822
bytes without lossy UTF-8 conversion.

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
