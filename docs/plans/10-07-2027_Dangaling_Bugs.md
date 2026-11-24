# HeroMail bug plan

Reviewed on 2026-11-24 against the HeroMail 1.0.0 working tree. The original
finding numbers are kept for reference.

## Outstanding

- **#3 · Yahoo/AOL login retries · Open.** An `AUTHENTICATIONFAILED` reply
  can still lead to three total login attempts. Explicit login rate-limit
  replies already bypass retries. Confirm which authentication failures on
  Yahoo/AOL warrant retrying, then apply and test that policy.
  [Login retry logic](../../src/imap_client.rs#L718)
- **#4 · Counts after cancellation · Partial.** Task cancellation now
  stops cooperatively at cancellation checks. A cancelled bulk mutation
  can still leave callers without final counts for work that completed
  before it stopped. Preserve and surface those counts through the task
  contract.
  [Task result handling](../../src/mcp/tasks.rs#L442) ·
  [Bulk mutation sweep](../../src/lib.rs#L2483)
- **#6 · UIDONLY on reused sessions · Partial.** Connections can enable
  UIDONLY at login, and sessions already in UID mode skip the command.
  The lazy path can still issue `ENABLE UIDONLY` on a reused session after
  it has opened a mailbox. Ensure UIDONLY is enabled before the first
  `SELECT` or `EXAMINE` and verify that the fallback works with a strict
  server.
  [Lazy enable path](../../src/lib.rs#L1137) ·
  [RFC 5161](https://datatracker.ietf.org/doc/html/rfc5161#section-3.1)
- **#10 · File access · Partial.** Missing configured workspaces are
  now reported as unavailable (#10a), and path errors list only live
  directory grants (#10c). Keeping operations confined through the point
  of use remains open (#10b).
  [Root handling](../../src/mcp/file_access.rs#L100) ·
  [Path validation](../../src/mcp/file_access.rs#L166)
- **#11 · Resource reads and completion · Partial.** Oversized resources
  now return errors with the limit and a usable alternative (#11a).
  Inline attachments allow 10 MiB of decoded data. The attachment inventory
  and download paths accept encoded messages up to 70 MiB.
  Fetching individual MIME parts with the attachment numbers published by
  `/info` (#11b) and reducing live `STATUS` calls for UIDVALIDITY
  completion (#11c) remain open.
  [Size error mapping](../../src/mcp/resources.rs#L563) ·
  [Attachment read](../../src/mcp/resources.rs#L806) ·
  [UIDVALIDITY completion](../../src/mcp/resources.rs#L936)
- **#15 · Tagged IMAP refusals · Partial.** Workarounds protect selected
  paths, including move reconciliation. The workspace still pins
  `async-imap` 0.12.0. Upstream commit `5aea27a` makes reported `NO` and
  `BAD` errors carry a structured `StatusResponse`. The current upstream
  SEARCH and FETCH parsers still discard the tagged completion without
  checking its status. Open PR #146 covers SEARCH and several other
  commands. FETCH still needs coverage. Adopt a corrected dependency
  and verify that both SEARCH and FETCH surface command refusals.
  [Pinned dependency](../../Cargo.lock#L213) ·
  [Structured status commit](https://github.com/chatmail/async-imap/commit/5aea27adaaff113c27e1646be2caf1adaa5ef365) ·
  [Upstream parsers](https://github.com/chatmail/async-imap/blob/main/src/parse.rs) ·
  [Upstream PR #146](https://github.com/chatmail/async-imap/pull/146)
## Deferred from 1.0.0

- **#16 · Release history.** Reconstruct the 0.5 and 0.6
  changelog sections from Git history. Both version ranges lack local
  release tags, and the changelog goes directly from Unreleased to 0.4.0.
  The earlier review recorded the parent workspace's architecture link as
  fixed. The parent document is outside this checkout.
  [Changelog](../../CHANGELOG.md#L416)

## Completed findings

| # | Finding | Resolution |
| --- | --- | --- |
| 1 | Lost connections during account-wide scans | A checked NOOP verifies liveness. A lost connection stops the scan and reports uncovered mailboxes in `skipped` where supported, including `list_flags` and `find_attachments`. Thread preview fails when the connection is lost. |
| 2 | Stuck or falsely completed move reconciliation | Journal decisions require a live IMAP response. Reconciliation can re-examine `needsAttention` or dismiss a named move with `dismiss` or CLI `--dismiss`. Results expose `dismissed` and `errors`. The destination copy is checked immediately before removing the source. |
| 5 | Unsolicited FETCH responses creating bogus rows | Ranking and message-fetch paths match responses to requested UIDs. Unsolicited flag updates stay out of the results. |
| 7 | Keepalive racing with account acquisition | Keepalive takes an account permit before pinging. |
| 8 | Blocking keyring reads | Keyring calls have a deadline and return distinct keyring errors. |
| 9 | UID-mode walk after a repeated range | Each page must advance to a lower upper UID bound. A repeated range returns an error. The original proposal also listed a separate iteration cap. The decreasing bound is the guard in use today. |
| 12 | Tool router rebuilt per request | A shared router is built once. Each server patches the account list into its own schema copies. |
| 13 | Partial attachment downloads | Targets are checked before writing. If a write fails, cleanup removes files written by the call and reports any cleanup failures. |
| 14 | Inconsistent `AGENTMAIL_CACHE_DIR` layout | The header cache and move journal use the same directory convention. |
| 17 | Post-mutation connection failures masking the result | A checked NOOP determines whether to reuse the connection. Confirmed mutation results stand. Flag read-back also rejects unrelated FETCH rows and detects a lost connection. |
| 18 | Partial LIST responses entering the mailbox catalog | The LIST path checks the connection before returning a catalog for caching. |
| 19 | Closed streams appearing as empty single-mailbox reads | The read wrapper checks liveness and can retry on a fresh session. UID reads check liveness before treating a missing response as a missing message. |

## Fixed alongside these findings

- Canonical attachment filenames are capped at 240 bytes in both the
  download output and the `/info` resource.
- Draft recipient fields document `Name <address>`. Mail Accounts settings
  offer From addresses found in Sent as aliases.
