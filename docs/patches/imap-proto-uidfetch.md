# Patch: imap-proto UIDFETCH response parsing (RFC 9586)

Status: applied as a local vendored fork at `vendor/imap-proto`, wired via
`[patch.crates-io]` in the root `Cargo.toml`. Upstream when possible to drop
the fork.

## Why

Yahoo/AOL (and any RFC 9586 server) expose the **entire** mailbox only in UID
Mode, entered with `ENABLE UIDONLY`. In UID Mode the server replies to
fetches with `* <uid> UIDFETCH (…)` instead of `* <seq> FETCH (…)`. Stock
imap-proto 0.16.7 has no `UIDFETCH` parser, so async-imap's `parse_fetches`
would fail on every response — which is why agentmail is stuck in Limited
Mode behind the visible-window limit (10k/100k), working around it with
windowed search + delete-drain loops.

This patch unblocks UID Mode. `ENABLE` support already exists
(`imap_client::enable`); the remaining work is wiring `ENABLE UIDONLY` +
`PARTIAL`-based iteration into the account scan (a separate, behavior-level
change that needs real-server probe validation before shipping).

## The change

`vendor/imap-proto/src/parser/rfc3501/mod.rs`, mirroring `message_data_fetch`:

```rust
// UIDFETCH (RFC 9586): identical to FETCH, but the leading number is the UID
// and the UID data item may be omitted. Surface as Response::Fetch with a
// synthesized Uid attribute so existing consumers and `.uid()` work unchanged.
fn message_data_uidfetch(i: &[u8]) -> IResult<&[u8], Response<'_>> {
    map(
        tuple((number, tag_no_case(" UIDFETCH "), msg_att_list)),
        |(uid, _, mut attrs)| {
            if !attrs.iter().any(|attr| matches!(attr, AttributeValue::Uid(_))) {
                attrs.insert(0, AttributeValue::Uid(uid));
            }
            Response::Fetch(uid, attrs)
        },
    )(i)
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
through async-imap unchanged.

## Upstreaming

The change is small and general (any UIDONLY server benefits). Open a PR
adding `message_data_uidfetch` to imap-proto; once released, drop
`vendor/imap-proto` and the `[patch.crates-io]` stanza.
