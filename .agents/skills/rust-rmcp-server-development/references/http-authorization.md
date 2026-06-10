# HTTP Authorization

Use this reference when an HTTP MCP server must act as an OAuth protected resource, when a client needs to authenticate against one, or when enabling the `rmcp` `auth` feature.

Verified on 2026-06-09 against:

- MCP specification 2025-11-25, authorization and security best practices pages.
- `rmcp` 1.7.0 source: `transport/auth.rs` (`AuthorizationManager`, `AuthClient`, `OAuthClientConfig`, `AuthError`) and the `auth` / `auth-client-credentials-jwt` features.
- `modelcontextprotocol/rust-sdk` `docs/OAUTH_SUPPORT.md` and examples: `examples/clients/src/auth/oauth_client.rs`, `examples/clients/src/auth/client_credentials.rs`, `examples/servers/src/simple_auth_streamhttp.rs`, `examples/servers/src/complex_auth_streamhttp.rs`.
- `modelcontextprotocol/rust-sdk` auth issues and PRs for gap status: 531, 651, 707, 784, 876–879, 887, 888.
- crates.io listings for gap-filling crates: `tower-oauth2-resource-server` 0.12, `jwt-authorizer` 0.15, `oauth2` 5.0 (introspection/revocation modules confirmed in source), `keyring`/`keyring-core`, `oauth2-test-server`.

Scope: HTTP transports only; stdio servers take credentials from environment or local config. The ext-auth extensions (OAuth client credentials profile, enterprise-managed authorization) are covered in the sibling skill `mcp-apps-auth-rmcp-development`, `references/auth-extensions.md`.

## Actors And Flow

- The MCP server is an OAuth 2.1 **resource server**. It never issues tokens; it validates them.
- The **authorization server** issues tokens. It can be your IdP; the MCP server only needs to point at it.
- The MCP client is an OAuth **client** running the authorization-code flow with PKCE.

The whole flow: client hits the server → `401` with metadata pointer → client fetches protected resource metadata (RFC 9728) → fetches authorization server metadata (RFC 8414) → registers if needed (RFC 7591 dynamic registration or a client ID metadata document) → runs PKCE authorization-code flow with the `resource` parameter (RFC 8707) → sends `Authorization: Bearer` on every MCP request.

## Server Side: Acting As A Protected Resource

Spec requirements, in implementation order:

1. **Serve RFC 9728 protected resource metadata** and point clients at it from the 401:

   ```http
   HTTP/1.1 401 Unauthorized
   WWW-Authenticate: Bearer resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", scope="files:read"
   ```

   The metadata document lives at `/.well-known/oauth-protected-resource` (optionally suffixed with the MCP endpoint path) and names your authorization servers:

   ```json
   {
     "resource": "https://mcp.example.com/mcp",
     "authorization_servers": ["https://auth.example.com"],
     "scopes_supported": ["files:read", "files:write"]
   }
   ```

2. **Validate the token on every request**: signature or introspection, expiry, and — non-negotiable — audience binding per RFC 8707: the token must have been issued for this server's canonical URI. Accepting tokens issued for another resource is the "token passthrough" anti-pattern the spec explicitly forbids.
3. **Use the right status codes**: `401` for missing, invalid, or expired tokens (with the `WWW-Authenticate` challenge); `403` for valid tokens with insufficient scope (include the required `scope` so clients can upgrade).
4. **Accept tokens only from the `Authorization` header**, never query strings, and never write tokens into logs, tool results, or `structuredContent`.

### RMCP Integration

Token validation runs as ordinary axum/tower middleware in front of the Streamable HTTP service, so MCP handling never sees unauthenticated requests. Shape, following the rust-sdk auth server examples:

```rust
// Shape only — pair with your real token validator (JWKS, introspection, ...).
use axum::{Router, middleware, routing::get};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, tower::StreamableHttpService,
};

let mcp_service = StreamableHttpService::new(
    || Ok(MyServer::new()),
    LocalSessionManager::default().into(),
    Default::default(),
);

let app = Router::new()
    .nest_service("/mcp", mcp_service)
    .layer(middleware::from_fn(validate_bearer)) // 401/403 before MCP handling
    .route(
        "/.well-known/oauth-protected-resource",
        get(protected_resource_metadata),
    );
```

Bind the validated principal and scopes into the request in the middleware, and enforce authorization *again* inside tools and resources — transport-level auth says who is calling, not whether this tool may touch that row. The exact mechanism for reading request state inside handlers has changed across rmcp releases; check current docs for the context extension extractor before relying on it.

The full working pattern, including an in-process demo authorization server with consent pages, is `examples/servers/src/complex_auth_streamhttp.rs` in the rust-sdk; `simple_auth_streamhttp.rs` is the minimal version.

## Client Side

What the spec requires of clients:

- Discover metadata from the `WWW-Authenticate` header or the well-known URI; never hardcode the authorization server.
- Include the RFC 8707 `resource` parameter in **both** the authorization request and the token request, set to the server's canonical URI (`https://mcp.example.com/mcp` — scheme included, no fragment).
- Use PKCE, request least-privilege scopes, and send the token only in the `Authorization` header.

The `auth` feature implements this machinery. Adapted from `docs/OAUTH_SUPPORT.md`:

```rust
use anyhow::Context;
use rmcp::transport::auth::{AuthClient, OAuthState};

let mut oauth = OAuthState::new(&server_url, None).await?;
oauth
    .start_authorization(&["files:read"], REDIRECT_URI, Some("My MCP Client"))
    .await?;
let url = oauth.get_authorization_url().await?;
// Send the user to `url`; your redirect endpoint receives code + state.
oauth.handle_callback(auth_code, csrf_state).await?;

let manager = oauth
    .into_authorization_manager()
    .context("authorization flow not completed")?; // returns Option
let http = AuthClient::new(reqwest::Client::default(), manager);
// Use `http` with the Streamable HTTP client transport.
```

