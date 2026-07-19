# Yahoo / AOL IMAP gateway quirks (empirically confirmed)

AOL mail runs on Yahoo IMAP infrastructure (`imap.aol.com` routes through
`*.omega.yahoo.com` pods). Three server behaviors were confirmed against a
live account on 2026-07-18 with the probes in `examples/`; all three shape
agentmail's scan and delete design.

Capabilities observed: `IMAP4rev1 IDLE MOVE UIDPLUS UIDONLY X-UIDONLY
SPECIAL-USE NAMESPACE CHILDREN LIST-EXTENDED LIST-STATUS ENABLE UNSELECT
LITERAL+ COMPRESS=DEFLATE ID OBJECTID PARTIAL APPENDLIMIT=41697280
MESSAGELIMIT=1000 XYMHIGHESTMODSEQ X-MSG-EXT`.

## 1. `HEADER.FIELDS` responses filter out `List-Unsubscribe` / `List-Unsubscribe-Post`

A request for `BODY.PEEK[HEADER.FIELDS (List-Unsubscribe
List-Unsubscribe-Post List-Id FROM DATE Message-ID)]` returns
`List-Id`/`From`/`Date`/`Message-ID` but silently omits both
`List-Unsubscribe` headers — even for messages that verifiably carry them
(the same message read via full `BODY.PEEK[HEADER]` shows both, including
`List-Unsubscribe-Post: List-Unsubscribe=One-Click`).

The full six-variant bisect (`examples/probe_header_fields.rs`, run
2026-07-19 against a live message) proves the filtering is unconditional
for every partial-header form: singleton requests, reordered pairs, and
even `HEADER.FIELDS.NOT` (which never named the header for exclusion) all
omit `List-Unsubscribe`, while full `BODY.PEEK[HEADER]` returns it. No
request reshaping helps — only the full block.

Mechanism: FIELDS-family responses are synthesized from the provider's
parsed metadata index, not sliced from the stored message — visible
because `From` comes back MIME-decoded in FIELDS responses but raw
(`=?utf-8?q?...?=`) in the full header. The index does not carry the
tokenized unsubscribe headers (they embed per-recipient addresses and
tokens), which also explains why SEARCH cannot match them (quirk #2).

agentmail response: ranking scans detect the inconsistency (many `List-Id`
rows, zero unsubscribe headers anywhere) and refetch the mailbox with full
headers; the account is remembered as quirky for the process lifetime
(`header_cache.rs` quirk detection, `imap_client::header_fields_quirk`),
with the persisted projection as the restart-proof signal for deletion
flows. Full-header fetching is the only viable path on these servers.

## 2. `SEARCH` cannot match `List-*` headers

`UID SEARCH HEADER List-Id "<value>"` returns nothing on mailboxes full of
matching messages, while `FROM`/`TO`/`SUBJECT` searches work. Consistent
with quirk #1: the backend's header index excludes List-* fields. A tagged
`NO` here is additionally invisible to async-imap (it swallows tagged
NO/BAD on SEARCH/FETCH), so a rejection and an empty result look identical.

agentmail response: on quirk-flagged accounts, List-Id deletion flows fall
back from server search to enumerating the visible mailbox and confirming
the exact List-Id locally (`Agentmail::exact_list_id_uids`).

## 3. The visible mailbox is an absolute ~10,000-message window

`EXAMINE`/`STATUS` report `EXISTS`/`MESSAGES` = 10,000 for a much deeper
INBOX. The window covers the newest messages; deleting from it makes older
messages backfill into view. Probed on 2026-07-18
(`examples/probe_uid_window.rs`):

```text
EXAMINE "INBOX": EXISTS=10000 UIDNEXT=Some(434865)
UID SEARCH ALL: 10000 UIDs, visible span 419232..434864
UID SEARCH UID 399232:419231 (below window): 0 hits
UID FETCH 414232:414332 (UID) (below window): 0 hits
STATUS: MESSAGES=10000 (also windowed)
```

The window is absolute per session: neither `UID SEARCH` nor direct
`UID FETCH` reaches below it, and `STATUS` reports the windowed count.
Only the provider's webmail shows the true mailbox size.

**The bulk-export endpoint widens the window 10×.** `export.imap.aol.com`
serves the same mailbox (identical `UIDVALIDITY`, continuous `UIDNEXT`)
with `EXISTS = 100,000` instead of 10,000 — and advertises a
**byte-identical capability list** (all 22, including `MESSAGELIMIT=1000`;
probed 2026-07-19 with `examples/probe_capabilities.rs`). The window size
is therefore per-endpoint serving policy, invisible to CAPABILITY, and
discoverable only by measurement. The 100k window is still absolute
(below-window `UID SEARCH`/`UID FETCH` return nothing). Prefer the export
endpoint for bulk cleanup: ~10× fewer drain passes, same protocol
behavior, same quirks.

agentmail response: scans can only rank what is visible (documented
limitation); account-wide deletes repeat select→search→delete passes until
a pass finds nothing (`MAX_WINDOW_DRAIN_PASSES` drain loops), which walks
the entire deep mailbox through the window. `MESSAGELIMIT=1000` (RFC 9738)
is additionally respected by keeping every FETCH chunk and windowed-search
span at ≤1,000 messages, with `UID SEARCH ALL` results cross-checked
against `EXISTS` (`imap_client::search_all_uids_checked`).
