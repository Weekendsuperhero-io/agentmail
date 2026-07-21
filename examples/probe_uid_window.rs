//! Probe whether a windowed IMAP server (Yahoo/AOL pin the visible mailbox
//! to its newest ~10,000 messages) honors direct UID addressing BELOW the
//! visible window. Strict IMAP says no — `EXISTS` defines the session's
//! mailbox — but gateway servers are often looser, and if this one answers,
//! whole-mailbox scans and deletes can UID-walk past the window instead of
//! draining it via delete→backfill passes.
//!
//! Read-only: EXAMINE + SEARCH + FETCH (UID) only; nothing is modified.
//!
//! ```sh
//! PROBE_HOST=imap.aol.com PROBE_USER='user@verizon.net' \
//! PROBE_PASS='app-password' PROBE_MAILBOX=INBOX \
//! cargo run --example probe_uid_window
//! ```

use agentmail::config::AccountConfig;
use agentmail::imap_client::{self, timed_uid_fetch_collect_pub};
use agentmail::secret::Secret;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
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
        auth: agentmail::AuthMethod::Password,
    };
    let mut session = imap_client::connect(&account, &pass).await?;

    let mb = imap_client::examine(&mut session, &mailbox).await?;
    println!(
        "EXAMINE {mailbox:?}: EXISTS={} UIDNEXT={:?} UIDVALIDITY={:?}",
        mb.exists, mb.uid_next, mb.uid_validity
    );

    // The visible UID span.
    let visible = imap_client::search_uids(&mut session, "ALL").await?;
    let (min_visible, max_visible) = match (visible.iter().min(), visible.iter().max()) {
        (Some(min), Some(max)) => (*min, *max),
        _ => {
            println!("mailbox is empty; nothing to probe");
            return Ok(());
        }
    };
    println!(
        "UID SEARCH ALL: {} UIDs, visible span {min_visible}..{max_visible}",
        visible.len()
    );
    if u64::from(mb.exists) == visible.len() as u64 && min_visible > 1 {
        println!(
            "→ window suspected: {} visible of a UID space starting well above 1",
            visible.len()
        );
    }

    // Decisive test 1: UID SEARCH strictly below the visible window.
    let below_hi = min_visible.saturating_sub(1);
    let below_lo = min_visible.saturating_sub(20_000).max(1);
    if below_hi >= below_lo {
        let below =
            imap_client::search_uids(&mut session, &format!("UID {below_lo}:{below_hi}")).await?;
        println!(
            "UID SEARCH UID {below_lo}:{below_hi} (below window): {} hits{}",
            below.len(),
            if below.is_empty() {
                ""
            } else {
                " ← SERVER ANSWERS BEYOND THE WINDOW"
            }
        );
    }

    // Decisive test 2: direct UID FETCH below the window (some gateways
    // treat FETCH and SEARCH visibility differently).
    let fetch_lo = min_visible.saturating_sub(5_000).max(1);
    let fetch_hi = min_visible.saturating_sub(4_900).max(1);
    if fetch_hi > fetch_lo {
        let fetched =
            timed_uid_fetch_collect_pub(&mut session, &format!("{fetch_lo}:{fetch_hi}"), "(UID)")
                .await?;
        let hits = fetched
            .into_iter()
            .filter_map(|item| item.ok().and_then(|fetch| fetch.uid))
            .count();
        println!(
            "UID FETCH {fetch_lo}:{fetch_hi} (UID) (below window): {hits} hits{}",
            if hits == 0 {
                ""
            } else {
                " ← FETCH REACHES BEYOND THE WINDOW"
            }
        );
    }

    // Reference: does STATUS report the true count or the windowed count?
    let status = imap_client::mailbox_status(&mut session, &mailbox, false).await?;
    println!(
        "STATUS: MESSAGES={:?} UIDNEXT={:?} (vs EXAMINE EXISTS={})",
        status.exists, status.uid_next, mb.exists
    );

    println!(
        "\nInterpretation: any below-window hits mean the server honors direct UID\n\
         addressing past its visible view — scans/deletes can UID-walk the whole\n\
         mailbox. Zero hits everywhere means the window is absolute for this\n\
         session and delete→backfill drain passes are the only way through it."
    );
    Ok(())
}
