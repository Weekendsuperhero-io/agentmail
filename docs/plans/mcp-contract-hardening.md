# MCP Contract and Cache Hardening Plan

Status: implemented for AgentMail 0.3.0

## Objective

Make AgentMail safe and useful against very large live mailboxes without persisting email bodies or opaque unsubscribe tokens. The durable cache remains a disposable projection of mailbox identity and selected headers. Every reusable IMAP UID is paired with the mailbox's UIDVALIDITY epoch, discovery responses stay compact, and larger content is read explicitly through bounded MCP resources.

## Compatibility policy

This is an intentional 0.3.0 breaking contract change. Unsafe UID-only selectors and resource URIs are removed instead of supported as a transitional fallback. Callers must rediscover a message after UIDVALIDITY changes.

## Message identity and stale-action safety

The reusable message selector is:

```json
{
  "mailbox": "INBOX",
  "uidValidity": 123456,
  "uid": 42
}
```

Both numeric identity fields must be non-zero, and `mailbox` must be non-empty. Single-mailbox discovery responses may place `uidValidity` on the response wrapper and keep only `uid` on each row.

The following delayed UID consumers require `expectedUidValidity`: delete messages, delete by sender, move, download attachments, add flags, remove flags, direct UID reads, and source reads. The implementation validates the supplied epoch immediately after SELECT or EXAMINE and before UID commands, cache fences, filesystem writes, or other side effects. A missing server UIDVALIDITY or any mismatch fails closed.

MCP tool mismatches return invalid parameters with refresh guidance. Resource mismatches return resource-not-found. The safe resource forms are:

```text
email://{account}/{mailbox}/{uidValidity}/{uid}
email://{account}/{mailbox}/{uidValidity}/{uid}/headers
email://{account}/{mailbox}/{uidValidity}/{uid}/source
```

## Discovery and ranking contracts

`get_messages` and `search_messages` return compact metadata by default. Rows contain the UID, subject, normalized sender fields, date, flags, size when available, and a safe resource URI. Bodies and exact headers are explicit resource reads.

Account-wide attachment results retain mailbox, UIDVALIDITY, and UID for every hit and sort deterministically by date. Ranking rows from `top_senders`, `top_subscriptions`, and `top_mailing_lists` include a newest safe sample identity so the documented discovery-to-action workflows are complete.

Ranking uses live offset pagination. The default is 10 rows, the maximum is 100, and responses include the current offset plus `nextOffset` when another page exists. Pages may shift when live mail changes; cursors do not promise a snapshot.

## Cache projection and SQL ranking

SQLite remains a disposable local projection and continues to use WAL mode. Its identity key is account, mailbox, UIDVALIDITY, and UID. A UIDVALIDITY change discards the old mailbox epoch before publishing replacement rows. Stable mailboxes fetch only the new UID tail and reconcile deletions without downloading all headers again.

The projection stores only fields needed for ranking and discovery. It stores normalized List-Id data plus booleans such as `hasListHeaders` and `advertisedOneClick`; it never stores raw `List-Unsubscribe` or `List-Post` values. The schema and projection version are bumped so an older token-bearing disposable database is rebuilt.

Warm ranking synchronizes and validates mailbox snapshots, then performs aggregation, deterministic ordering, and pagination in SQL. It does not load every cached row into a Rust vector. Message-ID deduplication is account-wide; absent Message-IDs remain distinct by mailbox and UID. The in-memory ranking path is only a cache-disabled or cache-error fallback and must produce the same ordering and totals.

## Mailbox policy

The public MCP mailbox list requires an account, is paginated with a default of 100 and maximum of 500, and exposes selectable mailboxes only. Filtering happens before STATUS calls. IMAP4rev2 `\\NonExistent` is treated as unselectable because it implies `\\Noselect`.

The internal mailbox catalog remains complete so hierarchy and scan planning continue to work. Selectable aggregate views such as `\\All`, `\\Flagged`, and `\\Important` may be listed, but the mutation planner continues to exclude unsafe aggregate targets. Drafts and Trash resolution ignores unselectable mailboxes. A selectable child under an unselectable parent remains discoverable.

## MCP payload policy

MCP handlers use dedicated wire DTOs and explicit output schemas. Each result has one machine-readable `structuredContent` object and a short human-readable text content block; the full JSON value is not duplicated as text.

Ordinary results omit opaque unsubscribe URLs and raw list headers, IMAP host and username, repeated account/mailbox fields, redundant sender composites, echoed draft inputs, and duplicated operation-state fields. Destructive audit data remains: found, deleted, failed, skipped, fallback/permanent disposition, DKIM state, and cleanup completeness.

The principal bounds are:

| Result | Default | Maximum |
| --- | ---: | ---: |
| Get/search metadata rows | 25 | 50 |
| Attachment hits | 25 | 100 |
| Ranking rows | 10 | 100 |
| MCP mailbox rows | 100 | 500 |
| Mailing-list sender preview | 5 | 5 |
| Mailbox/skipped breakdown | 50 | 50 |

Fallback text is capped at 8,000 characters. Markdown bodies are capped at 100,000 characters, exact header resources at 64 KiB, and raw-source resources at 256 KiB. A transient full-message fetch used for bounded extraction may not exceed 64 MiB.

## Task lifecycle

Task state is process-local and is not persisted. Tasks have a creation-based 24-hour TTL, a capacity of 128 unexpired tasks, repeatable result reads until expiry, and a newest-first list page size of 25 with opaque cursors. Every task operation prunes expired entries; an expired running task is cancelled. New work is rejected when all 128 retained entries are still live.

## Implementation sequence

1. Add shared UIDVALIDITY validation and safe identity/resource types.
2. Update discovery, attachment, mutation, CLI, and mailbox behavior.
3. Migrate the cache projection and implement SQL-backed ranking pagination.
4. Replace RMCP JSON wrappers with compact explicit-schema results and bounded wire DTOs.
5. Add resource limits and task retention.
6. Update prompts, public documentation, changelog, and contract examples.
7. Run formatting, unit, integration, property, and lint checks.

## Acceptance tests

The implementation is complete when tests prove:

- Missing or stale UIDVALIDITY stops before every UID command and side effect.
- Discovery schemas expose complete reusable identities and remain free of `$ref` and `$defs`.
- Attachment UIDs cannot collide across mailboxes and account-wide ordering is deterministic.
- Warm rankings issue no full header refetch and do not materialize every cache row in Rust.
- SQL and fallback ranking results have parity, including Message-ID deduplication and tie ordering.
- Cache migration removes the former token-bearing projection and WAL remains enabled.
- `\\Noselect` and `\\NonExistent` mailboxes are excluded without hiding selectable children.
- Ordinary tool schemas and runtime results contain no raw unsubscribe tokens, host, or username.
- MCP text content is a compact summary rather than a serialized copy of structured content.
- Every default, maximum, pagination boundary, resource byte limit, task TTL, and task capacity is enforced.
- The ignored 216,000-message stress case can be run against the SQLite ranking path before large-mailbox deployment.
