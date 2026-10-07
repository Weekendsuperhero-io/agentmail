# Agentmail Decisions

Architectural decisions, deferred work, and rationale for future reference.

---

## 0.7.0 — A Draft Is From One Of The Account's Own Identities

### Decision

A draft's `From` is RESOLVED, never echoed. `draft::resolve_from` returns one of
`AccountConfig::identities()` — the primary (`email`, or an email-shaped login
when no `email` is set), then the aliases — under the configured
`display_name`, so `display_name = "Mark Blake"` makes drafts
`From: Mark Blake <you@example.com>`. `create_draft` and `update_draft` take an
optional `from` (`Name <address>` or an address), and both results report the
`from` actually written. `list_accounts` lists each account's identities, and
`list_identities` adds what its Sent mail actually used.

Before this, `From` was always the bare, lowercased `canonical_email()` — or the
LOGIN NAME when there was none, which failed inside lettre as "Invalid email
address 'johnappleseed'". No tool could choose a sender, the config had no
name, and `update_draft` overwrote whatever `From` another client had set.

### Why `from` is refused outside `identities()`

An agent can be asked — or prompt-injected — to write a draft "from" anything.
A `From` the account does not own is at best rewritten or rejected by the
provider at send time, and at worst accepted by a permissive server as a spoof
the user never meant. The account's configuration is the only authority on
what the user sends as, so `from` must match it. The refusal lists the
identities and names the fix (add the address as an alias), which turns a dead
end into one settings change.

A distinct email-shaped login is NOT an identity. A login is a credential, not
an address the user chose to publish (MCP.md has always kept logins out of
`list_accounts`); it stays in `canonical_addresses()`, which only recognizes the
account's own mail. Listing it in `aliases` makes it sendable, so normalization
no longer drops an alias equal to the login.

### Why a reply uses the identity the original was addressed to

Mail sent to `sales@` and answered from `mark@` leaks the personal address and
reads as a different correspondent; every mainstream client answers from the
address the message reached. The order is the original's own `From` when it is
ours (a follow-up to our own mail stays on the address it went out from), then
the first identity in its `To`, then its `Cc`. Mail that reached us by Bcc names
no identity, so it falls back to the primary.

### Why `update_draft` preserves `From`

`update_draft` replaced `From` with the primary on every edit, so a draft the
user began in Mail.app from an alias silently changed sender when an agent
fixed a typo. Without `from`, the replacement now keeps the current `From` —
name included, the display name filling a blank one — when that address is
one of ours. A foreign `From` is replaced by the default, since keeping it would
let an old draft carry a sender the account no longer owns.

### Why the Sent scan, not provider APIs

IMAP has no identity listing. Gmail's `sendAs` settings and Fastmail's JMAP
`Identity` both need OAuth or API tokens, and the app-password accounts Agent
Muse configures hold neither. What an account has actually sent is readable on
every IMAP server: `list_identities` reads the `From` of the newest N Sent
messages with `BODY.PEEK`. That is EVIDENCE, not permission — it never feeds
`from` validation, so a forged or one-off `From` sitting in Sent cannot widen
what a draft may claim. A discovered address must be added as an alias first.

### Why `from` is in the results, and derived

A caller cannot predict the reply rule or a preserved sender, so the result says
what was written. It is formatted from the resolved mailbox, never copied from
the request, and with our own quoting rather than lettre's `Mailbox` `Display`
(which fails on a CR or LF in the name): a name with a comma comes back quoted
and parses as one mailbox.

### Consequence

`AccountConfig` gains `display_name` (validated: no control characters, at most
256 characters; `validate_display_name` is exported so an embedder applies the
same rule). The draft facade (`create_draft_with_headers`, `create_reply_draft`,
`update_draft`) gains `from`, and `MessageInfo.reply_to` becomes `Vec<String>`
so a reply goes to EVERY Reply-To address — a breaking release, 0.7.0. 35 tools
→ 36. Pinned by `the_default_sender_is_the_display_name_on_the_primary_address`,
`an_explicit_sender_must_be_one_of_the_accounts_identities`,
`a_reply_is_from_the_identity_the_original_was_sent_to`,
`an_updated_draft_keeps_a_sender_of_ours_and_replaces_a_foreign_one`,
`identities_are_the_primary_then_aliases_never_a_distinct_login`, and
`draft_tools_take_a_sender_and_refuse_one_the_account_does_not_own`.

