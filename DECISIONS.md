# Agentmail Decisions

Architectural decisions, deferred work, and rationale for future reference.

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
a drift like this cannot land without the docs being updated with it. Pinned by
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
