# Patch: surface tagged NO/BAD on SEARCH and FETCH (async-imap 0.11.3)

## The bug

`async-imap`'s `parse_status` (SELECT/EXAMINE/STATUS) inspects the tagged
`Response::Done` and returns `Err(Error::No | Error::Bad)` on a server
rejection. But `parse_ids` (backs `uid_search`) and `parse_fetches` (backs
`uid_fetch`) drive the stream with `take_while(filter)`, and `filter_sync`
returns `false` for the command's own tagged `Done` **without reading its
status**. The stream simply ends, so a tagged `NO`/`BAD` after a SEARCH or
FETCH becomes an empty (or partial) `Ok` instead of an error.

Impact on agentmail: a server that rejects an over-limit `UID SEARCH ALL`
(RFC 9738 `MESSAGELIMIT`, e.g. Yahoo/AOL) returns an empty membership that a
scan would publish as "mailbox is empty"; a rejected FETCH chunk returns
fewer rows than requested and silently shrinks the projection.

## agentmail's defense today (works without this patch)

- SEARCH: `imap_client::search_all_uids_checked` cross-checks the result
  count against `EXISTS` and rediscovers membership in bounded `UID lo:hi`
  windows on mismatch — this also recovers the *correct* UID set on a
  MESSAGELIMIT server, which the upstream fix alone would not.
- FETCH: `header_cache::reconcile_fetch_chunk` treats a wholly-empty
  non-empty chunk as a swallowed rejection and errors (retried by the scan
  resume loop) rather than pruning the whole chunk from membership.

This patch removes the root cause so those guards become belt-and-suspenders
rather than load-bearing.

## The fix

Replace the `take_while(filter)` drain in `parse_ids` and `parse_fetches`
with an explicit tagged-`Done` match that mirrors `parse_status`: on
`Status::No`/`Status::Bad` for the command tag, return `Err`; on `Ok`, break.

### `src/parse.rs` — `parse_ids`

```rust
pub(crate) async fn parse_ids<T: Stream<Item = io::Result<ResponseData>> + Unpin>(
    stream: &mut T,
    unsolicited: channel::Sender<UnsolicitedResponse>,
    command_tag: RequestId,
) -> Result<HashSet<u32>> {
    let mut ids: HashSet<u32> = HashSet::new();

    while let Some(resp) = stream.try_next().await? {
        match resp.parsed() {
            Response::Done { tag, status, code, information, .. } if tag == &command_tag => {
                use imap_proto::Status;
                match status {
                    Status::Ok => break,
                    Status::Bad => return Err(Error::Bad(format!("code: {code:?}, info: {information:?}"))),
                    Status::No => return Err(Error::No(format!("code: {code:?}, info: {information:?}"))),
                    _ => return Err(Error::Io(io::Error::other(format!(
                        "status: {status:?}, code: {code:?}, information: {information:?}"
                    )))),
                }
            }
            Response::MailboxData(MailboxDatum::Search(cs)) => {
                for c in cs {
                    ids.insert(*c);
                }
            }
            _ => handle_unilateral(resp, unsolicited.clone()),
        }
    }

    Ok(ids)
}
```

### `src/parse.rs` — `parse_fetches`

Convert from the `take_while` + `filter_map` combinator to a `try_unfold`
(or hand-rolled stream) that yields `Err(Error::No/Bad)` when the tagged
`Done` reports a rejection, and stops on `Ok`. Same status match as above.

## How to apply in this repo

Prefer a one-commit fork over vendoring the whole crate:

```toml
# Cargo.toml
[patch.crates-io]
async-imap = { git = "https://github.com/<org>/async-imap", branch = "fix-tagged-status" }
```

1. Fork `async-imap`, apply the two edits above, add a regression test that
   feeds a scripted `<tag> NO [LIMIT] ...` after `* SEARCH` and asserts
   `Err(Error::No(_))`.
2. Point the `[patch.crates-io]` stanza at the fork branch.
3. Open the same change upstream; drop the patch once released.

Keep `search_all_uids_checked` and `reconcile_fetch_chunk` regardless — they
handle the MESSAGELIMIT *windowing* the library cannot know about.