## 0.7.0 — Liveness Means A Tagged OK, And Pool Clocks Count Sleep

### Decision

`imap_client::ping` sends `NOOP` by hand and passes only on its own tagged
`OK`; end-of-stream and an untagged `BYE` are a lost connection. The pool's idle
stamps, the `[LIMIT]` login cooldown and the mailbox-catalog TTL are measured on
the WALL clock (`SystemTime`), and a clock set back past a stamp counts as
expired.

### Rationale

async-imap's `noop()` drains with `take_while(filter)`, so a stream that simply
ends is a successful NOOP. On macOS, security-framework maps a reset connection
to a zero-byte read — that same end — so after sleep or a server `BYE` every
pooled session passed its ping, and the next mutation failed on the dead
socket; a draft APPEND then reported an "ambiguous" outcome it did not have.
`async_imaps_own_noop_reports_a_closed_stream_as_alive` pins the upstream
behavior, so the day it changes is visible.

`Instant` on macOS is `CLOCK_UPTIME_RAW`, which stops while the machine sleeps.
A session idled overnight therefore looked seconds old and was handed out
instead of evicted, a cooldown resumed where it paused, and the catalog served a
layout loaded hours earlier. The server's clocks run through our sleep, so ours
must too. A backwards wall clock cannot say how much time passed; treating it
as expired costs at most one reconnect or one LIST, while trusting it could
stretch a cooldown by however far the clock moved.

## 0.7.0 — A Scanned Mailbox Counts Once The Connection Answers After It

### Decision

An account-wide scan or sweep counts a mailbox only after
`imap_client::confirm_alive` — a `NOOP` that only its own tagged `OK` passes —
succeeds following the mailbox's last command. A lost connection ends the scan
at once: that mailbox and every later one go to `skipped`, and the session is
dropped, not pooled. A mailbox that failed on a connection the probe then
proves alive is skipped on its own and the scan goes on. `list_flags` and
`find_attachments` gain `skipped` like the sweeps; a single-mailbox scan, and
`preview_thread_record`, fail instead of answering partially.

### Rationale

async-imap ends SELECT/EXAMINE (`parse_mailbox`), SEARCH (`parse_ids`) and FETCH
quietly at end-of-stream, so a dropped connection is not an error but an empty
mailbox, an empty search, a short fetch. The loops classified failures per
mailbox and moved on, so after a drop every remaining mailbox "drained" with
nothing found — or failed on its own and was skipped — and the sweep returned
`Ok` with `session_usable` still true, pooling the dead session. On a hung
connection each remaining mailbox instead waited out the timeout, 120 s in the
app. Checking errors with `is_connection_error()` alone could not fix it,
because the commonest failure was not an error at all.

A connection that ended can't answer a NOOP afterwards, so one probe per mailbox
proves every answer before it came from a live connection — a property no
per-command check gives without rewriting every command by hand, which is what
the async-imap fork (finding 15) would do upstream. It costs one round trip per
mailbox. It does not catch a tagged `NO` that async-imap swallows on a live
connection; that is still finding 15.

### Consequence

`ListFlagsResponse` and `FindAttachmentsResponse` gain `skipped` (outputs:
`skipped`, `skippedTotal`, `skippedTruncated`). In a sweep, a mailbox whose
connection became unusable mid-mailbox is now listed in `skipped` as well as
in `mailboxes`, since later passes never ran. Pinned by
`a_sweep_stops_at_a_lost_connection_and_reports_what_it_did_not_cover`,
`a_flag_scan_reports_the_mailboxes_a_lost_connection_left_unscanned`,
`a_flag_scan_skips_a_refused_mailbox_and_scans_the_rest`,
`a_single_mailbox_flag_scan_fails_on_a_lost_connection`,
`an_attachment_scan_reports_the_mailboxes_a_lost_connection_left_unscanned`,
and `thread_discovery_fails_when_the_connection_is_lost`.

## 0.7.0 — Move Reconciliation Acts Only On A Live Server's Answer

### Decision

