//! Prove — with numbers, not vibes — whether agentmail actually HOLDS the IMAP
//! connections it opens, or re-LOGINs on every operation.
//!
//! It runs the same cheap read (`list_mailboxes`) twice against one account
//! with an idle gap in between (long enough to cross a keepalive tick, short
//! enough to stay reuse-eligible), then reads the pool's lifecycle counters:
//!
//!   fresh_logins == 1 && idle_reuses >= 1  → HELD  (one LOGIN, the rest reused)
//!   fresh_logins >= 2                       → NOT HELD (a LOGIN per operation)
//!
//! Both the Limited and UID-Mode pools share this hold/reuse/keepalive code and
//! feed the same counters, so this is representative of the ranking/sweep path
//! too. This proves the LIBRARY's behavior inside one process; if the library
//! holds here but the running app still re-LOGINs, the loss is in the app's
//! pool lifecycle (a new pool per mail-account toggle, a death-watch
//! reconnect), not in agentmail.
//!
//! ```sh
//! POOL_ACCOUNT=gmail cargo run --example pool_holds
//! POOL_ACCOUNT=aol POOL_GAP_SECS=35 cargo run --example pool_holds
//! ```
//! Read-only: it only lists mailboxes. Set RUST_LOG=agentmail::connection=debug
//! to also watch the per-acquire reuse/LOGIN lines.

use std::time::Duration;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let account = env("POOL_ACCOUNT").expect("set POOL_ACCOUNT to the account name to test");
    let gap = Duration::from_secs(env("POOL_GAP_SECS").map_or(Ok(35), |v| v.parse())?);
    let keepalive = Duration::from_secs(env("POOL_KEEPALIVE_SECS").map_or(Ok(30), |v| v.parse())?);

    let config = agentmail::Config::load()?;
    // Mirror the app: keepalive on, generous idle window.
    let mail = agentmail::Agentmail::builder(config)
        .keepalive(keepalive)
        .max_idle(Duration::from_secs(20 * 60))
        .build();

    eprintln!(
        "account={account}  keepalive={}s  idle_gap={}s  (gap must exceed keepalive and stay under the 5-min reuse floor)\n",
        keepalive.as_secs(),
        gap.as_secs(),
    );

    // --- Operation 1: the first acquire must pay a fresh LOGIN. -------------
    eprintln!("op 1: list_mailboxes …");
    let first = mail.list_mailboxes(Some(&account)).await;
    match &first {
        Ok(r) => eprintln!("  ok — {} mailboxes", r.mailboxes.len()),
        Err(e) => {
            // A cooldown/connect failure here isn't a holding result — say so.
            eprintln!("  failed: {e}");
            eprintln!(
                "\nCannot run the proof: the first connection didn't succeed. If this is an \
                 AOL/Yahoo LOGIN cooldown, wait for it to clear and retry; otherwise fix \
                 connectivity first."
            );
            return Ok(());
        }
    }
    let after1 = mail.connection_stats();
    eprintln!("  stats: {}\n", serde_json::to_string(&after1)?);

    // --- Idle gap: keepalive should NOOP the held session, not drop it. ----
    eprintln!(
        "holding idle for {}s (keepalive should ping the held session) …",
        gap.as_secs()
    );
    tokio::time::sleep(gap).await;

    // --- Operation 2: this one must REUSE the held session. ----------------
    eprintln!("\nop 2: list_mailboxes …");
    match mail.list_mailboxes(Some(&account)).await {
        Ok(r) => eprintln!("  ok — {} mailboxes", r.mailboxes.len()),
        Err(e) => eprintln!("  failed: {e}"),
    }
    let after2 = mail.connection_stats();
    eprintln!("  stats: {}\n", serde_json::to_string(&after2)?);

    // --- Verdict from the counters. ----------------------------------------
    println!("=== VERDICT ===");
    println!(
        "fresh_logins={}  idle_reuses={}  keepalive_pings={}  keepalive_drops={}",
        after2.fresh_logins, after2.idle_reuses, after2.keepalive_pings, after2.keepalive_drops
    );
    if after2.fresh_logins == 1 && after2.idle_reuses >= 1 {
        println!(
            "✅ HELD — one LOGIN total, the second operation reused the pooled session. \
             Connections are being held and reused."
        );
    } else if after2.fresh_logins >= 2 && after2.keepalive_drops >= 1 {
        println!(
            "❌ NOT HELD — the second operation paid another LOGIN because keepalive could not \
             keep the socket alive across the gap (keepalive_drops>0: the server closed the idle \
             connection). Shorten the keepalive interval below the server's idle timeout."
        );
    } else if after2.fresh_logins >= 2 {
        println!(
            "❌ NOT HELD — the second operation paid another LOGIN even though the session was \
             never dropped by keepalive. The session was not returned to (or not found in) the \
             pool. This is a holding bug in the pool path."
        );
    } else {
        println!(
            "⚠️  inconclusive — op 2 may not have completed a normal acquire (see its output \
             above)."
        );
    }
    Ok(())
}
