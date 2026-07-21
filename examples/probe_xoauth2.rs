//! Diagnose XOAUTH2 IMAP auth against Gmail / Yahoo / AOL. Shows the raw SASL
//! exchange so a rejection is precise: it DECODES the server's error challenge
//! (the base64 JSON that `check-connection` collapses to "Invalid
//! credentials") to tell expired-token from wrong-scope from wrong-user.
//!
//! ```sh
//! PROBE_HOST=imap.gmail.com PROBE_USER='you@gmail.com' \
//!   PROBE_TOKEN='ya29.<access token, scope https://mail.google.com/>' \
//!   cargo run --example probe_xoauth2
//! ```
//! Read-only: it authenticates and disconnects, nothing else.

use std::time::Duration;

use async_imap::Authenticator;
use tokio::net::TcpStream;

/// Logs each SASL step so a hang or rejection is localizable.
struct Logging<'a> {
    user: &'a str,
    token: &'a str,
    step: usize,
}

impl Authenticator for Logging<'_> {
    type Response = String;

    fn process(&mut self, challenge: &[u8]) -> String {
        self.step += 1;
        if self.step == 1 {
            eprintln!(
                "[step 1] server prompted for the SASL response (challenge empty={}); sending user + Bearer token",
                challenge.is_empty()
            );
            format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
        } else {
            // Gmail/Yahoo send a base64 JSON error here, e.g.
            // {"status":"400","schemes":"Bearer","scope":"https://mail.google.com/"}
            eprintln!(
                "[step {}] server returned an ERROR challenge → {}",
                self.step,
                String::from_utf8_lossy(challenge)
            );
            String::new() // empty response so the tagged NO surfaces
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = std::env::var("PROBE_HOST").unwrap_or_else(|_| "imap.gmail.com".to_string());
    let port: u16 = std::env::var("PROBE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(993);
    let user = std::env::var("PROBE_USER").expect("set PROBE_USER");
    let token_raw = std::env::var("PROBE_TOKEN").expect("set PROBE_TOKEN (OAuth access token)");
    let token = token_raw.trim();

    eprintln!(
        "token: {} chars{}",
        token.len(),
        if token != token_raw {
            " (had surrounding whitespace — trimmed)"
        } else {
            ""
        }
    );

    let tcp = TcpStream::connect((host.as_str(), port)).await?;
    let connector = tokio_native_tls::TlsConnector::from(native_tls::TlsConnector::new()?);
    let tls = connector.connect(&host, tcp).await?;
    let mut client = async_imap::Client::new(tls);

    // Consume the greeting first (the fix): the AUTHENTICATE handshake does not
    // skip leading untagged responses, so an unread greeting deadlocks it.
    match tokio::time::timeout(Duration::from_secs(15), client.read_response()).await {
        Ok(Ok(Some(greeting))) => eprintln!("greeting: {:?}", greeting.parsed()),
        Ok(_) => eprintln!("greeting: (none/closed)"),
        Err(_) => {
            eprintln!(
                "RESULT: no greeting within 15s — network/TLS reached the port but the server never spoke. Not an auth problem."
            );
            return Ok(());
        }
    }

    let auth = Logging {
        user: &user,
        token,
        step: 0,
    };
    eprintln!("sending AUTHENTICATE XOAUTH2 …\n");
    match tokio::time::timeout(
        Duration::from_secs(20),
        client.authenticate("XOAUTH2", auth),
    )
    .await
    {
        Ok(Ok(_session)) => {
            eprintln!(
                "\nRESULT: ✅ XOAUTH2 AUTH OK — the token authenticates. Wire this account with auth = \"xoauth2\"."
            );
        }
        Ok(Err((error, _))) => {
            eprintln!(
                "\nRESULT: ❌ server rejected auth: {error}\n\
                 The decoded ERROR challenge above is the real reason:\n\
                 - status 400 / 'invalid_grant' → token expired or malformed (get a fresh one)\n\
                 - a 'scope' field → the token lacks the IMAP scope (Gmail needs https://mail.google.com/)\n\
                 - mismatched user → PROBE_USER must match the token's account"
            );
        }
        Err(_) => {
            eprintln!(
                "\nRESULT: ⏱ hung 20s. If '[step 1]' printed, the server took our SASL response and went silent (unexpected). If NO step printed, the greeting consumption or the '+' continuation is off — capture this output."
            );
        }
    }
    Ok(())
}