`reconcile_journaled_move` changes the journal only on an answer a live server
gave. Opening a mailbox: a new UIDVALIDITY, `NO [NONEXISTENT]`, or an open
without UIDVALIDITY on a connection a NOOP then proves alive parks the move as
`needsAttention`; anything else — a dropped connection, a timeout, a refusal
such as `[UNAVAILABLE]` — returns the error and leaves the operation exactly as
it was (`reconcile_moves` counts it `pending` and lists it in `errors`).
"Not there" comes from `uid_search_checked`, which needs the search's own
tagged `OK`, and counts only on a session that sees the whole mailbox. A source
is deleted only right after its copy (the `COPYUID`) has been seen in the
destination. `needsAttention` moves are examined again on every reconcile,
resuming from what the journal knows for certain: `Copied` with a COPYUID,
otherwise `CopyInFlight`, whose UIDNEXT rule never copies twice. `dismiss`
(`reconcile_moves` `dismiss: true` + `operationId`, `Agentmail::dismiss_move`)
closes one move for good and touches neither mailbox.

### Rationale

async-imap returns `Ok` from SELECT at end-of-stream and drains SEARCH with
`take_while`, so one dropped connection read as two different facts. During
SELECT it looked like a mailbox without UIDVALIDITY — "source mailbox epoch
changed" — and parked the move as `needsAttention`, which reconcile then
returned unexamined. A pending move holds its source and destination against
rename and delete, and the only remedy offered was to reconcile it: one
connection drop could lock two mailboxes for good. After SELECT it looked like
an empty search — "source gone" — and marked the move complete, releasing the
claim while the source survived beside its copy.

Re-examining is what unsticks the moves the old classification parked, and the
copy check is what makes re-examining safe. A move can wait days, and the very
symptom of a pending move — the message still showing in its old mailbox — is
what invites a person to delete the "duplicate" copy. Reconcile used to delete
the source anyway; now it keeps it and asks. (The tool description already
promised it removes a source "only after it can prove the destination copy".)
Without a COPYUID (`UIDNOTSTICKY`) there is no copy to look for, and the COPY's
`OK` stays the only evidence.

On a Limited Mode session (Yahoo/AOL outside UID Mode) UIDs below the window
search as absent, so absence proves nothing there. Yahoo and AOL advertise
MOVE, so the journal never runs on them today; the guard is for a server that
pairs UIDONLY with no MOVE.

`dismiss` names one operation because closing every pending move at once is
the bulk "forget what we were doing" the journal exists to prevent.

### Consequence

Journal state `dismissed`, pruned with `complete` and `copy_failed`; no schema
change, since older builds read inactive rows only by id. `ReconcileMovesResponse`
gains `dismissed` and `errors`, and `failed` now means only a rejected COPY (an
attempt error used to count there). Pinned by
`a_connection_lost_while_selecting_leaves_the_move_as_it_was`,
`a_connection_lost_after_selecting_does_not_complete_the_move`,
`a_copy_gone_from_the_destination_keeps_the_source`,
`a_move_needing_attention_is_re_examined_and_finished`,
`re_examining_without_a_copyuid_never_copies_twice`,
`a_dismissed_move_no_longer_blocks_its_mailboxes`, and
`async_imaps_own_search_reports_a_closed_stream_as_no_match` (the upstream
behavior, so the day it changes is visible).

---

## 0.5.0 — Two Tool Pairs Become One Tool Each

### `add_flags` + `remove_flags` → `update_flags`

Not a count reduction — a correctness one. The two tools took identical
identity arguments and differed only in direction, so "mark read and clear the
colour" was TWO calls across TWO UIDVALIDITY windows. The mailbox could be
renumbered between them, and the second call would then be refused with the
first already applied: a half-finished change with no single result to report.

`Agentmail::update_flags` does both inside one SELECT. Order is fixed and
documented rather than incidental — **remove, then the colour, then add** — so a
flag named in both lists ends up SET, and a colour survives a `remove` list that
also names `\Flagged`. `add_flags` and `remove_flags` remain as thin public
wrappers; they are API and the CLI uses one.

The merge also retired a genuine wart: both tools had a `color` key meaning
different things (a colour name in one, and `clearColor: bool` in the other).
One `color` field now takes a name or `"none"`, so no invalid combination is
expressible.

### `create_reply_draft` → `create_draft { replyToMessage }`

`create_reply_draft` was `create_draft` plus a source identity — it already
delegated to the same `create_draft_with_headers` — so it duplicated the entire
body, format, attachment and Markdown surface in a second schema and a second
description, in every `tools/list`.

Merging was done ADDITIVELY, into `create_draft`, which is what keeps it safe:
`replyToMessage` is optional, so nothing `create_draft` could already do stopped
working. The library's `create_reply_draft` is untouched — the derivation is
real logic worth keeping as API; only the tool surface merged.

