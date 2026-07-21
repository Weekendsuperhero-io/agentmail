//! Prove that ONE UID-Mode connection serves both a ranking scan and ordinary
//! reads — no Limited↔UID mode switch, so no extra LOGIN — on a UIDONLY-capable
//! account (Yahoo/AOL).
//!
//! Born-UID-Mode (RFC 9586 `ENABLE UIDONLY` at connect) is on by default when
//! the header cache is persistent. This runs these steps, in order, on one
//! account:
//!
//! ```text
//! 1. check_connection         - opens the one born-UID connection
//! 2. list_capabilities        - is UIDONLY / MESSAGELIMIT advertised?
//! 3. list_mailboxes           - an ordinary LIST (Limited-style op)
//! 4. top_mailing_lists INBOX  - a UID-Mode ranking scan
//! 5. list_mailboxes           - ordinary again, reusing the SAME connection
//! ```
//!
//! It prints connection_stats() after each. On a UIDONLY account you should see
//! `fresh_logins == 1` the whole way: one connection, both kinds of work, zero
//! mode-switch logins. (On Gmail/Outlook — no UIDONLY — one Limited connection
//! serves everything just the same; born-UID simply doesn't apply.)
//!
//! ```sh
//! UID_ACCOUNT=aol cargo run --example uid_mode
//! ```
//! Read-only: it lists and ranks; nothing is modified. The first run warms the
//! cache (a full INBOX walk), so it is slower than subsequent runs.

use std::time::Duration;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn print_stats(mail: &agentmail::Agentmail, label: &str) {
    let s = mail.connection_stats();
    eprintln!(
        "   → fresh_logins={} idle_reuses={}   ({label})",
        s.fresh_logins, s.idle_reuses
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let account = env("UID_ACCOUNT").expect("set UID_ACCOUNT to the account name to test");
    let config = agentmail::Config::load()?;
    // Persistent cache (the default) => born-UID on for UIDONLY-capable accounts.
    let mail = agentmail::Agentmail::builder(config)
        .keepalive(Duration::from_secs(30))
        .max_idle(Duration::from_secs(20 * 60))
        .build();

    eprintln!("account={account}\n");

    // 1. Open the connection.
    eprintln!("1. check_connection …");
    let status = mail.check_connection(&account).await?;
    if !status.connected {
        eprintln!("   not connected: {:?}", status.error);
        eprintln!(
            "\nCannot run the proof — fix connectivity (or wait out an AOL login cooldown) and retry."
        );
        return Ok(());
    }
    print_stats(&mail, "after connect");

    // 2. Capabilities — does born-UID apply here?
    eprintln!("\n2. list_capabilities …");
    let caps = mail.list_capabilities(&account).await?;
    let uidonly = caps
        .capabilities
        .iter()
        .any(|c| c.eq_ignore_ascii_case("UIDONLY"));
    eprintln!(
        "   UIDONLY: {}",
        if uidonly {
            "yes — born-UID Mode applies"
        } else {
            "no — Limited Mode (normal for Gmail/Outlook)"
        }
    );
    if let Some(ml) = caps
        .capabilities
        .iter()
        .find(|c| c.to_uppercase().starts_with("MESSAGELIMIT"))
    {
        eprintln!("   {ml}");
    }
    print_stats(&mail, "after caps");

    // 3. An ordinary read (LIST — not a UID command).
    eprintln!("\n3. list_mailboxes (ordinary LIST) …");
    let boxes = mail.list_mailboxes(Some(&account)).await?;
    eprintln!("   {} mailboxes", boxes.mailboxes.len());
    print_stats(&mail, "after list");

    // 4. A UID-Mode ranking scan (walks INBOX by UID).
    eprintln!("\n4. top_mailing_lists INBOX (UID-Mode scan) …");
    let ranked = mail
        .top_mailing_lists(Some("INBOX"), &account, 0, 10, None, None)
        .await?;
    eprintln!(
        "   ranked {} lists across {} messages",
        ranked.unique_lists, ranked.total_messages
    );
    print_stats(&mail, "after rank");

    // 5. Ordinary read again — must reuse the SAME connection.
    eprintln!("\n5. list_mailboxes again (reuse) …");
    let _ = mail.list_mailboxes(Some(&account)).await?;
    print_stats(&mail, "after list #2");

    // Verdict.
    let s = mail.connection_stats();
    println!("\n=== VERDICT ===");
    println!(
        "fresh_logins={}  idle_reuses={}",
        s.fresh_logins, s.idle_reuses
    );
    if uidonly {
        if s.fresh_logins == 1 {
            println!(
                "✅ ONE born-UID connection served a UID-Mode ranking AND ordinary reads — \
                 no mode-switch LOGIN."
            );
        } else {
            println!(
                "❌ {} logins — expected 1. A Limited↔UID switch or a pool reset opened extra \
                 connections; capture the app log ('opened a fresh IMAP connection').",
                s.fresh_logins
            );
        }
    } else {
        println!(
            "ℹ️  account is not UIDONLY-capable, so born-UID does not apply (normal for \
             Gmail/Outlook). One Limited connection still served everything: fresh_logins={}.",
            s.fresh_logins
        );
    }
    Ok(())
}
