# Patch: imap-proto UIDFETCH response parsing (RFC 9586)

Status: applied as a local vendored fork of **imap-proto 0.17.0** at
`vendor/imap-proto`, wired via `[patch.crates-io]` in the root `Cargo.toml`.
Upstream when possible to drop the fork.

History: first applied to 0.16.7 (async-imap 0.11). Rebased onto pristine
0.17.0 for async-imap 0.12 — 0.17 moved to nom 8 (combinators are `Parser`s
driven by `.parse(i)`, and `tuple((…))` is a bare tuple) and flattened the
source tree (`src/parser/rfc3501/` → `src/rfc3501/`). 0.17.0 still has no
UIDFETCH parser, so the patch is the same function in the new idiom. The
vendored tree is the published crate's `src/`, `examples/`, manifest, lockfile
and licences; that function and its `alt` entry are the only edits.

## Why

Yahoo/AOL (and any RFC 9586 server) expose the **entire** mailbox only in UID
Mode, entered with `ENABLE UIDONLY`. In UID Mode the server replies to
fetches with `* <uid> UIDFETCH (…)` instead of `* <seq> FETCH (…)`. Stock
imap-proto (0.16.7 and 0.17.0 alike) has no `UIDFETCH` parser, so
async-imap's `parse_fetches` would fail on every response — which is why
agentmail is stuck in Limited Mode behind the visible-window limit
(10k/100k), working around it with windowed search + delete-drain loops.

This patch unblocks UID Mode. `ENABLE` support already exists
(`imap_client::enable`); the remaining work is wiring `ENABLE UIDONLY` +
`PARTIAL`-based iteration into the account scan (a separate, behavior-level
change that needs real-server probe validation before shipping).

## The change

`vendor/imap-proto/src/rfc3501/mod.rs`, mirroring 0.17's `message_data_fetch`
(marked `[agentmail local patch — see docs/patches/]` in the source):

```rust
// UIDFETCH (RFC 9586): identical to FETCH, but the leading number is the UID
// and the UID data item may be omitted. Surface as Response::Fetch with a
// synthesized Uid attribute so existing consumers and `.uid()` work unchanged.
fn message_data_uidfetch(i: &[u8]) -> IResult<&[u8], Response<'_>> {
    map(
        (number, tag_no_case(" UIDFETCH "), msg_att_list),
        |(uid, _, mut attrs)| {
            if !attrs.iter().any(|attr| matches!(attr, AttributeValue::Uid(_))) {
                attrs.insert(0, AttributeValue::Uid(uid));
            }
            Response::Fetch(uid, attrs)
        },
    )
    .parse(i)
}
```

Wired into the `response_data` alt right after `message_data_fetch`.

Mapping to `Response::Fetch` (not a new variant) is deliberate: every FETCH
consumer — including async-imap's `parse_fetches` — works with zero further
changes, and synthesizing the `Uid` attribute makes `Fetch::uid()` resolve
even when the server omits it (which UID Mode permits).

## Test

`imap_client::tests::uidfetch_response_is_parsed_with_synthesized_uid` scripts
a server replying `* 42 UIDFETCH (RFC822.SIZE 5)` (UID omitted) and asserts
the fetched item's UID is `Some(42)` — proving the leading number is surfaced
through async-imap unchanged. Two parser-level tests pin the shapes AOL
actually sends, through `imap_proto::Response::parse` (0.17's entry point;
0.16's was `parser::parse_response`):
`uidonly_uidfetch_line_parses_via_the_patched_imap_proto` (`UIDFETCH (UID n)`)
and `uidfetch_with_a_body_section_literal_parses_via_the_patched_imap_proto`
(a `BODY[HEADER.FIELDS …]` literal inside a UIDFETCH).

## Re-vendoring

To move to a new imap-proto release: copy the published crate (not a git
checkout — the `.cargo_vcs_info.json` records which commit it was built from)
over `vendor/imap-proto`, re-apply `message_data_uidfetch` plus its `alt` entry
in `response_data` right after `message_data_fetch`, and run the three tests
above. `examples/probe_uidonly.rs` against a real Yahoo/AOL account is the
end-to-end check.

## Upstreaming

The change is small and general (any UIDONLY server benefits). Open a PR
adding `message_data_uidfetch` to imap-proto; once released, drop
`vendor/imap-proto` and the `[patch.crates-io]` stanza.
