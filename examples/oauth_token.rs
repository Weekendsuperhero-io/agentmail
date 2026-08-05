//! Mint an OAuth 2.0 access token for XOAUTH2 IMAP via the loopback
//! (installed-app, RFC 8252) flow. Prints the ACCESS TOKEN to paste into
//! agentmail's `password.raw`, and the REFRESH TOKEN for a long-lived
//! `password.cmd` refresh helper.
//!
//! Register this exact redirect URI on your OAuth app:
//!     http://127.0.0.1:8535/callback     (change the port with OAUTH_PORT)
//!
//! Gmail (self-serve; scope must be the full-IMAP scope):
//!     OAUTH_CLIENT_ID=... OAUTH_CLIENT_SECRET=... \
//!       cargo run --example oauth_token
//!   (Gmail endpoints + scope https://mail.google.com/ are the defaults.)
//!
//! Yahoo / AOL (mail scope needs an approved app; no PKCE; secret required —
//! these are confidential clients, so the secret goes via HTTP Basic auth).
//! Use the login host that matches the MAILBOX: api.login.yahoo.com for
//! @yahoo.com, api.login.aol.com for @aol.com / @verizon.net.
//!     OAUTH_CLIENT_ID=... OAUTH_CLIENT_SECRET=... OAUTH_NO_PKCE=1 \
//!       OAUTH_AUTH_URL=https://api.login.aol.com/oauth2/request_auth \
//!       OAUTH_TOKEN_URL=https://api.login.aol.com/oauth2/get_token \
//!       OAUTH_SCOPE=mail-w \
//!       cargo run --example oauth_token
//!   (Yahoo mailbox → swap both hosts to api.login.yahoo.com.)
//!
//! The client secret is optional only for pure-PKCE public clients (Gmail);
//! when present it is sent via HTTP Basic auth, which Google/Yahoo/AOL accept.

use std::io::Write as _;
use std::time::Duration;

use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_b64url(len: usize) -> String {
    let mut buf = vec![0u8; len];
    // rand 0.10: `thread_rng().fill_bytes` became top-level `rand::fill`.
    rand::fill(&mut buf[..]);
    b64url(&buf)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client_id = env("OAUTH_CLIENT_ID").expect("set OAUTH_CLIENT_ID");
    let client_secret = env("OAUTH_CLIENT_SECRET");
    let auth_url = env("OAUTH_AUTH_URL")
        .unwrap_or_else(|| "https://accounts.google.com/o/oauth2/v2/auth".into());
    let token_url =
        env("OAUTH_TOKEN_URL").unwrap_or_else(|| "https://oauth2.googleapis.com/token".into());
    let scope = env("OAUTH_SCOPE").unwrap_or_else(|| "https://mail.google.com/".into());
    let port: u16 = env("OAUTH_PORT").map_or(Ok(8535), |p| p.parse())?;
    let use_pkce = env("OAUTH_NO_PKCE").is_none();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    // PKCE (S256) + CSRF state.
    let verifier = random_b64url(32);
    let challenge = b64url(Sha256::digest(verifier.as_bytes()).as_slice());
    let state = random_b64url(16);

    // Build the authorization URL with proper query encoding.
    let mut url = reqwest::Url::parse(&auth_url)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", &client_id);
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("scope", &scope);
        q.append_pair("state", &state);
        // `access_type=offline` + `prompt=consent` are Google conventions that
        // force a refresh_token; Yahoo/AOL return one by default and may reject
        // the unknown params, so only send them for Google (or OAUTH_OFFLINE=1).
        if auth_url.contains("google") || env("OAUTH_OFFLINE").is_some() {
            q.append_pair("access_type", "offline");
            q.append_pair("prompt", "consent");
        }
        if use_pkce {
            q.append_pair("code_challenge", &challenge);
            q.append_pair("code_challenge_method", "S256");
        }
    }
    let url = url.to_string();

    // Start the loopback listener BEFORE opening the browser.
    let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        format!("could not bind {redirect_uri} ({e}); is the port free and does it match the registered redirect URI?")
    })?;

    eprintln!("redirect URI (must be registered): {redirect_uri}");
    eprintln!(
        "PKCE: {}\n",
        if use_pkce {
            "S256"
        } else {
            "disabled (OAUTH_NO_PKCE)"
        }
    );
    eprintln!("Opening the consent page. If it doesn't open, paste this URL:\n\n{url}\n");
    let _ = std::process::Command::new("open").arg(&url).spawn();

    // Wait for the redirect (one connection), bounded.
    let code = tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let (mut sock, _) = listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await?;
            let request = String::from_utf8_lossy(&buf[..n]);
            let target = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");
            // Ignore favicon/other stray hits; only /callback carries the code.
            if !target.starts_with("/callback") {
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n").await;
                continue;
            }
            let query = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))?;
            let mut got_code = None;
            let mut got_state = None;
            let mut got_error = None;
            for (k, v) in query.query_pairs() {
                match k.as_ref() {
                    "code" => got_code = Some(v.into_owned()),
                    "state" => got_state = Some(v.into_owned()),
                    "error" => got_error = Some(v.into_owned()),
                    _ => {}
                }
            }
            let body = "<html><body style='font-family:sans-serif'>Authorization received — you can close this tab.</body></html>";
            let _ = sock
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;

            if let Some(err) = got_error {
                return Err(format!("authorization error from the provider: {err}").into());
            }
            if got_state.as_deref() != Some(state.as_str()) {
                return Err("state mismatch (possible CSRF) — aborting".into());
            }
            return got_code.ok_or_else(|| "redirect carried no authorization code".into());
        }
        #[allow(unreachable_code)]
        Ok::<String, Box<dyn std::error::Error>>(String::new())
    })
    .await
    .map_err(|_| "timed out waiting for the browser redirect (300s)")??;

    eprintln!("\nexchanging the authorization code for tokens …");
    // Encode the x-www-form-urlencoded body via the Url query encoder (this
    // reqwest build has no `.form()`).
    let mut body_url = reqwest::Url::parse("http://x/")?;
    {
        let mut q = body_url.query_pairs_mut();
        q.append_pair("grant_type", "authorization_code");
        q.append_pair("code", &code);
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("client_id", &client_id);
        if use_pkce {
            q.append_pair("code_verifier", &verifier);
        }
    }
    let body = body_url.query().unwrap_or("").to_string();
    let mut request = reqwest::Client::new()
        .post(&token_url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body);
    if let Some(secret) = client_secret.as_deref() {
        request = request.basic_auth(&client_id, Some(secret));
    }
    let response = request.send().await?;
    let status = response.status();
    let json: serde_json::Value = response.json().await?;
    if !status.is_success() {
        return Err(format!("token endpoint returned {status}: {json}").into());
    }

    let access = json["access_token"].as_str().unwrap_or("");
    let refresh = json["refresh_token"].as_str();
    let expires = json["expires_in"].as_i64().unwrap_or(0);

    println!("\n=== SUCCESS ===");
    println!(
        "access_token (paste into agentmail password.raw; expires in {expires}s):\n{access}\n"
    );
    match refresh {
        Some(r) => println!("refresh_token (store securely for a password.cmd helper):\n{r}"),
        None => println!(
            "(no refresh_token returned — for Gmail ensure access_type=offline + a fresh consent; \
             re-run with prompt=consent if you'd previously granted access)"
        ),
    }
    println!(
        "\nagentmail config:\n[accounts.<name>]\nhost = \"...\"\nusername = \"<the mailbox address>\"\nauth = \"xoauth2\"\npassword.raw = \"<access_token above>\""
    );
    std::io::stdout().flush()?;
    Ok(())
}
