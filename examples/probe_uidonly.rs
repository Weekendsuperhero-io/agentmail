//! Validate RFC 9586 UID Mode against a live server before wiring it into the
//! account scan. Answers the decisive questions:
//!   1. Does `ENABLE UIDONLY` succeed?
//!   2. Does it lift the visible-window limit — can we now `UID FETCH` BELOW
//!      the Limited-Mode floor (which returned nothing before)?
//!   3. Does the server reply with `UIDFETCH`, and does our patched parser
//!      surface the UID (including when the UID item is omitted)?
//!   4. Does `PARTIAL` paginate a `1:*` fetch?
//!
//! Read-only: EXAMINE + UID SEARCH + UID FETCH (BODY.PEEK not needed here).
//! Connection auto-negotiates DEFLATE + sends the ID command, so this also
//! smoke-tests those.
//!
//! ```sh
//! PROBE_HOST=export.imap.aol.com PROBE_USER='user@verizon.net' \
//! PROBE_PASS='app-password' PROBE_MAILBOX=INBOX \
//! cargo run --example probe_uidonly
//! ```

use agentmail::config::AccountConfig;
use agentmail::imap_client::{self, ImapSession};
use agentmail::secret::Secret;
use async_imap::imap_proto::{AttributeValue, Response, Status};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Drive a raw command and collect `(uid, size)` from every FETCH/UIDFETCH
/// response until the tagged completion. Returns the tagged status text too.
async fn raw_uid_fetch(
    session: &mut ImapSession,
    command: &str,
) -> Result<(Vec<(u32, Option<u32>)>, String), Box<dyn std::error::Error>> {
    let tag = session.run_command(command).await?;
    let mut rows = Vec::new();
    loop {
        let Some(response) = session.read_response().await? else {
            return Err("connection closed mid-fetch".into());
        };
        match response.parsed() {
            Response::Fetch(num, attrs) => {
                let uid = attrs
                    .iter()
                    .find_map(|a| match a {
                        AttributeValue::Uid(u) => Some(*u),
                        _ => None,
                    })
                    .unwrap_or(*num);
                let size = attrs.iter().find_map(|a| match a {
                    AttributeValue::Rfc822Size(s) => Some(*s),
                    _ => None,
                });
                rows.push((uid, size));
            }
            Response::Done {
                tag: done,
                status,
                information,
                ..
            } if done == &tag => {
                let text = format!("{status:?} {information:?}");
                if !matches!(status, Status::Ok) {
                    return Ok((rows, text)); // report NO/BAD without erroring
                }
                return Ok((rows, text));
            }
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (Some(host), Some(user), Some(pass)) =
        (env("PROBE_HOST"), env("PROBE_USER"), env("PROBE_PASS"))
    else {
        eprintln!("Set PROBE_HOST, PROBE_USER, PROBE_PASS (and optionally PROBE_MAILBOX).");
        std::process::exit(2);
    };
    let port: u16 = env("PROBE_PORT").map_or(Ok(993), |value| value.parse())?;
    let mailbox = env("PROBE_MAILBOX").unwrap_or_else(|| "INBOX".to_string());
    let account = AccountConfig {
        host,
        port,
        username: user,
        password: Some(Secret::new_raw(&pass)),
        tls: true,
        max_connections: None,
    };

    // connect() negotiates DEFLATE and sends ID — reaching here proves both.
    let mut session = imap_client::connect(&account, &pass).await?;
    println!("connected (DEFLATE + ID negotiated)");

    // 1. Limited (default) mode: the visible window and its UID floor.
    let before = imap_client::examine(&mut session, &mailbox).await?;
    let visible = imap_client::search_uids(&mut session, "ALL").await?;
    let floor = visible.iter().min().copied().unwrap_or(0);
    println!(
        "\n[Limited mode] EXISTS={} UIDNEXT={:?}; visible UID floor={floor}, {} UIDs",
        before.exists,
        before.uid_next,
        visible.len()
    );

    // 2. Enter UID Mode.
    match imap_client::enable(&mut session, "UIDONLY").await {
        Ok(enabled) => println!("\nENABLE UIDONLY → OK; server ENABLED: {enabled:?}"),
        Err(error) => {
            println!("\nENABLE UIDONLY → FAILED: {error}\n(UID Mode unavailable on this server)");
            return Ok(());
        }
    }
    let after = imap_client::examine(&mut session, &mailbox).await?;
    println!(
        "[UID mode] EXAMINE EXISTS={} UIDNEXT={:?}",
        after.exists, after.uid_next
    );

    // 3. THE decisive test: fetch a UID range strictly BELOW the old window
    //    floor. Limited mode returned nothing here; UID Mode should not.
    if floor > 5_000 {
        let lo = floor - 5_000;
        let hi = floor - 4_991;
        let (rows, status) = raw_uid_fetch(
            &mut session,
            &format!("UID FETCH {lo}:{hi} (UID RFC822.SIZE)"),
        )
        .await?;
        println!(
            "\n[below-window] UID FETCH {lo}:{hi} → {} rows ({status}){}",
            rows.len(),
            if rows.is_empty() {
                "  — still windowed"
            } else {
                "  ← UID MODE OPENS THE FULL MAILBOX"
            }
        );
        for (uid, size) in rows.iter().take(3) {
            println!("    UID {uid} size {size:?}  (UIDFETCH parsed by the local patch)");
        }
    }

    // 4. PARTIAL pagination of a whole-mailbox fetch (newest 5).
    let (page, status) = raw_uid_fetch(
        &mut session,
        "UID FETCH 1:* (UID RFC822.SIZE) (PARTIAL -1:-5)",
    )
    .await?;
    println!(
        "\n[PARTIAL] UID FETCH 1:* … (PARTIAL -1:-5) → {} rows ({status})",
        page.len()
    );
    for (uid, size) in &page {
        println!("    UID {uid} size {size:?}");
    }

    println!(
        "\nVerdict: below-window rows > 0 confirms UID Mode lifts the window — the scan can\n\
         then walk the whole mailbox with UID FETCH 1:* + PARTIAL windows (≤ MESSAGELIMIT).\n\
         Any parsed UID above proves the imap-proto UIDFETCH patch works on real wire data."
    );
    Ok(())
}
