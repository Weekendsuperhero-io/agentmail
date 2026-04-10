# Agentmail Decisions

Architectural decisions, deferred work, and rationale for future reference.

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
