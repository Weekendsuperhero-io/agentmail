//! Dump the raw CAPABILITY list a server advertises after login — for
//! diffing provider endpoints (e.g. `imap.aol.com` vs the bulk-export
//! endpoint `export.imap.aol.com`, which serves a 10× larger mailbox window).
//!
//! ```sh
//! PROBE_HOST=export.imap.aol.com PROBE_USER='user@verizon.net' \
//! PROBE_PASS='app-password' cargo run --example probe_capabilities
//! ```

use agentmail::config::AccountConfig;
use agentmail::imap_client;
use agentmail::secret::Secret;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (Some(host), Some(user), Some(pass)) =
        (env("PROBE_HOST"), env("PROBE_USER"), env("PROBE_PASS"))
    else {
        eprintln!("Set PROBE_HOST, PROBE_USER, PROBE_PASS.");
        std::process::exit(2);
    };
    let port: u16 = env("PROBE_PORT").map_or(Ok(993), |value| value.parse())?;

    let account = AccountConfig {
        host: host.clone(),
        port,
        username: user,
        password: Some(Secret::new_raw(&pass)),
        tls: true,
        max_connections: None,
    };
    let mut session = imap_client::connect(&account, &pass).await?;
    let capabilities = session.capabilities().await?;
    let mut names: Vec<String> = capabilities
        .iter()
        .map(|capability| format!("{capability:?}"))
        .collect();
    names.sort();
    println!("{host}: {} capabilities", names.len());
    for name in names {
        println!("  {name}");
    }
    Ok(())
}