`to`, `cc`, `inReplyTo` and `references` are derived when `replyToMessage` is
present, and supplying them anyway is REFUSED rather than merged or ignored.
Two sources for one recipient field is how a reply quietly goes to the wrong
people.

### What was NOT merged, and why

The `delete_by_*` / `move_by_*` families and the four `top_*` rankings look
mergeable and are not. Each criterion needs different fields (a sender needs
address AND display name; a domain or List-Id needs one string), so a flat merge
stops the schema expressing which fields go together, and a proper discriminated
union needs `oneOf`/`$defs` — which `tool_schemas_are_ref_free` exists to
prevent, because some hosts reject `$ref`. Their per-tool descriptions also
carry distinct safety rules ("never use `delete_by_sender` for a mailing list")
that would dissolve into one generic paragraph.

`preview_thread_record` / `export_thread_record` stay split because the split IS
the confirmation gate. `list_pending_moves` / `reconcile_moves` stay split
because one is `read_only` and the other destructive, and those annotations
drive permission prompts.

### Consequence

37 tools → 35. The count is asserted in two places
(`tool_schemas_are_ref_free`, `tools_list_has_35_annotated_tools`) precisely so
a drift like this cannot land without the docs being updated with it. (0.7.0
added `list_identities`: 36, and the second guard is now
`tools_list_has_36_annotated_tools`.) Pinned by
`update_flags_exposes_add_remove_and_color_as_one_call`,
`create_draft_absorbs_the_reply_form`, and
`a_reply_draft_refuses_recipients_it_would_derive`.

## 0.5.0 — The Handshake Instructions Are A Document, Not A Paragraph

### Decision

The server `instructions` are structured Markdown: a title, ten `##` sections,
short wrapped lines, and lists where the content is a list. Previously they were
one unbroken ~7 KB paragraph of semicolon-joined clauses, assembled from
`\`-continued string fragments.

### Rationale

Instructions are the first thing a model reads and the only guidance that
arrives before it has done anything. Everything in the old text was true and
hard-won — the UIDVALIDITY fence, the aggregate-view rule, the
`delete_by_sender` warning — but it was delivered as an undifferentiated wall,
which is the format least likely to be retrieved at the moment any one rule
matters. Sections give a model somewhere to look; short lines survive being
quoted back.

Nothing was dropped. The rewrite reorganised, split and shortened, and added
what recent changes made true: draft bodies are Markdown sent as
`multipart/alternative`, and `outputDir` defaults to the session workspace.

One line was DELETED as wrong rather than dense: *"Start with list_accounts to
discover configured accounts."* The accounts are now in every `account`
argument's enum, so that sentence instructed a call the schema had just made
unnecessary — and it is exactly the instruction agents were obeying.

### Guard

`the_handshake_instructions_stay_scannable` asserts every section is present,
that no line exceeds 90 characters (the property whose loss is how prose
collapses back into a paragraph), and that the retired `list_accounts`
instruction has not returned.

## 0.5.0 — Accounts Are In The Schema, Not Only Behind A Tool Call

### Decision

`list_tools` patches the live account names into every tool's `account`
argument as a JSON Schema `enum`, and the completion whitelist now covers
EVERY advertised `email://` template rather than five of six.

### Rationale

The accounts were discoverable three ways — a `list_accounts` tool, one
`email://{account}` resource each, and argument completion — and agents used
none of them for the thing they actually needed. They opened every session with
`list_accounts` before touching mail, because that is the only channel that
answers the question *at the moment it is asked*: filling in `account` on a
tool call.

A resource an agent must notice, read and interpret is not the same affordance
as a value in the schema of the argument that wants it. Completion cannot help
either — MCP's `completion/complete` covers `ref/prompt` and `ref/resource`
only; there is no completion for tool arguments. The schema is the only place
that reaches the model at the point of decision, and rmcp derives it at compile
time, when the accounts are not yet known. Patching at list time is the
established answer (the bridge does exactly this for `subagent`'s runtime
`composer_id` enum — RULES §8b).

`list_accounts` remains: it reports which account is the DEFAULT, which the
enum cannot. Its description no longer tells agents to call it first.

### The completion bug it surfaced

`is_email_template` listed the body, headers, source, info and attachment
templates but NOT `EMAIL_MAILBOX_TEMPLATE` — which `email_resource_templates`
advertises FIRST, and which is the one template an agent reaches for before it
knows any UID. Completing `account` or `mailbox` against it returned an empty
list, indistinguishable from "this server has no accounts".

