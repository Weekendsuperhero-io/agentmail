use clap::{Parser, Subcommand};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "agentmail", about = "IMAP email client and MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Start the MCP server (JSON-RPC over stdio)
    Serve,
    /// List configured accounts
    ListAccounts,
    /// List mailboxes for an account
    ListMailboxes {
        #[arg(long)]
        account: Option<String>,
    },
    /// Create a new mailbox on the server
    CreateMailbox {
        #[arg(long)]
        account: String,
        #[arg(long)]
        name: String,
    },
    /// Check IMAP connection for an account
    CheckConnection {
        #[arg(long)]
        account: String,
    },
    /// List IMAP server capabilities for an account
    ListCapabilities {
        #[arg(long)]
        account: String,
    },
    /// Store a password in the system keychain for an account
    SetPassword {
        #[arg(long)]
        account: String,
    },
    /// Interactively configure a new IMAP account
    Configure {
        /// Provider preset: gmail, icloud, outlook, fastmail, or omit for custom
        provider: Option<String>,
    },
    /// List all flags in use across messages (omit --mailbox to scan all mailboxes)
    ListFlags {
        #[arg(long)]
        account: String,
        #[arg(long)]
        mailbox: Option<String>,
    },
    /// Rank senders by message count (omit --mailbox to scan all mailboxes)
    RankSenders {
        #[arg(long)]
        account: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Rank bulk-mail senders by List-Unsubscribe-Post presence, then count (omit --mailbox to scan all)
    RankUnsubscribe {
        #[arg(long)]
        account: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Find messages with attachments (omit --mailbox to scan all mailboxes)
    FindAttachments {
        #[arg(long)]
        account: String,
        #[arg(long)]
        mailbox: Option<String>,
        #[arg(long, default_value = "0")]
        offset: usize,
        #[arg(long, default_value = "25")]
        limit: usize,
    },
    /// Download attachments from a message
    DownloadAttachments {
        #[arg(long)]
        account: String,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long)]
        uid: u32,
        #[arg(long, default_value = ".")]
        output_dir: String,
    },
    /// Fetch messages from a mailbox (for testing)
    GetMessages {
        #[arg(long)]
        account: String,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long, default_value = "0")]
        offset: usize,
        #[arg(long, default_value = "10")]
        limit: usize,
        /// Include normalized markdown body content
        #[arg(long)]
        include_content: bool,
        /// Include the full raw headers map
        #[arg(long)]
        include_headers: bool,
    },
    /// Fetch specific messages by UID
    GetMessagesByUid {
        #[arg(long)]
        account: String,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long, num_args = 1..)]
        uids: Vec<u32>,
        #[arg(long, default_value = "false")]
        include_content: bool,
    },
    /// Add flags and/or set Apple Mail color on a message
    AddFlags {
        #[arg(long)]
        account: String,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long)]
        uid: u32,
        /// Flags to add (e.g. "\\Seen")
        #[arg(long)]
        flags: Vec<String>,
        /// Apple Mail color: red, orange, yellow, green, blue, purple, gray
        #[arg(long)]
        color: Option<String>,
    },
    /// Create a draft email
    CreateDraft {
        #[arg(long)]
        account: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
        #[arg(long, num_args = 1..)]
        to: Vec<String>,
        #[arg(long, num_args = 0..)]
        cc: Vec<String>,
        #[arg(long, num_args = 0..)]
        bcc: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing — logs to stderr so MCP JSON-RPC on stdout is unaffected.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    agentmail::credentials::init_keyring();
    let cli = Cli::parse();

    match cli.command.unwrap_or(CliCommand::Serve) {
        CliCommand::Serve => {
            let mk = agentmail::Agentmail::from_default_config().map_err(|e| {
                eprintln!("agentmail: failed to load config: {}", e);
                e
            })?;
            agentmail::mcp::serve_stdio(mk).await
        }
        CliCommand::ListAccounts => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk.list_accounts().await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::ListMailboxes { account } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk.list_mailboxes(account.as_deref()).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::CreateMailbox { account, name } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk.create_mailbox(&account, &name).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::CheckConnection { account } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let status = mk.check_connection(&account).await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        }
        CliCommand::ListCapabilities { account } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk.list_capabilities(&account).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::SetPassword { account } => {
            let config = agentmail::Config::load()?;
            let acct_config = config
                .accounts
                .get(&account)
                .ok_or_else(|| format!("Account '{}' not found in config", account))?;

            eprint!(
                "Enter password for {} ({}): ",
                account, acct_config.username
            );
            let mut password = String::new();
            std::io::stdin().read_line(&mut password)?;
            let password = password.trim();

            agentmail::credentials::set_password(&account, acct_config, password).await?;
            eprintln!("Password stored successfully.");
            Ok(())
        }
        CliCommand::Configure { provider } => configure_account(provider.as_deref()).await,
        CliCommand::ListFlags { account, mailbox } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk.list_flags(mailbox.as_deref(), &account, None).await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::DownloadAttachments {
            account,
            mailbox,
            uid,
            output_dir,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .download_attachments(&mailbox, &account, uid, std::path::Path::new(&output_dir))
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::FindAttachments {
            account,
            mailbox,
            offset,
            limit,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .find_attachments(mailbox.as_deref(), &account, offset, limit, None)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::RankSenders {
            account,
            mailbox,
            limit,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .group_by_sender(mailbox.as_deref(), &account, limit, None)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::RankUnsubscribe {
            account,
            mailbox,
            limit,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .group_by_list(mailbox.as_deref(), &account, limit, None)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::GetMessages {
            account,
            mailbox,
            offset,
            limit,
            include_content,
            include_headers,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .get_messages(
                    &mailbox,
                    &account,
                    offset,
                    limit,
                    include_content,
                    include_headers,
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::GetMessagesByUid {
            account,
            mailbox,
            uids,
            include_content,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .get_messages_by_uid(&mailbox, &account, &uids, include_content, false)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::AddFlags {
            account,
            mailbox,
            uid,
            flags,
            color,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .add_flags(&mailbox, &account, uid, &flags, color.as_deref())
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        CliCommand::CreateDraft {
            account,
            subject,
            body,
            to,
            cc,
            bcc,
        } => {
            let mk = agentmail::Agentmail::from_default_config()?;
            let value = mk
                .create_draft(&account, &subject, &body, &to, &cc, &bcc)
                .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Interactive account configuration
// ---------------------------------------------------------------------------

fn prompt(label: &str) -> Result<String, Box<dyn std::error::Error>> {
    eprint!("{}", label);
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

fn prompt_default(label: &str, default: &str) -> Result<String, Box<dyn std::error::Error>> {
    eprint!("{} [{}]: ", label, default);
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    let val = buf.trim();
    if val.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(val.to_string())
    }
}

struct ProviderPreset {
    host: &'static str,
    port: u16,
    username_hint: &'static str,
}

fn provider_preset(name: &str) -> Option<ProviderPreset> {
    match name {
        "gmail" => Some(ProviderPreset {
            host: "imap.gmail.com",
            port: 993,
            username_hint: "you@gmail.com",
        }),
        "icloud" => Some(ProviderPreset {
            host: "imap.mail.me.com",
            port: 993,
            username_hint: "your iCloud username (not full email)",
        }),
        "outlook" | "hotmail" | "live" => Some(ProviderPreset {
            host: "outlook.office365.com",
            port: 993,
            username_hint: "you@outlook.com",
        }),
        "fastmail" => Some(ProviderPreset {
            host: "imap.fastmail.com",
            port: 993,
            username_hint: "you@fastmail.com",
        }),
        "yahoo" => Some(ProviderPreset {
            host: "imap.mail.yahoo.com",
            port: 993,
            username_hint: "you@yahoo.com",
        }),
        _ => None,
    }
}

async fn configure_account(provider: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("agentmail account setup\n");

    // 1. Resolve provider
    let provider_name = match provider {
        Some(p) => p.to_lowercase(),
        None => {
            let val = prompt("Provider (gmail, icloud, outlook, fastmail, yahoo, custom): ")?;
            val.to_lowercase()
        }
    };
    let preset = provider_preset(&provider_name);

    // 2. Account name
    let default_name = if preset.is_some() {
        provider_name.clone()
    } else {
        String::new()
    };
    let account_name = if default_name.is_empty() {
        prompt("Account name: ")?
    } else {
        prompt_default("Account name", &default_name)?
    };
    if account_name.is_empty() {
        return Err("Account name cannot be empty".into());
    }

    // 3. Host / port / username
    let (host, port, username) = if let Some(ref p) = preset {
        let host = prompt_default("IMAP host", p.host)?;
        let port_str = prompt_default("IMAP port", &p.port.to_string())?;
        let port: u16 = port_str.parse().unwrap_or(p.port);
        eprintln!("  (hint: {})", p.username_hint);
        let username = prompt("Username: ")?;
        (host, port, username)
    } else {
        let host = prompt("IMAP host: ")?;
        let port_str = prompt_default("IMAP port", "993")?;
        let port: u16 = port_str.parse().unwrap_or(993);
        let username = prompt("Username: ")?;
        (host, port, username)
    };
    if host.is_empty() || username.is_empty() {
        return Err("Host and username are required".into());
    }

    // 4. Password method
    eprintln!("\nPassword storage:");
    eprintln!("  1. keyring  - Store in system keychain (recommended)");
    eprintln!("  2. command  - Read from a shell command at runtime");
    eprintln!("  3. raw      - Store in config file (not recommended)");
    let method = prompt_default("Method", "keyring")?;

    let (password_toml, need_store_password) = match method.as_str() {
        "command" | "cmd" | "2" => {
            let default_cmd = format!(
                "security find-internet-password -s {} -a {} -w",
                host, username
            );
            eprintln!("  (hint: use the default to read Apple Mail's stored password)");
            let cmd = prompt_default("Command", &default_cmd)?;
            (format!("password.cmd = {:?}", cmd), false)
        }
        "raw" | "3" => {
            let pw = prompt("Password: ")?;
            (format!("password.raw = {:?}", pw), false)
        }
        _ => {
            // keyring (default)
            (format!("password.keyring = {:?}", username), true)
        }
    };

    // 5. Write config file
    let config_path = agentmail::Config::default_path();
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Check if account already exists
    if config_path.exists() {
        let content = std::fs::read_to_string(&config_path)?;
        let key = format!("[accounts.{}]", account_name);
        if content.contains(&key) {
            return Err(format!(
                "Account '{}' already exists in {}. Edit the file directly to modify it.",
                account_name,
                config_path.display()
            )
            .into());
        }
    }

    let mut section = format!("\n[accounts.{}]\n", account_name);
    section.push_str(&format!("host = {:?}\n", host));
    if port != 993 {
        section.push_str(&format!("port = {}\n", port));
    }
    section.push_str(&format!("username = {:?}\n", username));
    section.push_str(&format!("{}\n", password_toml));

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config_path)?;
    file.write_all(section.as_bytes())?;
    eprintln!(
        "\nWrote account '{}' to {}",
        account_name,
        config_path.display()
    );

    // 6. Store password in keyring if needed
    if need_store_password {
        eprint!("Enter password for {} ({}): ", account_name, username);
        let mut pw = String::new();
        std::io::stdin().read_line(&mut pw)?;
        let pw = pw.trim();

        let entry =
            secret::keyring::KeyringEntry::try_new(&username).map_err(|e| format!("{}", e))?;
        let mut secret = secret::Secret::new_keyring_entry(entry);
        secret
            .set(pw)
            .await
            .map_err(|e| format!("Failed to store password: {}", e))?;
        eprintln!("Password stored in system keychain.");
    }

    // 7. Test connection
    let test = prompt_default("\nTest connection?", "y")?;
    if test.starts_with('y') || test.starts_with('Y') {
        eprintln!("Connecting to {}:{}...", host, port);
        let config = agentmail::Config::load()?;
        let mk = agentmail::Agentmail::new(config);
        let status = mk.check_connection(&account_name).await?;
        if status.connected {
            eprintln!("Connected successfully!");
        } else {
            eprintln!(
                "Connection failed: {}",
                status.error.as_deref().unwrap_or("unknown error")
            );
        }
    }

    Ok(())
}
