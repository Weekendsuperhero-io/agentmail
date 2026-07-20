//! Validate RFC 9586 UID Mode against a live server before wiring it into the
//! account scan. Answers the decisive questions:
//!   1. Does `ENABLE UIDONLY` succeed?
//!   2. Does it lift the visible-window limit — can we now `UID FETCH` BELOW
//!      the Limited-Mode floor (which returned nothing before)?
//!   3. Does the server reply with `UIDFETCH`, and does our patched parser
//!      surface the UID (including when the UID item is omitted)?
//!   4. Does `PARTIAL` paginate a `1:*` fetch?
//!   5. Does HEADER.FIELDS still deliver List-Id in UID Mode (same-message
//!      Limited-vs-UID compare + a below-the-old-floor sample)? The ranking
//!      projection depends on it.
//!
//! Read-only: EXAMINE + UID SEARCH + UID FETCH (BODY.PEEK for the header probe).
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

/// Tally of how HEADER.FIELDS body sections came back for a fetch. `with_from`
/// is the control: if bodies arrive at all it should track `total`, so a high
/// `from` with a zero `list_id` means List-Id was specifically stripped/absent
/// rather than the whole body section being empty.
#[derive(Default)]
struct HeaderStats {
    total: usize,
    with_body: usize,
    with_from: usize,
    with_list_id: usize,
    with_unsub: usize,
}

impl std::fmt::Display for HeaderStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} rows: body={} from={} list-id={} list-unsub={}",
            self.total, self.with_body, self.with_from, self.with_list_id, self.with_unsub
        )
    }
}

/// Run a HEADER.FIELDS fetch and tally what came back per row. Works in either
/// mode (FETCH or UIDFETCH responses) — the point is to compare the two.
async fn header_field_stats(
    session: &mut ImapSession,
    command: &str,
) -> Result<HeaderStats, Box<dyn std::error::Error>> {
    let tag = session.run_command(command).await?;
    let mut stats = HeaderStats::default();
    loop {
        let Some(response) = session.read_response().await? else {
            return Err("connection closed mid-fetch".into());
        };
        match response.parsed() {
            Response::Fetch(_, attrs) => {
                stats.total += 1;
                let mut body = String::new();
                for attr in attrs {
                    if let AttributeValue::BodySection {
                        data: Some(bytes), ..
                    } = attr
                    {
                        body.push_str(&String::from_utf8_lossy(bytes).to_ascii_lowercase());
                    }
                }
                if !body.trim().is_empty() {
                    stats.with_body += 1;
                }
                if body.contains("from:") {
                    stats.with_from += 1;
                }
                if body.contains("list-id:") {
                    stats.with_list_id += 1;
                }
                if body.contains("list-unsubscribe:") {
                    stats.with_unsub += 1;
                }
            }
            Response::Done { tag: done, .. } if done == &tag => break,
            _ => {}
        }
    }
    Ok(stats)
}

const HEADER_FIELDS: &str = "(BODY.PEEK[HEADER.FIELDS (List-Id List-Unsubscribe From)])";

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

    // 1b. Baseline: HEADER.FIELDS on the newest 200 in Limited Mode. This is
    //     the SAME 200 messages we re-sample after ENABLE, so any drop in the
    //     List-Id count is attributable to UID Mode, not to sampling.
    let limited = header_field_stats(
        &mut session,
        &format!("UID FETCH 1:* {HEADER_FIELDS} (PARTIAL -1:-200)"),
    )
    .await?;
    println!("[Limited headers] newest 200 → {limited}");

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

    // 4. Pagination the RIGHT way: fixed `PARTIAL -1:-N`, shrinking the UID
    //    range below the lowest UID of the prior page (Yahoo/AOL's idiom).
    let (page1, _) = raw_uid_fetch(
        &mut session,
        "UID FETCH 1:* (UID RFC822.SIZE) (PARTIAL -1:-5)",
    )
    .await?;
    let page1_low = page1.iter().map(|(uid, _)| *uid).min().unwrap_or(0);
    println!(
        "\n[page 1] UID FETCH 1:* … (PARTIAL -1:-5) → {} rows, lowest UID {page1_low}",
        page1.len()
    );
    if page1_low > 1 {
        let below = page1_low - 1;
        let (page2, status) = raw_uid_fetch(
            &mut session,
            &format!("UID FETCH 1:{below} (UID RFC822.SIZE) (PARTIAL -1:-5)"),
        )
        .await?;
        let page2_high = page2.iter().map(|(uid, _)| *uid).max().unwrap_or(0);
        println!(
            "[page 2] UID FETCH 1:{below} … (PARTIAL -1:-5) → {} rows, highest UID {page2_high} ({status}){}",
            page2.len(),
            if !page2.is_empty() && page2_high < page1_low {
                "  ← PAGINATION WORKS (older messages)"
            } else {
                "  — pagination did NOT advance"
            }
        );
    }

    // 5. Does HEADER.FIELDS still deliver List-Id in UID Mode? The ranking
    //    projection lives or dies on this. Compare three samples:
    //    (a) newest 200 in UID Mode — the SAME messages as the Limited baseline;
    //    (b) newest 200 at or below the old visible floor — mail Limited Mode
    //        could NEVER see, i.e. exactly what UID Mode unlocks.
    let uid_newest = header_field_stats(
        &mut session,
        &format!("UID FETCH 1:* {HEADER_FIELDS} (PARTIAL -1:-200)"),
    )
    .await?;
    println!("\n[UID headers] newest 200 → {uid_newest}");

    if floor > 200 {
        let deep = header_field_stats(
            &mut session,
            &format!("UID FETCH 1:{floor} {HEADER_FIELDS} (PARTIAL -1:-200)"),
        )
        .await?;
        println!("[UID headers] newest 200 below the old floor (UID ≤ {floor}) → {deep}");

        // 6. The make-or-break for top_subscriptions. HEADER.FIELDS strips
        //    List-Unsubscribe above; does the FULL header block (BODY.PEEK
        //    [HEADER], which no server filters) recover it? Same below-floor
        //    window, which carries real list mail. If `list-unsub` jumps from 0
        //    to >0 here, the full-header scan can rank subscriptions; if it is
        //    still 0, AOL does not expose List-Unsubscribe over IMAP at all and
        //    subscriptions must be inferred from List-Id instead.
        let full = header_field_stats(
            &mut session,
            &format!("UID FETCH 1:{floor} (BODY.PEEK[HEADER]) (PARTIAL -1:-200)"),
        )
        .await?;
        println!("[UID FULL headers] newest 200 below the old floor → {full}");
    }

    println!(
        "\nVerdict:\n\
         - page 2 older than page 1 → the shrinking-range walk covers the whole mailbox.\n\
         - Limited vs UID `list-id` on the newest 200 (same messages) are equal → HEADER.FIELDS\n\
           behaves identically in both modes; the low newest-window count is just personal mail.\n\
         - The below-floor `list-id` count is the real signal: that previously-invisible mail is\n\
           where the lists live, so ranking must walk the whole mailbox.\n\
         - Step 6 is decisive for top_subscriptions: FIELDS `list-unsub`=0 but FULL `list-unsub`>0\n\
           means the full-header scan recovers the unsubscribe flag HEADER.FIELDS strips. FULL\n\
           `list-unsub`=0 too means the header is not exposed over IMAP and subscriptions must fall\n\
           back to List-Id inference."
    );
    Ok(())
}
