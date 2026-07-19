//! Empirical bisect probe for servers whose `HEADER.FIELDS` responses omit
//! requested headers (observed on AOL/Yahoo IMAP: `List-Unsubscribe` and
//! `List-Unsubscribe-Post` are missing while `List-Id`/`From`/`Date`/
//! `Message-ID` from the same request are returned).
//!
//! Read-only: every fetch uses `BODY.PEEK`, so no flags change.
//!
//! Usage:
//! ```sh
//! PROBE_HOST=imap.aol.com \
//! PROBE_USER='user@verizon.net' \
//! PROBE_PASS='app-password' \
//! PROBE_MAILBOX=INBOX \
//! PROBE_UID=110906 \
//! cargo run --example probe_header_fields
//! ```
//! `PROBE_PORT` defaults to 993, `PROBE_MAILBOX` to INBOX. Pick a `PROBE_UID`
//! of a recent bulk message (e.g. a LinkedIn or retailer mail) — one that
//! certainly carries List-Unsubscribe. Find one via `get_messages`.

use agentmail::config::AccountConfig;
use agentmail::imap_client::{self, extract_header_value_pub, timed_uid_fetch_collect_pub};
use agentmail::secret::Secret;

const VARIANTS: &[(&str, &str)] = &[
    (
        "1. combined (current scan shape)",
        "(UID BODY.PEEK[HEADER.FIELDS (List-Unsubscribe List-Unsubscribe-Post List-Id FROM DATE Message-ID)])",
    ),
    (
        "2. List-Unsubscribe alone (no prefix twin)",
        "(UID BODY.PEEK[HEADER.FIELDS (List-Unsubscribe FROM)])",
    ),
    (
        "3. List-Unsubscribe-Post alone",
        "(UID BODY.PEEK[HEADER.FIELDS (List-Unsubscribe-Post FROM)])",
    ),
    (
        "4. reordered pair (Post first)",
        "(UID BODY.PEEK[HEADER.FIELDS (List-Unsubscribe-Post List-Unsubscribe List-Id FROM DATE Message-ID)])",
    ),
    (
        "5. full header block (ground truth)",
        "(UID BODY.PEEK[HEADER])",
    ),
];

const HEADERS_OF_INTEREST: &[&str] = &[
    "List-Unsubscribe",
    "List-Unsubscribe-Post",
    "List-Id",
    "From",
    "Message-ID",
];

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (Some(host), Some(user), Some(pass), Some(uid)) = (
        env("PROBE_HOST"),
        env("PROBE_USER"),
        env("PROBE_PASS"),
        env("PROBE_UID"),
    ) else {
        eprintln!("Set PROBE_HOST, PROBE_USER, PROBE_PASS, and PROBE_UID (see example header).");
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

    let mut session = imap_client::connect(&account, &pass).await?;
    let caps = imap_client::ServerCaps::fetch(&mut session).await?;
    println!("connected; capability count: {}", caps_len(&caps));
    imap_client::examine(&mut session, &mailbox).await?;
    println!("examined {mailbox:?}; probing UID {uid}\n");

    for (label, items) in VARIANTS {
        println!("── {label}");
        println!("   items: {items}");
        let fetched = timed_uid_fetch_collect_pub(&mut session, &uid, items).await?;
        let mut block: Option<String> = None;
        for item in fetched {
            let fetch = item?;
            if fetch.uid.map(|value| value.to_string()).as_deref() == Some(uid.as_str()) {
                block = fetch
                    .header()
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
            }
        }
        match block {
            None => println!("   !! no header section returned at all"),
            Some(block) => {
                for name in HEADERS_OF_INTEREST {
                    match extract_header_value_pub(&block, name) {
                        Some(value) => {
                            let mut preview = value;
                            preview.truncate(80);
                            println!("   {name}: PRESENT  {preview}");
                        }
                        None => println!("   {name}: absent"),
                    }
                }
                println!("   (block: {} bytes)", block.len());
            }
        }
        println!();
    }

    println!(
        "Interpretation: if variant 5 shows List-Unsubscribe PRESENT while variant 1 shows it \
         absent, the server filters HEADER.FIELDS responses. If variants 2-4 differ from 1, the \
         prefix-overlapping name pair is the trigger and a reshaped request suffices (Branch A); \
         if 2-4 are all absent too, only a full-header fallback works (Branch B)."
    );
    Ok(())
}

fn caps_len(caps: &imap_client::ServerCaps) -> usize {
    // ServerCaps doesn't expose iteration; a has() spot-check keeps this
    // example decoupled from internals.
    ["IMAP4REV1", "IDLE", "MOVE", "UIDPLUS"]
        .iter()
        .filter(|cap| caps.has(cap))
        .count()
}