### Guard

Empty `enum` is unsatisfiable, so a server with no configured account skips the
patch and leaves the argument an unconstrained string — the caller then gets
"no such account" from the server instead of an unexplained schema rejection.
Pinned by `every_account_argument_advertises_the_configured_accounts`,
`an_empty_account_list_would_be_unsatisfiable_hence_the_guard`,
`tools_list_carries_the_live_accounts_in_the_account_enum`, and
`completion_covers_every_advertised_resource_template` (which enumerates
`resources/templates/list` rather than hardcoding, so a new template cannot be
added without its completion).

## 0.5.0 — Draft Bodies Are Markdown, Sent As `multipart/alternative`

### Decision

A draft body is read as Markdown and composed as `multipart/alternative`: the
source **exactly as written** as `text/plain`, then an HTML rendering of it.
`plainTextOnly: true` opts back out to a single unrendered part. The default is
ON — an author writing `**bold**` means emphasis, and a reader seeing literal
asterisks is the failure.

### The standard, and what it is not

`multipart/alternative` (RFC 2046 §5.1.4) is the standard for formatted mail:
one message in several representations, ordered by RISING preference, so a
client renders the richest form it can and none is left staring at markup.
Plain first, HTML last.

Outlook's third compose format — the one its UI calls **Rich Text** — is not
this and is not a standard. It is TNEF, `application/ms-tnef`, which reaches
any non-Outlook recipient as a `winmail.dat` attachment. We never produce it.
Apple Mail's "Make Rich Text" is a different thing again: it sends HTML, which
is why formatted mail *looks* like RTF in a Mac client while nothing RTF is on
the wire.

### Two injection routes, closed differently

**Raw HTML is escaped, never emitted.**

A draft body is author-supplied text arriving through a tool call. Treating
`<...>` in it as markup would let the caller decide what runs in a recipient's
mail client. `pulldown_cmark` surfaces raw HTML as its own events and we map
them to TEXT — escaped rather than dropped, because silently deleting what
someone wrote is its own failure. There is no sanitiser to keep in step with,
because no author markup ever passes through. Smart punctuation stays OFF: it
would rewrite quotes and dashes in the HTML half while the plain half kept the
originals, and the two halves of an alternative must say the same thing.

**Unsafe URL schemes lose their link.** `push_html` escapes a destination for
HTML but does not filter its scheme, so `[click](javascript:alert(1))` rendered
as a working `<a href="javascript:...">` — verified against the real renderer,
not assumed. Escaping raw HTML does nothing about this: the markup is OURS and
the payload rides an attribute we generated, which is why a review finding aimed
at "sanitize the HTML output" would have missed it. `is_safe_url` allowlists
`http`, `https`, `mailto` and `tel` (plus relative destinations, which cannot
execute); anything else has its link or image UNWRAPPED — the text survives, the
destination does not become clickable. Nothing is lost overall: the plain half of
the `alternative` still carries the author's Markdown verbatim.

The allowlist reads schemes after stripping ASCII whitespace and control
characters, because clients strip them too — `java\tscript:` is `javascript:` by
the time anything acts on it, and a checker reading the raw string sees a scheme
called `java`.

An HTML sanitiser (`ammonia` or similar) was NOT added. It cleans HTML we do not
produce: passthrough is off at the parser, so there is no author markup to
sanitise, and it would not have caught the scheme hole either — a sanitiser that
did would be a second, larger source of truth for a rule this file states in
twenty lines.

### Shape

With attachments the alternative NESTS inside `multipart/mixed` — one body in
two representations, then the files. A sibling `text/html` beside the
attachments would read as two different bodies.

Styling is one inline `style` on `<body>` (font stack, size, line height).
Clients strip `<style>` blocks and never fetch external CSS, so anything else
is decoration only some readers see; colours and backgrounds are left alone so
the reader's theme, dark mode included, still governs.

### Consequence

`pulldown-cmark` (+2 transitive crates) is a new dependency. `BodyFormat` is
public API on `Agentmail::{create_draft_with_headers, create_reply_draft,
update_draft}`; `create_draft` and the CLI take the default. Pinned by
`a_markdown_body_ships_as_alternative_with_the_source_as_the_plain_half`,
`raw_html_in_a_body_is_escaped_never_emitted`,
`plain_text_only_emits_a_single_unrendered_part`, and
`attachments_nest_the_alternative_inside_mixed`.

