# Email list and authentication standards

This directory records the email header and authentication standards AgentMail
uses for subscription discovery, unsubscribe, and cleanup. See
[How subscription cleanup works](subscription-cleanup.md) for the complete
workflow. The RFC Editor remains authoritative for the standards.

## Implemented standards

| RFC | Subject | AgentMail use |
| --- | --- | --- |
| [2369](https://www.rfc-editor.org/rfc/rfc2369.html) | List command header URI syntax | Parses ordered angle-bracket URIs, comments, folding, URI-internal commas, and broken-MTA whitespace |
| [2919](rfc-2919-list-id.md) | `List-Id` | Uses the identifier inside angle brackets for list ranking and exact cleanup matching |
| [6376](https://www.rfc-editor.org/rfc/rfc6376.html) | DKIM | Verifies the complete live-fetched message and inspects the passing signature's `h=` coverage |
| [8058](rfc-8058-one-click-unsubscribe.md) | One-click unsubscribe | Requires exact headers, consent, one HTTPS URI, DKIM coverage, a form POST, and direct 2xx success |

## One-click execution invariants

1. Discovery stores only UID identity and a limited immutable header
   projection. `advertisedOneClick` is a syntax hint, not an authentication
   claim.
2. Execution requires `(mailbox, expected UIDVALIDITY, UID)` and rejects a
   changed or missing UID epoch before fetching the target.
3. Exactly one `List-Unsubscribe` and one `List-Unsubscribe-Post` field must be
   present. The post value is the single case-insensitive ABNF literal
   `List-Unsubscribe=One-Click`; substrings and extra arguments are rejected.
4. At least one locally verified DKIM signature must pass and cover both list
   fields. Account-wide List-Id cleanup additionally requires that same passing
   signature to cover the single `List-Id` header. Header-only caching is
   insufficient for this check, so the complete message is fetched with a
   bounded `BODY.PEEK[]<0.N>`, never persisted, and dropped after verification.
   A size-only preflight caps this transient source at 64 MiB without limiting
   the number of messages later matched for cleanup.
5. The URI list must contain exactly one HTTPS URI and no HTTP alternative.
   Credentials, fragments, missing hosts, and unusable ports are rejected.
6. The host is resolved once. Empty answers, non-public addresses, or a mix of
   public and private addresses fail closed. Validated addresses are pinned
   into a fresh client with proxies, redirects, referer generation, and retries
   disabled.
7. The request body is exactly `List-Unsubscribe=One-Click` with
   `application/x-www-form-urlencoded`. Only the direct endpoint's 2xx response
   is success.
8. `confirmOneClick=true` is mandatory. Cleanup is a separate decision and
   defaults off. Combined cleanup requires a signed List-Id. Cleanup after a
   failed unsubscribe and permanent disposal each require explicit policy
   choices. Gmail routes deletion through Trash because in-place EXPUNGE only
   removes the selected label.

## Cache privacy boundary

The persistent SQLite projection may contain mailbox identity, UID membership,
sender address/name, date, Message-ID, a normalized List-Id, and boolean syntax
markers such as `hasListHeaders` and `advertisedOneClick`. It does not retain
List-Unsubscribe or List-Post values, opaque recipient tokens, bodies, subjects,
recipients, flags, attachments, credentials, DKIM results, complete headers, or
raw messages. A schema-version migration securely rebuilds older token-bearing
projections before they can be reused.
