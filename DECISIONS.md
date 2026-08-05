# Agentmail Decisions

Architectural decisions, deferred work, and rationale for future reference.

---

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