## 0.5.0 — The Draft Ceiling Is The Server's, Not Only Ours

### Decision

`check_draft_size` bounds a composed draft by the SMALLER of
`MAX_DRAFT_MIME_BYTES` (64 MiB) and the server's RFC 7889 `APPENDLIMIT=N`, and
the refusal names which bound was hit. A bare `APPENDLIMIT` token means the
limit varies per mailbox and is reported via `STATUS`; `ServerCaps::append_limit`
returns `None` there rather than guess, leaving our ceiling in force.

### Rationale

The limit was a single client-side constant. Gmail advertises
`APPENDLIMIT=35651584` — 34 MiB, roughly half our ceiling — so a draft between
the two passed our check and was rejected by the server at APPEND: after
composing the message, after reading every attachment off disk, and, for
`update_draft`'s emulated replace, at exactly the step whose ordering exists to
guarantee the new content is durable first. A bound the server publishes on its
capability line should not be discovered by failing.

The check moved AFTER the connection is acquired, since the bound is
per-account. `server_caps` is cached per account, so a warm pool pays no extra
round trip.

### Scope

Not applied to reads. `MAX_DRAFT_MIME_BYTES` also bounds the source fetch in
`update_draft`; that is a read ceiling and `APPENDLIMIT` says nothing about it.

## 0.5.0 — Composing A Message Is Not A Filesystem Operation

### Decision

`load_draft_attachments` resolves the session's file sandbox ONLY when there is
a file to read. A draft with an empty attachment list returns immediately, and
`create_draft` / `create_reply_draft` / `update_draft` no longer touch the file
policy at all in that case.

### Rationale

All three resolved `file_access_for_request` unconditionally, before looking at
whether anything was attached. The embedded server has no ambient sandbox — it
takes one per request from `_meta["io.agentmuse/workspaceRoot"]` — so in a
session that carried no workspace, saving a plain text draft failed with
`-32602 this embedded AgentMail file operation requires an active session
workspace`. Three consecutive draft saves failed that way on 2026-09-03 across
two accounts, and the agent gave up and opened a `mailto:` link instead, which
saves nothing to the server at all.

The guard was in the right place conceptually and the wrong place mechanically:
it gated the TOOL rather than the operation. Reading an attachment off disk is
a file operation and still refuses without a workspace — pinned by the second
half of `a_draft_with_no_attachments_needs_no_workspace`.

### Note

The reason those sessions had no workspace was a host-side defect, fixed
separately (`app-api` passed the user-chosen project path, which is empty when
the user picked no folder, rather than the session's resolved working
directory). Both were needed: even with a workspace always present, gating a
zero-file operation on one is wrong.

## 0.5.0 — `update_draft` Emulates REPLACE Instead Of Refusing It

### Decision

`update_draft` no longer requires RFC 8508 REPLACE. Where the server advertises
it, the swap stays one atomic command. Where it does not, AgentMail emulates it
as APPEND-then-discard and returns the new identity, with no change to the tool
contract other than an optional `warning`.

The order is the safety argument: APPEND first, so the replacement is durable
before anything is destroyed. The worst outcome is a duplicate draft, never a
lost one. The superseded draft is then discarded through the same policy-aware
path `delete_messages` uses (`discard_mode_for` picks `Permanent` only where
UIDPLUS makes UID EXPUNGE targeted, or on Gmail where `trash_for_mode` routes
it to `[Gmail]/Trash`; everything else disposes through Trash). A failed
discard sets `warning` and still returns success — the draft was written, and
an error would send the caller back to rewrite one that already exists.

### Rationale — the refusal exported the risk it was avoiding

The previous behavior refused with "server does not advertise RFC 8508 REPLACE;
refusing a non-atomic APPEND+DELETE fallback," reasoning that a disconnect
mid-emulation could leave duplicate drafts. That reasoning was sound about the
hazard and wrong about who would bear it.

REPLACE is rare. Neither Gmail nor iCloud implements it, which is every
`update_draft` call in our own logs — four consecutive failures across both
accounts. Agents did not stop editing drafts; they ran `create_draft` +
`delete_messages` by hand instead. That is the identical two commands with the
same disconnect window and NONE of the guards: no `\Draft` verification, no
UIDVALIDITY fence, no Gmail-label handling, no UIDPLUS check before an EXPUNGE.
Refusing did not prevent the non-atomic sequence — it relocated the sequence to
the one place with no safeguards, and cost a round trip to discover.

