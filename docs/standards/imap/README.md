# IMAP standards reference

This directory is a curated working set of IMAP RFCs used while designing and
reviewing AgentMail. It is not a complete or authoritative mirror of IMAP
standards, and the local conversions may not include later errata.

Use the [RFC Editor](https://www.rfc-editor.org/) for canonical RFC text and the
[IANA IMAP Capabilities Registry](https://www.iana.org/assignments/imap-capabilities/imap-capabilities.xhtml)
to discover currently registered extensions.

## Included documents

| RFC | Subject | Relevance to AgentMail |
| --- | --- | --- |
| [3501](rfc-3501-imap4rev1.md) | IMAP4rev1 | Implemented compatibility baseline and UID identity model; obsoleted by RFC 9051 |
| [4466](rfc-4466-imap4-abnf-extensions.md) | Collected IMAP ABNF extensions | Parser and extension syntax; folded into rev2 |
| [4469](rfc4469InternetMessageAccessProtocolImapCatenateExtension.md) | CATENATE | Possible future server-side composition/append optimization |
| [4731](rfc4731Imap4ExtensionToSearchCommandForControllingWhatKindOfInformationIsReturned.md) | ESEARCH | Future compact search-result optimization |
| [5032](rfc-5032-within-search.md) | WITHIN search | Possible server-side date-window optimization |
| [5161](rfc5161TheImapEnableExtension.md) | ENABLE | Future prerequisite for extensions that require explicit enabling, including QRESYNC |
| [5182](rfc-5182-searchres.md) | SEARCHRES | Possible repeated-search optimization; folded into rev2 |
| [5550](rfc5550TheInternetEmailToSupportDiverseServiceEnvironmentsLemonadeProfile.md) | LEMONADE profile | Historical mobile-efficiency design context; not implemented |
| [6154](rfc6154ImapListExtensionForSpecialUseMailboxes.md) | SPECIAL-USE | Implemented mailbox-role parsing and scan planning |
| [7162](rfc-7162-condstore-qresync.md) | CONDSTORE and QRESYNC | Design reference; neither CONDSTORE deltas nor QRESYNC are used by the cache |
| [7377](rfc-7377-multimailbox-search.md) | Multimailbox search | Possible replacement for sequential account-wide scans |
| [8474](rfc-8474-objectid.md) | OBJECTID | Future mailbox rename/move identity optimization; not currently used |
| [9051](rfc-9051-imap4rev2.md) | IMAP4rev2 | Confirms UID identity rules; pure rev2 is not yet a supported client profile |
| [9755](rfc-9755-imap-utf8.md) | IMAP support for UTF-8 | Current rev1 UTF-8 extension; rev2 includes equivalent behavior |

## AgentMail synchronization invariants

- Cached message identity is `(mailbox, UIDVALIDITY, UID)`.
- Missing `UIDVALIDITY` forces a live uncached scan. A changed value starts a
  new epoch and prevents reuse of old UIDs or headers.
- An unchanged `UIDVALIDITY`, `UIDNEXT`, and message-count tuple permits a hit
  for the immutable ranking projection.
- A UIDNEXT delta alone never proves a pure append because UID allocation can
  contain gaps. AgentMail searches the actual UID tail and requires a stable
  before/after mailbox snapshot plus the expected resulting count.
- Deletions and mixed changes run `UID SEARCH ALL`, prune vanished membership,
  and fetch headers only for live UIDs without a same-epoch marker.
- Header chunks commit incrementally, but UID membership publishes atomically.
  An account mutation generation prevents an in-flight scan from overwriting
  newer state.
- If a busy mailbox cannot produce a stable search snapshot, AgentMail can
  return and persist the point-in-time membership, but deliberately omits a
  reusable UIDNEXT marker so the next call reconciles again.
- Flags are not cached. HIGHESTMODSEQ changes therefore do not invalidate the
  immutable projection; QRESYNC and OBJECTID remain future optimizations, not
  correctness requirements.
- A delayed UID action must carry the observed UIDVALIDITY as well as mailbox
  and UID. `delete_messages`, `delete_by_sender`, `move_message`,
  `download_attachments`, `add_flags`, `remove_flags`, and
  `unsubscribe_message` require it as `expectedUidValidity`. Each performs a
  live mailbox selection and rejects a missing or changed epoch before using
  the UID.
- MCP body, exact-header, and raw-source resources encode the same complete
  identity as
  `email://{account}/{mailbox}/{uidValidity}/{uid}[/headers|/source]`. A stale
  epoch is resource-not-found, never permission to read a potentially recycled
  UID. The raw `/source` representation is returned through the MCP base64
  `blob` field so non-UTF-8 RFC822 octets remain lossless.

## Persistent projection boundary

The header cache is a disposable SQLite projection at schema version 3. Its
tables separate account mutation revisions, mailbox snapshot state, current UID
membership, and immutable header-derived ranking rows. Header rows contain only
sender address/name, date, Message-ID, normalized List-Id/display name, and
booleans recording list-header and advertised-one-click presence.

The projection does not retain List-Unsubscribe URLs, raw list-action headers,
recipient tokens, subjects, recipients, flags, attachments, passwords,
authentication tokens, keychain secrets, bodies, or complete messages. Its
account namespace does include the configured account name, IMAP host/port/TLS
mode, and login username to prevent data from different server identities from
colliding. One-click DKIM verification may fetch one bounded complete message
transiently, but it is never written to this database.

SQLite runs with `journal_mode=WAL`, `synchronous=NORMAL`, and foreign keys
enabled. A version mismatch rebuilds the projection rather than attempting to
preserve derived data. Migration enables secure deletion, runs `VACUUM`, and
truncates the WAL so pre-v3 token-bearing columns are not left in free pages or
the journal. Unix cache directories and database files are restricted to
`0700` and `0600` respectively.

## Scan-planning invariants

- `\NoSelect` is the IMAP attribute that forbids selection. IMAP has no
  separate standard `\NoScan` attribute, and AgentMail currently has no custom
  per-mailbox exclusion setting.
- Discovery uses one selectable `\All` mailbox exclusively when the server
  declares one. It never scans `\All` and other mailboxes together.
- Enumerated discovery and mutation plans skip `\All`, `\Drafts`, `\Flagged`,
  `\Important`, `\Junk`, and `\Trash`. They retain storage roles such as
  `\Archive`, `\Sent`, `\Memos`, `\Scheduled`, and `\Snoozed`.
- Exact-name fallback is used only when the server provides no recognized
  role. An explicitly supplied mailbox bypasses automatic planning.
- The public MCP `list_mailboxes` projection exposes selectable mailboxes only
  and paginates them before issuing per-page `STATUS` commands. The internal
  full layout remains available for hierarchy, completion, special-use
  resolution, and scan planning.

## Current IMAP4rev2 boundary

AgentMail gates the rev1-only `RECENT` STATUS item and the cache itself uses
commands shared by rev1 and rev2. However, the client stack and integration
matrix have not yet validated a pure `IMAP4rev2` server end to end, including
rev2-only capability/UTF-8 negotiation and response grammar. Dual-profile
servers are used through their `IMAP4rev1` capability; pure rev2 remains
unsupported rather than being advertised on partial evidence.

## High-priority additions

These specifications are directly relevant to current behavior or to closing
the rev2 gap and should be added when that area is implemented or reviewed:

- RFC 4315 — UIDPLUS
- RFC 5530 — IMAP response codes
- RFC 5819 — LIST-STATUS
- RFC 6851 — MOVE
- RFC 7888 — non-synchronizing literals
- RFC 8314 — TLS for email access
- RFC 8457 — `$Important` and `\Important`
- RFC 9979 — `\Memos`, `\Scheduled`, and `\Snoozed`

## Maintenance policy

- Prefer short filenames in the form `rfc-<number>-<topic>.md`.
- Record implementation status here rather than implying that inclusion means
  complete protocol support.
- Add an RFC when AgentMail implements, validates, or makes a design decision
  based on it; do not attempt to mirror every registered IMAP extension.
- Check the canonical RFC and errata before using a local copy as a normative
  implementation reference.