What `AuthorizationManager` handles for you: RFC 9728/8414 metadata discovery, RFC 7591 dynamic client registration (`register_client`) or pre-registered IDs (`configure_client_id`), client ID metadata documents (`start_authorization_with_metadata_url` — an HTTPS URL as the client ID), PKCE, token refresh, pluggable credential/state stores (`set_credential_store`, `set_state_store`), and scope upgrades after a `403` (`AuthError::is_insufficient_scope`, `request_scope_upgrade`).

For machine-to-machine flows without a user, enable `auth-client-credentials-jwt` and follow `examples/clients/src/auth/client_credentials.rs` (`ClientCredentialsConfig`, JWT assertions preferred over static secrets).

## RMCP Coverage And Gaps

As of `rmcp` 1.7.0; the auth module moves fast, so re-check the linked issues before building a workaround.

Client side, working and spec-conformant: PKCE (S256), RFC 9728 discovery (both the `WWW-Authenticate` `resource_metadata` pointer and well-known fallbacks), RFC 8414/OIDC metadata discovery, RFC 7591 dynamic registration, client ID metadata documents, the RFC 8707 `resource` parameter on authorize and token requests, scope selection priority (challenge > resource metadata > AS metadata), `403` scope upgrades, automatic refresh, and client credentials (secret or private-key JWT).

The gaps, and what to do about each:

- **Resource-server validation is entirely yours — but mostly crate-fillable.** The SDK ships no Bearer parsing, JWT/JWKS verification, RFC 7662 introspection, audience check, scope enforcement, RFC 9728 metadata serving, or `WWW-Authenticate` builder — the auth server examples are pedagogy, not library code. Options: drop-in tower middleware — `tower-oauth2-resource-server` (JWKS fetch/cache/rotation, `iss`/`aud`/`exp` validation, claims into request extensions) or `jwt-authorizer` (axum/tonic, OIDC discovery, claim checks), with the `aliri` crates as a scope-extractor alternative. For opaque tokens, RFC 7662 introspection comes from the `oauth2` crate already in rmcp's `auth` dependency tree (`introspect()`). Hand-roll with `jsonwebtoken` only when you need full control, or terminate auth at a trusted fronting proxy and forward the principal in a verified header. RFC 9728 metadata serving stays manual everywhere — no crate exists — but it is one static JSON route (example above).
- **Enterprise-Managed Authorization (ext-auth) is unimplemented** — no RFC 8693 token exchange, RFC 7523 JWT bearer grants, or ID-JAG handling (tracking: rust-sdk issue 531). No crate implements RFC 8693 either (checked crates.io), but the exchange is a single POST to the IdP's token endpoint; validate ID-JAG JWTs with the same middleware as above. The MCP transport still carries an ordinary Bearer token, so nothing else changes.
- **No RFC 9207 `iss` validation in 1.7.0** (landed upstream after the release; rust-sdk issue 876). Option: your redirect endpoint receives the authorization response — compare its `iss` parameter against the discovered issuer before calling `handle_callback`.
- **Scope step-up replaces scopes instead of accumulating them** (SEP-2350; fix in review, rust-sdk PR 888). Option: on re-authorization, request the explicit union of the scopes you already hold plus the new challenge.
- **Refresh can fail with `invalid_client`** when dynamic registration issued a `client_secret` the AS also requires on refresh (rust-sdk issue 784). Options: pre-register and set the secret via `OAuthClientConfig::new(..).with_client_secret(..)`, or treat refresh failure as a signal to re-run the flow.
- **Credentials are not isolated per authorization server** (SEP-2352; rust-sdk issue 879). Option: one `OAuthState`/`AuthorizationManager` and one `CredentialStore` per MCP server; never share stores across servers.
- **Well-known discovery ordering can miss path-hosted authorization servers** (SEP-2351; fix in review, rust-sdk PR 887). Option: when discovery fails against a path-based AS, set `AuthorizationMetadata` explicitly with `set_metadata`.
- **No token revocation helper, and credential/state stores are in-memory by default.** Options: RFC 7009 revocation is also in the `oauth2` crate already in the tree (`revoke_token()`); implement `CredentialStore`/`StateStore` over the `keyring`/`keyring-core` crates (OS keychain) or disk so tokens survive restarts.

For integration-testing any of this without a real IdP, `oauth2-test-server` is an in-memory OAuth 2.0/OIDC authorization server with dynamic-registration support, built for exercising MCP auth flows. Several alternative Rust MCP frameworks also bundle server-side auth — they replace `rmcp` rather than extend it, so treat switching as an architecture decision, not a patch.

## Gateways

A gateway terminates two independent auth legs and must never bridge them with the same token:

- Downstream: the gateway is the protected resource; tokens are validated with the gateway as audience.
- Upstream: the gateway is a client using its own credentials (or a real per-user token exchange).
- Forwarding a downstream bearer token upstream is the forbidden passthrough pattern; and proxying a shared third-party client ID requires per-client user consent to avoid the confused deputy problem.

Mechanics live in `gateway-patterns.md`.

## Checklist

- Protected resource metadata is served and the 401 challenge points at it.
- Every request's token is checked for signature, expiry, **and audience**; foreign-audience tokens are rejected.
- `401` versus `403` semantics are correct and scope challenges are present.
- The `resource` parameter is sent on authorization and token requests (client side).
- Tokens never appear in query strings, logs, tool output, or `structuredContent`.
- Authorization is enforced again inside handlers, not only at the HTTP boundary.