Owning the emulation makes the hazard bounded and reported rather than
unmanaged and silent.

### Consequence

Tool description, server instructions, MCP.md and README no longer promise a
refusal, and now say plainly that a replaced draft has a new UID. Pinned by
`a_superseded_draft_is_only_expunged_where_uidplus_makes_it_targeted` (the
disposal choice) and `update_draft_advertises_emulation_not_refusal` (the
contract).

## 0.5.0 — Tool Results Carry URIs As Links Only

### Decision

No output type has a URI field. `WireOutput::resource_uris(&self)` returns the
message URIs a result should LINK, `compact_result` turns them into
`ResourceLink` blocks BEFORE serializing, and `structured_content` is the output
verbatim. Types that mention no message — every write tool, `list_accounts` —
keep the trait's empty default.

### Rationale

A URI inside a JSON string is a URI no aggregator can rewrite. Behind the Agent
Muse bridge our `ResourceLink` blocks are namespaced on the way out
(`email://agentmail/Gmail/…`) while the identical URI in our text is not — the
bridge cannot reach inside a backend's payload. An agent that lifted
`resourceUri` out of the JSON therefore read under a spelling nobody
advertised. That worked (the bridge accepts both) but the reply came back under
the OTHER spelling, and at least one host renders a read whose contents carry an
unrequested URI as if the call never executed (2026-09-03). Two channels
publishing two spellings of one identity is the defect; one channel is the fix.

### Why not build it and strip it

The first cut of this kept `resource_uri` on the row DTOs, marked it
`#[schemars(skip)]`, and deleted it from the serialized value on the way out.
Two arguments were offered for that, and both were wrong.

*"The URI is computed where the account and mailbox are in scope."* True of a
ROW — `MessageMetadataOutput` does not know its account — but false of the
OUTPUT, which is where the accessor lives. Every result type carries `account`
and `mailbox`, and every row its own identity, so nothing needed plumbing.

*"A typed opt-in fails open: forget the impl and the raw URI ships."* Circular.
With the field deleted, forgetting an impl yields NO LINKS — a missing
affordance, not a leaked URI. The leak hazard existed only because the field
existed, which is what the strip was cleaning up.

What remained was one chokepoint versus nine small accessors, against a type
that no longer described its own wire format, a `structured_content` that was a
serialized value edited afterwards by key name, and a delete that would remove
any future `resourceUri` meaning something else. Not creating it is strictly
better, and it is what SOUL.md's "strongly typed end-to-end" asks for.

### Scope

Only tool RESULTS. Resource CONTENTS — the `email://{account}` catalog, the
`/info` hub — keep their embedded URIs, because `contents` has no link block and
JSON is the only navigation channel a read has.

### Consequence

Tool descriptions, prompts, and the server instructions direct agents to follow
the links rather than read a field. `create_draft` additionally warns that a
draft's UID is not durable: re-saving a draft (here or in any other mail client)
appends a new message and expunges the old one, and some servers discard an
APPENDed draft outright — the failure that exposed all of this.

The acceptance test `a_tool_result_carries_uris_as_links_only_never_as_json`
asserts the OUTCOME (links present; no `resourceUri` and no `email://` on either
channel; none in the declared `outputSchema`) rather than the mechanism, so it
passed unchanged across the rewrite — which is the evidence it was aimed at the
right thing.

## 0.5.0 — Evidence Archives Are Filesystem Tools

### Decision

Keep `/source` as a bounded MCP resource for context use, and provide
`download_message_source` plus `download_thread` for exact RFC822 evidence
archives. The tools move bytes directly from IMAP to files under
`AGENTMAIL_FILE_ROOT`; the model never has to read and re-emit those bytes.

Each saved message is fetched with `BODY.PEEK[]` after a live UIDVALIDITY and
size check, created without overwrite, and accompanied by SHA-256, parsed
metadata, and a local DNS-backed DKIM result. The bulk tool accepts a
caller-selected UID set; its name is convenience terminology, not server-side
thread discovery.

SPF remains absent unless a future trusted delivery-metadata source provides
the SMTP peer IP, HELO, and envelope sender. A message's own
`Authentication-Results` header is not independent verification.

### Rationale

MCP resources are deliberately delivered through model context. They cannot
provide a reliable byte-for-byte transfer to disk when the model must re-emit
their content. A server-side filesystem side effect preserves the original
octets and supports a verifiable manifest.

---

## 0.5.0 — iCloud Mail OAuth Remains Provider-Gated

### Decision

Do not implement or advertise a self-service Apple/iCloud Mail OAuth flow from
Sign in with Apple. Continue to document app-specific passwords unless Apple
onboards AgentMail into its supported third-party app authorization program
and supplies the required Mail integration contract.

### Rationale

Apple Support documents [Apple Account authorization for supported third-party
Mail apps](https://support.apple.com/en-us/121539), but Apple's public manual
[iCloud Mail server settings](https://support.apple.com/en-us/102525) still use
an app-specific password. The Xcode
[Sign in with Apple capability](https://developer.apple.com/documentation/xcode/configuring-sign-in-with-apple)
authenticates a user to the developer's app, and its published scopes expose
[contact information](https://developer.apple.com/documentation/authenticationservices/asauthorization/scope),
not mailbox access. Apple's public Account & Organizational Data Sharing OAuth
authorization publishes only
[`edu.users.read` and `edu.classes.read`](https://developer.apple.com/documentation/AccountOrganizationalDataSharing/Request-an-authorization).

Those sources do not publish an iCloud Mail client-registration path, Mail
scope, refresh contract, or IMAP XOAUTH2 bearer-token mapping that AgentMail can
implement independently. Existing generic XOAUTH2 support remains usable only
when a provider or external helper supplies a valid access token.

---

## 0.2.1 — Microsoft Graph API Support

### Decision

Outlook / Microsoft 365 support was removed from the provider list in 0.1.x because Microsoft disabled basic authentication (username + app password) for IMAP on personal accounts (outlook.com, hotmail.com, live.com) in September 2024. Microsoft 365 work/school accounts depend on tenant admin settings — many have also disabled basic auth.

Unlike Gmail, iCloud, Yahoo, and Fastmail, Microsoft does not offer app-specific passwords for IMAP. The only supported authentication path is OAuth2 via the Microsoft Identity Platform.

### Scope of Work

**Option A: OAuth2 XOAUTH2 over IMAP**

Continue using the IMAP protocol but authenticate with OAuth2 tokens instead of passwords.

- Register an Azure AD application (requires Microsoft Partner/Developer account)
- Implement OAuth2 Authorization Code flow with PKCE for token acquisition
- Implement XOAUTH2 SASL mechanism for IMAP LOGIN (`AUTH=XOAUTH2`)
- Token refresh handling (access tokens expire every ~60 minutes)
- Secure token storage (keyring or encrypted file)
- Consent scopes: `https://outlook.office365.com/IMAP.AccessAsUser.All`
- Works with both personal and work/school accounts

**Estimated complexity:** Medium. The IMAP protocol and all existing tools remain unchanged — only the authentication layer changes. `async-imap` supports custom authenticators.

**Option B: Microsoft Graph API (REST)**

Replace IMAP entirely with the Microsoft Graph REST API for Outlook accounts.

- Register an Azure AD application
- Implement OAuth2 Authorization Code flow with PKCE
- Implement Graph API client for: list folders, list/search messages, get message content, delete messages, move messages, create drafts, manage flags
- Map Graph API responses to existing `MessageInfo`, `MailboxInfo` types
- Handle pagination (Graph uses `@odata.nextLink`, not IMAP UIDs)
- Handle delta queries for efficient sync
- Consent scopes: `Mail.ReadWrite`, `Mail.Send`

**Estimated complexity:** High. Requires a parallel mail backend abstraction — IMAP for Gmail/iCloud/Yahoo/Fastmail, Graph for Outlook. All tool implementations would need to dispatch through an abstraction layer.

### Recommendation

**Start with Option A** (OAuth2 XOAUTH2 over IMAP). It's less invasive — all existing IMAP code, tools, and connection pooling continue to work. The only change is swapping password-based LOGIN for XOAUTH2-based LOGIN. Option B can be revisited if Microsoft further restricts IMAP access.

### Dependencies

- `oauth2` crate (already a transitive dependency via rmcp's `auth` feature, but not currently used directly)
- Azure AD app registration (one-time setup, distributes client_id with the binary)
- Token storage mechanism (extend `Secret` enum or use a dedicated token cache)

### Blocked On

- Azure AD app registration and client_id provisioning
- Decision on whether to bundle a client_id or require users to register their own app
